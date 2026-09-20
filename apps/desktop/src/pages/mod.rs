//! Minimal desktop pages. Advanced controls remain available through the
//! core/CLI and can return only when a concrete UI need justifies them.

pub mod automation;
pub mod dashboard;
pub mod hardware;
pub mod performance;
pub mod profiles;
pub mod settings;
pub mod validation;

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
    Performance,
    Automation,
    Validation,
    Hardware,
    Settings,
}

impl PageId {
    pub const ALL: [PageId; 7] = [
        PageId::Dashboard,
        PageId::Performance,
        PageId::Profiles,
        PageId::Automation,
        PageId::Validation,
        PageId::Hardware,
        PageId::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PageId::Dashboard => "概览",
            PageId::Profiles => "配置档",
            PageId::Performance => "性能控制",
            PageId::Automation => "自动切换",
            PageId::Validation => "性能验证",
            PageId::Hardware => "硬件能力",
            PageId::Settings => "设置",
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            PageId::Dashboard => IconName::LayoutDashboard,
            PageId::Profiles => IconName::Star,
            PageId::Performance => IconName::Settings,
            PageId::Automation => IconName::Settings,
            PageId::Validation => IconName::LayoutDashboard,
            PageId::Hardware => IconName::Settings,
            PageId::Settings => IconName::Settings,
        }
    }
}
