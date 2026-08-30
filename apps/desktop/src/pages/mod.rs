//! Minimal desktop pages. Advanced controls remain available through the
//! core/CLI and can return only when a concrete UI need justifies them.

pub mod dashboard;
pub mod profiles;
pub mod settings;

use gpui_component::IconName;
use phelper_core::app::{AppState, EngineStatus};

/// Short user-facing reason shown where write controls are unavailable.
pub fn control_unavailable_label(state: &AppState) -> &'static str {
    match state.engine {
        EngineStatus::Starting => "正在准备控制…",
        EngineStatus::TelemetryOnly => "当前为只读模式",
        EngineStatus::Failed(_) => "控制暂不可用",
        EngineStatus::Running => "控制可用",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageId {
    Dashboard,
    Profiles,
    Settings,
}

impl PageId {
    pub const ALL: [PageId; 3] = [PageId::Dashboard, PageId::Profiles, PageId::Settings];

    pub fn label(self) -> &'static str {
        match self {
            PageId::Dashboard => "概览",
            PageId::Profiles => "配置档",
            PageId::Settings => "设置",
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            PageId::Dashboard => IconName::LayoutDashboard,
            PageId::Profiles => IconName::Star,
            PageId::Settings => IconName::Settings,
        }
    }
}
