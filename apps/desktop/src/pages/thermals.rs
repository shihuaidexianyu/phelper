//! Fan controls used by the combined Performance page: fan mode segmented
//! buttons (曲线/最大/手动) + dual manual RPM sliders (step 100, range =
//! caps.fan clamp — sliders are created lazily by the shell once the clamp is
//! probed; SliderState has no runtime min/max setters).
//! Manual sliders stay disabled until the mode is actually Manual (an
//! implicit mode-switch on drag would be a surprise write — fail closed).

use gpui::{Context, Entity, ParentElement, Styled, Window, div, prelude::FluentBuilder, px};
use gpui_component::{
    ActiveTheme, Disableable, StyledExt,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
    slider::Slider,
};
use phelper_core::app::AppState;
use phelper_core::app::fmt;
use phelper_core::app::runtime::AppHandle;
use phelper_core::app::state::{KnobId, KnobStatus, knob_enabled};
use phelper_domain::command::ControlCommand;
use phelper_domain::policy::{FAN_CURVE_POINT_COUNT, FanCurve, FanCurvePoint, FanLevels, FanMode};
use phelper_domain::telemetry::ids;

use crate::shell::{CurveOrigin, ShellView, ThermalState};
use crate::widgets::knob_row;

fn fan_mode_label(mode: &FanMode) -> String {
    match mode {
        // FirmwareAuto is an internal fail-safe state, not a user-selectable
        // software mode. Keep the status honest without exposing the
        // firmware handoff as if it were an app-controlled automatic curve.
        FanMode::FirmwareAuto => "未接管".into(),
        _ => fmt::fan_mode_zh(mode),
    }
}

pub fn render_content(
    state: &AppState,
    app: &AppHandle,
    thermal: &ThermalState,
    cx: &mut Context<ShellView>,
) -> gpui::Div {
    let theme = cx.theme();

    let reason = knob_enabled(state.caps.as_ref(), KnobId::FanMode, &state.experimental).err();
    let idle = KnobStatus::Idle;

    let snap = state.telemetry.as_deref();
    let live = |id: phelper_domain::telemetry::MetricId| {
        snap.and_then(|s| s.samples.get(&id))
            .and_then(|s| s.value.as_f64())
    };
    let live_cpu = live(ids::FAN_CPU_RPM);
    let live_gpu = live(ids::FAN_GPU_RPM);
    let cur_mode = state.observed.fan_mode.value();
    // 0x27 max-fan is a separate flag OVER the mode (0x2E) — while it is
    // on, the honest "current" is 最大转速 regardless of the mode readback.
    let max_on = state.observed.max_fan.value().copied().unwrap_or(false);
    let manual_active = !max_on && matches!(cur_mode, Some(FanMode::Manual(_)));
    let curve_active = !max_on && matches!(cur_mode, Some(FanMode::Curve(_)));
    let live_fan = format!(
        "CPU {} · GPU {} RPM",
        live_cpu.map_or_else(|| "—".to_string(), |rpm| format!("{rpm:.0}")),
        live_gpu.map_or_else(|| "—".to_string(), |rpm| format!("{rpm:.0}")),
    );

    // ---- mode card: segmented buttons ----
    let mode_card = {
        let observed_line = if max_on {
            "全速".to_string()
        } else {
            match cur_mode {
                Some(m) => fan_mode_label(m),
                None => "未接管".to_string(),
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
                .on_click(
                    cx.listener(move |_, _: &gpui::ClickEvent, _: &mut Window, _cx| {
                        app2.dispatch(KnobId::FanMode, ControlCommand::SetFanMode(target));
                    }),
                )
        };
        let curve_settings = Button::new("fan-curve-settings")
            .label(if thermal.curve_expanded {
                "收起设置"
            } else {
                "曲线设置"
            })
            .outline()
            .on_click(cx.listener(
                |this: &mut ShellView, _: &gpui::ClickEvent, _: &mut Window, cx| {
                    this.thermal.curve_expanded = !this.thermal.curve_expanded;
                    cx.notify();
                },
            ));
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
                    .items_center()
                    .gap_2()
                    .child(div().text_base().font_semibold().child("风扇"))
                    .child(
                        div()
                            .px_2()
                            .py(px(2.))
                            .rounded_full()
                            .border_1()
                            .border_color(theme.border)
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(observed_line),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(live_fan),
                    )
                    .child(div().flex_1())
                    .child(curve_settings),
            )
            .child(
                div()
                    .h_flex()
                    .gap_1()
                    .child(mk_btn("最大转速", FanMode::Max, max_on, false, 0))
                    .child(mk_btn(
                        "手动",
                        manual_target,
                        manual_active,
                        thermal.fan_sliders.is_none(),
                        1,
                    ))
                    .child(mk_btn(
                        "曲线",
                        FanMode::Curve(thermal.curve),
                        curve_active,
                        !thermal.curve_seeded,
                        2,
                    )),
            )
            .when_some(reason, |d, r| {
                d.child(div().text_xs().text_color(theme.muted_foreground).child(r))
            })
    };

    // ---- manual levels card ----
    let manual_card = {
        let clamp_note =
            state
                .caps
                .as_ref()
                .and_then(|c| match (c.fan.clamp_min, c.fan.clamp_max) {
                    (Some(lo), Some(hi)) => Some(format!("{}–{} RPM", lo * 100, hi * 100)),
                    _ => None,
                });
        let mut rows = div().v_flex().gap_1().w_full();
        match &thermal.fan_sliders {
            Some((cpu_e, gpu_e)) => {
                for (label, entity, set_rpm, live_rpm) in [
                    ("CPU 风扇", cpu_e, thermal.cpu_rpm, live_cpu),
                    ("GPU 风扇", gpu_e, thermal.gpu_rpm, live_gpu),
                ] {
                    let row_reason = reason;
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
                        .child("手动转速暂不可用"),
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
                    .child(div().text_base().font_semibold().child("手动"))
                    .when_some(clamp_note, |d, n| {
                        d.child(div().text_xs().text_color(theme.muted_foreground).child(n))
                    }),
            )
            .child(rows)
    };

    // ---- software curve card ------------------------------------------
    let curve_card = if thermal.curve_expanded {
        let apply_app = app.clone();
        let curve_source = if curve_active {
            "当前使用"
        } else {
            match thermal.curve_origin {
                Some(CurveOrigin::Saved) => "上次保存 · 尚未应用",
                Some(CurveOrigin::Profile) => "配置档 · 尚未应用",
                Some(CurveOrigin::Draft) => "待应用",
                Some(CurveOrigin::Active) => "当前使用",
                None => "固件曲线不可读取",
            }
        };
        let apply_button = Button::new("fan-curve-apply")
            .label("应用曲线")
            .primary()
            .disabled(reason.is_some() || !thermal.curve_seeded)
            .on_click(cx.listener(
                move |this: &mut ShellView, _: &gpui::ClickEvent, _: &mut Window, cx| {
                    let inputs = this.thermal.curve_inputs.clone();
                    let read = |index: usize| {
                        inputs[index]
                            .read(cx)
                            .text()
                            .to_string()
                    };
                    let parse_u8 = |index: usize, label: &str| {
                        read(index)
                            .trim()
                            .parse::<u8>()
                            .map_err(|_| format!("{label} 不是有效温度"))
                    };
                    let parse_rpm = |index: usize, label: &str| {
                        let rpm = read(index)
                            .trim()
                            .parse::<u32>()
                            .map_err(|_| format!("{label} 不是有效转速"))?;
                        if rpm == 0 || rpm % 100 != 0 || rpm / 100 > u16::MAX as u32 {
                            return Err(format!("{label} 需为 100 RPM 的正整数倍"));
                        }
                        Ok((rpm / 100) as u16)
                    };
                    let curve = (|| {
                        let mut points = [FanCurvePoint::new(0, 0, 0); FAN_CURVE_POINT_COUNT];
                        for (row, point) in points.iter_mut().enumerate() {
                            let base = row * 3;
                            *point = FanCurvePoint::new(
                                parse_u8(base, "温度")?,
                                parse_rpm(base + 1, "CPU 风扇")?,
                                parse_rpm(base + 2, "GPU 风扇")?,
                            );
                        }
                        let curve = FanCurve::new(points);
                        curve.validate().map_err(str::to_owned)?;
                        if let Some(caps) = this.state.caps.as_ref() {
                            let (Some(lo), Some(hi)) = (caps.fan.clamp_min, caps.fan.clamp_max)
                            else {
                                return Err("当前没有可用的风扇范围".to_string());
                            };
                            for (row, point) in curve.points.iter().enumerate() {
                                if point.cpu < lo
                                    || point.cpu > hi
                                    || point.gpu < lo
                                    || point.gpu > hi
                                {
                                    return Err(format!(
                                        "第 {} 行风扇转速超出当前设备范围",
                                        row + 1
                                    ));
                                }
                            }
                        }
                        Ok(curve)
                    })();

                    match curve {
                        Ok(curve) => {
                            this.thermal.curve = curve;
                            this.thermal.curve_seeded = true;
                            this.thermal.curve_origin = Some(CurveOrigin::Draft);
                            this.thermal.curve_note = None;
                            apply_app.dispatch(
                                KnobId::FanMode,
                                ControlCommand::SetFanMode(FanMode::Curve(curve)),
                            );
                        }
                        Err(message) => {
                            this.thermal.curve_note = Some((message, false));
                        }
                    }
                    cx.notify();
                },
            ));

        let mut presets = div().h_flex().gap_1();
        for (label, curve, tag) in [
            ("安静", FanCurve::quiet(), 0usize),
            ("均衡", FanCurve::balanced(), 1),
            ("性能", FanCurve::performance(), 2),
        ] {
            let app2 = app.clone();
            presets = presets.child(
                Button::new(("fan-curve-preset", tag))
                    .label(label)
                    .outline()
                    .disabled(reason.is_some())
                    .on_click(cx.listener(
                        move |this: &mut ShellView,
                              _: &gpui::ClickEvent,
                              window: &mut Window,
                              cx| {
                            this.set_curve_form(curve, window, cx);
                            this.thermal.curve_seeded = true;
                            this.thermal.curve_origin = Some(CurveOrigin::Draft);
                            this.thermal.curve_note = None;
                            app2.dispatch(
                                KnobId::FanMode,
                                ControlCommand::SetFanMode(FanMode::Curve(curve)),
                            );
                            cx.notify();
                        },
                    )),
            );
        }

        let mut rows = div().v_flex().gap_1().w_full();
        for row in 0..FAN_CURVE_POINT_COUNT {
            let base = row * 3;
            let field = |input: &Entity<InputState>| div().w(px(72.)).child(Input::new(input));
            rows = rows.child(
                div()
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(24.))
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("{}", row + 1)),
                    )
                    .child(field(&thermal.curve_inputs[base]))
                    .child(field(&thermal.curve_inputs[base + 1]))
                    .child(field(&thermal.curve_inputs[base + 2])),
            );
        }

        let labels = div()
            .h_flex()
            .gap_2()
            .items_center()
            .child(div().w(px(24.)))
            .child(
                div()
                    .w(px(72.))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("温度 °C"),
            )
            .child(
                div()
                    .w(px(72.))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("CPU RPM"),
            )
            .child(
                div()
                    .w(px(72.))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("GPU RPM"),
            );

        Some(
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
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .h_flex()
                                .items_center()
                                .gap_2()
                                .child(div().text_base().font_semibold().child("风扇曲线"))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(curve_source),
                                ),
                        )
                        .child(presets),
                )
                .child(labels)
                .child(rows)
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("温度取 CPU / GPU 较高值"),
                        )
                        .child(apply_button),
                )
                .when_some(thermal.curve_note.as_ref(), |d, (message, _)| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(theme.danger)
                            .child(message.clone()),
                    )
                }),
        )
    } else {
        None
    };

    div()
        .v_flex()
        .gap_3()
        .w_full()
        .child(mode_card)
        .when(manual_active, |d| d.child(manual_card))
        .when_some(curve_card, |d, card| d.child(card))
}
