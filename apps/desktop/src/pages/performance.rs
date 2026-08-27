//! Performance: the single tuning surface for CPU policy and fan behavior.
//! Overall presets live on the Profiles page; this page exposes deliberate
//! manual tuning plus the fan controls that need their own safety gates.

use gpui::{
    Context, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::input::InputState;
use gpui_component::{
    ActiveTheme, Disableable, StyledExt,
    button::{Button, ButtonVariants},
    input::Input,
    slider::{Slider, SliderValue},
};
use phelper_core::app::runtime::AppHandle;
use phelper_core::app::state::{KnobId, knob_enabled};
use phelper_core::app::{AppState, validate};
use phelper_domain::command::ControlCommand;
use phelper_domain::policy::{CpuPolicy, GpuPlatformPolicy};
use phelper_domain::profile::GpuPolicyPatch;
use phelper_domain::state::ObservedValue;
use phelper_domain::telemetry::ids;

use crate::shell::{ExpState, PerfState, ShellView, ThermalState};
use crate::widgets::knob_row;

use super::dashboard::page_root;
use super::thermals;

// ---- slider → command maps (used by the shell's subscriptions) ----

pub fn epp_ac_cmd(f: f32) -> ControlCommand {
    ControlCommand::SetCpuPolicy(CpuPolicy {
        epp_ac: Some(f.round() as u8),
        ..Default::default()
    })
}
pub fn epp_dc_cmd(f: f32) -> ControlCommand {
    ControlCommand::SetCpuPolicy(CpuPolicy {
        epp_dc: Some(f.round() as u8),
        ..Default::default()
    })
}
pub fn epp1_ac_cmd(f: f32) -> ControlCommand {
    ControlCommand::SetCpuPolicy(CpuPolicy {
        epp1_ac: Some(f.round() as u8),
        ..Default::default()
    })
}
pub fn epp1_dc_cmd(f: f32) -> ControlCommand {
    ControlCommand::SetCpuPolicy(CpuPolicy {
        epp1_dc: Some(f.round() as u8),
        ..Default::default()
    })
}
/// 0 = unlimited; sub-400 drag positions snap to 0 (envelope is 0|400..=6000).
pub fn freq_ac_cmd(f: f32) -> ControlCommand {
    let mhz = if f < 400. { 0 } else { f.round() as u32 };
    ControlCommand::SetCpuPolicy(CpuPolicy {
        max_freq_mhz_ac: Some(mhz),
        ..Default::default()
    })
}
pub fn freq_dc_cmd(f: f32) -> ControlCommand {
    let mhz = if f < 400. { 0 } else { f.round() as u32 };
    ControlCommand::SetCpuPolicy(CpuPolicy {
        max_freq_mhz_dc: Some(mhz),
        ..Default::default()
    })
}

fn slider_f32(v: SliderValue) -> f32 {
    match v {
        SliderValue::Single(f) => f,
        SliderValue::Range(a, _) => a,
    }
}

fn observed_u8(v: &ObservedValue<u8>) -> String {
    match v.value() {
        Some(x) => format!("当前 {x}"),
        None => "当前 —".to_string(),
    }
}

pub fn render(
    state: &AppState,
    app: &AppHandle,
    perf: &PerfState,
    thermal: &ThermalState,
    exp: &ExpState,
    cx: &mut Context<ShellView>,
) -> impl IntoElement {
    if !state.writes_available() {
        let theme = cx.theme();
        return page_root("perf-scroll").child(
            div().v_flex().p_4().child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(super::control_unavailable_label(state)),
            ),
        );
    }

    // Build the fan controls before taking the theme borrow below; the
    // helper also needs mutable access to the GPUI context for listeners.
    let thermal_content = thermals::render_content(state, app, thermal, cx);
    let theme = cx.theme();

    let enabled = |knob: KnobId| knob_enabled(state.caps.as_ref(), knob, &state.experimental).err();
    let status_of = |knob: KnobId| state.knobs.get(&knob).cloned().unwrap_or_default();

    // ---- compact performance cockpit -----------------------------------
    let epp_val = |e: &gpui::Entity<gpui_component::slider::SliderState>| {
        slider_f32(e.read(cx).value()).round() as i64
    };
    let compact_slider = |label: &'static str,
                          knob: KnobId,
                          entity: &gpui::Entity<gpui_component::slider::SliderState>,
                          value: String| {
        let reason = enabled(knob);
        knob_row::compact_knob_row(
            cx,
            label,
            Slider::new(entity).disabled(reason.is_some()),
            value,
            &status_of(knob),
            reason,
        )
    };

    let cpu_card = {
        let ac_column = div()
            .v_flex()
            .gap_1()
            .flex_1()
            .p_2()
            .rounded_md()
            .bg(theme.background)
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("交流电源"),
            )
            .child(compact_slider(
                "P 核 EPP",
                KnobId::EppAc,
                &perf.epp_ac,
                format!(
                    "设 {} · {}",
                    epp_val(&perf.epp_ac),
                    observed_u8(&state.observed.epp_ac)
                ),
            ))
            .child(compact_slider(
                "E 核 EPP",
                KnobId::Epp1Ac,
                &perf.epp1_ac,
                format!(
                    "设 {} · {}",
                    epp_val(&perf.epp1_ac),
                    observed_u8(&state.observed.epp1_ac)
                ),
            ));
        let dc_column = div()
            .v_flex()
            .gap_1()
            .flex_1()
            .p_2()
            .rounded_md()
            .bg(theme.background)
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("电池电源"),
            )
            .child(compact_slider(
                "P 核 EPP",
                KnobId::EppDc,
                &perf.epp_dc,
                format!(
                    "设 {} · {}",
                    epp_val(&perf.epp_dc),
                    observed_u8(&state.observed.epp_dc)
                ),
            ))
            .child(compact_slider(
                "E 核 EPP",
                KnobId::Epp1Dc,
                &perf.epp1_dc,
                format!(
                    "设 {} · {}",
                    epp_val(&perf.epp1_dc),
                    observed_u8(&state.observed.epp1_dc)
                ),
            ));

        // Max frequency has no readback channel; keep the distinction in a
        // small value line rather than giving it a whole separate card.
        let max_freq =
            |label: &'static str,
             knob: KnobId,
             entity: &gpui::Entity<gpui_component::slider::SliderState>| {
                let set_v = epp_val(entity);
                let set_label = if set_v < 400 {
                    "不限".to_string()
                } else {
                    format!("{set_v} MHz")
                };
                compact_slider(label, knob, entity, format!("目标 {set_label}"))
            };
        let ac_column = ac_column.child(max_freq("频率上限", KnobId::MaxFreqAc, &perf.freq_ac));
        let dc_column = dc_column.child(max_freq("频率上限", KnobId::MaxFreqDc, &perf.freq_dc));

        div()
            .v_flex()
            .gap_3()
            .w_full()
            .min_w_0()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.group_box)
            .child(
                div().h_flex().items_center().justify_between().child(
                    div()
                        .v_flex()
                        .gap_px()
                        .child(div().text_base().font_semibold().child("CPU"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("EPP 0 性能 · 100 省电"),
                        ),
                ),
            )
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .w_full()
                    .child(ac_column)
                    .child(dc_column),
            )
    };

    let page_header = {
        let profile = state.desired.profile.as_deref().unwrap_or("自定义");
        div()
            .h_flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .v_flex()
                    .gap_px()
                    .child(div().text_xl().font_semibold().child("性能")),
            )
            .child(
                div().h_flex().gap_2().child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded_full()
                        .border_1()
                        .border_color(theme.border)
                        .text_xs()
                        .child(profile.to_string()),
                ),
            )
    };

    // Drawer BEFORE banner: both take &mut cx; building the banner first
    // would hold the borrow across the drawer call (E0499).
    let drawer = experimental_drawer(state, app, exp, perf.advanced_expanded, cx);
    let banner = knob_row::outcome_banner(
        cx,
        state.evidence.back(),
        perf.banner_expanded,
        |this: &mut ShellView| {
            let perf = this
                .perf
                .as_mut()
                .expect("performance controls initialized");
            perf.banner_expanded = !perf.banner_expanded;
        },
    );

    page_root("perf-scroll").child(
        div()
            .v_flex()
            .gap_3()
            .p_4()
            .w_full()
            .child(page_header)
            .child(cpu_card)
            .child(thermal_content)
            .when_some(drawer, |d, x| d.child(x))
            .when_some(banner, |d, b| d.child(b)),
    )
}

/// Experimental drawer (plan D-G): 0x22 cTGP/PPAB switches (stable M3 —
/// merged over the live 0x21 readback; dstate read-only with the M5
/// "ineffective on 8BAB" finding) + 0x29 PL1/PL2/PL4 (permanent double
/// gate; envelope mirrored client-side via validate::power_limits before
/// any dispatch). Whole drawer hidden when neither gate passes (stable
/// build → 0x29 section gone; 0x22 stays wherever usable).
fn experimental_drawer(
    state: &AppState,
    app: &AppHandle,
    exp: &ExpState,
    expanded: bool,
    cx: &mut Context<ShellView>,
) -> Option<gpui::Div> {
    if !state.experimental.gpu_policy_drawer && !state.experimental.power_limits_drawer {
        return None;
    }
    let theme = cx.theme();
    let toggle = Button::new("advanced-toggle")
        .label(if expanded { "收起" } else { "展开" })
        .outline()
        .on_click(cx.listener(
            |this: &mut ShellView, _: &gpui::ClickEvent, _: &mut Window, cx| {
                let perf = this
                    .perf
                    .as_mut()
                    .expect("performance controls initialized");
                perf.advanced_expanded = !perf.advanced_expanded;
                cx.notify();
            },
        ));

    // ---- 0x22 GPU platform policy (stable) ----
    let gpu_card = if state.experimental.gpu_policy_drawer {
        let reason =
            knob_enabled(state.caps.as_ref(), KnobId::GpuPolicy, &state.experimental).err();
        let base = state.observed.gpu_platform_policy.value().copied();
        // Custom toggle, NOT gpui-component's Switch (M6, verified on-device
        // at the pinned rev): a programmatically-checked Switch paints the
        // CHECKED track but leaves the thumb stuck at the unchecked end —
        // its keyed thumb spring never adopts the new target (two id-keying
        // workarounds both failed). Plain divs: the geometry IS the data,
        // re-rendered every 50 ms tick. Geometry mirrors switch.rs:
        // track 36×20, thumb 16, inset 2.
        let mk_toggle = |label: &'static str,
                         field_ctgp: bool,
                         base: Option<GpuPlatformPolicy>,
                         reason: Option<&'static str>| {
            let app2 = app.clone();
            let checked = base
                .map(|p| if field_ctgp { p.ctgp } else { p.ppab })
                .unwrap_or(false);
            let disabled = reason.is_some() || base.is_none();
            let track_bg: gpui::Background =
                if checked { theme.primary } else { theme.switch }.into();
            let thumb_bg: gpui::Background = theme.switch_thumb.into();
            let track = div()
                .id(if field_ctgp {
                    "gpu-toggle-ctgp"
                } else {
                    "gpu-toggle-ppab"
                })
                .w(px(36.))
                .h(px(20.))
                .rounded(px(20.))
                .flex()
                .items_center()
                .bg(if disabled {
                    track_bg.opacity(0.5)
                } else {
                    track_bg
                })
                .when(!disabled, |d| {
                    d.cursor_pointer().on_click(move |_, _, _| {
                        // Patch write: only the toggled field moves. The
                        // coordinator merges over a FRESH 0x21 read taken at
                        // write time — never over this cached base (a stale
                        // merge would clobber the untouched field).
                        let patch = if field_ctgp {
                            GpuPolicyPatch {
                                ctgp: Some(!checked),
                                ..Default::default()
                            }
                        } else {
                            GpuPolicyPatch {
                                ppab: Some(!checked),
                                ..Default::default()
                            }
                        };
                        app2.dispatch(
                            KnobId::GpuPolicy,
                            ControlCommand::SetGpuPlatformPolicyPatch(patch),
                        );
                    })
                })
                .child(
                    div()
                        .size(px(16.))
                        .rounded(px(16.))
                        .ml(if checked { px(18.) } else { px(2.) })
                        .bg(if disabled {
                            thumb_bg.opacity(0.35)
                        } else {
                            thumb_bg
                        }),
                );
            div()
                .h_flex()
                .gap_2()
                .items_center()
                .child(track)
                .child(div().text_sm().child(label))
        };
        let readback_line = match base {
            Some(p) => format!(
                "cTGP {} · PPAB {}",
                if p.ctgp { "开" } else { "关" },
                if p.ppab { "开" } else { "关" },
            ),
            None => "当前不可读取".to_string(),
        };
        div()
            .v_flex()
            .gap_2()
            .flex_1()
            .min_w_0()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.group_box)
            .child(
                div()
                    .v_flex()
                    .gap_px()
                    .child(div().text_sm().font_semibold().child("GPU 平台策略"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("cTGP / PPAB"),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .gap_4()
                    .child(mk_toggle("cTGP", true, base, reason))
                    .child(mk_toggle("PPAB", false, base, reason)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(readback_line),
            )
            .when_some(reason, |d, r| {
                d.child(div().text_xs().text_color(theme.warning).child(r))
            })
    } else {
        div()
    };

    // ---- CPU power limits (permanent experimental) ----
    let pl_card = if state.experimental.power_limits_drawer {
        let reason = knob_enabled(
            state.caps.as_ref(),
            KnobId::PowerLimits,
            &state.experimental,
        )
        .err();
        let snap = state.telemetry.as_deref();
        let live = |id: phelper_domain::telemetry::MetricId| {
            snap.and_then(|s| s.samples.get(&id))
                .and_then(|s| s.value.as_f64())
                .map(|v| format!("{v:.1} W"))
                .unwrap_or_else(|| "—".into())
        };
        let apply_app = app.clone();
        let power_field = |label: &'static str, input: &Entity<InputState>| {
            div()
                .v_flex()
                .gap_px()
                .w(px(72.))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(Input::new(input).disabled(reason.is_some()))
        };
        div()
            .v_flex()
            .gap_2()
            .flex_1()
            .min_w_0()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.warning)
            .bg(theme.group_box)
            .child(
                div()
                    .v_flex()
                    .gap_px()
                    .child(div().text_sm().font_semibold().child("功耗墙"))
                    .child(div().text_xs().text_color(theme.warning).child("实验功能")),
            )
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(power_field("PL1", &exp.pl1))
                    .child(power_field("PL2", &exp.pl2))
                    .child(power_field("PL4", &exp.pl4))
                    .child(
                        Button::new("pl-apply")
                            .label("应用")
                            .primary()
                            .disabled(reason.is_some())
                            .on_click(cx.listener(
                                move |this: &mut ShellView,
                                      _: &gpui::ClickEvent,
                                      _: &mut Window,
                                      app_cx| {
                                    let read =
                                        |e: &Entity<InputState>| e.read(app_cx).text().to_string();
                                    let (pl1s, pl2s, pl4s) = {
                                        let exp = this
                                            .exp
                                            .as_ref()
                                            .expect("experimental controls initialized");
                                        (read(&exp.pl1), read(&exp.pl2), read(&exp.pl4))
                                    };
                                    let parse = |s: String| {
                                        s.trim()
                                            .parse::<i64>()
                                            .map_err(|_| format!("「{}」不是有效数字", s.trim()))
                                    };
                                    let note = match (|| {
                                        let pl1 = parse(pl1s)?;
                                        let pl2 = parse(pl2s)?;
                                        let pl4 = if pl4s.trim().is_empty() {
                                            0
                                        } else {
                                            parse(pl4s)?
                                        };
                                        validate::power_limits(pl1, pl2, pl4)
                                    })() {
                                        Ok(limits) => {
                                            apply_app.dispatch(
                                                KnobId::PowerLimits,
                                                ControlCommand::SetPowerLimits(limits),
                                            );
                                            ("已提交".to_string(), true)
                                        }
                                        Err(msg) => (msg, false),
                                    };
                                    this.exp
                                        .as_mut()
                                        .expect("experimental controls initialized")
                                        .note = Some(note);
                                    app_cx.notify();
                                },
                            )),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "当前 {} · {} · {}",
                        live(ids::CPU_PL1_W),
                        live(ids::CPU_PL2_W),
                        live(ids::CPU_PL4_W),
                    )),
            )
            .when_some(exp.note.as_ref(), |d, (msg, ok)| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(if *ok { theme.success } else { theme.danger })
                        .child(msg.clone()),
                )
            })
            .when_some(reason, |d, r| {
                d.child(div().text_xs().text_color(theme.warning).child(r))
            })
    } else {
        div()
    };

    let cards = div()
        .h_flex()
        .gap_3()
        .w_full()
        .child(gpu_card)
        .child(pl_card);

    Some(
        div()
            .v_flex()
            .gap_2()
            .w_full()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .v_flex()
                            .gap_px()
                            .child(div().text_sm().font_semibold().child("高级")),
                    )
                    .child(toggle),
            )
            .when(expanded, |d| d.child(cards)),
    )
}
