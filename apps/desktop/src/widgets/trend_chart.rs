//! TrendChart — 5-minute dual-series area chart (temp + power) fed by the
//! telemetry ring store via `AppHandle::history` (§39 passthrough; the
//! chart never touches collectors). Both series average onto ONE fixed
//! 120-bucket time grid (fmt::time_grid) — unique x labels, time-aligned
//! mixed cadences, sub-second jitter smoothed.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use gpui::{App, Hsla, IntoElement, ParentElement, Styled, div, linear_color_stop, linear_gradient, px};
use gpui_component::{ActiveTheme, StyledExt, chart::AreaChart};
use phelper_core::app::fmt;
use phelper_core::app::runtime::AppHandle;
use phelper_domain::telemetry::MetricId;

const WINDOW: Duration = Duration::from_secs(300);
const MAX_POINTS: usize = 120;
/// Design pull cadence for chart data (§39 note in the M6 plan): the page
/// re-renders at the 250 ms tick, but history pulls + downsampling +
/// label formatting happen at 1 Hz behind this cache (v0.2 — v0.1
/// re-pulled every tick, 4× the designed rate).
const PULL_EVERY: Duration = Duration::from_secs(1);

/// One chart's cached points. Interior-mutable so page render functions
/// keep their `&` signatures.
#[derive(Default)]
pub struct ChartCache(RefCell<Option<(Instant, Vec<TrendPoint>)>>);

impl ChartCache {
    pub fn points(&self, app: &AppHandle, id_a: MetricId, id_b: MetricId) -> Vec<TrendPoint> {
        if let Some((at, pts)) = &*self.0.borrow()
            && at.elapsed() < PULL_EVERY
        {
            return pts.clone();
        }
        let pts = trend_points(app, id_a, id_b);
        *self.0.borrow_mut() = Some((Instant::now(), pts.clone()));
        pts
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

/// A titled chart card. Series A = temperature, B = power (by convention
/// of the callers); colors from the theme chart palette.
pub fn render(
    id: &'static str,
    title: &str,
    points: Vec<TrendPoint>,
    a: TrendSeries,
    b: TrendSeries,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let bg = theme.background;
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
                .child(div().text_sm().text_color(theme.muted_foreground).child(title.to_string()))
                .child(legend(a.name, a.color))
                .child(legend(b.name, b.color)),
        )
        .child(
            div().h(px(160.)).child(
                AreaChart::new(points)
                    .id(id)
                    .x(|d| d.time.clone())
                    .y(|d| d.a)
                    .stroke(a.color)
                    .fill(grad(a.color))
                    .name(a.name)
                    .y(|d| d.b)
                    .stroke(b.color)
                    .fill(grad(b.color))
                    .name(b.name)
                    .tick_margin(15),
            ),
        )
}

fn legend(name: &'static str, color: Hsla) -> impl IntoElement {
    div()
        .h_flex()
        .gap_1()
        .items_center()
        .child(div().size_2().rounded_full().bg(color))
        .child(div().text_xs().child(name))
}
