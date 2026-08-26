//! JournalView — the control journal live tail (§48), newest first. Each
//! row: relative time, origin badge, command summary, status, duration;
//! expanded rows show the §56 per-step evidence table (backend / firmware
//! return / before→after / verification). Cross-process: CLI writes show
//! up here within ~2 s via the pump's 1 Hz tailer.

use std::collections::{BTreeSet, VecDeque};

use gpui::{Context, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div, px};
use gpui_component::{ActiveTheme, StyledExt};
use phelper_core::app::fmt;
use phelper_core::control::journal::{JournalEntry, JournalOrigin};

/// Stable-ish row key (entries carry no id).
pub fn key_of(e: &JournalEntry, index: usize) -> String {
    format!("{}-{:?}-{}", e.at_epoch_ms, e.origin, index)
}

pub fn render<V: 'static>(
    entries: &VecDeque<JournalEntry>,
    expanded: &BTreeSet<String>,
    limit: usize,
    empty_hint: &str,
    cx: &mut Context<V>,
    on_toggle: impl Fn(&mut V, String) + 'static + Copy,
    on_more: impl Fn(&mut V) + 'static + Copy,
) -> impl IntoElement {
    let theme = cx.theme();
    let now = phelper_core::app::now_epoch_ms();

    let mut list = div().v_flex().gap_1();
    if entries.is_empty() {
        return div()
            .v_flex()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(empty_hint.to_string()),
            );
    }

    // v0.2: render at most `limit` rows — the Diagnostics page re-renders
    // at the 250 ms tick and a full 200-entry tail (each row a multi-div
    // tree with per-row listeners) was the page's heaviest element cost.
    // The remaining rows sit one click away behind 显示更多.
    let total = entries.len();
    for (i, e) in entries.iter().rev().take(limit).enumerate() {
        let key = key_of(e, i);
        let is_open = expanded.contains(&key);

        let origin_color = match e.origin {
            JournalOrigin::User => theme.info,
            JournalOrigin::Keepalive => theme.muted_foreground,
            JournalOrigin::Safety => theme.warning,
            JournalOrigin::Shutdown => theme.danger,
        };
        let status = fmt::control_status_zh(&e.outcome.status);
        let status_color = match &e.outcome.status {
            phelper_domain::command::ControlStatus::Applied { .. } => theme.success,
            phelper_domain::command::ControlStatus::Rejected { .. } => theme.danger,
            phelper_domain::command::ControlStatus::Partial => theme.warning,
        };

        let header = {
            let key_c = key.clone();
            div()
                .id(SharedString::from(format!("jr-{key}")))
                .h_flex()
                .gap_2()
                .px_2()
                .py_1()
                .w_full()
                .rounded_md()
                .cursor_pointer()
                .hover(|s| s.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _: &mut Window, cx| {
                    on_toggle(this, key_c.clone());
                    cx.notify();
                }))
                .child(
                    div()
                        .w(px(76.))
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(fmt::age_zh(now, e.at_epoch_ms)),
                )
                .child(
                    div()
                        .w(px(48.))
                        .text_xs()
                        .text_color(origin_color)
                        .child(fmt::journal_origin_zh(e.origin)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .child(fmt::command_summary_zh(&e.outcome.command)),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(status_color)
                        .child(status),
                )
                .child(
                    div()
                        .w(px(64.))
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .text_right()
                        .child(format!("{} ms", e.outcome.duration.as_millis())),
                )
        };

        let mut row = div().v_flex().w_full().child(header);
        if is_open && !e.outcome.steps.is_empty() {
            let mut steps = div()
                .v_flex()
                .gap_1()
                .ml_4()
                .mt_1()
                .mb_2()
                .p_2()
                .rounded_md()
                .bg(theme.muted.opacity(0.3));
            for s in &e.outcome.steps {
                steps = steps.child(
                    div()
                        .v_flex()
                        .gap_px()
                        .child(
                            div()
                                .h_flex()
                                .gap_2()
                                .child(div().text_sm().child(s.step.clone()))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(s.backend.clone()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.info)
                                        .child(fmt::verification_zh(&s.verification)),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!(
                                    "{} · {} → {}",
                                    s.firmware_return.clone().unwrap_or_else(|| "—".into()),
                                    s.before.clone().unwrap_or_else(|| "—".into()),
                                    s.after.clone().unwrap_or_else(|| "—".into())
                                )),
                        ),
                );
            }
            row = row.child(steps);
        }
        list = list.child(row);
    }
    if total > limit {
        list = list.child(
            div()
                .id("jr-more")
                .h_flex()
                .justify_center()
                .w_full()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .hover(|s| s.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _: &mut Window, cx| {
                    on_more(this);
                    cx.notify();
                }))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.info)
                        .child(format!("显示更多（还有 {} 条）", total - limit)),
                ),
        );
    }
    list
}
