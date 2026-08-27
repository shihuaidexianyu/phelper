//! Profiles: choose and apply one overall performance preset.
//!
//! The normal path is deliberately flat: current state in the header, one
//! compact list, and an apply button on each row. Low-frequency file actions
//! stay behind the small management toggle.

use gpui::{
    ClickEvent, Context, Hsla, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    ActiveTheme, Disableable, StyledExt,
    button::{Button, ButtonVariants},
};
use phelper_core::app::AppState;
use phelper_core::app::runtime::AppHandle;
use phelper_core::app::state::{KnobId, knob_enabled};
use phelper_core::profiles::{ProfileRegistry, profiles_dir};
use phelper_domain::command::ControlCommand;

use crate::shell::{ProfileState, ShellView};

use super::dashboard::page_root;

/// Accent per built-in. User-defined profiles use the neutral foreground.
fn accent_of(theme: &gpui_component::theme::Theme, name: &str, is_builtin: bool) -> Hsla {
    if !is_builtin {
        return theme.muted_foreground;
    }
    match name {
        "silent" => theme.info,
        "balanced" => theme.warning,
        "gaming" => theme.danger,
        "cpu-max" => theme.success,
        _ => theme.chart_4,
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

/// Keep the built-in copy short: it should identify the outcome, not explain
/// the implementation behind the preset.
fn summary(name: &str, description: &str) -> String {
    match name {
        "silent" => "安静省电 · 低速曲线".into(),
        "balanced" => "均衡性能 · 散热曲线".into(),
        "gaming" => "游戏优先 · 风扇曲线".into(),
        "cpu-max" => "持续性能 · 风扇全速".into(),
        _ => description.to_string(),
    }
}

pub fn render(
    state: &AppState,
    app: &AppHandle,
    prof: &ProfileState,
    cx: &mut Context<ShellView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let writes = state.writes_available();
    let active = state.desired.profile.as_deref();
    let selected_name = prof
        .selected
        .as_deref()
        .or(active)
        .or(state.profiles.first().map(|p| p.name.as_str()))
        .map(str::to_owned);
    let current_title = active.map(display_name).unwrap_or("未应用").to_string();

    let manage_button = Button::new("profile-manage")
        .label(if prof.management_expanded {
            "收起管理"
        } else {
            "管理"
        })
        .outline()
        .on_click(
            cx.listener(|this: &mut ShellView, _: &ClickEvent, _: &mut Window, cx| {
                this.prof.management_expanded = !this.prof.management_expanded;
                cx.notify();
            }),
        );

    let page_header = div()
        .h_flex()
        .items_center()
        .justify_between()
        .child(div().text_xl().font_semibold().child("配置档"))
        .child(
            div()
                .h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .px_2()
                        .py(px(2.))
                        .rounded_full()
                        .border_1()
                        .border_color(theme.border)
                        .text_xs()
                        .child(format!("当前 · {current_title}")),
                )
                .child(manage_button),
        );

    let management = if prof.management_expanded {
        let export_name = selected_name.clone();
        let export_button = Button::new("profile-export")
            .label("导出 TOML")
            .outline()
            .disabled(export_name.is_none())
            .on_click(cx.listener(
                move |this: &mut ShellView, _: &ClickEvent, _: &mut Window, app_cx| {
                    let note = match export_name.as_deref() {
                        Some(name) => match ProfileRegistry::load_default().get(name) {
                            Some(p) => match phelper_core::profiles::to_toml(p) {
                                Ok(toml) => {
                                    app_cx
                                        .write_to_clipboard(gpui::ClipboardItem::new_string(toml));
                                    ("配置已复制".to_string(), true)
                                }
                                Err(e) => (format!("序列化失败：{e}"), false),
                            },
                            None => (format!("找不到档位「{name}」"), false),
                        },
                        None => ("没有可导出的配置".to_string(), false),
                    };
                    this.prof.note = Some(note);
                    app_cx.notify();
                },
            ));
        let open_dir_button = Button::new("profile-open-dir")
            .label("打开配置目录")
            .outline()
            .on_click(cx.listener(
                move |this: &mut ShellView, _: &ClickEvent, _: &mut Window, app_cx| {
                    let dir = profiles_dir();
                    let ok = std::fs::create_dir_all(&dir).is_ok()
                        && std::process::Command::new("explorer")
                            .arg(&dir)
                            .spawn()
                            .is_ok();
                    this.prof.note = Some((
                        if ok {
                            format!("已打开 {}", dir.display())
                        } else {
                            "打开目录失败".to_string()
                        },
                        ok,
                    ));
                    app_cx.notify();
                },
            ));
        let refresh_app = app.clone();
        let refresh_button = Button::new("profile-refresh")
            .label("刷新列表")
            .outline()
            .on_click(cx.listener(
                move |this: &mut ShellView, _: &ClickEvent, _: &mut Window, app_cx| {
                    refresh_app.refresh_profiles();
                    this.prof.note = Some(("列表已刷新".to_string(), true));
                    app_cx.notify();
                },
            ));
        Some(
            div()
                .h_flex()
                .gap_2()
                .flex_wrap()
                .child(export_button)
                .child(open_dir_button)
                .child(refresh_button),
        )
    } else {
        None
    };

    let mut rows = div().v_flex().gap_1().w_full();
    for (ix, p) in state.profiles.iter().enumerate() {
        let accent = accent_of(theme, &p.name, p.is_builtin);
        let is_active = active == Some(p.name.as_str());
        let is_selected = selected_name.as_deref() == Some(p.name.as_str());
        let name = p.name.clone();
        let title = display_name(&p.name).to_string();
        let description = summary(&p.name, &p.description);
        let apply_name = p.name.clone();
        let apply_app = app.clone();
        let gate: Option<&'static str> = if !writes {
            Some(super::control_unavailable_label(state))
        } else if p.has_experimental_fields && !state.experimental.power_limits_drawer {
            Some("此档位当前不可用")
        } else {
            knob_enabled(state.caps.as_ref(), KnobId::Profile, &state.experimental).err()
        };

        let base_button = Button::new(("profile-apply", ix))
            .label(if is_active { "当前" } else { "应用" })
            .disabled(gate.is_some() || is_active)
            .on_click(cx.listener(
                move |_: &mut ShellView, _: &ClickEvent, _: &mut Window, _cx| {
                    apply_app.dispatch(
                        KnobId::Profile,
                        ControlCommand::ApplyProfile {
                            profile: apply_name.clone(),
                        },
                    );
                },
            ));
        let apply_button = if is_selected && !is_active {
            base_button.primary()
        } else {
            base_button.outline()
        };

        rows = rows.child(
            div()
                .id(("profile-row", ix))
                .h_flex()
                .gap_2()
                .items_center()
                .w_full()
                .h(px(52.))
                .min_w_0()
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(if is_selected { accent } else { theme.border })
                .bg(if is_selected {
                    theme.background
                } else {
                    theme.group_box
                })
                .cursor_pointer()
                .child(div().size_2().rounded_full().bg(accent))
                .child(
                    div()
                        .v_flex()
                        .gap_px()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .h_flex()
                                .items_center()
                                .gap_2()
                                .child(div().text_sm().font_semibold().child(title))
                                .when(p.has_experimental_fields, |d| {
                                    d.child(div().text_xs().text_color(theme.warning).child("实验"))
                                }),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .h(px(16.))
                                .overflow_hidden()
                                .child(description),
                        ),
                )
                .when_some(gate, |d, reason| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(reason),
                    )
                })
                .child(apply_button)
                .on_click(cx.listener(
                    move |this: &mut ShellView, _: &ClickEvent, _: &mut Window, cx| {
                        this.prof.selected = Some(name.clone());
                        cx.notify();
                    },
                )),
        );
    }

    let profile_list = div()
        .v_flex()
        .gap_2()
        .w_full()
        .p_2()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_box)
        .child(if state.profiles.is_empty() {
            div()
                .h(px(52.))
                .items_center()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("没有可用配置")
        } else {
            rows
        });

    let note = prof.note.as_ref().map(|(msg, ok)| {
        div()
            .text_xs()
            .text_color(if *ok { theme.success } else { theme.danger })
            .child(msg.clone())
    });
    let warnings = if state.profile_warnings.is_empty() {
        None
    } else {
        Some(
            div()
                .v_flex()
                .gap_1()
                .w_full()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(theme.warning)
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.warning)
                        .child("部分自定义配置未加载"),
                )
                .children(state.profile_warnings.iter().map(|w| {
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(w.clone())
                })),
        )
    };
    let banner = crate::widgets::knob_row::outcome_banner(
        cx,
        state.evidence.back(),
        prof.banner_expanded,
        |this: &mut ShellView| {
            this.prof.banner_expanded = !this.prof.banner_expanded;
        },
    );

    page_root("profiles-scroll").child(
        div()
            .v_flex()
            .gap_3()
            .p_4()
            .w_full()
            .child(page_header)
            .when_some(management, |d, actions| d.child(actions))
            .child(profile_list)
            .when_some(note, |d, n| d.child(n))
            .when_some(warnings, |d, w| d.child(w))
            .when_some(banner, |d, b| d.child(b)),
    )
}
