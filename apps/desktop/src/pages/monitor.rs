//! Monitor: a compact live-value table. It answers only "what is the machine
//! doing right now?"; raw register and provider details stay out of the main
//! control surface.

use gpui::{Context, IntoElement, ParentElement, Styled, div, px};
use gpui_component::{ActiveTheme, StyledExt, input::Input};
use phelper_core::app::AppState;
use phelper_core::app::fmt;
use phelper_core::telemetry::registry;
use phelper_domain::telemetry::{MetricId, MetricQuality, ids};

use crate::shell::{MonitorState, ShellView};

use super::dashboard::page_root;

fn metric_label(id: MetricId) -> &'static str {
    match id {
        ids::CPU_PKG_TEMP_C => "CPU 温度",
        ids::CPU_TJ_MAX_C => "CPU 温度上限",
        ids::CPU_PKG_POWER_W => "CPU 功率",
        ids::CPU_EFFECTIVE_CLOCK_MHZ => "CPU 有效频率",
        ids::CPU_PL1_W => "CPU 功耗上限 PL1",
        ids::CPU_PL2_W => "CPU 功耗上限 PL2",
        ids::CPU_PL4_W => "CPU 功耗上限 PL4",
        ids::CPU_EPP_AC => "CPU EPP · 交流",
        ids::CPU_EPP_DC => "CPU EPP · 电池",
        ids::CPU_EPP1_AC => "E 核 EPP · 交流",
        ids::CPU_EPP1_DC => "E 核 EPP · 电池",
        ids::CPU_UTIL_PERCENT => "CPU 利用率",
        ids::MEM_USED_BYTES => "内存已用",
        ids::MEM_TOTAL_BYTES => "内存总量",
        ids::DISK_READ_BPS => "磁盘读取",
        ids::DISK_WRITE_BPS => "磁盘写入",
        ids::NET_RX_BPS => "网络接收",
        ids::NET_TX_BPS => "网络发送",
        ids::GPU_TEMP_C => "GPU 温度",
        ids::GPU_POWER_W => "GPU 功率",
        ids::GPU_UTIL_PERCENT => "GPU 利用率",
        ids::GPU_CORE_CLOCK_MHZ => "GPU 核心频率",
        ids::GPU_MEM_CLOCK_MHZ => "GPU 显存频率",
        ids::GPU_PSTATE => "GPU 状态",
        ids::GPU_VRAM_USED_BYTES => "GPU 显存已用",
        ids::GPU_POWER_LIMIT_W => "GPU 功耗上限",
        ids::FAN_CPU_RPM => "CPU 风扇",
        ids::FAN_GPU_RPM => "GPU 风扇",
        ids::POWER_AC_ONLINE => "电源",
        ids::POWER_BATTERY_PERCENT => "电池电量",
        // Raw register/bitmask metrics stay out of the user-facing table.
        _ => id.0,
    }
}

pub(crate) fn is_monitor_metric(id: MetricId) -> bool {
    !matches!(
        id,
        ids::CPU_TJ_MAX_C
            | ids::CPU_PL1_W
            | ids::CPU_PL2_W
            | ids::CPU_PL4_W
            | ids::CPU_EPP_AC
            | ids::CPU_EPP_DC
            | ids::CPU_EPP1_AC
            | ids::CPU_EPP1_DC
            | ids::GPU_POWER_LIMIT_W
            | ids::CPU_THERMAL_STATUS_RAW
            | ids::CPU_POWER_LIMIT_RAW
            | ids::GPU_THROTTLE_REASONS_RAW
    )
}

fn bytes_zh(v: f64) -> String {
    if v >= 1024. * 1024. * 1024. {
        format!("{:.1} GB", v / (1024. * 1024. * 1024.))
    } else if v >= 1024. * 1024. {
        format!("{:.1} MB", v / (1024. * 1024.))
    } else if v >= 1024. {
        format!("{:.1} KB", v / 1024.)
    } else {
        format!("{v:.0} B")
    }
}

fn rate_zh(v: f64) -> String {
    if v >= 1024. * 1024. {
        format!("{:.1} MB/s", v / (1024. * 1024.))
    } else if v >= 1024. {
        format!("{:.1} KB/s", v / 1024.)
    } else {
        format!("{v:.0} B/s")
    }
}

fn value_zh(id: MetricId, v: f64, unit: &str) -> String {
    match id {
        ids::MEM_USED_BYTES | ids::MEM_TOTAL_BYTES | ids::GPU_VRAM_USED_BYTES => bytes_zh(v),
        ids::DISK_READ_BPS | ids::DISK_WRITE_BPS | ids::NET_RX_BPS | ids::NET_TX_BPS => rate_zh(v),
        ids::POWER_AC_ONLINE => {
            if v > 0.5 {
                "交流".into()
            } else {
                "电池".into()
            }
        }
        _ if v.abs() >= 100. => format!("{v:.0} {unit}"),
        _ => format!("{v:.1} {unit}"),
    }
}

pub fn render(
    state: &AppState,
    mon: &MonitorState,
    cx: &mut Context<ShellView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let query = mon.filter.read(cx).text().to_string().to_lowercase();
    let snap = state.telemetry.as_deref();
    let w_id = px(210.);
    let w_val = px(140.);
    let w_quality = px(64.);
    let page_header = div()
        .h_flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(div().text_xl().font_semibold().child("监视器"))
        .child(div().w(px(240.)).child(Input::new(&mon.filter)));
    let header = div()
        .h_flex()
        .gap_2()
        .py_1()
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(div().w(w_id).child("指标"))
        .child(div().w(w_val).child("当前值"))
        .child(div().w(w_quality).child("状态"));

    let mut shown = 0usize;
    let mut rows = div().v_flex().w_full();
    for meta in registry::all() {
        if !is_monitor_metric(meta.id) {
            continue;
        }
        let label = metric_label(meta.id);
        if !query.is_empty()
            && !label.to_lowercase().contains(query.as_str())
            && !meta.id.0.contains(query.as_str())
        {
            continue;
        }
        shown += 1;
        let sample = snap.and_then(|s| s.samples.get(&meta.id));
        let (value_s, quality, stale) = match sample {
            Some(s) => {
                let age = s.timestamp.elapsed();
                let stale = age > meta.cadence * 3
                    || matches!(
                        s.quality,
                        MetricQuality::Stale
                            | MetricQuality::Unavailable
                            | MetricQuality::Unsupported
                    )
                    || s.value.as_f64().is_none();
                (
                    s.value
                        .as_f64()
                        .map(|v| value_zh(meta.id, v, meta.unit))
                        .unwrap_or_else(|| "—".into()),
                    if stale {
                        "陈旧"
                    } else {
                        fmt::quality_zh(s.quality)
                    },
                    stale,
                )
            }
            None => ("—".into(), "不可用", true),
        };
        let value_color = if stale {
            theme.muted_foreground
        } else {
            theme.foreground
        };
        let quality_color = if stale {
            theme.warning
        } else {
            theme.muted_foreground
        };
        // Healthy samples need no badge. Reserve the column only for a
        // stale/degraded value so the table stays quiet during normal use.
        let quality_s = if stale || quality != "实时" {
            quality
        } else {
            ""
        };
        rows = rows.child(
            div()
                .h_flex()
                .gap_2()
                .py_px()
                .child(div().w(w_id).text_sm().child(label))
                .child(
                    div()
                        .w(w_val)
                        .text_sm()
                        .text_color(value_color)
                        .child(value_s),
                )
                .child(
                    div()
                        .w(w_quality)
                        .text_sm()
                        .text_color(quality_color)
                        .child(quality_s),
                ),
        );
    }
    if shown == 0 {
        rows = rows.child(
            div()
                .py_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("没有匹配的指标"),
        );
    }

    page_root("monitor-scroll").child(
        div()
            .v_flex()
            .gap_2()
            .p_4()
            .w_full()
            .child(page_header)
            .child(header)
            .child(rows),
    )
}
