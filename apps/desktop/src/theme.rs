//! The desktop has one fixed visual language: a neutral One Dark palette.

use gpui::{App, Hsla};
use gpui_component::{Theme, ThemeMode, ThemeTokens};

fn color(hex: u32) -> Hsla {
    gpui::rgb(hex).into()
}

pub fn apply(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);

    let background = color(0x282c34);
    let chrome = color(0x21252b);
    let surface = color(0x2c313a);
    let hover = color(0x323842);
    let active = color(0x3e4451);
    let foreground = color(0xabb2bf);
    let muted_foreground = color(0x7f848e);
    let primary = color(0x61afef);
    let primary_hover = color(0x74b9f0);
    let primary_active = color(0x4b9ddd);
    let danger = color(0xe06c75);
    let warning = color(0xe5c07b);
    let success = color(0x98c379);
    let cyan = color(0x56b6c2);
    let magenta = color(0xc678dd);

    {
        let theme = Theme::global_mut(cx);
        let colors = &mut theme.colors;

        colors.background = background;
        colors.foreground = foreground;
        colors.border = active;
        colors.input = active;
        colors.ring = primary;
        colors.selection = active;
        colors.accent = active;
        colors.accent_foreground = foreground;
        colors.muted = hover;
        colors.muted_foreground = muted_foreground;
        colors.group_box = surface;
        colors.group_box_foreground = foreground;
        colors.popover = surface;
        colors.popover_foreground = foreground;

        colors.sidebar = chrome;
        colors.sidebar_foreground = foreground;
        colors.sidebar_border = active;
        colors.sidebar_accent = active;
        colors.sidebar_accent_foreground = foreground;
        colors.sidebar_primary = primary;
        colors.sidebar_primary_foreground = chrome;
        colors.title_bar = chrome;
        colors.title_bar_border = active;
        colors.status_bar = chrome;
        colors.status_bar_border = active;

        colors.button = surface;
        colors.button_foreground = foreground;
        colors.button_hover = hover;
        colors.button_active = active;
        colors.primary = primary;
        colors.primary_foreground = chrome;
        colors.primary_hover = primary_hover;
        colors.primary_active = primary_active;
        colors.button_primary = primary;
        colors.button_primary_foreground = chrome;
        colors.button_primary_hover = primary_hover;
        colors.button_primary_active = primary_active;
        colors.secondary = surface;
        colors.secondary_foreground = foreground;
        colors.secondary_hover = hover;
        colors.secondary_active = active;
        colors.button_secondary = surface;
        colors.button_secondary_foreground = foreground;
        colors.button_secondary_hover = hover;
        colors.button_secondary_active = active;

        colors.danger = danger;
        colors.danger_foreground = chrome;
        colors.danger_hover = color(0xe47d85);
        colors.danger_active = color(0xc85b64);
        colors.warning = warning;
        colors.warning_foreground = chrome;
        colors.success = success;
        colors.success_foreground = chrome;
        colors.info = primary;
        colors.info_foreground = chrome;
        colors.link = primary;
        colors.link_hover = primary_hover;
        colors.link_active = primary_active;
        colors.caret = primary;

        colors.list = background;
        colors.list_even = background;
        colors.list_head = surface;
        colors.list_hover = hover;
        colors.list_active = active;
        colors.list_active_border = primary;
        colors.scrollbar = background;
        colors.scrollbar_thumb = active;
        colors.scrollbar_thumb_hover = muted_foreground;
        colors.skeleton = hover;
        colors.tiles = surface;
        colors.chart_1 = primary;
        colors.chart_2 = success;
        colors.chart_3 = warning;
        colors.chart_4 = magenta;
        colors.chart_5 = cyan;
        colors.red = danger;
        colors.green = success;
        colors.blue = primary;
        colors.yellow = warning;
        colors.magenta = magenta;
        colors.cyan = cyan;
        theme.tokens = ThemeTokens::from(&theme.colors);
    }

    Theme::sync_base(cx);
}
