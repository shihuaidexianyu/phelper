//! Dashboard (plan D-G): status badges, 8 metric cards, fan card, two
//! 5-minute trend charts. Pure `&AppState` render.

use gpui::{App, Div, InteractiveElement, IntoElement, ParentElement, Styled, div, prelude::FluentBuilder, px};
use gpui_component::{ActiveTheme, StyledExt, scroll::ScrollableElement};
use phelper_core::app::fmt;
use phelper_core::app::runtime::AppHandle;
use phelper_core::app::{AppState, EngineStatus};
use phelper_core::telemetry::registry;
use phelper_domain::state::ObservedValue;
use phelper_domain::telemetry::ids;

use crate::widgets::metric_card::MetricCard;
use crate::widgets::trend_chart;

fn cadence(id: phelper_domain::telemetry::MetricId) -> std::time::Duration {
    registry::meta(id)
        .map(|m| m.cadence)
        .unwrap_or(std::time::Duration::from_secs(1))
}

fn badge(cx: &App, label: String, provenance: Option<&'static str>) -> Div {
    let theme = cx.theme();
    let color = match provenance {
        Some("已验证") => theme.success,
        Some("信任写入") => theme.info,
        _ => theme.muted_foreground,
    };
    div()
        .h_flex()
        .gap_2()
        .px_3()
        .py_1()
        .rounded_full()
        .border_1()
        .border_color(theme.border)
        .child(div().text_sm().child(label))
        .when_some(provenance, |this, p| {
            this.child(div().text_xs().text_color(color).child(p))
        })
}

pub fn render(state: &AppState, app: &AppHandle, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let snap = state.telemetry.as_deref();
    let sample = |id: phelper_domain::telemetry::MetricId| snap.and_then(|s| s.samples.get(&id));

    // ---- status row ----
    let engine_label = match &state.engine {
        EngineStatus::Starting => "引擎启动中…".to_string(),
        EngineStatus::Running => "引擎运行中".to_string(),
        EngineStatus::TelemetryOnly => "遥测模式（控制不可用）".to_string(),
        EngineStatus::Failed(e) => format!("引擎故障：{e}"),
    };
    let profile_label = match &state.desired.profile {
        Some(p) => format!("配置档：{p}"),
        None => "配置档：未应用".to_string(),
    };
    let thermal_label = match state.observed.thermal_mode.value() {
        Some(m) => format!("散热模式：{}", fmt::thermal_mode_zh(*m)),
        None => "散热模式：未知".to_string(),
    };
    let thermal_prov = match &state.observed.thermal_mode {
        ObservedValue::Unknown => None,
        v => Some(fmt::observed_provenance_zh(v)),
    };
    let ac_online = sample(ids::POWER_AC_ONLINE)
        .and_then(|s| s.value.as_f64())
        .map(|v| v > 0.5);
    let battery = sample(ids::POWER_BATTERY_PERCENT).and_then(|s| s.value.as_f64());
    let power_label = match (ac_online, battery) {
        (Some(true), Some(b)) => format!("电源：交流 · 电池 {b:.0}%"),
        (Some(false), Some(b)) => format!("电源：电池 {b:.0}%"),
        _ => "电源：未知".to_string(),
    };

    // ---- metric cards (CPU ×4 + GPU ×4) ----
    let cards = [
        ("CPU 温度", ids::CPU_PKG_TEMP_C, 1, "°C"),
        ("CPU 功率", ids::CPU_PKG_POWER_W, 1, "W"),
        ("CPU 有效频率", ids::CPU_EFFECTIVE_CLOCK_MHZ, 0, "MHz"),
        ("CPU 利用率", ids::CPU_UTIL_PERCENT, 0, "%"),
        ("GPU 温度", ids::GPU_TEMP_C, 1, "°C"),
        ("GPU 功率", ids::GPU_POWER_W, 1, "W"),
        ("GPU 核心频率", ids::GPU_CORE_CLOCK_MHZ, 0, "MHz"),
        ("GPU 利用率", ids::GPU_UTIL_PERCENT, 0, "%"),
    ];
    let card_row = |row: &[( &str, phelper_domain::telemetry::MetricId, usize, &str)]| {
        div()
            .h_flex()
            .w_full()
            .gap_3()
            .flex_wrap()
            .children(row.iter().map(|(title, id, dec, unit)| {
                MetricCard::from_sample(title, sample(*id), cadence(*id), *dec, unit).render(cx)
            }))
    };

    // ---- fan card ----
    let fan_cpu = sample(ids::FAN_CPU_RPM).and_then(|s| s.value.as_f64());
    let fan_gpu = sample(ids::FAN_GPU_RPM).and_then(|s| s.value.as_f64());
    let fan_value = match (fan_cpu, fan_gpu) {
        (Some(c), Some(g)) => format!("{c:.0} / {g:.0}"),
        _ => "—".to_string(),
    };
    let fan_mode_label = match state.observed.fan_mode.value() {
        Some(m) => format!(
            "{} · {}",
            fmt::fan_mode_zh(m),
            fmt::observed_provenance_zh(&state.observed.fan_mode)
        ),
        None => "模式未知".to_string(),
    };
    let fan_card = MetricCard::custom("风扇 CPU / GPU", fan_value, "RPM", fan_mode_label).render(cx);

    // ---- trend charts ----
    let cpu_pts = trend_chart::trend_points(app, ids::CPU_PKG_TEMP_C, ids::CPU_PKG_POWER_W);
    let gpu_pts = trend_chart::trend_points(app, ids::GPU_TEMP_C, ids::GPU_POWER_W);

    page_root("dashboard-scroll").child(
        div()
            .v_flex()
            .gap_3()
            .p_4()
            .w_full()
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(badge(cx, engine_label, None))
                    .child(badge(cx, profile_label, None))
                    .child(badge(cx, thermal_label, thermal_prov))
                    .child(badge(cx, power_label, None)),
            )
            .child(card_row(&cards[0..4]))
            .child(card_row(&cards[4..8]))
            .child(div().h_flex().w_full().gap_3().child(fan_card))
            .child(
                div()
                    .h_flex()
                    .w_full()
                    .gap_3()
                    .h(px(230.))
                    .child(trend_chart::render(
                        "cpu-trend",
                        "CPU 温度 / 功率（5 分钟）",
                        cpu_pts,
                        trend_chart::TrendSeries {
                            name: "温度 °C",
                            color: theme.chart_1,
                        },
                        trend_chart::TrendSeries {
                            name: "功率 W",
                            color: theme.chart_3,
                        },
                        cx,
                    ))
                    .child(trend_chart::render(
                        "gpu-trend",
                        "GPU 温度 / 功率（5 分钟）",
                        gpu_pts,
                        trend_chart::TrendSeries {
                            name: "温度 °C",
                            color: theme.chart_2,
                        },
                        trend_chart::TrendSeries {
                            name: "功率 W",
                            color: theme.chart_4,
                        },
                        cx,
                    )),
            ),
    )
}

/// Page root: gpui-component's scrollable (gpui's own overflow_y_scroll
/// mis-measures content width; the component rewrite exists for that).
pub fn page_root(id: &'static str) -> gpui_component::scroll::Scrollable<gpui::Stateful<Div>> {
    div().id(id).size_full().overflow_y_scrollbar()
}
