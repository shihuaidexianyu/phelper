//! Shell — sidebar + page switching. The `Entity<AppState>` observer chain
//! replaces the v0.2-d 50 ms ticker: pump writes go through
//! `GpuiStatePublisher`, which notifies the entity, which fires the
//! `_app_state_sub` closure below — the closure copies the new state and
//! only notifies the shell if the per-page fingerprint moved.

use std::time::Duration;

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, Window,
    div, px,
};
use gpui_component::{
    ActiveTheme, h_flex,
    input::{InputEvent, InputState},
    sidebar::{Sidebar, SidebarCollapsible, SidebarHeader, SidebarMenu, SidebarMenuItem},
    slider::{SliderEvent, SliderState, SliderValue},
    v_flex,
};
use phelper_core::app::AppState;
use phelper_core::app::runtime::AppHandle;
use phelper_core::app::state::KnobId;
use phelper_domain::command::ControlCommand;
use phelper_domain::os_policy::{
    CpuPlacement, GpuPreference, MemoryPriority, ProcessPriority, QosLevel, ThreadPriority,
};
use phelper_domain::policy::{FAN_CURVE_POINT_COUNT, FanCurve, FanLevels, FanMode};
use phelper_domain::resident::ResidentSettings;
use phelper_domain::state::ObservedValue;
use phelper_domain::telemetry::ids;

use crate::overlay::OverlayController;
use crate::pages::{PageId, applications, dashboard, monitor, performance, profiles, settings};
use crate::resident::ResidentRuntimeHandle;

/// Performance page view-state: local slider intent is seeded from observed
/// values once and is never stomped mid-drag. The PPM bounds stay behind a
/// small advanced disclosure so the normal page remains compact.
pub struct PerfState {
    pub epp_ac: Entity<SliderState>,
    pub epp_dc: Entity<SliderState>,
    pub epp1_ac: Entity<SliderState>,
    pub epp1_dc: Entity<SliderState>,
    pub freq_ac: Entity<SliderState>,
    pub freq_dc: Entity<SliderState>,
    pub min_perf_ac: Entity<SliderState>,
    pub min_perf_dc: Entity<SliderState>,
    pub max_perf_ac: Entity<SliderState>,
    pub max_perf_dc: Entity<SliderState>,
    pub seeded: bool,
    pub bounds_seeded: bool,
    pub banner_expanded: bool,
    pub advanced_expanded: bool,
    pub software_advanced_expanded: bool,
}

/// Fan view-state. Fan sliders are created LAZILY once the fan
/// clamp is probed (SliderState has no runtime min/max setters); the ×100
/// RPM fields mirror the slider values so one slider's Change can dispatch
/// a complete Manual(cpu, gpu) command without cross-entity reads.
pub struct ThermalState {
    pub fan_sliders: Option<(Entity<SliderState>, Entity<SliderState>)>,
    pub cpu_rpm: u16,
    pub gpu_rpm: u16,
    /// Four curve rows × (temperature, CPU level, GPU level). Inputs are
    /// local editing state; nothing is written until the user applies the
    /// complete curve.
    pub curve_inputs: [Entity<InputState>; FAN_CURVE_POINT_COUNT * 3],
    pub curve: FanCurve,
    pub curve_seeded: bool,
    pub curve_origin: Option<CurveOrigin>,
    pub curve_expanded: bool,
    pub curve_note: Option<(String, bool)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurveOrigin {
    /// The coordinator has acknowledged this curve as the current app-side
    /// fan policy.
    Active,
    /// Loaded from core persistence for editing; it is not active until the
    /// user applies it again.
    Saved,
    /// Loaded from a named profile for editing.
    Profile,
    /// Chosen or edited locally and waiting for an explicit apply.
    Draft,
}

/// Profiles page view-state: selected card + outcome banner + transient
/// action note (export/refresh feedback).
pub struct ProfileState {
    pub selected: Option<String>,
    pub banner_expanded: bool,
    pub management_expanded: bool,
    pub note: Option<(String, bool)>,
}

/// Monitor page view-state: the substring filter input (text is read from
/// the entity at render; no mirrored string needed).
pub struct MonitorState {
    pub filter: Entity<InputState>,
}

/// Settings page view-state now lives in `Entity<SettingsPageState>`
/// (see `pages::settings`). The shell keeps a handle to read the current
/// theme for the shell-level fingerprint.

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

/// Application scheduling page state.  The inputs are deliberately local to
/// the page; the pump owns all Windows handles and publishes active policies
/// through AppState.
pub struct OsPolicyState {
    pub pid: Entity<InputState>,
    pub tid: Entity<InputState>,
    pub cpu_sets: Entity<InputState>,
    pub affinity_group: Entity<InputState>,
    pub affinity_mask: Entity<InputState>,
    pub ideal_group: Entity<InputState>,
    pub ideal_number: Entity<InputState>,
    pub placement: CpuPlacement,
    pub qos: QosLevel,
    pub process_priority: ProcessPriority,
    pub thread_priority: ThreadPriority,
    pub memory_priority: MemoryPriority,
    pub gpu_preference: GpuPreference,
    pub placement_touched: bool,
    pub qos_touched: bool,
    pub process_priority_touched: bool,
    pub thread_priority_touched: bool,
    pub memory_priority_touched: bool,
    pub gpu_touched: bool,
    pub advanced: bool,
    pub note: Option<(String, bool)>,
}

pub struct ShellView {
    pub(crate) app: AppHandle,
    /// Live `AppState` (kept in sync by the entity observer below).
    pub(crate) state: AppState,
    pub(crate) page: PageId,
    /// Last painted visual fingerprint + paint time — the observer's
    /// `cx.notify()` is gated on this (5 s forced-refresh backstop).
    pub(crate) last_fp: Option<u64>,
    pub(crate) last_paint: std::time::Instant,
    /// §Phase 2: page state initialization runs ONCE in `new()`. The seed
    /// pass needs `&mut Window` (slider.set_value ignores it but takes the
    /// arg by reference) and observes the just-arrived `AppState`, so it
    /// lives in render with a one-shot guard. After the first call the
    /// in-page `seeded` flags stay set and the call becomes a no-op.
    seeded_once: bool,
    pub perf: PerfState,
    pub thermal: ThermalState,
    pub prof: ProfileState,
    pub mon: MonitorState,
    pub settings_page: Entity<settings::SettingsPageState>,
    pub resident_runtime: ResidentRuntimeHandle,
    pub overlay: OverlayController,
    pub exp: ExpState,
    pub os: OsPolicyState,
    /// §Phase 2: page entities own their chart caches / interactive state.
    /// The shell composes them once in `new()` and renders via `Render`.
    pub dash_page: Entity<dashboard::DashboardPageState>,
    /// Owned by the shell so the `Entity<AppState>` outlives the shell
    /// (the bridge closure on the main thread also keeps a clone; this
    /// field is the shell's "I am still watching" anchor).
    #[allow(dead_code)]
    app_state: Entity<AppState>,
    _appearance_sub: Subscription,
    _app_state_sub: Subscription,
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
    let e = cx.new(|_| {
        SliderState::new()
            .min(min)
            .max(max)
            .step(step)
            .default_value(0.)
    });
    cx.subscribe(&e, move |this: &mut ShellView, _, ev: &SliderEvent, cx| {
        if let SliderEvent::Change(SliderValue::Single(v)) = ev {
            this.app.dispatch(knob, map(*v));
            // Drag feedback paints immediately (v0.2-d: the tick skips
            // unchanged frames, so interactive events notify themselves).
            cx.notify();
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
    let cpu = cx.new(|_| {
        SliderState::new()
            .max(max_rpm)
            .min(min_rpm)
            .step(100.)
            .default_value(min_rpm)
    });
    let gpu = cx.new(|_| {
        SliderState::new()
            .max(max_rpm)
            .min(min_rpm)
            .step(100.)
            .default_value(min_rpm)
    });
    cx.subscribe(&cpu, |this: &mut ShellView, _, ev: &SliderEvent, cx| {
        if let SliderEvent::Change(SliderValue::Single(v)) = ev {
            this.thermal.cpu_rpm = (*v / 100.).round() as u16;
            let levels = FanLevels::new(this.thermal.cpu_rpm, this.thermal.gpu_rpm);
            this.app.dispatch(
                KnobId::FanMode,
                ControlCommand::SetFanMode(FanMode::Manual(levels)),
            );
            cx.notify();
        }
    })
    .detach();
    cx.subscribe(&gpu, |this: &mut ShellView, _, ev: &SliderEvent, cx| {
        if let SliderEvent::Change(SliderValue::Single(v)) = ev {
            this.thermal.gpu_rpm = (*v / 100.).round() as u16;
            let levels = FanLevels::new(this.thermal.cpu_rpm, this.thermal.gpu_rpm);
            this.app.dispatch(
                KnobId::FanMode,
                ControlCommand::SetFanMode(FanMode::Manual(levels)),
            );
            cx.notify();
        }
    })
    .detach();
    (cpu, gpu)
}

fn curve_inputs(
    cx: &mut Context<ShellView>,
    window: &mut Window,
) -> [Entity<InputState>; FAN_CURVE_POINT_COUNT * 3] {
    std::array::from_fn(|index| {
        let placeholder = match index % 3 {
            0 => "°C",
            1 | 2 => "RPM",
            _ => unreachable!(),
        };
        cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
    })
}

impl ShellView {
    pub(crate) fn set_curve_form(
        &mut self,
        curve: FanCurve,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let values = curve
            .points
            .iter()
            .flat_map(|point| {
                [
                    point.temp_c.to_string(),
                    point.cpu_rpm().to_string(),
                    point.gpu_rpm().to_string(),
                ]
            })
            .collect::<Vec<_>>();
        let inputs = {
            self.thermal.curve = curve;
            self.thermal.curve_inputs.clone()
        };
        for (input, value) in inputs.iter().zip(values) {
            input.update(cx, |s, cx| s.set_value(value, window, cx));
        }
    }

    fn create_performance_state(
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (PerfState, ThermalState, ExpState) {
        let perf = PerfState {
            epp_ac: knob_slider(cx, 0., 100., 5., KnobId::EppAc, performance::epp_ac_cmd),
            epp_dc: knob_slider(cx, 0., 100., 5., KnobId::EppDc, performance::epp_dc_cmd),
            epp1_ac: knob_slider(cx, 0., 100., 5., KnobId::Epp1Ac, performance::epp1_ac_cmd),
            epp1_dc: knob_slider(cx, 0., 100., 5., KnobId::Epp1Dc, performance::epp1_dc_cmd),
            freq_ac: knob_slider(
                cx,
                0.,
                6000.,
                100.,
                KnobId::MaxFreqAc,
                performance::freq_ac_cmd,
            ),
            freq_dc: knob_slider(
                cx,
                0.,
                6000.,
                100.,
                KnobId::MaxFreqDc,
                performance::freq_dc_cmd,
            ),
            min_perf_ac: knob_slider(
                cx,
                0.,
                100.,
                5.,
                KnobId::MinPerfAc,
                performance::min_perf_ac_cmd,
            ),
            min_perf_dc: knob_slider(
                cx,
                0.,
                100.,
                5.,
                KnobId::MinPerfDc,
                performance::min_perf_dc_cmd,
            ),
            max_perf_ac: knob_slider(
                cx,
                0.,
                100.,
                5.,
                KnobId::MaxPerfAc,
                performance::max_perf_ac_cmd,
            ),
            max_perf_dc: knob_slider(
                cx,
                0.,
                100.,
                5.,
                KnobId::MaxPerfDc,
                performance::max_perf_dc_cmd,
            ),
            seeded: false,
            bounds_seeded: false,
            banner_expanded: false,
            advanced_expanded: false,
            software_advanced_expanded: false,
        };
        let curve_inputs = curve_inputs(cx, window);
        for input in &curve_inputs {
            cx.subscribe(input, |_this: &mut ShellView, _, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            })
            .detach();
        }
        let thermal = ThermalState {
            fan_sliders: None,
            cpu_rpm: 0,
            gpu_rpm: 0,
            curve_inputs,
            curve: FanCurve::balanced(),
            curve_seeded: false,
            curve_origin: None,
            curve_expanded: false,
            curve_note: None,
        };
        let exp = ExpState {
            pl1: cx.new(|cx| InputState::new(window, cx).placeholder("PL1 W")),
            pl2: cx.new(|cx| InputState::new(window, cx).placeholder("PL2 W")),
            pl4: cx.new(|cx| InputState::new(window, cx).placeholder("PL4 W · 空=不改")),
            seeded: false,
            note: None,
        };
        (perf, thermal, exp)
    }

    fn create_monitor_state(window: &mut Window, cx: &mut Context<Self>) -> MonitorState {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("搜索指标…"));
        cx.subscribe(&filter, |_this: &mut ShellView, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();
        MonitorState { filter }
    }

    fn create_os_policy_state(window: &mut Window, cx: &mut Context<Self>) -> OsPolicyState {
        let inputs = [
            cx.new(|cx| InputState::new(window, cx).placeholder("PID")),
            cx.new(|cx| InputState::new(window, cx).placeholder("TID")),
            cx.new(|cx| InputState::new(window, cx).placeholder("CPU Set ID…")),
            cx.new(|cx| InputState::new(window, cx).placeholder("组")),
            cx.new(|cx| InputState::new(window, cx).placeholder("Affinity mask")),
            cx.new(|cx| InputState::new(window, cx).placeholder("理想组")),
            cx.new(|cx| InputState::new(window, cx).placeholder("理想核")),
        ];
        for input in &inputs {
            cx.subscribe(input, |_this: &mut ShellView, _, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            })
            .detach();
        }
        OsPolicyState {
            pid: inputs[0].clone(),
            tid: inputs[1].clone(),
            cpu_sets: inputs[2].clone(),
            affinity_group: inputs[3].clone(),
            affinity_mask: inputs[4].clone(),
            ideal_group: inputs[5].clone(),
            ideal_number: inputs[6].clone(),
            placement: CpuPlacement::All,
            qos: QosLevel::System,
            process_priority: ProcessPriority::Normal,
            thread_priority: ThreadPriority::Normal,
            memory_priority: MemoryPriority::Normal,
            gpu_preference: GpuPreference::System,
            placement_touched: false,
            qos_touched: false,
            process_priority_touched: false,
            thread_priority_touched: false,
            memory_priority_touched: false,
            gpu_touched: false,
            advanced: false,
            note: None,
        }
    }

    fn seed_performance_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Seed each slider independently from the first observed readback —
        // one missing processor class must not blank the other class.
        if !self.perf.seeded {
            let obs = &self.state.observed;
            let get = |v: &ObservedValue<u8>| v.value().copied();
            for (entity, value) in [
                (&self.perf.epp_ac, get(&obs.epp_ac)),
                (&self.perf.epp_dc, get(&obs.epp_dc)),
                (&self.perf.epp1_ac, get(&obs.epp1_ac)),
                (&self.perf.epp1_dc, get(&obs.epp1_dc)),
            ] {
                if let Some(value) = value {
                    let entity = entity.clone();
                    entity.update(cx, |s, cx| s.set_value(value as f32, window, cx));
                }
            }
            if self.state.windows_ppm.is_some() {
                self.perf.seeded = true;
            }
        }

        // PPM hard bounds are optional on Windows/CPU combinations. Seed
        // each available value independently, then stop retrying once the
        // coordinator has produced one software-policy snapshot.
        if !self.perf.bounds_seeded && self.state.windows_ppm.is_some() {
            let obs = &self.state.observed;
            let get = |v: &ObservedValue<u8>| v.value().copied();
            for (entity, value) in [
                (&self.perf.min_perf_ac, get(&obs.min_performance_ac)),
                (&self.perf.min_perf_dc, get(&obs.min_performance_dc)),
                (&self.perf.max_perf_ac, get(&obs.max_performance_ac)),
                (&self.perf.max_perf_dc, get(&obs.max_performance_dc)),
            ] {
                if let Some(value) = value {
                    let entity = entity.clone();
                    entity.update(cx, |s, cx| s.set_value(value as f32, window, cx));
                }
            }
            self.perf.bounds_seeded = true;
        }

        if !self.thermal.curve_seeded && self.state.caps.is_some() {
            let curve_source = match self.state.observed.fan_mode.value() {
                Some(FanMode::Curve(curve)) => Some((*curve, CurveOrigin::Active)),
                _ => self
                    .state
                    .last_saved_fan_curve
                    .map(|curve| (curve, CurveOrigin::Saved))
                    .or_else(|| {
                        self.state.desired.profile.as_deref().and_then(|name| {
                            self.state
                                .profiles
                                .iter()
                                .find(|profile| profile.name == name)
                                .and_then(|profile| match profile.fan_mode {
                                    Some(FanMode::Curve(curve)) => {
                                        Some((curve, CurveOrigin::Profile))
                                    }
                                    _ => None,
                                })
                        })
                    }),
            };
            if let Some((curve, origin)) = curve_source {
                self.set_curve_form(curve, window, cx);
                self.thermal.curve_seeded = true;
                self.thermal.curve_origin = Some(origin);
            }
        }

        // Create the manual-fan sliders once the clamp is probed, seeded
        // from the observed manual levels (if already in Manual) else from
        // live RPM (clamped; fans may read 0 at idle fan-stop). Programmatic
        // set_value does NOT fire SliderEvent::Change — no dispatch (D6:
        // journal stayed clean through EPP seeding).
        if self.thermal.fan_sliders.is_none() {
            let clamp =
                self.state
                    .caps
                    .as_ref()
                    .and_then(|c| match (c.fan.clamp_min, c.fan.clamp_max) {
                        (Some(lo), Some(hi)) => Some((lo, hi)),
                        _ => None,
                    });
            if let Some((lo, hi)) = clamp {
                let snap = self.state.telemetry.as_deref();
                let live = |id| {
                    snap.and_then(|s| s.samples.get(&id))
                        .and_then(|s| s.value.as_f64())
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
                let (cpu_e, gpu_e) = fan_sliders(cx, (lo * 100) as f32, (hi * 100) as f32);
                cpu_e.update(cx, |s, cx| s.set_value((c0 * 100) as f32, window, cx));
                gpu_e.update(cx, |s, cx| s.set_value((g0 * 100) as f32, window, cx));
                self.thermal.cpu_rpm = c0;
                self.thermal.gpu_rpm = g0;
                self.thermal.fan_sliders = Some((cpu_e, gpu_e));
            }
        }

        // Seed the 0x29 inputs once from the live 0x610 TELEMETRY
        // readback (250 ms). observed.power_limits stays Unknown until a
        // write verifies this session, so it can't seed a fresh session — the
        // telemetry metrics are the same hardware readback the CLI shows.
        // PL4 stays empty = NO_CHANGE (mirrors the CLI's --pl4).
        if !self.exp.seeded {
            let snap = self.state.telemetry.as_deref();
            let val = |id| {
                snap.and_then(|s| s.samples.get(&id))
                    .and_then(|s| s.value.as_f64())
            };
            if let (Some(pl1), Some(pl2)) = (val(ids::CPU_PL1_W), val(ids::CPU_PL2_W)) {
                let pl1_input = self.exp.pl1.clone();
                let pl2_input = self.exp.pl2.clone();
                pl1_input.update(cx, |s, cx| {
                    s.set_value(format!("{}", pl1.round() as i64), window, cx)
                });
                pl2_input.update(cx, |s, cx| {
                    s.set_value(format!("{}", pl2.round() as i64), window, cx)
                });
                self.exp.seeded = true;
            }
        }
    }

    pub fn new(
        app: AppHandle,
        app_state: Entity<AppState>,
        ui_settings: phelper_core::app::settings::UiSettings,
        resident_runtime: ResidentRuntimeHandle,
        overlay: OverlayController,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Observer on the live `Entity<AppState>` — the producer (pump) is
        // the only writer; the observer copies into our snapshot and only
        // notifies the shell when the per-page fingerprint moved (5 s
        // backstop fails open toward painting).
        let app_state_sub = cx.observe(&app_state, |this, app_state, cx| {
            this.state = app_state.read(cx).clone();
            let fp = this.fingerprint(&*cx);
            if Some(fp) != this.last_fp
                || this.last_paint.elapsed() >= Duration::from_secs(5)
            {
                this.last_fp = Some(fp);
                this.last_paint = std::time::Instant::now();
                cx.notify();
            }
        });
        // OS theme flipped while we run: re-resolve "跟随系统" live. The
        // settings page owns the pref now; we read it via entity.
        let settings_entity = cx.new(|cx_settings| {
            settings::SettingsPageState::new(
                app_state.clone(),
                ui_settings.clone(),
                window,
                resident_runtime.clone(),
                overlay.clone(),
                cx_settings,
            )
        });
        let settings_for_appearance = settings_entity.clone();
        let appearance_sub = cx.observe_window_appearance(window, move |_this, _window, cx| {
            let pref = settings_for_appearance.read_with(cx, |s, _| s.theme());
            if pref == phelper_core::app::settings::ThemePref::System {
                settings::apply_pref(pref, cx);
            }
            cx.notify();
        });

        // §Phase 2: page state is initialized ONCE here. No more imperative
        // side effects inside `Render::render`. The seeding logic that
        // previously ran on every render now runs reactively inside the
        // `app_state_sub` observer below — by the time the observer fires,
        // `self.state` is already the new snapshot.
        let (perf, thermal, exp) = Self::create_performance_state(window, cx);
        let mon = Self::create_monitor_state(window, cx);
        let os = Self::create_os_policy_state(window, cx);

        Self {
            app: app.clone(),
            state: AppState::default(),
            page: PageId::Dashboard,
            last_fp: None,
            last_paint: std::time::Instant::now(),
            dash_page: cx.new(|cx_dash| {
                dashboard::DashboardPageState::new(app_state.clone(), app, cx_dash)
            }),
            settings_page: settings_entity,
            perf,
            thermal,
            prof: ProfileState {
                selected: None,
                banner_expanded: false,
                management_expanded: false,
                note: None,
            },
            mon,
            resident_runtime,
            overlay,
            exp,
            os,
            app_state,
            seeded_once: false,
            _appearance_sub: appearance_sub,
            _app_state_sub: app_state_sub,
        }
    }
}

impl Render for ShellView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // §Phase 2: page state is initialized ONCE in `new()` — no
        // imperative side effects during render beyond the very first
        // call (which seeds slider values from observed readback; the
        // `seeded_once` flag makes the call a no-op on every later paint).
        if !self.seeded_once {
            self.seed_performance_state(window, cx);
            self.seeded_once = true;
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
            PageId::Dashboard => {
                dashboard::DashboardPageState::render_into(&self.dash_page, window, cx)
            }
            PageId::Performance => {
                performance::render(&self.state, &self.app, &self.perf, &self.thermal, &self.exp, cx)
                    .into_any_element()
            }
            PageId::Profiles => {
                profiles::render(&self.state, &self.app, &self.prof, cx).into_any_element()
            }
            PageId::Monitor => monitor::render(&self.state, &self.mon, cx).into_any_element(),
            PageId::Settings => {
                settings::SettingsPageState::render_into(&self.settings_page, window, cx)
            }
            PageId::Applications => {
                applications::render(&self.state, &self.app, &self.os, cx).into_any_element()
            }
        };

        let theme = cx.theme();

        h_flex()
            .size_full()
            .bg(theme.background)
            .child(
                Sidebar::new("phelper-nav")
                    .collapsible(SidebarCollapsible::None)
                    .w(px(132.))
                    .header(
                        SidebarHeader::new().child(
                            v_flex().child(div().text_lg().child("phelper")).child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("OMEN 16"),
                            ),
                        ),
                    )
                    .child(menu),
            )
            .child(v_flex().h_full().flex_1().min_w_0().child(content))
    }
}
