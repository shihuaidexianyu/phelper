//! Read-only overview: engine state, current intent, and essential telemetry.

use std::time::Duration;

use gpui::{Div, InteractiveElement, ParentElement, Styled, div, prelude::FluentBuilder};
use gpui_component::{ActiveTheme, StyledExt, scroll::ScrollableElement};
use phelper_core::app::{AppState, EngineStatus, fmt};
use phelper_core::telemetry::registry;
use phelper_domain::policy::FanMode;
use phelper_domain::telemetry::{MetricId, MetricQuality, ids};

use crate::widgets::metric_card::MetricCard;

fn cadence(id: MetricId) -> Duration {
    registry::meta(id)
        .map(|meta| meta.cadence)
        .unwrap_or(Duration::from_secs(1))
}

fn fan_mode_label(mode: &FanMode) -> String {
    match mode {
        FanMode::FirmwareAuto => "固件自动".into(),
        _ => fmt::fan_mode_zh(mode),
    }
}

pub fn render(
    state: &AppState,
    cx: &mut gpui::Context<crate::shell::ShellView>,
) -> gpui_component::scroll::Scrollable<gpui::Stateful<Div>> {
    let theme = cx.theme();
    let engine_banner = match &state.engine {
        EngineStatus::Starting | EngineStatus::Running => None,
        EngineStatus::TelemetryOnly => Some((
            "当前为只读模式；配置档控制已停用。".to_string(),
            theme.warning,
        )),
        EngineStatus::Failed(error) => Some((format!("启动失败：{error}"), theme.danger)),
    };

    let snapshot = state.telemetry.as_deref();
    let loading = snapshot.is_none() && !matches!(state.engine, EngineStatus::Failed(_));
    let sample = |id: MetricId| snapshot.and_then(|snapshot| snapshot.samples.get(&id));
    let fan_mode = state
        .observed
        .fan_mode
        .value()
        .map(fan_mode_label)
        .unwrap_or_else(|| "模式未知".into());

    let metrics = [
        ("CPU 温度", ids::CPU_PKG_TEMP_C, 1, "°C"),
        ("CPU 功率", ids::CPU_PKG_POWER_W, 1, "W"),
        ("CPU 利用率", ids::CPU_UTIL_PERCENT, 0, "%"),
        ("GPU 温度", ids::GPU_TEMP_C, 1, "°C"),
        ("GPU 功率", ids::GPU_POWER_W, 1, "W"),
        ("GPU 利用率", ids::GPU_UTIL_PERCENT, 0, "%"),
    ];
    let metric_row = |items: &[(&str, MetricId, usize, &str)]| {
        div()
            .h_flex()
            .w_full()
            .gap_3()
            .flex_wrap()
            .children(items.iter().map(|(title, id, decimals, unit)| {
                if loading {
                    MetricCard::skeleton(title, false).render(cx)
                } else {
                    MetricCard::from_sample(title, sample(*id), cadence(*id), *decimals, unit)
                        .render(cx)
                }
            }))
    };

    let cpu_fan = sample(ids::FAN_CPU_RPM);
    let gpu_fan = sample(ids::FAN_GPU_RPM);
    let fan_value = match (
        cpu_fan.and_then(|sample| sample.value.as_f64()),
        gpu_fan.and_then(|sample| sample.value.as_f64()),
    ) {
        (Some(cpu), Some(gpu)) => format!("{cpu:.0} / {gpu:.0}"),
        _ => "—".into(),
    };
    let fan_stale = cpu_fan.is_none()
        || gpu_fan.is_none()
        || cpu_fan.is_some_and(|sample| sample.timestamp.elapsed() > cadence(ids::FAN_CPU_RPM) * 3)
        || gpu_fan.is_some_and(|sample| sample.timestamp.elapsed() > cadence(ids::FAN_GPU_RPM) * 3);
    let fan_quality = if cpu_fan.is_none() || gpu_fan.is_none() {
        Some(MetricQuality::Unavailable)
    } else {
        [cpu_fan, gpu_fan]
            .into_iter()
            .flatten()
            .map(|sample| sample.quality)
            .find(|quality| !matches!(quality, MetricQuality::Fresh))
            .or_else(|| fan_stale.then_some(MetricQuality::Stale))
    };
    let fan_card = if loading {
        MetricCard::skeleton("风扇", true).render(cx)
    } else {
        MetricCard::custom(
            "风扇",
            fan_value,
            "RPM",
            fan_mode.clone(),
            fan_quality,
            fan_stale,
        )
        .render(cx)
    };

    page_root("dashboard-scroll").child(
        div()
            .v_flex()
            .gap_3()
            .p_4()
            .w_full()
            .when_some(engine_banner, |content, (message, color)| {
                content.child(
                    div()
                        .w_full()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .border_1()
                        .border_color(color.opacity(0.45))
                        .text_sm()
                        .text_color(color)
                        .child(message),
                )
            })
            .child(metric_row(&metrics[0..3]))
            .child(metric_row(&metrics[3..6]))
            .child(div().h_flex().w_full().child(fan_card)),
    )
}

pub fn page_root(id: &'static str) -> gpui_component::scroll::Scrollable<gpui::Stateful<Div>> {
    div().id(id).size_full().overflow_y_scrollbar()
}
