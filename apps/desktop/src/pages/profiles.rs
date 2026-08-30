//! The only write surface in the minimal UI: apply a built-in profile.

use gpui::{
    ClickEvent, Context, Hsla, IntoElement, ParentElement, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::{ActiveTheme, Disableable, StyledExt, button::Button};
use phelper_core::app::runtime::AppHandle;
use phelper_core::app::state::{KnobId, KnobStatus, profile_enabled};
use phelper_core::app::{AppState, fmt};
use phelper_domain::command::{ControlCommand, Verification};

use crate::shell::ShellView;

use super::dashboard::page_root;

fn accent_of(theme: &gpui_component::theme::Theme, name: &str) -> Hsla {
    match name {
        "silent" => theme.info,
        "balanced" => theme.warning,
        "gaming" => theme.danger,
        "cpu-max" => theme.success,
        _ => theme.muted_foreground,
    }
}

fn display_name(name: &str) -> &str {
    match name {
        "silent" => "安静",
        "balanced" => "均衡",
        "gaming" => "游戏",
        "cpu-max" => "极致",
        _ => name,
    }
}

fn summary(name: &str, description: &str) -> String {
    match name {
        "silent" => "安静省电 · 低速曲线".into(),
        "balanced" => "均衡性能 · 散热曲线".into(),
        "gaming" => "游戏优先 · 风扇曲线".into(),
        "cpu-max" => "持续性能 · 风扇全速".into(),
        _ => description.to_string(),
    }
}

pub fn render(state: &AppState, app: &AppHandle, cx: &mut Context<ShellView>) -> impl IntoElement {
    let theme = cx.theme();
    let active = state.desired.profile.as_deref();
    let busy = matches!(
        state.knob_status(KnobId::Profile),
        KnobStatus::Pending | KnobStatus::InFlight(_)
    );
    let outcome = match state.knob_status(KnobId::Profile) {
        KnobStatus::Idle | KnobStatus::Pending | KnobStatus::InFlight(_) => None,
        KnobStatus::Applied { verification, .. } => {
            if matches!(verification, Verification::Failed { .. }) {
                Some((fmt::verification_zh(verification), theme.danger))
            } else {
                None
            }
        }
        KnobStatus::Partial { .. } => Some((
            "配置档只完成了部分步骤，硬件已恢复到安全状态。".to_string(),
            theme.warning,
        )),
        KnobStatus::Failed { error, .. } => Some((
            format!("未能应用：{}", fmt::control_error_zh(error)),
            theme.danger,
        )),
    };

    let mut rows = div().v_flex().gap_2().w_full();
    for (index, profile) in state.profiles.iter().enumerate() {
        let accent = accent_of(theme, &profile.name);
        let is_active = active == Some(profile.name.as_str());
        let gate = if !state.writes_available() {
            Some(super::control_unavailable_label(state))
        } else {
            profile_enabled(state.caps.as_ref()).err()
        };
        let profile_name = profile.name.clone();
        let app = app.clone();
        let button = Button::new(("profile-apply", index))
            .label(if is_active { "当前" } else { "应用" })
            .outline()
            .disabled(is_active || busy || gate.is_some())
            .on_click(cx.listener(
                move |_: &mut ShellView, _: &ClickEvent, _: &mut Window, _| {
                    app.dispatch(
                        KnobId::Profile,
                        ControlCommand::ApplyProfile {
                            profile: profile_name.clone(),
                        },
                    );
                },
            ));

        rows = rows.child(
            div()
                .h_flex()
                .items_center()
                .gap_3()
                .w_full()
                .min_h(px(58.))
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(if is_active { accent } else { theme.border })
                .bg(theme.group_box)
                .child(div().size_2().rounded_full().bg(accent))
                .child(
                    div()
                        .v_flex()
                        .gap_1()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_sm()
                                .font_semibold()
                                .child(display_name(&profile.name).to_string()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(summary(&profile.name, &profile.description)),
                        ),
                )
                .when_some(gate, |row, reason| {
                    row.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(reason),
                    )
                })
                .child(button),
        );
    }

    if state.profiles.is_empty() {
        rows = rows.child(
            div()
                .h(px(58.))
                .items_center()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(
                    if matches!(state.engine, phelper_core::app::EngineStatus::Starting) {
                        "正在加载配置档…"
                    } else {
                        "没有可用的内置配置档"
                    },
                ),
        );
    }

    page_root("profiles-scroll").child(
        div()
            .v_flex()
            .gap_3()
            .p_4()
            .w_full()
            .when_some(outcome, |content, (message, color)| {
                content.child(
                    div()
                        .w_full()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .border_1()
                        .border_color(color.opacity(0.45))
                        .text_sm()
                        .text_color(color)
                        .child(message),
                )
            })
            .child(rows),
    )
}
