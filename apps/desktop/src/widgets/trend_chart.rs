//! TrendChart — 5-minute dual-series area chart (temp + power) fed by the
//! telemetry ring store via `AppHandle::history` (§39 passthrough; the
//! chart never touches collectors). Series are index-aligned after
//! bucket-downsampling to ≤120 points (fmt::downsample).

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

/// Pull both histories, downsample each, align by index (the two series
/// come from the same collector tick, so counts match; truncation to the
/// shorter guards the rest). x labels are age-from-now of the A sample.
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
    let da = fmt::downsample(&pairs(id_a), MAX_POINTS);
    let db = fmt::downsample(&pairs(id_b), MAX_POINTS);
    let n = da.len().min(db.len());
    // Histories arrive oldest→newest; trim to the NEWEST n on both sides.
    let da = &da[da.len() - n..];
    let db = &db[db.len() - n..];
    da.iter()
        .zip(db)
        .map(|((age, a), (_, b))| {
            let age = *age as u64;
            TrendPoint {
                time: format!("{}:{:02}", age / 60, age % 60),
                a: *a,
                b: *b,
            }
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
