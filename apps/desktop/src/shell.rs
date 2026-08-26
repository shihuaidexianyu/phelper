//! Shell — sidebar + page switching + the 250 ms state ticker (plan D-E).
//! The ticker is the ONLY AppState pull; pages render from the snapshot.

use std::collections::BTreeSet;
use std::time::Duration;

use gpui::{AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Task, Window, div, px};
use gpui_component::{ActiveTheme, h_flex, v_flex, input::{InputEvent, InputState}, sidebar::{Sidebar, SidebarCollapsible, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem}, slider::{SliderEvent, SliderState, SliderValue}};
use phelper_core::app::runtime::AppHandle;
use phelper_core::app::state::KnobId;
use phelper_core::app::{AppState, EngineStatus};
use phelper_domain::command::ControlCommand;
use phelper_domain::policy::{FanLevels, FanMode};
use phelper_domain::state::ObservedValue;
use phelper_domain::telemetry::ids;

use crate::pages::{PageId, dashboard, diagnostics, monitor, performance, profiles, settings, thermals};

/// Diagnostics page view-state (§42: page-local interactive state lives
/// in the shell, never in the pure page render).
pub struct DiagState {
    /// Expanded journal rows (journal_view::key_of keys).
    pub expanded: BTreeSet<String>,
    /// (message, ok) after an export attempt; None until first click.
    pub export_note: Option<(String, bool)>,
}

/// Performance page view-state: the six slider entities (local intent —
/// seeded once from observed values; never stomped mid-drag), plus the
/// outcome banner expansion toggle.
pub struct PerfState {
    pub epp_ac: Entity<SliderState>,
    pub epp_dc: Entity<SliderState>,
    pub epp1_ac: Entity<SliderState>,
    pub epp1_dc: Entity<SliderState>,
    pub freq_ac: Entity<SliderState>,
    pub freq_dc: Entity<SliderState>,
    pub seeded: bool,
    pub banner_expanded: bool,
}

/// Thermals page view-state. Fan sliders are created LAZILY once the fan
/// clamp is probed (SliderState has no runtime min/max setters); the ×100
/// RPM fields mirror the slider values so one slider's Change can dispatch
/// a complete Manual(cpu, gpu) command without cross-entity reads.
pub struct ThermalState {
    pub fan_sliders: Option<(Entity<SliderState>, Entity<SliderState>)>,
    pub cpu_rpm: u16,
    pub gpu_rpm: u16,
    pub banner_expanded: bool,
}

/// Profiles page view-state: selected card + outcome banner + transient
/// action note (export/refresh feedback).
pub struct ProfileState {
    pub selected: Option<String>,
    pub banner_expanded: bool,
    pub note: Option<(String, bool)>,
}

/// Monitor page view-state: the substring filter input (text is read from
/// the entity at render; no mirrored string needed).
pub struct MonitorState {
    pub filter: Entity<InputState>,
}

/// Settings page view-state: loaded theme pref + transient save note.
pub struct SettingsState {
    pub theme: phelper_core::app::settings::ThemePref,
    pub note: Option<(String, bool)>,
}

/// Experimental drawer view-state (Performance page bottom): 0x29 inputs
/// (PL1/PL2 seeded from the observed readback; PL4 left empty = NO_CHANGE,
/// mirroring the CLI's optional --pl4) + client-side validation note.
pub struct ExpState {
    pub pl1: Entity<InputState>,
    pub pl2: Entity<InputState>,
    pub pl4: Entity<InputState>,
    pub seeded: bool,
    pub note: Option<(String, bool)>,
}

pub struct ShellView {
    app: AppHandle,
    state: AppState,
    page: PageId,
    pub diag: DiagState,
    pub perf: PerfState,
    pub thermal: ThermalState,
    pub prof: ProfileState,
    pub mon: MonitorState,
    pub settings: SettingsState,
    pub exp: ExpState,
    _appearance_sub: gpui::Subscription,
    _tick: Task<()>,
}

/// One slider entity wired to dispatch `map(value)` on every Change — the
/// app-layer coalescer (§44) collapses drags to ≤1 in-flight per knob.
fn knob_slider(
    cx: &mut Context<ShellView>,
    min: f32,
    max: f32,
    step: f32,
    knob: KnobId,
    map: fn(f32) -> ControlCommand,
) -> Entity<SliderState> {
    let e = cx.new(|_| SliderState::new().min(min).max(max).step(step).default_value(0.));
    cx.subscribe(&e, move |this: &mut ShellView, _, ev: &SliderEvent, _| {
        if let SliderEvent::Change(SliderValue::Single(v)) = ev {
            this.app.dispatch(knob, map(*v));
        }
    })
    .detach();
    e
}

/// Manual-fan slider pair (CPU/GPU RPM) with the probed clamp as the range.
/// A Change on either dispatches a complete `Manual(cpu, gpu)` — the command
/// always carries both levels (the wire protocol has no per-fan NO_CHANGE).
fn fan_sliders(
    cx: &mut Context<ShellView>,
    min_rpm: f32,
    max_rpm: f32,
) -> (Entity<SliderState>, Entity<SliderState>) {
    // Builder order matters: SliderState's default max is 100 and EVERY
    // builder call runs update_thumb_pos (value.clamp(min, max) — panics
    // when min > max). `.min(2000)` before `.max(6300)` panics on the
    // intermediate (2000, 100) state (on-device crash D7). max FIRST.
    let cpu = cx.new(|_| SliderState::new().max(max_rpm).min(min_rpm).step(100.).default_value(min_rpm));
    let gpu = cx.new(|_| SliderState::new().max(max_rpm).min(min_rpm).step(100.).default_value(min_rpm));
    cx.subscribe(&cpu, |this: &mut ShellView, _, ev: &SliderEvent, _| {
        if let SliderEvent::Change(SliderValue::Single(v)) = ev {
            this.thermal.cpu_rpm = (*v / 100.).round() as u16;
            let levels = FanLevels::new(this.thermal.cpu_rpm, this.thermal.gpu_rpm);
            this.app
                .dispatch(KnobId::FanMode, ControlCommand::SetFanMode(FanMode::Manual(levels)));
        }
    })
    .detach();
    cx.subscribe(&gpu, |this: &mut ShellView, _, ev: &SliderEvent, _| {
        if let SliderEvent::Change(SliderValue::Single(v)) = ev {
            this.thermal.gpu_rpm = (*v / 100.).round() as u16;
            let levels = FanLevels::new(this.thermal.cpu_rpm, this.thermal.gpu_rpm);
            this.app
                .dispatch(KnobId::FanMode, ControlCommand::SetFanMode(FanMode::Manual(levels)));
        }
    })
    .detach();
    (cpu, gpu)
}

impl ShellView {
    pub fn new(app: AppHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let r = this.update(cx, |this, cx| {
                    this.state = this.app.state();
                    cx.notify();
                });
                if r.is_err() {
                    break;
                }
            }
        });
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("筛选指标 ID（子串）…"));
        cx.subscribe(&filter, |_this: &mut ShellView, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();
        // OS theme flipped while we run: re-resolve "跟随系统" live.
        let appearance_sub = cx.observe_window_appearance(window, |this, _window, cx| {
            if this.settings.theme == phelper_core::app::settings::ThemePref::System {
                settings::apply_pref(this.settings.theme, cx);
            }
        });
        Self {
            app,
            state: AppState::default(),
            page: PageId::Dashboard,
            diag: DiagState {
                expanded: BTreeSet::new(),
                export_note: None,
            },
            perf: PerfState {
                epp_ac: knob_slider(cx, 0., 100., 5., KnobId::EppAc, performance::epp_ac_cmd),
                epp_dc: knob_slider(cx, 0., 100., 5., KnobId::EppDc, performance::epp_dc_cmd),
                epp1_ac: knob_slider(cx, 0., 100., 5., KnobId::Epp1Ac, performance::epp1_ac_cmd),
                epp1_dc: knob_slider(cx, 0., 100., 5., KnobId::Epp1Dc, performance::epp1_dc_cmd),
                freq_ac: knob_slider(cx, 0., 6000., 100., KnobId::MaxFreqAc, performance::freq_ac_cmd),
                freq_dc: knob_slider(cx, 0., 6000., 100., KnobId::MaxFreqDc, performance::freq_dc_cmd),
                seeded: false,
                banner_expanded: false,
            },
            thermal: ThermalState {
                fan_sliders: None,
                cpu_rpm: 0,
                gpu_rpm: 0,
                banner_expanded: false,
            },
            prof: ProfileState {
                selected: None,
                banner_expanded: false,
                note: None,
            },
            mon: MonitorState { filter },
            settings: SettingsState {
                theme: phelper_core::app::settings::UiSettings::load().0.theme,
                note: None,
            },
            exp: ExpState {
                pl1: cx.new(|cx| InputState::new(window, cx).placeholder("PL1 W")),
                pl2: cx.new(|cx| InputState::new(window, cx).placeholder("PL2 W")),
                pl4: cx.new(|cx| InputState::new(window, cx).placeholder("PL4 W · 空=不改")),
                seeded: false,
                note: None,
            },
            _appearance_sub: appearance_sub,
            _tick,
        }
    }
}

impl Render for ShellView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Seed the EPP sliders once from the first observed readback —
        // sliders are local intent afterwards, never stomped mid-drag.
        if !self.perf.seeded {
            let obs = &self.state.observed;
            let get = |v: &ObservedValue<u8>| v.value().copied();
            if let (Some(ac), Some(dc), Some(a1), Some(d1)) = (
                get(&obs.epp_ac),
                get(&obs.epp_dc),
                get(&obs.epp1_ac),
                get(&obs.epp1_dc),
            ) {
                for (entity, v) in [
                    (&self.perf.epp_ac, ac),
                    (&self.perf.epp_dc, dc),
                    (&self.perf.epp1_ac, a1),
                    (&self.perf.epp1_dc, d1),
                ] {
                    let entity = entity.clone();
                    entity.update(cx, |s, cx| s.set_value(v as f32, window, cx));
                }
                self.perf.seeded = true;
            }
        }

        // Create the manual-fan sliders once the clamp is probed, seeded
        // from the observed manual levels (if already in Manual) else from
        // live RPM (clamped; fans may read 0 at idle fan-stop). Programmatic
        // set_value does NOT fire SliderEvent::Change — no dispatch (D6:
        // journal stayed clean through EPP seeding).
        if self.thermal.fan_sliders.is_none() {
            let clamp = self.state.caps.as_ref().and_then(|c| {
                match (c.fan.clamp_min, c.fan.clamp_max) {
                    (Some(lo), Some(hi)) => Some((lo, hi)),
                    _ => None,
                }
            });
            if let Some((lo, hi)) = clamp {
                let (cpu_e, gpu_e) = fan_sliders(cx, (lo * 100) as f32, (hi * 100) as f32);
                let snap = self.state.telemetry.as_deref();
                let live = |id| {
                    snap.and_then(|s| s.samples.get(&id)).and_then(|s| s.value.as_f64())
                };
                let from_live = |v: Option<f64>| match v {
                    Some(rpm) => ((rpm / 100.).round() as u16).clamp(lo, hi),
                    None => lo,
                };
                let (c0, g0) = match self.state.observed.fan_mode.value() {
                    Some(FanMode::Manual(l)) => (l.cpu.clamp(lo, hi), l.gpu.clamp(lo, hi)),
                    _ => (
                        from_live(live(ids::FAN_CPU_RPM)),
                        from_live(live(ids::FAN_GPU_RPM)),
                    ),
                };
                self.thermal.cpu_rpm = c0;
                self.thermal.gpu_rpm = g0;
                cpu_e.update(cx, |s, cx| s.set_value((c0 * 100) as f32, window, cx));
                gpu_e.update(cx, |s, cx| s.set_value((g0 * 100) as f32, window, cx));
                self.thermal.fan_sliders = Some((cpu_e, gpu_e));
            }
        }

        // Seed the 0x29 inputs once from the live 0x610 TELEMETRY
        // readback (250 ms). observed.power_limits stays Unknown until a
        // write verifies this session, so it can't seed a fresh session —
        // the telemetry metrics are the same hardware readback the CLI
        // shows. PL4 stays empty = NO_CHANGE (mirrors the CLI's --pl4).
        if !self.exp.seeded {
            let snap = self.state.telemetry.as_deref();
            let val = |id| {
                snap.and_then(|s| s.samples.get(&id)).and_then(|s| s.value.as_f64())
            };
            if let (Some(pl1), Some(pl2)) = (val(ids::CPU_PL1_W), val(ids::CPU_PL2_W)) {
                self.exp
                    .pl1
                    .update(cx, |s, cx| s.set_value(format!("{}", pl1.round() as i64), window, cx));
                self.exp
                    .pl2
                    .update(cx, |s, cx| s.set_value(format!("{}", pl2.round() as i64), window, cx));
                self.exp.seeded = true;
            }
        }

        // NOTE: build everything that needs `&mut cx` (listeners, page
        // renders) BEFORE taking `cx.theme()` — a live theme borrow blocks
        // passing cx as a function argument (E0502; method receivers get
        // two-phase borrows, arguments don't).
        let menu = SidebarMenu::new().children(PageId::ALL.map(|p| {
            SidebarMenuItem::new(p.label())
                .icon(p.icon())
                .active(self.page == p)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.page = p;
                    cx.notify();
                }))
        }));

        let content: gpui::AnyElement = match self.page {
            PageId::Dashboard => dashboard::render(&self.state, &self.app, cx).into_any_element(),
            PageId::Performance => {
                performance::render(&self.state, &self.app, &self.perf, &self.exp, cx).into_any_element()
            }
            PageId::Thermals => {
                thermals::render(&self.state, &self.app, &self.thermal, cx).into_any_element()
            }
            PageId::Profiles => {
                profiles::render(&self.state, &self.app, &self.prof, cx).into_any_element()
            }
            PageId::Monitor => {
                monitor::render(&self.state, &self.mon, cx).into_any_element()
            }
            PageId::Settings => {
                settings::render(&self.settings, cx).into_any_element()
            }
            PageId::Diagnostics => {
                diagnostics::render(&self.state, &self.app, &self.diag, cx).into_any_element()
            }
        };

        let theme = cx.theme();

        h_flex()
            .size_full()
            .bg(theme.background)
            .child(
                Sidebar::new("phelper-nav")
                    .collapsible(SidebarCollapsible::None)
                    .w(px(200.))
                    .header(SidebarHeader::new().child(
                        v_flex()
                            .child(div().text_lg().child("phelper"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("OMEN 16-wf0032TX"),
                            ),
                    ))
                    .child(SidebarGroup::new("页面").child(menu))
                    .footer(
                        div()
                            .p_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(match &self.state.engine {
                                EngineStatus::Starting => "引擎启动中…",
                                EngineStatus::Running => "引擎运行中",
                                EngineStatus::TelemetryOnly => "遥测模式",
                                EngineStatus::Failed(_) => "引擎故障",
                            }),
                    ),
            )
            .child(
                v_flex()
                    .h_full()
                    .flex_1()
                    .min_w_0()
                    .child(content),
            )
    }
}
