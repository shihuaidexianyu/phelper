//! Pages and page-level rendering helpers. Each page is a pure render
//! function of the current `AppState` (plan D-F); page-local interactive
//! state lives in the shell's ViewState, never here. Fan controls are rendered
//! as part of the combined Performance page.

pub mod dashboard;
pub mod monitor;
pub mod performance;
pub mod profiles;
pub mod settings;
pub mod thermals;

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
    Performance,
    Profiles,
    Monitor,
    Settings,
}

impl PageId {
    pub const ALL: [PageId; 5] = [
        PageId::Dashboard,
        PageId::Performance,
        PageId::Profiles,
        PageId::Monitor,
        PageId::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PageId::Dashboard => "仪表盘",
            PageId::Performance => "性能",
            PageId::Profiles => "配置档",
            PageId::Monitor => "监视器",
            PageId::Settings => "设置",
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            PageId::Dashboard => IconName::LayoutDashboard,
            PageId::Performance => IconName::Cpu,
            PageId::Profiles => IconName::Star,
            PageId::Monitor => IconName::ChartPie,
            PageId::Settings => IconName::Settings,
        }
    }
}
