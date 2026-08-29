//! Settings: theme and the three resident integrations. The page exposes
//! intent only; Windows reconciliation stays in the desktop worker.
//! §Phase 2 — owns its state. The shell reads via the entity handle.

use std::time::Duration;

use gpui::{
    AppContext, ClickEvent, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Subscription, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    ActiveTheme, Disableable, StyledExt, Theme, ThemeMode,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
};
use phelper_core::app::{
    AppState,
    settings::{ThemePref, UiSettings},
};
use phelper_domain::resident::{
    AutostartState, OmenKeyAction, OmenKeyCapability, OverlayPosition, ResidentSettings,
};

use crate::overlay::OverlayController;
use crate::resident::ResidentRuntimeHandle;

use super::dashboard::page_root;

pub struct SettingsPageState {
    theme: ThemePref,
    resident: ResidentSettings,
    shortcut: Entity<InputState>,
    profile_cycle: Entity<InputState>,
    note: Option<(String, bool)>,
    app_state: Entity<AppState>,
    resident_runtime: ResidentRuntimeHandle,
    overlay: OverlayController,
    last_fp: Option<u64>,
    last_paint: std::time::Instant,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPageState {
    pub fn new(
        app_state: Entity<AppState>,
        ui_settings: UiSettings,
        window: &mut Window,
        resident_runtime: ResidentRuntimeHandle,
        overlay: OverlayController,
        cx: &mut Context<Self>,
    ) -> Self {
        let shortcut = cx.new(|cx| InputState::new(window, cx).placeholder("Ctrl+Shift+F10"));
        let profile_cycle = cx.new(|cx| InputState::new(window, cx).placeholder("balanced,gaming"));
        shortcut.update(cx, |input, cx| {
            input.set_value(ui_settings.resident.omen_key.shortcut.clone(), window, cx);
        });
        profile_cycle.update(cx, |input, cx| {
            input.set_value(
                ui_settings.resident.omen_key.profile_cycle.join(","),
                window,
                cx,
            );
        });
        for input in [&shortcut, &profile_cycle] {
            let _ = cx.subscribe(input, |_this: &mut Self, _, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            });
        }
        let app_state_sub = cx.observe(&app_state, |this, app_state, cx| {
            let fp = this.fingerprint(&app_state.read(cx));
            if Some(fp) != this.last_fp
                || this.last_paint.elapsed() >= Duration::from_secs(5)
            {
                this.last_fp = Some(fp);
                this.last_paint = std::time::Instant::now();
                cx.notify();
            }
        });
        Self {
            theme: ui_settings.theme,
            resident: ui_settings.resident,
            shortcut,
            profile_cycle,
            note: None,
            app_state,
            resident_runtime,
            overlay,
            last_fp: None,
            last_paint: std::time::Instant::now(),
            _subscriptions: vec![app_state_sub],
        }
    }

    pub fn theme(&self) -> ThemePref {
        self.theme
    }

    fn fingerprint(&self, state: &AppState) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        std::mem::discriminant(&self.theme).hash(&mut h);
        format!("{:?}", state.resident.omen_key).hash(&mut h);
        format!("{:?}", state.resident.autostart).hash(&mut h);
        h.finish()
    }

    pub fn render_into(
        entity: &Entity<Self>,
        window: &mut Window,
        cx: &mut Context<crate::shell::ShellView>,
    ) -> gpui::AnyElement {
        entity.update(cx, |view, cx| view.render(window, cx).into_any_element())
    }
}

fn theme_mode_of(pref: ThemePref, appearance: gpui::WindowAppearance) -> ThemeMode {
    match pref {
        ThemePref::Dark => ThemeMode::Dark,
        ThemePref::Light => ThemeMode::Light,
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

impl SettingsPageState {
    fn sync_resident_fields(&mut self, cx: &mut Context<Self>) {
        self.resident.omen_key.shortcut = self.shortcut.read(cx).text().to_string();
        self.resident.omen_key.profile_cycle = self
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

    fn save(&mut self, cx: &mut Context<Self>) {
        self.sync_resident_fields(cx);
        let settings = UiSettings {
            theme: self.theme,
            resident: self.resident.clone(),
        };
        match settings.save() {
            Ok(()) => {
                self.resident_runtime.update(settings.resident);
                self.overlay.set_position(self.resident.overlay.position);
                self.note = None;
            }
            Err(error) => self.note = Some((format!("保存失败:{error}"), false)),
        }
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

impl Render for SettingsPageState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.app_state.read_with(cx, |s, _| s.clone());
        let theme = cx.theme();

        let mut theme_btns = div().h_flex().gap_1();
        for (label, pref, tag) in [
            ("深色", ThemePref::Dark, 0usize),
            ("浅色", ThemePref::Light, 1),
            ("系统", ThemePref::System, 2),
        ] {
            let active = self.theme == pref;
            theme_btns = theme_btns.child(
                Button::new(("theme", tag))
                    .label(label)
                    .when(active, |b| b.primary())
                    .when(!active, |b| b.outline())
                    .on_click(cx.listener(
                        move |this: &mut Self, _: &ClickEvent, _: &mut Window, app_cx| {
                            this.theme = pref;
                            this.sync_resident_fields(app_cx);
                            this.note = None;
                            let s = UiSettings {
                                theme: pref,
                                resident: this.resident.clone(),
                            };
                            match s.save() {
                                Ok(()) => apply_pref(pref, app_cx),
                                Err(e) => {
                                    this.note = Some((format!("保存失败:{e}"), false))
                                }
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
            let active = self.resident.omen_key.action == action;
            let disabled = action != OmenKeyAction::Default && !custom_available;
            omen_buttons = omen_buttons.child(
                Button::new(("omen-action", index))
                    .label(resident_action_label(action))
                    .disabled(disabled)
                    .when(active, |button| button.primary())
                    .when(!active, |button| button.outline())
                    .on_click(cx.listener(
                        move |this: &mut Self, _: &ClickEvent, _: &mut Window, app_cx| {
                            this.resident.omen_key.action = action;
                            this.save(app_cx);
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

        let custom_fields = match self.resident.omen_key.action {
            OmenKeyAction::NextProfile => Some(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(86.)).text_xs().child("循环顺序"))
                    .child(div().w(px(260.)).child(Input::new(&self.profile_cycle))),
            ),
            OmenKeyAction::SendShortcut => Some(
                div()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(86.)).text_xs().child("快捷键"))
                    .child(div().w(px(260.)).child(Input::new(&self.shortcut))),
            ),
            _ => None,
        };
        let needs_resident_apply = matches!(
            self.resident.omen_key.action,
            OmenKeyAction::NextProfile | OmenKeyAction::SendShortcut
        );

        let autostart = self.resident.autostart;
        let autostart_button = Button::new("resident-autostart")
            .label(if autostart { "开" } else { "关" })
            .when(autostart, |button| button.primary())
            .when(!autostart, |button| button.outline())
            .on_click(cx.listener(
                |this: &mut Self, _: &ClickEvent, _: &mut Window, app_cx| {
                    this.resident.autostart = !this.resident.autostart;
                    this.save(app_cx);
                    app_cx.notify();
                },
            ));
        let autostart_row = div()
            .h_flex()
            .items_center()
            .justify_between()
            .child(div().text_sm().font_semibold().child("登录启动"))
            .child(autostart_button);

        let visible_on_start = self.resident.overlay.visible_on_start;
        let overlay_start_button = Button::new("overlay-start")
            .label(if visible_on_start {
                "启动显示"
            } else {
                "启动隐藏"
            })
            .outline()
            .on_click(cx.listener(
                |this: &mut Self, _: &ClickEvent, _: &mut Window, app_cx| {
                    this.resident.overlay.visible_on_start =
                        !this.resident.overlay.visible_on_start;
                    this.save(app_cx);
                    app_cx.notify();
                },
            ));
        let position = self.resident.overlay.position;
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
                        move |this: &mut Self, _: &ClickEvent, _: &mut Window, app_cx| {
                            this.resident.overlay.position = next_position;
                            this.save(app_cx);
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
                            |this: &mut Self, _: &ClickEvent, _: &mut Window, app_cx| {
                                this.save(app_cx);
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
                        .child(format!("登录启动未生效:{detail}")),
                )
            });

        let note = self.note.as_ref().map(|(msg, ok)| {
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
}