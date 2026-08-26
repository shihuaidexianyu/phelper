//! Settings (plan D-G): theme preference （深/浅/跟随系统 → UiSettings
//! TOML, applied live via `Theme::change`) + About. Deliberately no
//! autostart row — dead UI is worse than missing UI (plan D-G).

use gpui::{ClickEvent, Context, IntoElement, ParentElement, Styled, Window, div, prelude::FluentBuilder};
use gpui_component::{ActiveTheme, StyledExt, Theme, ThemeMode, button::{Button, ButtonVariants}};
use phelper_core::app::settings::{ThemePref, UiSettings};
use phelper_core::persistence;

use crate::shell::{SettingsState, ShellView};

use super::dashboard::page_root;

fn theme_mode_of(pref: ThemePref, appearance: gpui::WindowAppearance) -> ThemeMode {
    match pref {
        ThemePref::Dark => ThemeMode::Dark,
        ThemePref::Light => ThemeMode::Light,
        // gpui-component's pinned ThemeMode has no System variant — resolve
        // it ourselves against the OS appearance (re-applied by the shell's
        // appearance observer when the OS theme flips mid-session).
        ThemePref::System => match appearance {
            gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark => ThemeMode::Dark,
            _ => ThemeMode::Light,
        },
    }
}

/// Apply a ThemePref NOW (System = resolve against the current OS value).
pub fn apply_pref(pref: ThemePref, cx: &mut gpui::App) {
    let mode = theme_mode_of(pref, cx.window_appearance());
    Theme::change(mode, None, cx);
}

pub fn render(settings: &SettingsState, cx: &mut Context<ShellView>) -> impl IntoElement {
    let theme = cx.theme();

    // ---- theme card ----
    let mut theme_btns = div().h_flex().gap_1();
    for (label, pref, tag) in [
        ("深色", ThemePref::Dark, 0usize),
        ("浅色", ThemePref::Light, 1),
        ("跟随系统", ThemePref::System, 2),
    ] {
        let active = settings.theme == pref;
        theme_btns = theme_btns.child(
            Button::new(("theme", tag))
                .label(label)
                .when(active, |b| b.primary())
                .when(!active, |b| b.outline())
                .on_click(cx.listener(move |this: &mut ShellView, _: &ClickEvent, _: &mut Window, app_cx| {
                    this.settings.theme = pref;
                    let s = UiSettings { theme: pref };
                    let note = match s.save() {
                        Ok(()) => {
                            apply_pref(pref, app_cx);
                            ("主题已保存并应用（settings.toml）".to_string(), true)
                        }
                        Err(e) => (format!("保存失败：{e}"), false),
                    };
                    this.settings.note = Some(note);
                    app_cx.notify();
                })),
        );
    }
    let theme_card = div()
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
                .child(div().text_base().font_semibold().child("主题"))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("立即生效 · 持久化到 settings.toml"),
                ),
        )
        .child(theme_btns);

    // ---- about card ----
    let data_dir = persistence::data_dir();
    let about_card = div()
        .v_flex()
        .gap_1()
        .w_full()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_box)
        .child(div().text_base().font_semibold().child("关于"))
        .child(
            div()
                .v_flex()
                .gap_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!("phelper v{} — OMEN 16-wf0032TX 专属性能/遥测控制器", env!("CARGO_PKG_VERSION")))
                .child("参考平台：主板 8BAB · BIOS F.30（M0–M6 全部实机验证）")
                .child(format!("数据目录：{}（设置 / 日志 / 控制日志账 / 用户档位 / 诊断报告）", data_dir.display()))
                .child("协议参考（OmenSuperHub / Linux hp-wmi / PawnIO）为 GPL —— 本项目仅依据协议行为与公开文档重新实现，未复制其代码（§55）")
                .child("PawnIO 签名模块（IntelMSR / IntelMCHBAR）为 LGPL 运行时数据；PresentMon / NVAPI SDK 为 MIT")
                .child("Rust + GPUI（gpui-component）· 单进程模块化单体 · UI 永不触碰硬件（AR-01）"),
        );

    let note = settings.note.as_ref().map(|(msg, ok)| {
        div()
            .text_xs()
            .text_color(if *ok { theme.success } else { theme.danger })
            .child(msg.clone())
    });

    page_root("settings-scroll").child(
        div()
            .v_flex()
            .gap_3()
            .p_4()
            .w_full()
            .child(theme_card)
            .child(about_card)
            .when_some(note, |d, n| d.child(n)),
    )
}
