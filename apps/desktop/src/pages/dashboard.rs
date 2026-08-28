//! Dashboard: current status, CPU/GPU metric cards, fan card, and two
//! 5-minute trend charts. Pure `&AppState` render.

use gpui::{
    App, Div, InteractiveElement, IntoElement, ParentElement, Styled, div, prelude::FluentBuilder,
    px,
};
use gpui_component::{ActiveTheme, StyledExt, scroll::ScrollableElement};
use phelper_core::app::fmt;
use phelper_core::app::runtime::AppHandle;
use phelper_core::app::{AppState, EngineStatus};
use phelper_core::telemetry::registry;
use phelper_domain::policy::FanMode;
use phelper_domain::telemetry::ids;

use crate::widgets::metric_card::MetricCard;
use crate::widgets::trend_chart;

fn cadence(id: phelper_domain::telemetry::MetricId) -> std::time::Duration {
    registry::meta(id)
        .map(|m| m.cadence)
        .unwrap_or(std::time::Duration::from_secs(1))
}

fn badge(cx: &App, label: String) -> Div {
    let theme = cx.theme();
    div()
        .h_flex()
        .gap_2()
        .px_3()
        .py_1()
        .rounded_full()
        .border_1()
        .border_color(theme.border)
        .child(div().text_xs().child(label))
}

fn fan_mode_label(mode: &FanMode) -> String {
    match mode {
        FanMode::FirmwareAuto => "未接管".into(),
        _ => fmt::fan_mode_zh(mode),
    }
}

fn profile_name(name: &str) -> &str {
    match name {
        "silent" => "安静",
        "balanced" => "均衡",
        "gaming" => "游戏",
        "cpu-max" => "极致",
        _ => name,
    }
}

/// Per-page view state (§42): one 1 Hz cache for each same-unit chart.
#[derive(Default)]
pub struct DashState {
    pub temp_chart: trend_chart::ChartCache,
    pub power_chart: trend_chart::ChartCache,
}

pub fn render(state: &AppState, app: &AppHandle, dash: &DashState, cx: &App) -> impl IntoElement {
    let root = page_root("dashboard-scroll");
    if state.telemetry.is_none()
        && matches!(state.engine, EngineStatus::Starting | EngineStatus::Running)
    {
        return root.child(skeleton_content(cx));
    }

    let theme = cx.theme();
    let snap = state.telemetry.as_deref();
    let sample = |id: phelper_domain::telemetry::MetricId| snap.and_then(|s| s.samples.get(&id));

    // ---- status row ----
    let profile_label = match &state.desired.profile {
        Some(p) => format!("配置 · {}", profile_name(p)),
        None => "配置 · 未应用".to_string(),
    };
    let thermal_label = match state.observed.thermal_mode.value() {
        Some(m) => format!("散热 · {}", fmt::thermal_mode_zh(*m)),
        None => "散热 · —".to_string(),
    };
    let ac_online = sample(ids::POWER_AC_ONLINE)
        .and_then(|s| s.value.as_f64())
        .map(|v| v > 0.5);
    let battery = sample(ids::POWER_BATTERY_PERCENT).and_then(|s| s.value.as_f64());
    let power_label = match (ac_online, battery) {
        (Some(true), Some(b)) => format!("交流 · 电池 {b:.0}%"),
        (Some(false), Some(b)) => format!("电池 · {b:.0}%"),
        _ => "电源 · —".to_string(),
    };

    // ---- metric cards: overview values only ----------------------------
    let cards = [
        ("CPU 温度", ids::CPU_PKG_TEMP_C, 1, "°C"),
        ("CPU 功率", ids::CPU_PKG_POWER_W, 1, "W"),
        ("CPU 利用率", ids::CPU_UTIL_PERCENT, 0, "%"),
        ("GPU 温度", ids::GPU_TEMP_C, 1, "°C"),
        ("GPU 功率", ids::GPU_POWER_W, 1, "W"),
        ("GPU 利用率", ids::GPU_UTIL_PERCENT, 0, "%"),
    ];
    let card_row = |row: &[(&str, phelper_domain::telemetry::MetricId, usize, &str)]| {
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
        Some(m) => fan_mode_label(m),
        None => "模式未知".to_string(),
    };
    let fan_card = MetricCard::custom("风扇", fan_value, "RPM", fan_mode_label).render(cx);

    // ---- trend charts ----
    let temp_pts = dash.temp_chart.points(
        app,
        ids::CPU_PKG_TEMP_C,
        ids::GPU_TEMP_C,
        cx.background_executor().clone(),
    );
    let power_pts = dash.power_chart.points(
        app,
        ids::CPU_PKG_POWER_W,
        ids::GPU_POWER_W,
        cx.background_executor().clone(),
    );
    let has_temp_trend = !temp_pts.is_empty();
    let has_power_trend = !power_pts.is_empty();
    let temp_chart = if has_temp_trend {
        Some(
            trend_chart::render(
                "温度（5 分钟）",
                temp_pts,
                trend_chart::TrendSeries {
                    name: "CPU",
                    color: theme.warning,
                },
                trend_chart::TrendSeries {
                    name: "GPU",
                    color: theme.info,
                },
                trend_chart::TEMPERATURE_RANGE,
                "°C",
                cx,
            )
            .into_any_element(),
        )
    } else if !dash.temp_chart.is_ready() {
        Some(trend_chart::skeleton("温度（5 分钟）", cx).into_any_element())
    } else {
        None
    };
    let power_chart = if has_power_trend {
        Some(
            trend_chart::render(
                "功率（5 分钟）",
                power_pts,
                trend_chart::TrendSeries {
                    name: "CPU",
                    color: theme.warning,
                },
                trend_chart::TrendSeries {
                    name: "GPU",
                    color: theme.info,
                },
                trend_chart::POWER_RANGE,
                "W",
                cx,
            )
            .into_any_element(),
        )
    } else if !dash.power_chart.is_ready() {
        Some(trend_chart::skeleton("功率（5 分钟）", cx).into_any_element())
    } else {
        None
    };
    let trend_row = if temp_chart.is_some() || power_chart.is_some() {
        let mut row = div().h_flex().w_full().gap_3().h(px(230.));
        if let Some(chart) = temp_chart {
            row = row.child(chart);
        }
        if let Some(chart) = power_chart {
            row = row.child(chart);
        }
        Some(row)
    } else {
        None
    };

    root.child(
        div()
            .v_flex()
            .gap_3()
            .p_4()
            .w_full()
            .child(div().text_xl().font_semibold().child("仪表盘"))
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(badge(cx, profile_label))
                    .child(badge(cx, thermal_label))
                    .child(badge(cx, power_label)),
            )
            .child(card_row(&cards[0..3]))
            .child(card_row(&cards[3..6]))
            .child(div().h_flex().w_full().gap_3().child(fan_card))
            .when_some(trend_row, |d, row| d.child(row)),
    )
}

fn skeleton_block(cx: &App, width: f32, height: f32) -> Div {
    div()
        .w(px(width))
        .h(px(height))
        .rounded_md()
        .bg(cx.theme().muted.opacity(0.22))
}

fn metric_skeleton(cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .v_flex()
        .gap_2()
        .p_3()
        .min_w_0()
        .flex_1()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .bg(theme.group_box)
        .child(skeleton_block(cx, 82., 12.))
        .child(skeleton_block(cx, 104., 28.))
}

fn metric_skeleton_row(cx: &App, count: usize) -> Div {
    let mut row = div().h_flex().w_full().gap_3();
    for _ in 0..count {
        row = row.child(metric_skeleton(cx));
    }
    row
}

fn skeleton_content(cx: &App) -> Div {
    let mut status = div().h_flex().w_full().gap_2();
    for width in [96., 118., 104.] {
        status = status.child(skeleton_block(cx, width, 24.));
    }
    div()
        .v_flex()
        .gap_3()
        .p_4()
        .w_full()
        .child(status)
        .child(metric_skeleton_row(cx, 3))
        .child(metric_skeleton_row(cx, 3))
        .child(div().h_flex().w_full().gap_3().child(metric_skeleton(cx)))
        .child(
            div()
                .h_flex()
                .w_full()
                .gap_3()
                .h(px(230.))
                .child(trend_chart::skeleton("温度（5 分钟）", cx))
                .child(trend_chart::skeleton("功率（5 分钟）", cx)),
        )
}

/// Page root: gpui-component's scrollable (gpui's own overflow_y_scroll
/// mis-measures content width; the component rewrite exists for that).
pub fn page_root(id: &'static str) -> gpui_component::scroll::Scrollable<gpui::Stateful<Div>> {
    div().id(id).size_full().overflow_y_scrollbar()
}
