//! Monitor (plan D-G): the full registry::all() metric table — id / live
//! value / owner / live source / quality / age / cadence / note — with a
//! substring filter on the metric id. owner ≠ live source rows highlight
//! the source (a fallback is in use); samples older than 3× cadence gray
//! out (stale ≠ live). Pure read — writes_available is irrelevant here.

use std::time::Instant;

use gpui::{Context, Hsla, IntoElement, ParentElement, Styled, div, prelude::FluentBuilder, px};
use gpui_component::{ActiveTheme, StyledExt, input::Input};
use phelper_core::app::fmt;
use phelper_core::app::AppState;
use phelper_core::telemetry::registry;
use phelper_domain::telemetry::MetricQuality;

use crate::shell::{MonitorState, ShellView};

use super::dashboard::page_root;

fn value_zh(v: f64) -> String {
    if v.abs() >= 100. { format!("{v:.0}") } else { format!("{v:.1}") }
}

fn quality_zh(q: MetricQuality) -> &'static str {
    match q {
        MetricQuality::Fresh => "新鲜",
        MetricQuality::Estimated => "估计",
        MetricQuality::Stale => "陈旧",
        _ => "不可用",
    }
}

fn cadence_zh(d: std::time::Duration) -> String {
    if d.as_millis() < 1000 { format!("{} ms", d.as_millis()) } else { format!("{} s", d.as_secs()) }
}

pub fn render(
    state: &AppState,
    mon: &MonitorState,
    cx: &mut Context<ShellView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let q = mon.filter.read(cx).text().to_string().to_lowercase();
    let snap = state.telemetry.as_deref();
    let now = Instant::now();

    // Column layout shared by header and rows.
    let w_id = px(190.);
    let w_val = px(110.);
    let w_src = px(105.);
    let w_q = px(60.);
    let w_age = px(70.);
    let w_cad = px(60.);

    let header = div()
        .h_flex()
        .gap_2()
        .py_1()
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(div().w(w_id).child("指标"))
        .child(div().w(w_val).child("值"))
        .child(div().w(w_src).child("归属"))
        .child(div().w(w_src).child("实际来源"))
        .child(div().w(w_q).child("质量"))
        .child(div().w(w_age).child("数据年龄"))
        .child(div().w(w_cad).child("周期"))
        .child(div().flex_1().child("备注"));

    let mut shown = 0usize;
    let mut rows = div().v_flex().w_full();
    for meta in registry::all() {
        if !q.is_empty() && !meta.id.0.contains(q.as_str()) {
            continue;
        }
        shown += 1;
        let sample = snap.and_then(|s| s.samples.get(&meta.id));
        let (value_s, live_source, quality, age_s, stale) = match sample {
            Some(s) => {
                let age = now.saturating_duration_since(s.timestamp);
                (
                    s.value.as_f64().map(value_zh).unwrap_or_else(|| "—".into()),
                    Some(s.source),
                    s.quality,
                    if age.as_secs() >= 60 {
                        format!("{} 分", age.as_secs() / 60)
                    } else {
                        format!("{} 秒", age.as_secs())
                    },
                    age > meta.cadence * 3,
                )
            }
            None => ("—".into(), None, MetricQuality::Unavailable, "—".into(), true),
        };
        let fallback = live_source.is_some_and(|src| src != meta.owner);
        let q_color: Hsla = match quality {
            MetricQuality::Fresh => theme.success,
            MetricQuality::Estimated | MetricQuality::Stale => theme.warning,
            _ => theme.muted_foreground,
        };
        let val_color = if stale { theme.muted_foreground } else { theme.foreground };
        rows = rows.child(
            div()
                .h_flex()
                .gap_2()
                .py_px()
                .child(div().w(w_id).text_sm().child(meta.id.0.to_string()))
                .child(
                    div().w(w_val).text_sm().text_color(val_color).child(format!(
                        "{} {}",
                        value_s, meta.unit
                    )),
                )
                .child(
                    div()
                        .w(w_src)
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(fmt::source_zh(meta.owner)),
                )
                .child(
                    div()
                        .w(w_src)
                        .text_sm()
                        .when(fallback, |d| d.text_color(theme.warning).font_semibold())
                        .child(live_source.map(fmt::source_zh).unwrap_or("—").to_string()),
                )
                .child(div().w(w_q).text_sm().text_color(q_color).child(quality_zh(quality)))
                .child(
                    div()
                        .w(w_age)
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(age_s),
                )
                .child(
                    div()
                        .w(w_cad)
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(cadence_zh(meta.cadence)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(meta.note),
                ),
        );
    }

    page_root("monitor-scroll").child(
        div()
            .v_flex()
            .gap_2()
            .p_4()
            .w_full()
            .child(
                div()
                    .h_flex()
                    .gap_3()
                    .items_center()
                    .child(div().w(px(280.)).child(Input::new(&mon.filter)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "显示 {shown} / {} 项指标 · 高亮 = 回退源使用中 · 灰 = 超过 3× 周期未更新",
                                registry::all().len()
                            )),
                    ),
            )
            .child(header)
            .child(rows),
    )
}
