//! KnobRow — the write-control row anatomy (plan D-F): label + control +
//! observed/expected value + lifecycle status badge + disabled reason.
//! Plus the outcome banner (§34: one human sentence, technical detail one
//! expand away). Pure presentation; dispatch happens in the page/shell.

use gpui::{
    App, Context, Div, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{ActiveTheme, StyledExt};
use phelper_core::app::fmt;
use phelper_core::app::state::{KnobStatus, OutcomeRecord};
use phelper_domain::command::ControlCommand;
use phelper_domain::command::{ControlStatus, Verification};
use phelper_domain::error::ControlError;
use phelper_domain::policy::FanMode;

fn command_label(cmd: &ControlCommand) -> String {
    match cmd {
        ControlCommand::ApplyProfile { profile } => format!("配置档 · {profile}"),
        ControlCommand::SetCpuPolicy(_) => "CPU 性能策略".into(),
        ControlCommand::SetThermalMode(mode) => format!("散热 · {}", fmt::thermal_mode_zh(*mode)),
        ControlCommand::SetFanMode(FanMode::FirmwareAuto) => "风扇 · 释放控制".into(),
        ControlCommand::SetFanMode(mode) => format!("风扇 · {}", fmt::fan_mode_zh(mode)),
        ControlCommand::SetGpuPlatformPolicy(_) | ControlCommand::SetGpuPlatformPolicyPatch(_) => {
            "GPU 平台".into()
        }
        ControlCommand::SetPowerLimits(_) => "功耗限制".into(),
        ControlCommand::SetMuxMode(_) => "显示模式".into(),
    }
}

fn error_label(error: &ControlError) -> &'static str {
    match error {
        ControlError::Unsupported => "当前设备不支持",
        ControlError::UnsafeRequest { .. } => "请求未通过安全检查",
        ControlError::PermissionDenied => "需要管理员权限",
        ControlError::DriverUnavailable { .. } | ControlError::BackendUnavailable { .. } => {
            "控制组件不可用"
        }
        ControlError::FirmwareRejected { .. } => "设备拒绝了设置",
        ControlError::VerificationFailed { .. } => "设置未生效",
        ControlError::Timeout => "操作超时",
        ControlError::Busy => "请稍后重试",
        ControlError::UnknownProfile { .. } => "找不到该配置档",
    }
}

/// Short lifecycle badge for a knob row; `None` when Idle.
pub fn status_badge(cx: &App, status: &KnobStatus) -> Option<Div> {
    let theme = cx.theme();
    let (text, color) = match status {
        KnobStatus::Idle => return None,
        // The button's disabled state already prevents duplicate writes.
        // Showing a transient lifecycle word beside every control adds noise
        // and was especially misleading during the normal coalescing window.
        KnobStatus::Pending | KnobStatus::InFlight(_) => return None,
        // The page-level outcome banner already confirms success. Repeating
        // "已应用" beside every control adds noise without a new decision.
        KnobStatus::Applied { verification, .. } => {
            if matches!(verification, Verification::Failed { .. }) {
                ("未验证", theme.warning)
            } else {
                return None;
            }
        }
        KnobStatus::Partial { .. } => ("未完全应用", theme.warning),
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

/// One knob row. `observed` is the current value or the intended value when
/// the platform has no readback channel.
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

/// Compact control row used by the performance cockpit. The regular row is
/// intentionally generous for evidence-heavy pages; this variant puts the
/// value and lifecycle badge above a full-width control so several related
/// knobs can share one column without sacrificing slider width.
#[allow(clippy::too_many_arguments)]
pub fn compact_knob_row(
    cx: &App,
    label: &'static str,
    control: impl IntoElement,
    value: String,
    status: &KnobStatus,
    disabled_reason: Option<&'static str>,
) -> Div {
    let theme = cx.theme();
    let disabled = disabled_reason.is_some();
    let mut meta = div()
        .h_flex()
        .items_center()
        .gap_2()
        .child(div().text_sm().child(label))
        .child(
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .flex_1()
                .justify_end()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(value),
                ),
        );
    if let Some(badge) = status_badge(cx, status) {
        meta = meta.child(badge);
    }
    let mut row = div().v_flex().gap_1().w_full().py_1().child(meta).child(
        div()
            .w_full()
            .when(disabled, |d| d.opacity(0.45))
            .child(control),
    );
    if let Some(reason) = disabled_reason {
        row = row.child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(reason),
        );
    }
    row
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
    let unverified = matches!(
        &rec.outcome.status,
        ControlStatus::Applied {
            verification: Verification::Failed { .. }
        }
    );
    if matches!(&rec.outcome.status, ControlStatus::Applied { .. }) && !unverified {
        // A successful write is reflected by the control's current value.
        // Keeping a second confirmation block only consumes vertical space;
        // actionable rejected/partial outcomes remain visible.
        return None;
    }
    let now = phelper_core::app::now_epoch_ms();

    let (status_text, color) = match &rec.outcome.status {
        ControlStatus::Applied { .. } => ("已写入 · 未验证".into(), theme.warning),
        ControlStatus::Rejected { error } => {
            (format!("未应用 · {}", error_label(error)), theme.danger)
        }
        ControlStatus::Partial => ("未完全应用".into(), theme.warning),
    };

    let mut body = div().v_flex().gap_1().child(
        div()
            .h_flex()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(color)
                    .child(command_label(&rec.outcome.command)),
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
                    .on_click(
                        cx.listener(move |this, _: &gpui::ClickEvent, _: &mut Window, cx| {
                            on_toggle(this);
                            cx.notify();
                        }),
                    )
                    .child(if expanded {
                        "收起详情"
                    } else {
                        "展开详情"
                    }),
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
