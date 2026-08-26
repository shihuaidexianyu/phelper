//! Pages — one module per sidebar entry. Each page is a pure render
//! function of the current `AppState` (plan D-F); page-local interactive
//! state lives in the shell's ViewState, never here.

pub mod dashboard;
pub mod diagnostics;
pub mod monitor;
pub mod performance;
pub mod profiles;
pub mod settings;
pub mod thermals;

use gpui_component::IconName;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageId {
    Dashboard,
    Performance,
    Thermals,
    Profiles,
    Monitor,
    Diagnostics,
    Settings,
}

impl PageId {
    pub const ALL: [PageId; 7] = [
        PageId::Dashboard,
        PageId::Performance,
        PageId::Thermals,
        PageId::Profiles,
        PageId::Monitor,
        PageId::Diagnostics,
        PageId::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PageId::Dashboard => "仪表盘",
            PageId::Performance => "性能",
            PageId::Thermals => "散热",
            PageId::Profiles => "配置档",
            PageId::Monitor => "监视器",
            PageId::Diagnostics => "诊断",
            PageId::Settings => "设置",
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            PageId::Dashboard => IconName::LayoutDashboard,
            PageId::Performance => IconName::Cpu,
            PageId::Thermals => IconName::Sun,
            PageId::Profiles => IconName::Star,
            PageId::Monitor => IconName::ChartPie,
            PageId::Diagnostics => IconName::Heart,
            PageId::Settings => IconName::Settings,
        }
    }
}
