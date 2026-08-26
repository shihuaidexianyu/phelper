//! Thermals (plan D-G): fan mode segmented buttons (固件自动/最大/手动) +
//! dual manual RPM sliders (step 100, range = caps.fan clamp — sliders are
//! created lazily by the shell once the clamp is probed; SliderState has no
//! runtime min/max setters) + temp/RPM trend charts + heartbeat semantics.
//! Manual sliders stay disabled until the mode is actually Manual (an
//! implicit mode-switch on drag would be a surprise write — fail closed).

use gpui::{Context, IntoElement, ParentElement, Styled, Window, div, prelude::FluentBuilder, px};
use gpui_component::{ActiveTheme, Disableable, StyledExt, button::{Button, ButtonVariants}, slider::Slider};
use phelper_core::app::fmt;
use phelper_core::app::runtime::AppHandle;
use phelper_core::app::state::{KnobId, KnobStatus, knob_enabled};
use phelper_core::app::AppState;
use phelper_domain::command::ControlCommand;
use phelper_domain::policy::{FanLevels, FanMode};
use phelper_domain::telemetry::ids;

use crate::shell::{ShellView, ThermalState};
use crate::widgets::{knob_row, trend_chart};

use super::dashboard::page_root;

pub fn render(
    state: &AppState,
    app: &AppHandle,
    thermal: &ThermalState,
    cx: &mut Context<ShellView>,
) -> impl IntoElement {
    let theme = cx.theme();

    if !state.writes_available() {
        return page_root("thermals-scroll").child(
            div().v_flex().p_4().child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("控制不可用（遥测模式）——本页写入控件已隐藏"),
            ),
        );
    }

    let reason = knob_enabled(state.caps.as_ref(), KnobId::FanMode, &state.experimental).err();
    let status = state.knobs.get(&KnobId::FanMode).cloned().unwrap_or_default();
    let idle = KnobStatus::Idle;

    let snap = state.telemetry.as_deref();
    let live = |id: phelper_domain::telemetry::MetricId| {
        snap.and_then(|s| s.samples.get(&id)).and_then(|s| s.value.as_f64())
    };
    let live_cpu = live(ids::FAN_CPU_RPM);
    let live_gpu = live(ids::FAN_GPU_RPM);
    let cur_mode = state.observed.fan_mode.value();
    // 0x27 max-fan is a separate flag OVER the mode (0x2E) — while it is
    // on, the honest "current" is 最大转速 regardless of the mode readback
    // (D8 on-device: card showed 固件自动 active with fans at full tilt).
    let max_on = state.observed.max_fan.value().copied().unwrap_or(false);
    let manual_active = !max_on && matches!(cur_mode, Some(FanMode::Manual(_)));

    // ---- mode card: segmented buttons ----
    let mode_card = {
        let observed_line = if max_on {
            format!(
                "当前：最大转速（{}）",
                fmt::observed_provenance_zh(&state.observed.max_fan)
            )
        } else {
            match cur_mode {
                Some(m) => format!(
                    "当前：{}（{}）",
                    fmt::fan_mode_zh(m),
                    fmt::observed_provenance_zh(&state.observed.fan_mode)
                ),
                None => "当前：未知".to_string(),
            }
        };
        let mk_btn = |label: &'static str,
                      target: FanMode,
                      active: bool,
                      extra_disabled: bool,
                      tag: usize| {
            let app2 = app.clone();
            Button::new(("fan-mode", tag))
                .label(label)
                .when(active, |btn| btn.primary())
                .when(!active, |btn| btn.outline())
                .disabled(reason.is_some() || extra_disabled)
                .on_click(cx.listener(move |_, _: &gpui::ClickEvent, _: &mut Window, _cx| {
                    app2.dispatch(KnobId::FanMode, ControlCommand::SetFanMode(target));
                }))
        };
        let manual_target = FanMode::Manual(FanLevels::new(thermal.cpu_rpm, thermal.gpu_rpm));
        div()
            .v_flex()
            .gap_2()
            .w_full()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.group_box)
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .child(div().text_base().font_semibold().child("风扇模式"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(observed_line),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .gap_1()
                    .child(mk_btn("固件自动", FanMode::FirmwareAuto, !max_on && matches!(cur_mode, Some(FanMode::FirmwareAuto)), false, 0))
                    .child(mk_btn("最大转速", FanMode::Max, max_on, false, 1))
                    .child(mk_btn("手动", manual_target, manual_active, thermal.fan_sliders.is_none(), 2)),
            )
            .when_some(reason, |d, r| {
                d.child(div().text_xs().text_color(theme.muted_foreground).child(r))
            })
            .child(
                div().h_flex().justify_end().w_full().child(
                    knob_row::status_badge(cx, &status).unwrap_or_else(div),
                ),
            )
    };

    // ---- manual levels card ----
    let manual_card = {
        let clamp_note = state.caps.as_ref().and_then(|c| {
            match (c.fan.clamp_min, c.fan.clamp_max) {
                (Some(lo), Some(hi)) => Some(format!(
                    "允许范围 {}–{} RPM（能力探测值，步进 100）",
                    lo * 100,
                    hi * 100
                )),
                _ => None,
            }
        });
        let mode_hint: Option<&'static str> = if max_on {
            Some("最大转速优先——手动调节在退出最大转速后可用")
        } else if manual_active {
            None
        } else {
            Some("先切换到「手动」模式再调节转速")
        };
        let mut rows = div().v_flex().gap_1().w_full();
        match &thermal.fan_sliders {
            Some((cpu_e, gpu_e)) => {
                for (label, entity, set_rpm, live_rpm) in [
                    ("CPU 风扇", cpu_e, thermal.cpu_rpm, live_cpu),
                    ("GPU 风扇", gpu_e, thermal.gpu_rpm, live_gpu),
                ] {
                    let row_reason = reason.or(mode_hint);
                    let live_label = live_rpm
                        .map(|v| format!("{v:.0}"))
                        .unwrap_or_else(|| "—".into());
                    rows = rows.child(knob_row::knob_row(
                        cx,
                        label,
                        Slider::new(entity).disabled(row_reason.is_some()),
                        format!("设：{} RPM · 当前：{live_label} RPM", set_rpm * 100),
                        &idle,
                        row_reason,
                    ));
                }
            }
            None => {
                rows = rows.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("手动转速范围未探测——手动调节不可用（固件自动 / 最大转速不受影响）"),
                );
            }
        }
        div()
            .v_flex()
            .gap_2()
            .w_full()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.group_box)
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .child(div().text_base().font_semibold().child("手动转速"))
                    .when_some(clamp_note, |d, n| {
                        d.child(div().text_xs().text_color(theme.muted_foreground).child(n))
                    }),
            )
            .child(rows)
    };

    // ---- trends ----
    let temp_pts = thermal.temp_chart.points(app, ids::CPU_PKG_TEMP_C, ids::GPU_TEMP_C);
    let fan_pts = thermal.fan_chart.points(app, ids::FAN_CPU_RPM, ids::FAN_GPU_RPM);
    let charts = div()
        .h_flex()
        .w_full()
        .gap_3()
        .h(px(230.))
        .child(trend_chart::render(
            "therm-temp-trend",
            "温度（5 分钟）",
            temp_pts,
            trend_chart::TrendSeries { name: "CPU °C", color: theme.chart_1 },
            trend_chart::TrendSeries { name: "GPU °C", color: theme.chart_2 },
            cx,
        ))
        .child(trend_chart::render(
            "therm-fan-trend",
            "风扇转速（5 分钟）",
            fan_pts,
            trend_chart::TrendSeries { name: "CPU RPM", color: theme.chart_3 },
            trend_chart::TrendSeries { name: "GPU RPM", color: theme.chart_4 },
            cx,
        ));

    // ---- heartbeat / restore semantics ----
    let safety_card = div()
        .v_flex()
        .gap_1()
        .w_full()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(theme.info)
        .bg(theme.group_box)
        .child(div().text_base().font_semibold().child("保持与恢复（心跳语义）"))
        .child(
            div()
                .v_flex()
                .gap_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("手动 / 最大转速在应用运行期间由 60 秒心跳保持；正常退出立即恢复固件自动（AR-12）")
                .child("手动写入要求温度数据新鲜（≤ 5 秒）；传感器断流 90 秒自动回固件控制")
                .child("≥ 90 °C 安全监控强制最大风扇，≤ 85 °C 释放回手动")
                .child("异常退出由固件看门狗约 120 秒兜底——本应用永远不会是唯一的散热保障"),
        );

    let banner = knob_row::outcome_banner(cx, state.evidence.back(), thermal.banner_expanded, |this: &mut ShellView| {
        this.thermal.banner_expanded = !this.thermal.banner_expanded;
    });

    page_root("thermals-scroll").child(
        div()
            .v_flex()
            .gap_3()
            .p_4()
            .w_full()
            .child(mode_card)
            .child(manual_card)
            .child(charts)
            .child(safety_card)
            .when_some(banner, |d, b| d.child(b)),
    )
}
