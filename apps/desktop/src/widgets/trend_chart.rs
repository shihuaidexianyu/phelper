//! TrendChart — 5-minute dual-series area chart fed by the
//! telemetry ring store via `AppHandle::history` (§39 passthrough; the
//! chart never touches collectors). Both series average onto ONE fixed
//! 120-bucket time grid (fmt::time_grid) — unique x labels, time-aligned
//! mixed cadences, sub-second jitter smoothed. Each chart contains metrics
//! with the same unit, and its y-axis uses a fixed physical range.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{
    App, BackgroundExecutor, Div, Hsla, IntoElement, ParentElement, Styled, div, linear_color_stop,
    linear_gradient, px,
};
use gpui_component::{ActiveTheme, StyledExt, chart::AreaChart};
use phelper_core::app::fmt;
use phelper_core::app::runtime::AppHandle;
use phelper_domain::telemetry::MetricId;

const WINDOW: Duration = Duration::from_secs(300);
const MAX_POINTS: usize = 120;
/// Design pull cadence for chart data (§39 note in the M6 plan): the page
/// re-renders at the 50 ms state tick, but history pulls + downsampling +
/// label formatting happen at 1 Hz behind this cache. The work is performed
/// by the GPUI background executor so a cold chart never blocks the render
/// thread.
const PULL_EVERY: Duration = Duration::from_secs(1);

#[derive(Default)]
struct ChartCacheState {
    ready_at: Option<Instant>,
    points: Vec<TrendPoint>,
    loading: bool,
    revision: u64,
}

/// One chart's cached points. The cache is shared with a background task so
/// page render functions only perform a short lock + clone of ready data.
#[derive(Default)]
pub struct ChartCache(Arc<Mutex<ChartCacheState>>);

impl ChartCache {
    pub fn points(
        &self,
        app: &AppHandle,
        id_a: MetricId,
        id_b: MetricId,
        background_executor: BackgroundExecutor,
    ) -> Vec<TrendPoint> {
        let should_load = {
            let mut state = self.0.lock().expect("chart cache lock poisoned");
            let expired = state
                .ready_at
                .is_none_or(|ready_at| ready_at.elapsed() >= PULL_EVERY);
            if expired && !state.loading {
                state.loading = true;
                true
            } else {
                false
            }
        };

        if should_load {
            let cache = Arc::clone(&self.0);
            let app = app.clone();
            background_executor
                .spawn(async move {
                    let points = trend_points(&app, id_a, id_b);
                    let mut state = cache.lock().expect("chart cache lock poisoned");
                    state.points = points;
                    state.ready_at = Some(Instant::now());
                    state.loading = false;
                    state.revision = state.revision.wrapping_add(1);
                })
                .detach();
        }

        self.0
            .lock()
            .expect("chart cache lock poisoned")
            .points
            .clone()
    }

    /// Changes whenever a background refresh publishes a new snapshot. The
    /// shell includes this in its visual fingerprint so the completed task
    /// gets painted on the next 50 ms tick without polling the chart data.
    pub fn revision(&self) -> u64 {
        self.0.lock().expect("chart cache lock poisoned").revision
    }

    pub fn is_ready(&self) -> bool {
        self.0
            .lock()
            .expect("chart cache lock poisoned")
            .ready_at
            .is_some()
    }
}

#[derive(Clone)]
pub struct TrendPoint {
    pub time: String,
    pub a: f64,
    pub b: f64,
}

/// Pull both histories onto ONE fixed 120-bucket time grid (2.5 s each)
/// and emit the intersection. The grid is the fix for the M6 "loopy
/// chart": gpui-component's `ScalePoint` resolves an x value by FIRST
/// match, so duplicate "M:SS" labels collapsed several points onto one x
/// and the polyline drew vertical zigzags — worst right after startup
/// (250 ms samples → ~4 points per 1 s label). Bucket ages are always
/// unique, mixed cadences (250 ms CPU vs 500 ms GPU) align by TIME
/// instead of index, and per-bucket averaging smooths sub-second jitter.
pub fn trend_points(app: &AppHandle, id_a: MetricId, id_b: MetricId) -> Vec<TrendPoint> {
    let now = Instant::now();
    let pairs = |id: MetricId| -> Vec<(f64, f64)> {
        app.history(id, WINDOW)
            .iter()
            .map(|s| {
                (
                    now.saturating_duration_since(s.timestamp).as_secs_f64(),
                    s.value.as_f64().unwrap_or(0.0),
                )
            })
            .collect()
    };
    let ga = fmt::time_grid(&pairs(id_a), WINDOW.as_secs_f64(), MAX_POINTS);
    let gb: std::collections::BTreeMap<usize, f64> =
        fmt::time_grid(&pairs(id_b), WINDOW.as_secs_f64(), MAX_POINTS)
            .into_iter()
            .collect();
    let width = WINDOW.as_secs_f64() / MAX_POINTS as f64;
    // Intersection only — never paint a series value the store didn't
    // produce. Buckets are indexed 0 = newest; the chart wants oldest first.
    ga.iter()
        .rev()
        .filter_map(|(bucket, av)| {
            gb.get(bucket).map(|bv| {
                let age = ((*bucket as f64) + 0.5) * width;
                let age = age as u64;
                TrendPoint {
                    time: format!("{}:{:02}", age / 60, age % 60),
                    a: *av,
                    b: *bv,
                }
            })
        })
        .collect()
}

/// One series of a TrendChart (label + color).
pub struct TrendSeries {
    pub name: &'static str,
    pub color: Hsla,
}

/// Fixed physical range used to map a series to the chart's stable 0..1
/// plotting space and to render its y-axis labels.
#[derive(Clone, Copy)]
pub struct TrendRange {
    pub min: f64,
    pub max: f64,
}

impl TrendRange {
    pub const fn new(min: f64, max: f64) -> Self {
        Self { min, max }
    }

    fn normalize(self, value: f64) -> f64 {
        if !value.is_finite() || !self.min.is_finite() || !self.max.is_finite() {
            return 0.0;
        }
        if self.max <= self.min {
            return 0.0;
        }
        ((value - self.min) / (self.max - self.min)).clamp(0.0, 1.0)
    }

    fn tick_labels(self, unit: &'static str) -> [String; 5] {
        let step = (self.max - self.min) / 4.0;
        std::array::from_fn(|index| {
            let value = self.max - step * index as f64;
            format!("{value:.0} {unit}")
        })
    }
}

pub const TEMPERATURE_RANGE: TrendRange = TrendRange::new(0.0, 100.0);
pub const POWER_RANGE: TrendRange = TrendRange::new(0.0, 200.0);

#[derive(Clone)]
struct PlotPoint {
    time: String,
    a: f64,
    b: f64,
}

/// A titled chart card with two same-unit, fixed-range trend series.
pub fn render(
    title: &str,
    points: Vec<TrendPoint>,
    a: TrendSeries,
    b: TrendSeries,
    range: TrendRange,
    unit: &'static str,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let bg = theme.background;
    let points = points
        .into_iter()
        .map(|point| PlotPoint {
            time: point.time,
            a: range.normalize(point.a),
            b: range.normalize(point.b),
        })
        .collect::<Vec<_>>();
    let grad = |c: Hsla| {
        linear_gradient(
            0.,
            linear_color_stop(c.opacity(0.35), 1.),
            linear_color_stop(bg.opacity(0.05), 0.),
        )
    };
    div()
        .v_flex()
        .gap_2()
        .p_3()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_box)
        .child(
            div()
                .h_flex()
                .gap_4()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(title.to_string()),
                )
                .child(legend(a.name, a.color))
                .child(legend(b.name, b.color)),
        )
        .child(
            div()
                .h_flex()
                .w_full()
                .child(y_axis(range, unit, cx))
                .child(
                    div().h(px(160.)).flex_1().min_w_0().child(
                        AreaChart::new(points)
                            .x(|d| d.time.clone())
                            .y(|d| d.a)
                            .stroke(a.color)
                            .fill(grad(a.color))
                            .name(a.name)
                            .y(|d| d.b)
                            .stroke(b.color)
                            .fill(grad(b.color))
                            .name(b.name)
                            // AreaChart derives its y-domain from the current
                            // data. This transparent ceiling pins that domain
                            // to 0..1 after physical values are normalized.
                            .y(|_| 1.0_f64)
                            .stroke(bg.opacity(0.0))
                            .fill(bg.opacity(0.0))
                            .tick_margin(15),
                    ),
                ),
        )
}

/// A fixed-size chart card shown while the first history snapshot is being
/// prepared. It keeps the page geometry stable without inventing values.
pub fn skeleton(title: &str, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .v_flex()
        .gap_2()
        .p_3()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_box)
        .child(
            div()
                .h_flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(title.to_string()),
                )
                .child(
                    div()
                        .h(px(10.))
                        .flex_1()
                        .rounded_sm()
                        .bg(theme.muted.opacity(0.18)),
                ),
        )
        .child(
            div()
                .h_flex()
                .h(px(160.))
                .child(
                    div()
                        .w(px(62.))
                        .h_full()
                        .border_r_1()
                        .border_color(theme.border),
                )
                .child(
                    div()
                        .h_full()
                        .flex_1()
                        .rounded_md()
                        .bg(theme.muted.opacity(0.10)),
                ),
        )
}

fn y_axis(range: TrendRange, unit: &'static str, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .v_flex()
        .justify_between()
        .h(px(160.))
        .w(px(62.))
        .py(px(2.))
        .border_r_1()
        .border_color(theme.border)
        .children(range.tick_labels(unit).into_iter().map(|label| {
            div()
                .text_xs()
                .text_right()
                .text_color(theme.muted_foreground)
                .child(label)
        }))
}

fn legend(name: &'static str, color: Hsla) -> impl IntoElement {
    div()
        .h_flex()
        .gap_1()
        .items_center()
        .child(div().size_2().rounded_full().bg(color))
        .child(div().text_xs().child(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_range_keeps_plot_coordinates_stable() {
        let range = TrendRange::new(0.0, 100.0);
        assert_eq!(range.normalize(0.0), 0.0);
        assert_eq!(range.normalize(50.0), 0.5);
        assert_eq!(range.normalize(100.0), 1.0);
        assert_eq!(range.normalize(120.0), 1.0);
    }

    #[test]
    fn invalid_range_and_samples_do_not_break_render_data() {
        assert_eq!(TrendRange::new(1.0, 1.0).normalize(2.0), 0.0);
        assert_eq!(TrendRange::new(0.0, 100.0).normalize(f64::NAN), 0.0);
    }
}
