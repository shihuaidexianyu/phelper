//! KnobRow — the write-control row anatomy (plan D-F): label + control +
//! observed/expected value + lifecycle status badge + disabled reason.
//! Plus the outcome banner (§34: one human sentence, technical detail one
//! expand away). Pure presentation; dispatch happens in the page/shell.

use gpui::{App, Context, Div, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px};
use gpui_component::{ActiveTheme, StyledExt};
use phelper_core::app::fmt;
use phelper_core::app::state::{KnobStatus, OutcomeRecord};
use phelper_domain::command::{ControlStatus, Verification};

/// Short lifecycle badge for a knob row; `None` when Idle.
pub fn status_badge(cx: &App, status: &KnobStatus) -> Option<Div> {
    let theme = cx.theme();
    let (text, color) = match status {
        KnobStatus::Idle => return None,
        KnobStatus::Pending => ("排队中…", theme.info),
        KnobStatus::InFlight(_) => ("执行中…", theme.info),
        KnobStatus::Applied { verification, .. } => match verification {
            Verification::Verified => ("已验证", theme.success),
            Verification::TrustedNoReadback => ("信任写入", theme.success),
            Verification::Skipped => ("已应用", theme.muted_foreground),
            Verification::Failed { .. } => ("验证失败", theme.danger),
        },
        KnobStatus::Partial { .. } => ("部分完成", theme.warning),
        KnobStatus::Failed { .. } => ("失败", theme.danger),
    };
    Some(
        div()
            .w(px(72.))
            .text_xs()
            .text_color(color)
            .text_right()
            .child(text),
    )
}

/// One knob row. `observed` is the honest AR-10 readback text ("当前：42
/// （已验证）" / "期望值" for knobs with no readback channel).
#[allow(clippy::too_many_arguments)]
pub fn knob_row(
    cx: &App,
    label: &'static str,
    control: impl IntoElement,
    observed: String,
    status: &KnobStatus,
    disabled_reason: Option<&'static str>,
) -> Div {
    let theme = cx.theme();
    let disabled = disabled_reason.is_some();
    let mut row = div()
        .h_flex()
        .gap_3()
        .w_full()
        .py_1()
        .child(
            div()
                .w(px(150.))
                .text_sm()
                .when(disabled, |d| d.text_color(theme.muted_foreground))
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .when(disabled, |d| d.opacity(0.45))
                .child(control),
        )
        .child(
            div()
                .w(px(190.))
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(observed),
        );
    if let Some(badge) = status_badge(cx, status) {
        row = row.child(badge);
    } else {
        row = row.child(div().w(px(72.)));
    }
    let mut col = div().v_flex().w_full().child(row);
    if let Some(reason) = disabled_reason {
        col = col.child(
            div()
                .ml(px(162.))
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(reason),
        );
    }
    col
}

/// §34 outcome banner: one human sentence for the latest outcome; expand
/// for the per-step evidence table + the raw error detail.
pub fn outcome_banner<V: 'static>(
    cx: &mut Context<V>,
    record: Option<&OutcomeRecord>,
    expanded: bool,
    on_toggle: impl Fn(&mut V) + 'static + Copy,
) -> Option<impl IntoElement> {
    let theme = cx.theme();
    let rec = record?;
    let now = phelper_core::app::now_epoch_ms();

    let (status_text, color) = match &rec.outcome.status {
        ControlStatus::Applied { verification } => {
            (format!("已应用 · {}", fmt::verification_zh(verification)), theme.success)
        }
        ControlStatus::Rejected { error } => {
            (format!("已拒绝 · {}", fmt::control_error_zh(error)), theme.danger)
        }
        ControlStatus::Partial => ("部分完成（多步计划中途失败）".into(), theme.warning),
    };

    let mut body = div()
        .v_flex()
        .gap_1()
        .child(
            div()
                .h_flex()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(color)
                        .child(fmt::command_summary_zh(&rec.outcome.command)),
                )
                .child(div().text_sm().text_color(color).child(status_text))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(fmt::age_zh(now, rec.at_epoch_ms)),
                )
                .child(
                    div()
                        .id(SharedString::from("banner-toggle"))
                        .px_2()
                        .rounded_md()
                        .cursor_pointer()
                        .text_xs()
                        .text_color(theme.info)
                        .hover(|s| s.bg(theme.list_hover))
                        .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _: &mut Window, cx| {
                            on_toggle(this);
                            cx.notify();
                        }))
                        .child(if expanded { "收起详情" } else { "展开详情" }),
                ),
        );

    if expanded {
        let mut steps = div().v_flex().gap_1().mt_1();
        for s in &rec.outcome.steps {
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
        if let ControlStatus::Rejected { error } = &rec.outcome.status {
            steps = steps.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("错误详情：{error:?}")),
            );
        }
        body = body.child(steps);
    }

    Some(
        div()
            .w_full()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(color.opacity(0.5))
            .bg(color.opacity(0.08))
            .child(body),
    )
}
