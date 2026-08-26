//! Safety supervisor (§33.1): the write-time validator plus the two-layer
//! runtime safety net. Fail closed everywhere (AR-11) — when support,
//! freshness, or state is uncertain, the answer is no write.
//!
//! Layer 1 — thermal hysteresis: while the USER holds the fans (manual
//! levels or max fan), the firmware curve no longer protects the machine.
//! If cpu.pkg_temp_c reaches FORCE_MAX_FAN_AT_C the supervisor forces max
//! fan and remembers the user's mode; at RELEASE_MAX_FAN_AT_C it hands the
//! saved mode back. FirmwareAuto is exempt: there the firmware is already
//! the safety net (AR-12) and we must not second-guess it.
//!
//! Layer 2 — sensor-freeze watchdog: a blind controller is worse than no
//! controller. If temperature or fan samples stop flowing while the user
//! holds the fans, control returns to firmware automatic.

use std::time::{Duration, Instant};

use phelper_domain::capability::{CapabilitySet, Support};
use phelper_domain::command::ControlCommand;
use phelper_domain::error::ControlError;
use phelper_domain::policy::{CpuPolicy, FanLevels, FanMode};
use phelper_domain::state::ObservedState;

/// CPU package temperature at which user fan control is suspended.
pub const FORCE_MAX_FAN_AT_C: f64 = 90.0;
/// Hysteresis release: hand the saved fan mode back at or below this.
pub const RELEASE_MAX_FAN_AT_C: f64 = 85.0;
/// Sensor freeze watchdog: samples older than this count as stopped.
pub const SENSOR_STALE_AFTER: Duration = Duration::from_secs(90);
/// Pre-write gate: manual fan requires a temperature sample this fresh —
/// the hysteresis net must not fly blind even for a few seconds (R4).
pub const PREWRITE_TEMP_FRESH: Duration = Duration::from_secs(5);

/// What the safety layer needs from telemetry. A trait so tests inject a
/// fake; the coordinator implements it over `TelemetryHandle::snapshot()`.
pub trait ThermalFeed {
    /// Latest cpu.pkg_temp_c sample: (°C, when it was taken).
    fn pkg_temp_c(&self) -> Option<(f64, Instant)>;
    /// Latest 0x2D fan readback: (levels, when it was taken).
    fn fan_levels(&self) -> Option<(FanLevels, Instant)>;
    /// Latest MSR 0x610 readback via the PawnIO telemetry collector:
    /// (pl1_w, pl2_w, when it was taken). The 0x29 verification channel —
    /// the coordinator reads the telemetry store, never the MSR itself.
    fn power_limits_w(&self) -> Option<(f64, f64, Instant)>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyAction {
    /// Temperature hit the hysteresis ceiling — force max fan now.
    ForceMaxFan,
    /// Cooled back below the release point — restore the saved user mode.
    ReleaseTo(FanMode),
    /// Sensors froze while the user held the fans — firmware takes over.
    WatchdogRestoreAuto,
}

#[derive(Debug, Default)]
pub struct SafetySupervisor {
    /// Fan mode to release to after a thermal override. `Some` = override
    /// currently active.
    override_saved: Option<FanMode>,
}

impl SafetySupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn override_active(&self) -> bool {
        self.override_saved.is_some()
    }

    /// Bookkeeping hook for user-dispatched fan mode changes. The user is
    /// always allowed to set FirmwareAuto — that also releases any active
    /// override. A user mode set during an override replaces the saved
    /// mode (their latest intent wins on release).
    pub fn note_user_fan_mode(&mut self, mode: FanMode) {
        match mode {
            FanMode::FirmwareAuto => self.override_saved = None,
            m => {
                if self.override_saved.is_some() {
                    self.override_saved = Some(m);
                }
            }
        }
    }

    /// Write-time validation. Runs BEFORE anything touches hardware; every
    /// rejection maps to a structured ControlError, never a raw code.
    pub fn validate(
        &self,
        cmd: &ControlCommand,
        caps: &CapabilitySet,
        feed: &dyn ThermalFeed,
        observed: &ObservedState,
    ) -> Result<(), ControlError> {
        match cmd {
            ControlCommand::ApplyProfile { .. } => Err(ControlError::Unsupported),
            ControlCommand::SetMuxMode(_) => Err(ControlError::Unsupported),

            ControlCommand::SetPowerLimits(l) => {
                // Two independent gates, both required (§25/§54/§57):
                //   1. the experimental cargo feature must be COMPILED IN
                //      (feature-off builds reject by construction);
                //   2. the board capability must say Experimental (8BAB
                //      BoardProfile does, post-S2-arbitration).
                if !cfg!(feature = "experimental-hp-power-limits") {
                    return Err(ControlError::Unsupported);
                }
                if caps.power_limits != Support::Experimental {
                    return Err(ControlError::Unsupported);
                }
                if l.pl4_w != 0 || l.cpu_gpu_concurrent_w != 0 {
                    return Err(ControlError::UnsafeRequest {
                        reason: "pl4/cpu_gpu_concurrent writes are unverified on 8BAB; \
                                 set 0 (= leave unchanged, wire 0xFF)"
                            .into(),
                    });
                }
                // 13900HX envelope: PL1 15..=130 (default 55), PL2 15..=157
                // (max turbo 157); PL2 >= PL1 (the kernel's own invariant).
                if !(15..=130).contains(&l.pl1_w) {
                    return Err(ControlError::UnsafeRequest {
                        reason: format!("PL1 {}W outside 13900HX envelope 15..=130", l.pl1_w),
                    });
                }
                if !(15..=157).contains(&l.pl2_w) {
                    return Err(ControlError::UnsafeRequest {
                        reason: format!("PL2 {}W outside 13900HX envelope 15..=157", l.pl2_w),
                    });
                }
                if l.pl2_w < l.pl1_w {
                    return Err(ControlError::UnsafeRequest {
                        reason: format!("PL2 {}W < PL1 {}W", l.pl2_w, l.pl1_w),
                    });
                }
                Ok(())
            }

            ControlCommand::SetGpuPlatformPolicy(p) => {
                require_supported(caps.gpu_platform_policy)?;
                if !(1..=4).contains(&p.dstate) {
                    return Err(ControlError::UnsafeRequest {
                        reason: format!("dstate {} out of range 1..=4 (100/50/25/12.5%)", p.dstate),
                    });
                }
                // 0 is special: it is what 0x21 reads on 8BAB ("board has no
                // slowdown-temp knob"), so writing 0 back is the read-modify-
                // write PRESERVE of the firmware's own value — by construction
                // it cannot be unsafe here. Explicit user values are already
                // range-checked client-side; this band is the second fence.
                if p.slowdown_temp_c != 0 && !(30..=110).contains(&p.slowdown_temp_c) {
                    return Err(ControlError::UnsafeRequest {
                        reason: format!(
                            "gpu slowdown temp {}°C outside plausible band 30..=110 (0 = preserve board-absent value)",
                            p.slowdown_temp_c
                        ),
                    });
                }
                Ok(())
            }

            ControlCommand::SetCpuPolicy(p) => self.validate_cpu_policy(p, caps),

            ControlCommand::SetThermalMode(_) => require_supported(caps.thermal_mode),

            ControlCommand::SetFanMode(FanMode::FirmwareAuto) => {
                // The safety escape hatch is always allowed.
                Ok(())
            }
            ControlCommand::SetFanMode(FanMode::Max) => require_supported(caps.max_fan),
            ControlCommand::SetFanMode(FanMode::Manual(levels)) => {
                require_supported(caps.fan_manual_level)?;
                if self.override_active() {
                    return Err(ControlError::UnsafeRequest {
                        reason: "thermal override active (max fan forced); cool down first".into(),
                    });
                }
                // §27 priority: Max > Manual. Max→Manual directly is
                // rejected — go through FirmwareAuto first.
                if matches!(observed.max_fan.value(), Some(true)) {
                    return Err(ControlError::UnsafeRequest {
                        reason: "max fan is on; return to automatic before manual levels".into(),
                    });
                }
                self.validate_fan_levels(*levels, caps)?;
                // R4: never fly blind. The hysteresis net depends on fresh
                // temperature; without it manual fan is unsafe by definition.
                match feed.pkg_temp_c() {
                    Some((_, at)) if at.elapsed() <= PREWRITE_TEMP_FRESH => Ok(()),
                    _ => Err(ControlError::UnsafeRequest {
                        reason: "no fresh CPU temperature sample (≤5s); refusing blind manual fan control".into(),
                    }),
                }
            }
        }
    }

    fn validate_cpu_policy(
        &self,
        p: &CpuPolicy,
        caps: &CapabilitySet,
    ) -> Result<(), ControlError> {
        // R8: power limits in M2 poison the WHOLE command.
        if p.power_limits.is_some() {
            return Err(ControlError::Unsupported);
        }
        if p.epp_ac.is_some() || p.epp_dc.is_some() {
            require_supported(caps.ppm.epp)?;
            require_elevated(caps)?;
            for v in [p.epp_ac, p.epp_dc].into_iter().flatten() {
                if v > 100 {
                    return Err(ControlError::UnsafeRequest {
                        reason: format!("EPP {v} out of range 0..=100"),
                    });
                }
            }
        }
        if p.epp1_ac.is_some() || p.epp1_dc.is_some() {
            require_supported(caps.ppm.epp1)?;
            require_elevated(caps)?;
            for v in [p.epp1_ac, p.epp1_dc].into_iter().flatten() {
                if v > 100 {
                    return Err(ControlError::UnsafeRequest {
                        reason: format!("EPP1 {v} out of range 0..=100"),
                    });
                }
            }
        }
        if p.max_freq_mhz_ac.is_some() || p.max_freq_mhz_dc.is_some() {
            require_supported(caps.ppm.max_freq)?;
            require_elevated(caps)?;
            for v in [p.max_freq_mhz_ac, p.max_freq_mhz_dc].into_iter().flatten() {
                // 0 = unlimited; otherwise a coarse sanity band only.
                if v != 0 && !(400..=6000).contains(&v) {
                    return Err(ControlError::UnsafeRequest {
                        reason: format!("max frequency {v} MHz outside 0|400..=6000"),
                    });
                }
            }
        }
        if p.boost_policy.is_some() {
            // Boost values 5/6 may be firmware-rejected; readback
            // verification (AR-10) settles that at execute time, not here.
            require_elevated(caps)?;
        }
        Ok(())
    }

    fn validate_fan_levels(
        &self,
        levels: FanLevels,
        caps: &CapabilitySet,
    ) -> Result<(), ControlError> {
        if levels.is_auto() {
            // Manual(AUTO) is just FirmwareAuto with extra steps; harmless.
            return Ok(());
        }
        let (Some(lo), Some(hi)) = (caps.fan.clamp_min, caps.fan.clamp_max) else {
            // Fail closed: capability should have forced Unsupported already.
            return Err(ControlError::UnsafeRequest {
                reason: "no fan clamp range known (0x2F table + profile both failed)".into(),
            });
        };
        for (channel, v) in [("cpu", levels.cpu), ("gpu", levels.gpu)] {
            if v == 0 {
                continue; // 0 = leave that channel on firmware auto
            }
            if v < lo || v > hi {
                return Err(ControlError::UnsafeRequest {
                    reason: format!(
                        "{channel} fan level {v} outside clamp {lo}..={hi} (x100 RPM)"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Runtime safety evaluation. Called on the coordinator tick (≤1s while
    /// the user holds the fans). `observed` carries the current TrustedWrite/
    /// Verified fan state; `now` is injectable for tests.
    pub fn evaluate(
        &mut self,
        feed: &dyn ThermalFeed,
        observed: &ObservedState,
        now: Instant,
    ) -> Option<SafetyAction> {
        let user_fan_active = matches!(observed.fan_mode.value(), Some(FanMode::Manual(_)))
            || matches!(observed.max_fan.value(), Some(true))
            || self.override_active();

        // Watchdog first: blind controller hands back to firmware.
        if user_fan_active {
            let temp_stale = feed
                .pkg_temp_c()
                .is_none_or(|(_, at)| now.duration_since(at) > SENSOR_STALE_AFTER);
            let fan_stale = feed
                .fan_levels()
                .is_none_or(|(_, at)| now.duration_since(at) > SENSOR_STALE_AFTER);
            if temp_stale || fan_stale {
                self.override_saved = None;
                return Some(SafetyAction::WatchdogRestoreAuto);
            }
        }

        // Hysteresis release.
        if let Some(saved) = self.override_saved {
            if let Some((t, at)) = feed.pkg_temp_c()
                && now.duration_since(at) <= SENSOR_STALE_AFTER
                && t <= RELEASE_MAX_FAN_AT_C
            {
                self.override_saved = None;
                return Some(SafetyAction::ReleaseTo(saved));
            }
            return None;
        }

        // Hysteresis engage (only while the user holds the fans).
        if (matches!(observed.fan_mode.value(), Some(FanMode::Manual(_)))
            || matches!(observed.max_fan.value(), Some(true)))
            && let Some((t, at)) = feed.pkg_temp_c()
            && now.duration_since(at) <= SENSOR_STALE_AFTER
            && t >= FORCE_MAX_FAN_AT_C
        {
            let current = observed
                .fan_mode
                .value()
                .copied()
                .unwrap_or(FanMode::FirmwareAuto);
            self.override_saved = Some(current);
            return Some(SafetyAction::ForceMaxFan);
        }
        None
    }
}

fn require_supported(s: Support) -> Result<(), ControlError> {
    // M2 write gate: Supported only — Experimental is read-side only.
    if s == Support::Supported {
        Ok(())
    } else {
        Err(ControlError::Unsupported)
    }
}

fn require_elevated(caps: &CapabilitySet) -> Result<(), ControlError> {
    if caps.ppm.write_privileged {
        Ok(())
    } else {
        Err(ControlError::PermissionDenied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::capability::FanScale;
    use phelper_domain::policy::{BoostPolicy, ThermalMode};

    struct FakeFeed {
        temp: Option<(f64, Instant)>,
        fans: Option<(FanLevels, Instant)>,
    }

    impl FakeFeed {
        fn fresh(temp_c: f64) -> Self {
            Self {
                temp: Some((temp_c, Instant::now())),
                fans: Some((FanLevels::new(30, 30), Instant::now())),
            }
        }
        fn stale() -> Self {
            let old = Instant::now() - SENSOR_STALE_AFTER - Duration::from_secs(1);
            Self {
                temp: Some((70.0, old)),
                fans: Some((FanLevels::new(30, 30), old)),
            }
        }
    }

    impl ThermalFeed for FakeFeed {
        fn pkg_temp_c(&self) -> Option<(f64, Instant)> {
            self.temp
        }
        fn fan_levels(&self) -> Option<(FanLevels, Instant)> {
            self.fans
        }
        fn power_limits_w(&self) -> Option<(f64, f64, Instant)> {
            Some((55.0, 130.0, Instant::now()))
        }
    }

    fn caps_full() -> CapabilitySet {
        let mut c = CapabilitySet {
            known_board: true,
            ..Default::default()
        };
        c.thermal_mode = Support::Supported;
        c.fan_rpm_read = Support::Supported;
        c.fan_manual_level = Support::Supported;
        c.max_fan = Support::Supported;
        c.fan.scale = FanScale::Krpm;
        c.fan.clamp_min = Some(5);
        c.fan.clamp_max = Some(55);
        c.ppm.epp = Support::Supported;
        c.ppm.epp1 = Support::Supported;
        c.ppm.max_freq = Support::Supported;
        c.ppm.write_privileged = true;
        c.gpu_platform_policy = Support::Supported;
        c
    }

    fn observed_manual(levels: FanLevels) -> ObservedState {
        let mut o = ObservedState::default();
        o.fan_mode = phelper_domain::state::ObservedValue::TrustedWrite {
            value: FanMode::Manual(levels),
            at: Instant::now(),
        };
        o
    }

    fn observed_max_fan() -> ObservedState {
        let mut o = ObservedState::default();
        o.max_fan = phelper_domain::state::ObservedValue::TrustedWrite {
            value: true,
            at: Instant::now(),
        };
        o
    }

    // ---- validate: CPU policy ----

    #[test]
    fn epp_over_100_rejected() {
        let s = SafetySupervisor::new();
        let p = CpuPolicy {
            epp_ac: Some(101),
            ..Default::default()
        };
        let e = s
            .validate(
                &ControlCommand::SetCpuPolicy(p),
                &caps_full(),
                &FakeFeed::fresh(70.0),
                &ObservedState::default(),
            )
            .unwrap_err();
        assert!(matches!(e, ControlError::UnsafeRequest { .. }));
    }

    #[test]
    fn epp1_over_100_rejected() {
        let s = SafetySupervisor::new();
        let p = CpuPolicy {
            epp1_dc: Some(101),
            ..Default::default()
        };
        let e = s
            .validate(
                &ControlCommand::SetCpuPolicy(p),
                &caps_full(),
                &FakeFeed::fresh(70.0),
                &ObservedState::default(),
            )
            .unwrap_err();
        assert!(matches!(e, ControlError::UnsafeRequest { .. }));
    }

    #[test]
    fn epp1_requires_capability() {
        let s = SafetySupervisor::new();
        let mut caps = caps_full();
        caps.ppm.epp1 = Support::Unsupported;
        let p = CpuPolicy {
            epp1_ac: Some(20),
            ..Default::default()
        };
        let e = s
            .validate(
                &ControlCommand::SetCpuPolicy(p),
                &caps,
                &FakeFeed::fresh(70.0),
                &ObservedState::default(),
            )
            .unwrap_err();
        assert_eq!(e, ControlError::Unsupported);
    }

    #[test]
    fn power_limits_poisons_whole_command() {
        let s = SafetySupervisor::new();
        let p = CpuPolicy {
            epp_ac: Some(20),
            power_limits: Some(phelper_domain::policy::CpuPowerLimits {
                pl1_w: 55,
                pl2_w: 110,
                pl4_w: 215,
                cpu_gpu_concurrent_w: 0,
            }),
            ..Default::default()
        };
        let e = s
            .validate(
                &ControlCommand::SetCpuPolicy(p),
                &caps_full(),
                &FakeFeed::fresh(70.0),
                &ObservedState::default(),
            )
            .unwrap_err();
        assert_eq!(e, ControlError::Unsupported);
    }

    // ---- validate: GPU platform policy (0x22) ----

    fn gpu_policy() -> phelper_domain::policy::GpuPlatformPolicy {
        phelper_domain::policy::GpuPlatformPolicy {
            ctgp: true,
            ppab: true,
            dstate: 1,
            slowdown_temp_c: 87,
        }
    }

    #[test]
    fn gpu_policy_allowed_with_capability() {
        let s = SafetySupervisor::new();
        s.validate(
            &ControlCommand::SetGpuPlatformPolicy(gpu_policy()),
            &caps_full(),
            &FakeFeed::fresh(70.0),
            &ObservedState::default(),
        )
        .unwrap();
    }

    #[test]
    fn gpu_policy_requires_capability() {
        let s = SafetySupervisor::new();
        let mut caps = caps_full();
        caps.gpu_platform_policy = Support::Unsupported;
        let e = s
            .validate(
                &ControlCommand::SetGpuPlatformPolicy(gpu_policy()),
                &caps,
                &FakeFeed::fresh(70.0),
                &ObservedState::default(),
            )
            .unwrap_err();
        assert_eq!(e, ControlError::Unsupported);
    }

    #[test]
    fn gpu_policy_bad_dstate_rejected() {
        let s = SafetySupervisor::new();
        let mut p = gpu_policy();
        p.dstate = 5;
        let e = s
            .validate(
                &ControlCommand::SetGpuPlatformPolicy(p),
                &caps_full(),
                &FakeFeed::fresh(70.0),
                &ObservedState::default(),
            )
            .unwrap_err();
        assert!(matches!(e, ControlError::UnsafeRequest { .. }));
    }

    #[test]
    fn gpu_policy_implausible_slowdown_rejected() {
        let s = SafetySupervisor::new();
        let mut p = gpu_policy();
        p.slowdown_temp_c = 200;
        let e = s
            .validate(
                &ControlCommand::SetGpuPlatformPolicy(p),
                &caps_full(),
                &FakeFeed::fresh(70.0),
                &ObservedState::default(),
            )
            .unwrap_err();
        assert!(matches!(e, ControlError::UnsafeRequest { .. }));
    }

    #[test]
    fn gpu_policy_slowdown_zero_is_preserve() {
        // 8BAB's 0x21 readback reports slowdown_temp_c = 0 ("no knob").
        // A read-modify-write that carries 0 back must pass the safety
        // gate — it preserves the firmware's own value. (HIL-2 catch:
        // without this, every gpu-policy write on 8BAB is rejected.)
        let s = SafetySupervisor::new();
        let mut p = gpu_policy();
        p.slowdown_temp_c = 0;
        s.validate(
            &ControlCommand::SetGpuPlatformPolicy(p),
            &caps_full(),
            &FakeFeed::fresh(70.0),
            &ObservedState::default(),
        )
        .unwrap();
    }

    // ---- validate: 0x29 power limits (double gate) ----

    fn pl(pl1: u8, pl2: u8) -> phelper_domain::policy::CpuPowerLimits {
        phelper_domain::policy::CpuPowerLimits {
            pl1_w: pl1,
            pl2_w: pl2,
            pl4_w: 0,
            cpu_gpu_concurrent_w: 0,
        }
    }

    /// Feature-OFF builds reject by construction, even with Experimental
    /// caps and a sane payload (§57: the path must not exist).
    #[cfg(not(feature = "experimental-hp-power-limits"))]
    #[test]
    fn power_limits_rejected_without_feature() {
        let s = SafetySupervisor::new();
        let mut caps = caps_full();
        caps.power_limits = Support::Experimental;
        let e = s
            .validate(
                &ControlCommand::SetPowerLimits(pl(45, 90)),
                &caps,
                &FakeFeed::fresh(70.0),
                &ObservedState::default(),
            )
            .unwrap_err();
        assert_eq!(e, ControlError::Unsupported);
    }

    #[cfg(feature = "experimental-hp-power-limits")]
    #[test]
    fn power_limits_requires_experimental_caps() {
        let s = SafetySupervisor::new();
        // Supported-but-not-Experimental must NOT pass (§54 Tier C rule).
        let mut caps = caps_full();
        caps.power_limits = Support::Supported;
        let e = s
            .validate(
                &ControlCommand::SetPowerLimits(pl(45, 90)),
                &caps,
                &FakeFeed::fresh(70.0),
                &ObservedState::default(),
            )
            .unwrap_err();
        assert_eq!(e, ControlError::Unsupported);
    }

    #[cfg(feature = "experimental-hp-power-limits")]
    #[test]
    fn power_limits_range_ladder() {
        let s = SafetySupervisor::new();
        let mut caps = caps_full();
        caps.power_limits = Support::Experimental;
        let f = FakeFeed::fresh(70.0);
        let o = ObservedState::default();
        let check = |l| s.validate(&ControlCommand::SetPowerLimits(l), &caps, &f, &o);

        check(pl(45, 90)).unwrap(); // sane → pass

        let mut bad = pl(14, 90); // PL1 below envelope
        assert!(matches!(check(bad), Err(ControlError::UnsafeRequest { .. })));
        bad = pl(131, 140); // PL1 above envelope
        assert!(matches!(check(bad), Err(ControlError::UnsafeRequest { .. })));
        bad = pl(45, 158); // PL2 above envelope
        assert!(matches!(check(bad), Err(ControlError::UnsafeRequest { .. })));
        bad = pl(90, 45); // PL2 < PL1 (kernel invariant)
        assert!(matches!(check(bad), Err(ControlError::UnsafeRequest { .. })));
        bad = pl(45, 90);
        bad.pl4_w = 200; // pl4 unverified
        assert!(matches!(check(bad), Err(ControlError::UnsafeRequest { .. })));
        bad = pl(45, 90);
        bad.cpu_gpu_concurrent_w = 65; // cc unverified
        assert!(matches!(check(bad), Err(ControlError::UnsafeRequest { .. })));
    }

    #[test]
    fn unelevated_ppm_write_denied() {
        let s = SafetySupervisor::new();
        let mut caps = caps_full();
        caps.ppm.write_privileged = false;
        let p = CpuPolicy {
            epp_ac: Some(20),
            ..Default::default()
        };
        let e = s
            .validate(
                &ControlCommand::SetCpuPolicy(p),
                &caps,
                &FakeFeed::fresh(70.0),
                &ObservedState::default(),
            )
            .unwrap_err();
        assert_eq!(e, ControlError::PermissionDenied);
    }

    #[test]
    fn max_freq_sanity_band() {
        let s = SafetySupervisor::new();
        for bad in [1u32, 399, 6001] {
            let p = CpuPolicy {
                max_freq_mhz_dc: Some(bad),
                ..Default::default()
            };
            assert!(s
                .validate(
                    &ControlCommand::SetCpuPolicy(p),
                    &caps_full(),
                    &FakeFeed::fresh(70.0),
                    &ObservedState::default(),
                )
                .is_err());
        }
        for good in [0u32, 400, 2000, 6000] {
            let p = CpuPolicy {
                max_freq_mhz_dc: Some(good),
                ..Default::default()
            };
            assert!(s
                .validate(
                    &ControlCommand::SetCpuPolicy(p),
                    &caps_full(),
                    &FakeFeed::fresh(70.0),
                    &ObservedState::default(),
                )
                .is_ok());
        }
    }

    #[test]
    fn boost_needs_elevation_only() {
        let s = SafetySupervisor::new();
        let p = CpuPolicy {
            boost_policy: Some(BoostPolicy::EfficientAggressiveGuaranteed),
            ..Default::default()
        };
        assert!(s
            .validate(
                &ControlCommand::SetCpuPolicy(p),
                &caps_full(),
                &FakeFeed::fresh(70.0),
                &ObservedState::default(),
            )
            .is_ok());
    }

    // ---- validate: fan ----

    #[test]
    fn manual_fan_outside_clamp_rejected() {
        let s = SafetySupervisor::new();
        for bad in [FanLevels::new(4, 30), FanLevels::new(30, 56)] {
            let e = s
                .validate(
                    &ControlCommand::SetFanMode(FanMode::Manual(bad)),
                    &caps_full(),
                    &FakeFeed::fresh(70.0),
                    &ObservedState::default(),
                )
                .unwrap_err();
            assert!(matches!(e, ControlError::UnsafeRequest { .. }));
        }
    }

    #[test]
    fn manual_fan_needs_fresh_temperature() {
        let s = SafetySupervisor::new();
        let feed = FakeFeed {
            temp: Some((70.0, Instant::now() - Duration::from_secs(10))),
            fans: None,
        };
        let e = s
            .validate(
                &ControlCommand::SetFanMode(FanMode::Manual(FanLevels::new(30, 30))),
                &caps_full(),
                &feed,
                &ObservedState::default(),
            )
            .unwrap_err();
        assert!(matches!(e, ControlError::UnsafeRequest { .. }));
    }

    #[test]
    fn max_to_manual_rejected_manual_to_max_allowed() {
        let s = SafetySupervisor::new();
        let e = s
            .validate(
                &ControlCommand::SetFanMode(FanMode::Manual(FanLevels::new(30, 30))),
                &caps_full(),
                &FakeFeed::fresh(70.0),
                &observed_max_fan(),
            )
            .unwrap_err();
        assert!(matches!(e, ControlError::UnsafeRequest { .. }));
        assert!(s
            .validate(
                &ControlCommand::SetFanMode(FanMode::Max),
                &caps_full(),
                &FakeFeed::fresh(70.0),
                &observed_manual(FanLevels::new(30, 30)),
            )
            .is_ok());
    }

    #[test]
    fn firmware_auto_always_allowed() {
        let s = SafetySupervisor::new();
        let mut caps = caps_full();
        caps.fan_manual_level = Support::Unsupported;
        caps.max_fan = Support::Unsupported;
        assert!(s
            .validate(
                &ControlCommand::SetFanMode(FanMode::FirmwareAuto),
                &caps,
                &FakeFeed::fresh(70.0),
                &ObservedState::default(),
            )
            .is_ok());
    }

    #[test]
    fn experimental_is_not_writable_in_m2() {
        let s = SafetySupervisor::new();
        let mut caps = caps_full();
        caps.thermal_mode = Support::Experimental;
        let e = s
            .validate(
                &ControlCommand::SetThermalMode(ThermalMode::Performance),
                &caps,
                &FakeFeed::fresh(70.0),
                &ObservedState::default(),
            )
            .unwrap_err();
        assert_eq!(e, ControlError::Unsupported);
    }

    #[test]
    fn m2_rejects_out_of_scope_commands() {
        let s = SafetySupervisor::new();
        let f = FakeFeed::fresh(70.0);
        let o = ObservedState::default();
        let c = caps_full();
        // M3 note: SetGpuPlatformPolicy (0x22) moved IN scope (0x21 readback
        // verification). SetPowerLimits is gated (Experimental caps +
        // cargo feature) — it still rejects HERE because this caps_full()
        // deliberately leaves power_limits at NotProbed.
        for cmd in [
            ControlCommand::ApplyProfile {
                profile: "x".into(),
            },
            ControlCommand::SetMuxMode(phelper_domain::policy::MuxMode::Discrete),
            ControlCommand::SetPowerLimits(phelper_domain::policy::CpuPowerLimits {
                pl1_w: 55,
                pl2_w: 110,
                pl4_w: 215,
                cpu_gpu_concurrent_w: 0,
            }),
        ] {
            assert_eq!(s.validate(&cmd, &c, &f, &o).unwrap_err(), ControlError::Unsupported);
        }
    }

    // ---- evaluate: hysteresis ----

    #[test]
    fn hysteresis_engage_and_release() {
        let mut s = SafetySupervisor::new();
        let o = observed_manual(FanLevels::new(20, 20));
        // 89°C: nothing.
        assert_eq!(
            s.evaluate(&FakeFeed::fresh(89.0), &o, Instant::now()),
            None
        );
        assert!(!s.override_active());
        // 90°C: force max fan, manual(20,20) saved.
        assert_eq!(
            s.evaluate(&FakeFeed::fresh(90.0), &o, Instant::now()),
            Some(SafetyAction::ForceMaxFan)
        );
        assert!(s.override_active());
        // 86°C: still latched.
        assert_eq!(
            s.evaluate(&FakeFeed::fresh(86.0), &o, Instant::now()),
            None
        );
        // 85°C: release back to the saved manual mode.
        assert_eq!(
            s.evaluate(&FakeFeed::fresh(85.0), &o, Instant::now()),
            Some(SafetyAction::ReleaseTo(FanMode::Manual(FanLevels::new(20, 20))))
        );
        assert!(!s.override_active());
    }

    #[test]
    fn hysteresis_ignores_firmware_auto() {
        let mut s = SafetySupervisor::new();
        // 95°C but the firmware holds the fans — not our problem (AR-12).
        assert_eq!(
            s.evaluate(
                &FakeFeed::fresh(95.0),
                &ObservedState::default(),
                Instant::now()
            ),
            None
        );
    }

    // ---- evaluate: watchdog ----

    #[test]
    fn watchdog_restores_auto_on_freeze_while_manual() {
        let mut s = SafetySupervisor::new();
        let o = observed_manual(FanLevels::new(30, 30));
        assert_eq!(
            s.evaluate(&FakeFeed::stale(), &o, Instant::now()),
            Some(SafetyAction::WatchdogRestoreAuto)
        );
    }

    #[test]
    fn watchdog_ignores_freeze_when_firmware_auto() {
        let mut s = SafetySupervisor::new();
        assert_eq!(
            s.evaluate(&FakeFeed::stale(), &ObservedState::default(), Instant::now()),
            None
        );
    }
}
