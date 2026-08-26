//! Performance (plan D-G): all stable CPU knobs — EPP/EPP1 sliders (AC/DC
//! split), max-freq sliders (0 = unlimited, "期望值" — no readback channel
//! exists), boost policy button group (also 期望值), thermal mode segmented
//! buttons (has readback). Slider drags dispatch through the coalescer
//! (§44): latest-wins, ≤1 in-flight per knob, 250 ms min interval.

use gpui::{Context, Entity, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px};
use gpui_component::{ActiveTheme, Disableable, StyledExt, button::{Button, ButtonVariants}, input::Input, slider::{Slider, SliderValue}};
use gpui_component::input::InputState;
use phelper_core::app::fmt;
use phelper_core::app::runtime::AppHandle;
use phelper_core::app::state::{KnobId, knob_enabled};
use phelper_core::app::{AppState, validate};
use phelper_domain::command::ControlCommand;
use phelper_domain::policy::{BoostPolicy, CpuPolicy, GpuPlatformPolicy, ThermalMode};
use phelper_domain::profile::GpuPolicyPatch;
use phelper_domain::state::ObservedValue;
use phelper_domain::telemetry::ids;

use crate::shell::{ExpState, PerfState, ShellView};
use crate::widgets::knob_row;

use super::dashboard::page_root;

// ---- slider → command maps (used by the shell's subscriptions) ----

pub fn epp_ac_cmd(f: f32) -> ControlCommand {
    ControlCommand::SetCpuPolicy(CpuPolicy { epp_ac: Some(f.round() as u8), ..Default::default() })
}
pub fn epp_dc_cmd(f: f32) -> ControlCommand {
    ControlCommand::SetCpuPolicy(CpuPolicy { epp_dc: Some(f.round() as u8), ..Default::default() })
}
pub fn epp1_ac_cmd(f: f32) -> ControlCommand {
    ControlCommand::SetCpuPolicy(CpuPolicy { epp1_ac: Some(f.round() as u8), ..Default::default() })
}
pub fn epp1_dc_cmd(f: f32) -> ControlCommand {
    ControlCommand::SetCpuPolicy(CpuPolicy { epp1_dc: Some(f.round() as u8), ..Default::default() })
}
/// 0 = unlimited; sub-400 drag positions snap to 0 (envelope is 0|400..=6000).
pub fn freq_ac_cmd(f: f32) -> ControlCommand {
    let mhz = if f < 400. { 0 } else { f.round() as u32 };
    ControlCommand::SetCpuPolicy(CpuPolicy { max_freq_mhz_ac: Some(mhz), ..Default::default() })
}
pub fn freq_dc_cmd(f: f32) -> ControlCommand {
    let mhz = if f < 400. { 0 } else { f.round() as u32 };
    ControlCommand::SetCpuPolicy(CpuPolicy { max_freq_mhz_dc: Some(mhz), ..Default::default() })
}

fn slider_f32(v: SliderValue) -> f32 {
    match v {
        SliderValue::Single(f) => f,
        SliderValue::Range(a, _) => a,
    }
}

fn observed_u8(v: &ObservedValue<u8>) -> String {
    match v.value() {
        Some(x) => format!("当前：{x}（{}）", fmt::observed_provenance_zh(v)),
        None => "当前：未知".to_string(),
    }
}

const BOOSTS: [BoostPolicy; 7] = [
    BoostPolicy::Disabled,
    BoostPolicy::Enabled,
    BoostPolicy::Aggressive,
    BoostPolicy::EfficientEnabled,
    BoostPolicy::EfficientAggressive,
    BoostPolicy::AggressiveGuaranteed,
    BoostPolicy::EfficientAggressiveGuaranteed,
];

pub fn render(
    state: &AppState,
    app: &AppHandle,
    perf: &PerfState,
    exp: &ExpState,
    cx: &mut Context<ShellView>,
) -> impl IntoElement {
    let theme = cx.theme();

    if !state.writes_available() {
        return page_root("perf-scroll").child(
            div().v_flex().p_4().child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("控制不可用（遥测模式）——本页写入控件已隐藏"),
            ),
        );
    }

    let enabled = |knob: KnobId| {
        knob_enabled(state.caps.as_ref(), knob, &state.experimental).err()
    };
    let status_of = |knob: KnobId| {
        state.knobs.get(&knob).cloned().unwrap_or_default()
    };

    // ---- CPU policy card: 4 EPP sliders + 2 freq sliders ----
    let epp_val = |e: &gpui::Entity<gpui_component::slider::SliderState>| {
        slider_f32(e.read(cx).value()).round() as i64
    };
    let cpu_card = {
        let mut rows = div().v_flex().gap_1().w_full();
        let slider_rows: [(&'static str, KnobId, &gpui::Entity<gpui_component::slider::SliderState>, String); 4] = [
            ("EPP（交流）", KnobId::EppAc, &perf.epp_ac, observed_u8(&state.observed.epp_ac)),
            ("EPP（电池）", KnobId::EppDc, &perf.epp_dc, observed_u8(&state.observed.epp_dc)),
            ("E 核 EPP（交流）", KnobId::Epp1Ac, &perf.epp1_ac, observed_u8(&state.observed.epp1_ac)),
            ("E 核 EPP（电池）", KnobId::Epp1Dc, &perf.epp1_dc, observed_u8(&state.observed.epp1_dc)),
        ];
        for (label, knob, entity, obs) in slider_rows {
            let reason = enabled(knob);
            let set_v = epp_val(entity);
            let control = Slider::new(entity).disabled(reason.is_some());
            rows = rows.child(knob_row::knob_row(
                cx,
                label,
                control,
                format!("设：{set_v} · {obs}"),
                &status_of(knob),
                reason,
            ));
        }
        // Max freq: no readback channel — honest 期望值 labeling (AR-10).
        let freq_rows: [(&'static str, KnobId, &gpui::Entity<gpui_component::slider::SliderState>, Option<u32>); 2] = [
            ("频率上限（交流）", KnobId::MaxFreqAc, &perf.freq_ac, state.desired.cpu_policy.as_ref().and_then(|p| p.max_freq_mhz_ac)),
            ("频率上限（电池）", KnobId::MaxFreqDc, &perf.freq_dc, state.desired.cpu_policy.as_ref().and_then(|p| p.max_freq_mhz_dc)),
        ];
        for (label, knob, entity, desired) in freq_rows {
            let reason = enabled(knob);
            let set_v = epp_val(entity);
            let set_label = if set_v < 400 { "不限".to_string() } else { format!("{set_v} MHz") };
            let desired_label = match desired {
                Some(0) | None => "期望：不限".to_string(),
                Some(m) => format!("期望：{m} MHz"),
            };
            let control = Slider::new(entity).disabled(reason.is_some());
            rows = rows.child(knob_row::knob_row(
                cx,
                label,
                control,
                format!("设：{set_label} · {desired_label}（无回读）"),
                &status_of(knob),
                reason,
            ));
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
                div().h_flex().gap_2().child(
                    div().text_base().font_semibold().child("CPU 策略"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("0 = 性能优先，100 = 能效优先；拖动即时生效（合并限速 250 ms）"),
                ),
            )
            .child(rows)
    };

    // ---- boost policy (期望值 — Windows has no boost readback) ----
    let boost_card = {
        let reason = enabled(KnobId::Boost);
        let current = state.desired.cpu_policy.as_ref().and_then(|p| p.boost_policy);
        let mut btns = div().h_flex().gap_1().flex_wrap();
        for b in BOOSTS {
            let active = current == Some(b);
            let app2 = app.clone();
            btns = btns.child(
                Button::new(("boost", b as usize))
                    .label(fmt::boost_zh(b))
                    .when(active, |btn| btn.primary())
                    .when(!active, |btn| btn.outline())
                    .disabled(reason.is_some())
                    .on_click(cx.listener(move |_, _: &gpui::ClickEvent, _: &mut Window, _cx| {
                        app2.dispatch(
                            KnobId::Boost,
                            ControlCommand::SetCpuPolicy(CpuPolicy { boost_policy: Some(b), ..Default::default() }),
                        );
                    })),
            );
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
            .child(div().h_flex().gap_2().child(
                div().text_base().font_semibold().child("睿频策略"),
            ).child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "期望值：{}（Windows 无睿频回读通道）",
                        current.map(fmt::boost_zh).unwrap_or("未设置")
                    )),
            ))
            .child(btns)
            .when_some(reason, |d, r| {
                d.child(div().text_xs().text_color(theme.muted_foreground).child(r))
            })
            .child(
                div().h_flex().justify_end().w_full().child(
                    knob_row::status_badge(cx, &status_of(KnobId::Boost))
                        .unwrap_or_else(div),
                ),
            )
    };

    // ---- thermal mode (has readback) ----
    let thermal_card = {
        let reason = enabled(KnobId::ThermalMode);
        let current = state.observed.thermal_mode.value().copied();
        let prov = fmt::observed_provenance_zh(&state.observed.thermal_mode);
        let mut btns = div().h_flex().gap_1();
        for m in [ThermalMode::Balanced, ThermalMode::Performance] {
            let active = current == Some(m);
            let app2 = app.clone();
            btns = btns.child(
                Button::new(("thermal", m as usize))
                    .label(fmt::thermal_mode_zh(m))
                    .when(active, |btn| btn.primary())
                    .when(!active, |btn| btn.outline())
                    .disabled(reason.is_some())
                    .on_click(cx.listener(move |_, _: &gpui::ClickEvent, _: &mut Window, _cx| {
                        app2.dispatch(KnobId::ThermalMode, ControlCommand::SetThermalMode(m));
                    })),
            );
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
            .child(div().h_flex().gap_2().child(
                div().text_base().font_semibold().child("散热模式"),
            ).child(
                div().text_xs().text_color(theme.muted_foreground).child(match current {
                    // No "未知（未知）" double — provenance only adorns a value.
                    Some(m) => format!("当前：{}（{prov}）", fmt::thermal_mode_zh(m)),
                    None => "当前：未知".to_string(),
                }),
            ))
            .child(btns)
            .when_some(reason, |d, r| {
                d.child(div().text_xs().text_color(theme.muted_foreground).child(r))
            })
            .child(
                div().h_flex().justify_end().w_full().child(
                    knob_row::status_badge(cx, &status_of(KnobId::ThermalMode))
                        .unwrap_or_else(div),
                ),
            )
    };

    // ---- outcome banner ----
    // Drawer BEFORE banner: both take &mut cx; building the banner first
    // would hold the borrow across the drawer call (E0499).
    let drawer = experimental_drawer(state, app, exp, cx);
    let banner = knob_row::outcome_banner(cx, state.evidence.back(), perf.banner_expanded, |this: &mut ShellView| {
        this.perf.banner_expanded = !this.perf.banner_expanded;
    });

    page_root("perf-scroll").child(
        div()
            .v_flex()
            .gap_3()
            .p_4()
            .w_full()
            .child(cpu_card)
            .child(boost_card)
            .child(thermal_card)
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
    cx: &mut Context<ShellView>,
) -> Option<gpui::Div> {
    if !state.experimental.gpu_policy_drawer && !state.experimental.power_limits_drawer {
        return None;
    }
    let theme = cx.theme();

    // ---- 0x22 GPU platform policy (stable) ----
    let gpu_card = if state.experimental.gpu_policy_drawer {
        let reason = knob_enabled(state.caps.as_ref(), KnobId::GpuPolicy, &state.experimental).err();
        let base = state.observed.gpu_platform_policy.value().copied();
        let status = state.knobs.get(&KnobId::GpuPolicy).cloned().unwrap_or_default();
        // Custom toggle, NOT gpui-component's Switch (M6, verified on-device
        // at the pinned rev): a programmatically-checked Switch paints the
        // CHECKED track but leaves the thumb stuck at the unchecked end —
        // its keyed thumb spring never adopts the new target (two id-keying
        // workarounds both failed). Plain divs: the geometry IS the data,
        // re-rendered every 250 ms tick. Geometry mirrors switch.rs:
        // track 36×20, thumb 16, inset 2.
        let mk_toggle = |label: &'static str,
                         field_ctgp: bool,
                         base: Option<GpuPlatformPolicy>,
                         reason: Option<&'static str>| {
            let app2 = app.clone();
            let checked = base.map(|p| if field_ctgp { p.ctgp } else { p.ppab }).unwrap_or(false);
            let disabled = reason.is_some() || base.is_none();
            let track_bg: gpui::Background =
                if checked { theme.primary } else { theme.switch }.into();
            let thumb_bg: gpui::Background = theme.switch_thumb.into();
            let track = div()
                .id(if field_ctgp { "gpu-toggle-ctgp" } else { "gpu-toggle-ppab" })
                .w(px(36.))
                .h(px(20.))
                .rounded(px(20.))
                .flex()
                .items_center()
                .bg(if disabled { track_bg.opacity(0.5) } else { track_bg })
                .when(!disabled, |d| {
                    d.cursor_pointer().on_click(move |_, _, _| {
                        // Patch write: only the toggled field moves. The
                        // coordinator merges over a FRESH 0x21 read taken at
                        // write time — never over this cached base (a stale
                        // merge would clobber the untouched field).
                        let patch = if field_ctgp {
                            GpuPolicyPatch { ctgp: Some(!checked), ..Default::default() }
                        } else {
                            GpuPolicyPatch { ppab: Some(!checked), ..Default::default() }
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
                        .bg(if disabled { thumb_bg.opacity(0.35) } else { thumb_bg }),
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
                "dstate：{}（只读——8BAB 写入无效：rc=0 但 0x21 回读不动，M5 HIL 定论）· slowdown_temp：{} °C（写入时保留）· 回读：{} · {}（30 s 自动重读保持新鲜）",
                p.dstate,
                p.slowdown_temp_c,
                fmt::observed_provenance_zh(&state.observed.gpu_platform_policy),
                fmt::observed_age_zh(&state.observed.gpu_platform_policy)
            ),
            None => "0x21 回读缺失——无法安全合并，开关已禁用".to_string(),
        };
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
                    .child(div().text_base().font_semibold().child("GPU 平台策略（0x22 · 稳定）"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("cTGP / PPAB 即时生效 · 合并基 = 写入瞬间的新鲜 0x21 读取（非缓存）"),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .gap_4()
                    .child(mk_toggle("cTGP", true, base, reason))
                    .child(mk_toggle("PPAB", false, base, reason))
                    .child(
                        div().h_flex().flex_1().justify_end().child(
                            knob_row::status_badge(cx, &status).unwrap_or_else(div),
                        ),
                    ),
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

    // ---- 0x29 CPU power limits (permanent experimental) ----
    let pl_card = if state.experimental.power_limits_drawer {
        let reason = knob_enabled(state.caps.as_ref(), KnobId::PowerLimits, &state.experimental).err();
        let status = state.knobs.get(&KnobId::PowerLimits).cloned().unwrap_or_default();
        let snap = state.telemetry.as_deref();
        let live = |id: phelper_domain::telemetry::MetricId| {
            snap.and_then(|s| s.samples.get(&id))
                .and_then(|s| s.value.as_f64())
                .map(|v| format!("{v:.1} W"))
                .unwrap_or_else(|| "—".into())
        };
        let apply_app = app.clone();
        div()
            .v_flex()
            .gap_2()
            .w_full()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.warning)
            .bg(theme.group_box)
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .child(div().text_base().font_semibold().child("功耗墙（0x29 · 实验）"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.warning)
                            .child("已验证但未定型——永久双门控；关机显式恢复基线"),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().child("PL1"))
                    .child(div().w(px(80.)).child(Input::new(&exp.pl1).disabled(reason.is_some())))
                    .child(div().text_sm().child("PL2"))
                    .child(div().w(px(80.)).child(Input::new(&exp.pl2).disabled(reason.is_some())))
                    .child(div().text_sm().child("PL4"))
                    .child(div().w(px(96.)).child(Input::new(&exp.pl4).disabled(reason.is_some())))
                    .child(
                        Button::new("pl-apply")
                            .label("应用功耗墙")
                            .primary()
                            .disabled(reason.is_some())
                            .on_click(cx.listener(move |this: &mut ShellView, _: &gpui::ClickEvent, _: &mut Window, app_cx| {
                                let read = |e: &Entity<InputState>| e.read(app_cx).text().to_string();
                                let parse = |s: String| s.trim().parse::<i64>().map_err(|_| format!("「{}」不是有效数字", s.trim()));
                                let pl4s = read(&this.exp.pl4);
                                let note = match (|| {
                                    let pl1 = parse(read(&this.exp.pl1))?;
                                    let pl2 = parse(read(&this.exp.pl2))?;
                                    let pl4 = if pl4s.trim().is_empty() { 0 } else { parse(pl4s)? };
                                    validate::power_limits(pl1, pl2, pl4)
                                })() {
                                    Ok(limits) => {
                                        apply_app.dispatch(KnobId::PowerLimits, ControlCommand::SetPowerLimits(limits));
                                        ("已派发功耗墙写入（验证见横幅）".to_string(), true)
                                    }
                                    Err(msg) => (msg, false),
                                };
                                this.exp.note = Some(note);
                                app_cx.notify();
                            })),
                    )
                    .child(
                        div().h_flex().flex_1().justify_end().child(
                            knob_row::status_badge(cx, &status).unwrap_or_else(div),
                        ),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "活回读（250 ms）：PL1 {} · PL2 {} · PL4 {}（0x610 + MCHBAR 0x59B0 双通道）",
                        live(ids::CPU_PL1_W),
                        live(ids::CPU_PL2_W),
                        live(ids::CPU_PL4_W),
                    )),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("包络：PL1 15–130 W · PL2 15–157 W（≥ PL1）· PL4 30–200 W（空 = 不改）· 并发字段永久拒绝"),
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

    Some(
        div()
            .v_flex()
            .gap_2()
            .w_full()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.warning)
                    .child("高级功能区——0x22 稳定 / 0x29 实验（稳定构建中隐藏）"),
            )
            .child(gpu_card)
            .child(pl_card),
    )
}
