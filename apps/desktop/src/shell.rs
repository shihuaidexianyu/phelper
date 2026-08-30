//! Minimal application shell: one hardware read model and three destinations.

use std::sync::{Arc, Mutex, mpsc};

use gpui::{
    App, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, Styled,
    Subscription, Window, WindowControlArea, div, px,
};
use gpui_component::{
    ActiveTheme, StyledExt, h_flex,
    sidebar::{Sidebar, SidebarCollapsible, SidebarMenu, SidebarMenuItem},
    v_flex,
};
use phelper_core::app::AppState;
use phelper_core::app::runtime::AppHandle;

use crate::{
    pages::{PageId, dashboard, profiles, settings},
    resident::{ResidentCommand, ResidentUiState},
};

fn window_control(
    id: &'static str,
    mark: impl IntoElement,
    area: WindowControlArea,
    close: bool,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let hover_background = if close {
        theme.danger
    } else {
        theme.secondary_hover
    };
    let hover_foreground = if close {
        theme.danger_foreground
    } else {
        theme.foreground
    };

    div()
        .id(id)
        .flex()
        .w(px(44.))
        .h_full()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_color(theme.foreground)
        .hover(move |style| style.bg(hover_background).text_color(hover_foreground))
        .window_control_area(area)
        .child(mark)
}

pub struct ShellView {
    pub(crate) app: AppHandle,
    pub(crate) state: AppState,
    pub(crate) resident_state: Arc<Mutex<ResidentUiState>>,
    resident_commands: mpsc::Sender<ResidentCommand>,
    pub(crate) page: PageId,
    _app_state_sub: Subscription,
}

impl ShellView {
    pub fn new(
        app: AppHandle,
        app_state: Entity<AppState>,
        resident_state: Arc<Mutex<ResidentUiState>>,
        resident_commands: mpsc::Sender<ResidentCommand>,
        cx: &mut Context<Self>,
    ) -> Self {
        // Do not read the entity again from inside its own observer. The
        // publisher is authoritative and provides a lock-backed snapshot.
        let app_state_sub = cx.observe(&app_state, |this, _, cx| {
            this.state = this.app.state();
            cx.notify();
        });
        let state = app.state();

        Self {
            app,
            state,
            resident_state,
            resident_commands,
            page: PageId::Dashboard,
            _app_state_sub: app_state_sub,
        }
    }

    pub(crate) fn resident_snapshot(&self) -> ResidentUiState {
        self.resident_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn set_autostart(&mut self, enabled: bool, cx: &mut Context<Self>) {
        {
            let mut state = self
                .resident_state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.autostart_busy {
                return;
            }
            state.autostart_busy = true;
            state.autostart_error = None;
        }
        if self
            .resident_commands
            .send(ResidentCommand::SetAutostart(enabled))
            .is_err()
        {
            let mut state = self
                .resident_state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.autostart_busy = false;
            state.autostart_error = Some("后台服务不可用，请重新启动 phelper".into());
        }
        cx.notify();
    }
}

impl Render for ShellView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let menu = SidebarMenu::new().children(PageId::ALL.map(|page| {
            SidebarMenuItem::new(page.label())
                .icon(page.icon())
                .active(self.page == page)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.page = page;
                    cx.notify();
                }))
        }));

        let content = match self.page {
            PageId::Dashboard => dashboard::render(&self.state, cx).into_any_element(),
            PageId::Profiles => profiles::render(&self.state, &self.app, cx).into_any_element(),
            PageId::Settings => settings::render(self.resident_snapshot(), cx).into_any_element(),
        };

        let theme = cx.theme();
        let maximize_label = if window.is_maximized() { "❐" } else { "□" };
        let title_bar = h_flex()
            .w_full()
            .h(px(38.))
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                h_flex()
                    .h_full()
                    .flex_1()
                    .px_3()
                    .items_center()
                    .cursor_default()
                    .window_control_area(WindowControlArea::Drag)
                    .child(div().text_sm().font_semibold().child("phelper")),
            )
            .child(window_control(
                "window-minimize",
                div().w(px(10.)).h(px(1.)).bg(theme.foreground),
                WindowControlArea::Min,
                false,
                &*cx,
            ))
            .child(window_control(
                "window-maximize",
                div().text_base().font_semibold().child(maximize_label),
                WindowControlArea::Max,
                false,
                &*cx,
            ))
            .child(window_control(
                "window-close",
                div().text_base().font_semibold().child("×"),
                WindowControlArea::Close,
                true,
                &*cx,
            ));

        v_flex()
            .size_full()
            .bg(theme.background)
            .child(title_bar)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        Sidebar::new("phelper-nav")
                            .collapsible(SidebarCollapsible::None)
                            .w(px(124.))
                            .child(menu),
                    )
                    .child(v_flex().h_full().flex_1().min_w_0().child(content)),
            )
    }
}
