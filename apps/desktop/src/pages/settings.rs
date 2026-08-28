//! Settings: theme and the three resident integrations. The page exposes
//! intent only; Windows reconciliation stays in the desktop worker.

use gpui::{
    ClickEvent, Context, IntoElement, ParentElement, Styled, Window, div, prelude::FluentBuilder,
    px,
};
use gpui_component::{
    ActiveTheme, Disableable, StyledExt, Theme, ThemeMode,
    button::{Button, ButtonVariants},
    input::Input,
};
use phelper_core::app::{
    AppState,
    settings::{ThemePref, UiSettings},
};
use phelper_domain::resident::{AutostartState, OmenKeyAction, OmenKeyCapability, OverlayPosition};

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

fn sync_resident_fields(this: &mut ShellView, cx: &mut Context<ShellView>) {
    this.settings.resident.omen_key.shortcut = this.settings.shortcut.read(cx).text().to_string();
    this.settings.resident.omen_key.profile_cycle = this
        .settings
        .profile_cycle
        .read(cx)
        .text()
        .to_string()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect();
}

fn save(this: &mut ShellView, cx: &mut Context<ShellView>) {
    sync_resident_fields(this, cx);
    let settings = UiSettings {
        theme: this.settings.theme,
        resident: this.settings.resident.clone(),
    };
    match settings.save() {
        Ok(()) => {
            this.resident_runtime.update(settings.resident);
            this.overlay
                .set_position(this.settings.resident.overlay.position);
            this.settings.note = None;
        }
        Err(error) => this.settings.note = Some((format!("保存失败：{error}"), false)),
    }
}

fn resident_action_label(action: OmenKeyAction) -> &'static str {
    match action {
        OmenKeyAction::Default => "默认",
        OmenKeyAction::ToggleOverlay => "悬浮窗",
        OmenKeyAction::NextProfile => "切配置",
        OmenKeyAction::SendShortcut => "快捷键",
    }
}

pub fn render(
    state: &AppState,
    settings: &SettingsState,
    cx: &mut Context<ShellView>,
) -> impl IntoElement {
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
                        sync_resident_fields(this, app_cx);
                        this.settings.note = None;
                        let s = UiSettings {
                            theme: pref,
                            resident: this.settings.resident.clone(),
                        };
                        match s.save() {
                            Ok(()) => apply_pref(pref, app_cx),
                            Err(e) => this.settings.note = Some((format!("保存失败：{e}"), false)),
                        }
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

    let capability = state.resident.omen_key;
    let custom_available = capability == OmenKeyCapability::Supported;
    let omen_actions = [
        OmenKeyAction::Default,
        OmenKeyAction::ToggleOverlay,
        OmenKeyAction::NextProfile,
        OmenKeyAction::SendShortcut,
    ];
    let mut omen_buttons = div().h_flex().gap_1().flex_wrap();
    for (index, action) in omen_actions.into_iter().enumerate() {
        let active = settings.resident.omen_key.action == action;
        let disabled = action != OmenKeyAction::Default && !custom_available;
        omen_buttons = omen_buttons.child(
            Button::new(("omen-action", index))
                .label(resident_action_label(action))
                .disabled(disabled)
                .when(active, |button| button.primary())
                .when(!active, |button| button.outline())
                .on_click(cx.listener(
                    move |this: &mut ShellView, _: &ClickEvent, _: &mut Window, app_cx| {
                        this.settings.resident.omen_key.action = action;
                        save(this, app_cx);
                        app_cx.notify();
                    },
                )),
        );
    }
    let omen_detail = match capability {
        OmenKeyCapability::Unsupported => Some("此设备未检测到 OMEN 键事件".to_string()),
        OmenKeyCapability::Error => state.resident.omen_key_detail.clone(),
        OmenKeyCapability::Probing => Some("检测中".to_string()),
        _ => None,
    };
    let autostart_detail = (state.resident.autostart == AutostartState::Error)
        .then(|| state.resident.autostart_detail.clone())
        .flatten();
    let omen_row = div()
        .h_flex()
        .items_center()
        .justify_between()
        .gap_3()
        .w_full()
        .child(div().text_sm().font_semibold().child("OMEN 键"))
        .child(omen_buttons);

    let custom_fields = match settings.resident.omen_key.action {
        OmenKeyAction::NextProfile => Some(
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(div().w(px(86.)).text_xs().child("循环顺序"))
                .child(div().w(px(260.)).child(Input::new(&settings.profile_cycle))),
        ),
        OmenKeyAction::SendShortcut => Some(
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(div().w(px(86.)).text_xs().child("快捷键"))
                .child(div().w(px(260.)).child(Input::new(&settings.shortcut))),
        ),
        _ => None,
    };
    let needs_resident_apply = matches!(
        settings.resident.omen_key.action,
        OmenKeyAction::NextProfile | OmenKeyAction::SendShortcut
    );

    let autostart = settings.resident.autostart;
    let autostart_button = Button::new("resident-autostart")
        .label(if autostart { "开" } else { "关" })
        .when(autostart, |button| button.primary())
        .when(!autostart, |button| button.outline())
        .on_click(cx.listener(
            |this: &mut ShellView, _: &ClickEvent, _: &mut Window, app_cx| {
                this.settings.resident.autostart = !this.settings.resident.autostart;
                save(this, app_cx);
                app_cx.notify();
            },
        ));
    let autostart_row = div()
        .h_flex()
        .items_center()
        .justify_between()
        .child(div().text_sm().font_semibold().child("登录启动"))
        .child(autostart_button);

    let visible_on_start = settings.resident.overlay.visible_on_start;
    let overlay_start_button = Button::new("overlay-start")
        .label(if visible_on_start {
            "启动显示"
        } else {
            "启动隐藏"
        })
        .outline()
        .on_click(cx.listener(
            |this: &mut ShellView, _: &ClickEvent, _: &mut Window, app_cx| {
                this.settings.resident.overlay.visible_on_start =
                    !this.settings.resident.overlay.visible_on_start;
                save(this, app_cx);
                app_cx.notify();
            },
        ));
    let position = settings.resident.overlay.position;
    let position_buttons = [OverlayPosition::TopLeft, OverlayPosition::TopRight];
    let mut position_row = div().h_flex().gap_1();
    for (index, next_position) in position_buttons.into_iter().enumerate() {
        let active = position == next_position;
        position_row = position_row.child(
            Button::new(("overlay-position", index))
                .label(if next_position == OverlayPosition::TopLeft {
                    "左上"
                } else {
                    "右上"
                })
                .when(active, |button| button.primary())
                .when(!active, |button| button.outline())
                .on_click(cx.listener(
                    move |this: &mut ShellView, _: &ClickEvent, _: &mut Window, app_cx| {
                        this.settings.resident.overlay.position = next_position;
                        save(this, app_cx);
                        app_cx.notify();
                    },
                )),
        );
    }
    let overlay_row = div()
        .h_flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(div().text_sm().font_semibold().child("悬浮窗"))
        .child(
            div()
                .h_flex()
                .gap_1()
                .child(overlay_start_button)
                .child(position_row),
        );

    let resident_card = div()
        .w_full()
        .p_3()
        .v_flex()
        .gap_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_box)
        .child(div().text_base().font_semibold().child("常驻"))
        .child(autostart_row)
        .child(omen_row)
        .when_some(custom_fields, |card, fields| card.child(fields))
        .child(overlay_row)
        .when(needs_resident_apply, |card| {
            card.child(
                Button::new("resident-save")
                    .label("应用")
                    .primary()
                    .on_click(cx.listener(
                        |this: &mut ShellView, _: &ClickEvent, _: &mut Window, app_cx| {
                            save(this, app_cx);
                            app_cx.notify();
                        },
                    )),
            )
        })
        .when_some(omen_detail, |card, detail| {
            card.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(detail),
            )
        })
        .when_some(autostart_detail, |card, detail| {
            card.child(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(format!("登录启动未生效：{detail}")),
            )
        });

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
            .child(resident_card)
            .when_some(note, |d, n| d.child(n)),
    )
}
