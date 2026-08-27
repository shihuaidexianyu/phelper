//! Settings (plan D-G): theme preference （深/浅/跟随系统 → UiSettings
//! TOML, applied live via `Theme::change`) + About. Deliberately no
//! autostart row — dead UI is worse than missing UI (plan D-G).

use gpui::{
    ClickEvent, Context, IntoElement, ParentElement, Styled, Window, div, prelude::FluentBuilder,
};
use gpui_component::{
    ActiveTheme, StyledExt, Theme, ThemeMode,
    button::{Button, ButtonVariants},
};
use phelper_core::app::settings::{ThemePref, UiSettings};

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

    let mut theme_btns = div().h_flex().gap_1();
    for (label, pref, tag) in [
        ("深色", ThemePref::Dark, 0usize),
        ("浅色", ThemePref::Light, 1),
        ("系统", ThemePref::System, 2),
    ] {
        let active = settings.theme == pref;
        theme_btns = theme_btns.child(
            Button::new(("theme", tag))
                .label(label)
                .when(active, |b| b.primary())
                .when(!active, |b| b.outline())
                .on_click(cx.listener(
                    move |this: &mut ShellView, _: &ClickEvent, _: &mut Window, app_cx| {
                        this.settings.theme = pref;
                        let s = UiSettings { theme: pref };
                        let note = match s.save() {
                            Ok(()) => {
                                apply_pref(pref, app_cx);
                                ("主题已应用".to_string(), true)
                            }
                            Err(e) => (format!("保存失败：{e}"), false),
                        };
                        this.settings.note = Some(note);
                        app_cx.notify();
                    },
                )),
        );
    }
    let theme_row = div()
        .h_flex()
        .items_center()
        .justify_between()
        .gap_3()
        .w_full()
        .child(div().text_sm().font_semibold().child("主题"))
        .child(theme_btns);
    let theme_card = div()
        .w_full()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_box)
        .child(theme_row);

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
            .child(div().text_xl().font_semibold().child("设置"))
            .child(theme_card)
            .when_some(note, |d, n| d.child(n)),
    )
}
