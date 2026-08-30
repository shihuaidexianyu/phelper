//! Minimal resident settings. Keep lifecycle configuration out of the tray.

use gpui::{
    InteractiveElement, ParentElement, StatefulInteractiveElement, Styled, div,
    prelude::FluentBuilder, px,
};
use gpui_component::{ActiveTheme, StyledExt};

use crate::{resident::ResidentUiState, shell::ShellView};

pub fn render(state: ResidentUiState, cx: &mut gpui::Context<ShellView>) -> impl gpui::IntoElement {
    let theme = cx.theme();
    let enabled = state.autostart.unwrap_or(false);
    let interactive = state.autostart.is_some() && !state.autostart_busy;

    let toggle = div()
        .id("autostart-toggle")
        .h(px(22.))
        .w(px(38.))
        .p(px(3.))
        .flex()
        .items_center()
        .when(enabled, |toggle| toggle.justify_end())
        .when(!enabled, |toggle| toggle.justify_start())
        .rounded_full()
        .bg(if enabled {
            theme.primary
        } else {
            theme.secondary
        })
        .opacity(if state.autostart_busy { 0.6 } else { 1.0 })
        .when(interactive, |toggle| {
            toggle
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_autostart(!enabled, cx);
                }))
        })
        .child(div().size(px(16.)).rounded_full().bg(if enabled {
            theme.primary_foreground
        } else {
            theme.muted_foreground
        }));

    super::dashboard::page_root("settings-scroll").child(
        div().v_flex().p_4().w_full().child(
            div()
                .w_full()
                .p_4()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.group_box)
                .v_flex()
                .gap_3()
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .child(
                            div()
                                .v_flex()
                                .gap_1()
                                .child(div().text_sm().font_semibold().child("开机启动"))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("登录 Windows 后在后台运行，不打开主窗口"),
                                ),
                        )
                        .child(toggle),
                )
                .when_some(state.autostart_error, |card, error| {
                    card.child(div().text_xs().text_color(theme.danger).child(error))
                }),
        ),
    )
}
