//! Profiles (plan D-G): card design per docs/ui-mockup-performance-cards.png
//! — one accent color per built-in (silent 蓝 / balanced 黄 / gaming 红 /
//! cpu-max 绿, user profiles neutral), name + description + touches chips +
//! experimental badge. Details panel with 应用 / 导出 TOML（剪贴板）/
//! 打开配置目录 / 刷新 + registry warnings. A profile apply is a MULTI-STEP
//! plan (PPM → 0x29 → 0x22 → thermal → fan) — the outcome banner's expanded
//! step table is the honest evidence view (AR-10).

use gpui::{App, ClickEvent, Context, Hsla, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px};
use gpui_component::{ActiveTheme, Disableable, StyledExt, button::{Button, ButtonVariants}};
use phelper_core::app::runtime::AppHandle;
use phelper_core::app::state::{KnobId, knob_enabled};
use phelper_core::app::AppState;
use phelper_core::profiles::{ProfileRegistry, profiles_dir};
use phelper_domain::command::ControlCommand;

use crate::shell::{ProfileState, ShellView};
use crate::widgets::knob_row;

use super::dashboard::page_root;

/// Accent per built-in (user profiles neutral) — the mockup's color language.
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

fn chip(cx: &App, label: &'static str) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .px_2()
        .py(px(1.))
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(label)
}

pub fn render(
    state: &AppState,
    app: &AppHandle,
    prof: &ProfileState,
    cx: &mut Context<ShellView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let writes = state.writes_available();

    // ---- card grid ----
    let active = state.desired.profile.as_deref();
    let selected_name = prof
        .selected
        .as_deref()
        .or(active)
        .or(state.profiles.first().map(|p| p.name.as_str()))
        .map(|s| s.to_string());
    let mut cards = div().h_flex().gap_3().flex_wrap().w_full();
    for (ix, p) in state.profiles.iter().enumerate() {
        let accent = accent_of(theme, &p.name, p.is_builtin);
        let is_active = active == Some(p.name.as_str());
        let is_selected = selected_name.as_deref() == Some(p.name.as_str());
        let name = p.name.clone();
        cards = cards.child(
            div()
                .id(("profile", ix))
                .v_flex()
                .gap_2()
                .w(px(250.))
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(if is_selected { accent } else { theme.border })
                .bg(theme.group_box)
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().size_2().rounded_full().bg(accent))
                        .child(div().text_base().font_semibold().child(p.name.clone()))
                        .when(is_active, |d| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(accent)
                                    .child("当前"),
                            )
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .h(px(30.))
                        .overflow_hidden()
                        .child(p.description.clone()),
                )
                .child(
                    div()
                        .h_flex()
                        .gap_1()
                        .flex_wrap()
                        .children(p.touches.iter().map(|t| chip(cx, t))),
                )
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(if p.is_builtin { "内置" } else { "用户" }),
                        )
                        .when(p.has_experimental_fields, |d| {
                            d.child(div().text_xs().text_color(theme.warning).child("实验"))
                        }),
                )
                .on_click(cx.listener(move |this: &mut ShellView, _: &ClickEvent, _: &mut Window, cx| {
                    this.prof.selected = Some(name.clone());
                    cx.notify();
                })),
        );
    }

    // ---- details panel ----
    let details = match state
        .profiles
        .iter()
        .find(|p| selected_name.as_deref() == Some(p.name.as_str()))
    {
        Some(p) => {
            let gate: Option<&'static str> = if !writes {
                Some("控制不可用（遥测模式）")
            } else if p.has_experimental_fields && !state.experimental.power_limits_drawer {
                Some("含实验性功耗墙字段——本构建未启用实验功能")
            } else {
                knob_enabled(state.caps.as_ref(), KnobId::Profile, &state.experimental).err()
            };
            let status = state.knobs.get(&KnobId::Profile).cloned().unwrap_or_default();
            let apply_name = p.name.clone();
            let export_name = p.name.clone();
            let apply_app = app.clone();
            let refresh_app = app.clone();
            div()
                .v_flex()
                .gap_2()
                .w_full()
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.group_box)
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .child(div().text_base().font_semibold().child(format!("档位详情：{}", p.name)))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(if p.is_builtin { "内置档位" } else { "用户 TOML 档位" }),
                        ),
                )
                .child(div().text_sm().child(p.description.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "触及域：{} · 展开顺序 PPM → 0x29 → 0x22 → 散热 → 风扇（风扇最后——手动/最大会挂起固件温度曲线）",
                            p.touches.join(" / ")
                        )),
                )
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .child(
                            Button::new("profile-apply")
                                .label("应用")
                                .primary()
                                .disabled(gate.is_some())
                                .on_click(cx.listener(move |_, _: &ClickEvent, _: &mut Window, _cx| {
                                    apply_app.dispatch(
                                        KnobId::Profile,
                                        ControlCommand::ApplyProfile { profile: apply_name.clone() },
                                    );
                                })),
                        )
                        .child(
                            Button::new("profile-export")
                                .label("导出 TOML")
                                .outline()
                                .on_click(cx.listener(move |this: &mut ShellView, _: &ClickEvent, _: &mut Window, app_cx| {
                                    // Fresh load — exports what is on disk NOW
                                    // (pure file read, never a hardware touch).
                                    let note = match ProfileRegistry::load_default().get(&export_name) {
                                        Some(p) => match phelper_core::profiles::to_toml(p) {
                                            Ok(toml) => {
                                                app_cx.write_to_clipboard(gpui::ClipboardItem::new_string(toml));
                                                ("TOML 已复制到剪贴板".to_string(), true)
                                            }
                                            Err(e) => (format!("序列化失败：{e}"), false),
                                        },
                                        None => (format!("磁盘上找不到档位「{export_name}」"), false),
                                    };
                                    this.prof.note = Some(note);
                                    app_cx.notify();
                                })),
                        )
                        .child(
                            Button::new("profile-open-dir")
                                .label("打开配置目录")
                                .outline()
                                .on_click(cx.listener(move |this: &mut ShellView, _: &ClickEvent, _: &mut Window, app_cx| {
                                    let dir = profiles_dir();
                                    let ok = std::fs::create_dir_all(&dir).is_ok()
                                        && std::process::Command::new("explorer").arg(&dir).spawn().is_ok();
                                    this.prof.note = Some((
                                        if ok { format!("已打开 {}", dir.display()) } else { "打开目录失败".to_string() },
                                        ok,
                                    ));
                                    app_cx.notify();
                                })),
                        )
                        .child(
                            Button::new("profile-refresh")
                                .label("刷新列表")
                                .outline()
                                .on_click(cx.listener(move |this: &mut ShellView, _: &ClickEvent, _: &mut Window, app_cx| {
                                    refresh_app.refresh_profiles();
                                    this.prof.note = Some((
                                        "已刷新（引擎内部展开表随启动装载——新档位若应用失败请重启应用）".to_string(),
                                        true,
                                    ));
                                    app_cx.notify();
                                })),
                        )
                        .child(
                            div().h_flex().flex_1().justify_end().child(
                                knob_row::status_badge(cx, &status).unwrap_or_else(div),
                            ),
                        ),
                )
                .when_some(gate, |d, r| {
                    d.child(div().text_xs().text_color(theme.warning).child(r))
                })
        }
        None => div().v_flex().child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("没有可用档位（内置注册表缺失且用户目录为空）"),
        ),
    };

    // ---- note + warnings + banner ----
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
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(theme.warning)
                .bg(theme.group_box)
                .child(div().text_base().font_semibold().child("注册表警告"))
                .children(state.profile_warnings.iter().map(|w| {
                    div()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(w.clone())
                })),
        )
    };
    let banner = knob_row::outcome_banner(cx, state.evidence.back(), prof.banner_expanded, |this: &mut ShellView| {
        this.prof.banner_expanded = !this.prof.banner_expanded;
    });

    page_root("profiles-scroll").child(
        div()
            .v_flex()
            .gap_3()
            .p_4()
            .w_full()
            .child(cards)
            .child(details)
            .when_some(note, |d, n| d.child(n))
            .when_some(warnings, |d, w| d.child(w))
            .when_some(banner, |d, b| d.child(b)),
    )
}
