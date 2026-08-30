//! Performance profiles (§36): the built-in presets plus user TOML files
//! from `%LOCALAPPDATA%\phelper\profiles\*.toml`.
//!
//! Registry rules:
//! - Built-ins ship with evidence-based values measured on 8BAB (see each
//!   preset's comment) and are deliberately STABLE-CLEAN: no experimental
//!   fields, so a default build can apply every built-in.
//! - User files may add experimental fields (e.g. `power_limits`) — the
//!   apply-time gates decide, never the loader.
//! - A user file whose name shadows a built-in is SKIPPED with a warning
//!   (surprising shadowing is worse than an extra name).
//! - A broken TOML file is a warning, never a panic and never an abort
//!   (the engine must start with the profiles it could parse).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use phelper_domain::policy::{BoostPolicy, FanCurve, FanMode, ThermalMode};
use phelper_domain::profile::{GpuPolicyPatch, PerformanceProfile};

/// User profile directory: %LOCALAPPDATA%\phelper\profiles.
pub fn profiles_dir() -> PathBuf {
    crate::persistence::data_dir().join("profiles")
}

/// Name → profile, plus non-fatal load warnings for diagnostics.
#[derive(Debug, Clone, Default)]
pub struct ProfileRegistry {
    entries: BTreeMap<String, ProfileEntry>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct ProfileEntry {
    profile: PerformanceProfile,
    builtin: bool,
}

impl ProfileRegistry {
    /// Empty registry (tests inject exactly what they need).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Built-ins only.
    pub fn with_builtins() -> Self {
        let mut r = Self::empty();
        for (name, profile) in builtins() {
            r.entries.insert(
                name.to_string(),
                ProfileEntry {
                    profile,
                    builtin: true,
                },
            );
        }
        r
    }

    /// Built-ins + every parseable *.toml in `dir` (missing dir is fine).
    pub fn load(dir: &Path) -> Self {
        let mut r = Self::with_builtins();
        let Ok(rd) = std::fs::read_dir(dir) else {
            return r; // no user directory yet — not an error
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                r.warnings
                    .push(format!("{}: unreadable file name", path.display()));
                continue;
            };
            if r.entries.contains_key(name) {
                r.warnings.push(format!(
                    "{name}: user file {} shadows a built-in — skipped (rename it)",
                    path.display()
                ));
                continue;
            }
            match std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|text| {
                    toml::from_str::<PerformanceProfile>(&text).map_err(|e| e.to_string())
                }) {
                Ok(profile) => {
                    r.entries.insert(
                        name.to_string(),
                        ProfileEntry {
                            profile,
                            builtin: false,
                        },
                    );
                }
                Err(e) => {
                    r.warnings.push(format!("{}: {e}", path.display()));
                }
            }
        }
        r
    }

    /// Built-ins + user files from the default directory.
    pub fn load_default() -> Self {
        Self::load(&profiles_dir())
    }

    /// Register a profile programmatically (tests).
    pub fn insert(&mut self, name: &str, profile: PerformanceProfile) {
        self.entries.insert(
            name.to_string(),
            ProfileEntry {
                profile,
                builtin: false,
            },
        );
    }

    pub fn get(&self, name: &str) -> Option<&PerformanceProfile> {
        self.entries.get(name).map(|e| &e.profile)
    }

    pub fn is_builtin(&self, name: &str) -> bool {
        self.entries.get(name).is_some_and(|e| e.builtin)
    }

    /// (name, profile, is_builtin) in alphabetical order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &PerformanceProfile, bool)> {
        self.entries
            .iter()
            .map(|(n, e)| (n.as_str(), &e.profile, e.builtin))
    }
}

/// Serialize a profile as a TOML template (for `profile export`).
pub fn to_toml(profile: &PerformanceProfile) -> Result<String, toml::ser::Error> {
    toml::to_string_pretty(profile)
}

/// The shipped presets. Values are starting points grounded in 8BAB HIL
/// evidence (M1–M4), not guesses: the idle fan-stop fact, the reference
/// PPM defaults, and the 0x21 stock readback. Power limits are deliberately
/// ABSENT from every built-in — raising sustained limits is a per-user
/// experimental decision, not a preset.
fn builtins() -> Vec<(&'static str, PerformanceProfile)> {
    use phelper_domain::policy::CpuPolicy;
    vec![
        (
            "silent",
            PerformanceProfile {
                description: "安静省电，低负载低速运行".into(),
                thermal_mode: Some(ThermalMode::Balanced),
                // Keep fan ownership in the application. FirmwareAuto is a
                // release/fail-safe state, not a profile mode.
                fan: Some(FanMode::Curve(FanCurve::quiet())),
                cpu: CpuPolicy {
                    epp_ac: Some(80),
                    epp_dc: Some(95),
                    epp1_ac: Some(80),
                    epp1_dc: Some(95),
                    boost_policy: Some(BoostPolicy::EfficientAggressive),
                    ..CpuPolicy::default()
                },
                gpu_policy: None,
                power_limits: None,
                os_policy: None,
            },
        ),
        (
            "balanced",
            PerformanceProfile {
                description: "均衡性能与散热".into(),
                thermal_mode: Some(ThermalMode::Balanced),
                fan: Some(FanMode::Curve(FanCurve::balanced())),
                cpu: CpuPolicy {
                    epp_ac: Some(0),
                    epp_dc: Some(0),
                    epp1_ac: Some(0),
                    epp1_dc: Some(0),
                    max_freq_mhz_ac: Some(0),
                    max_freq_mhz_dc: Some(0),
                    boost_policy: Some(BoostPolicy::Aggressive),
                    ..CpuPolicy::default()
                },
                // The stock 0x21 readback on 8BAB (M3): cTGP/PPAB on.
                // dstate is deliberately ABSENT: M5 HIL (2026-08-26) found
                // 0x22 dstate writes ineffective on this board (0x21 keeps
                // reading the firmware's own value, 1 or 3 depending on
                // conditions) — a built-in must not carry a knob that
                // cannot verify.
                gpu_policy: Some(GpuPolicyPatch {
                    ctgp: Some(true),
                    ppab: Some(true),
                    dstate: None,
                    slowdown_temp_c: None,
                }),
                power_limits: None,
                os_policy: None,
            },
        ),
        (
            "gaming",
            PerformanceProfile {
                description: "游戏优先，响应更快，风扇按性能曲线运行".into(),
                thermal_mode: Some(ThermalMode::Performance),
                fan: Some(FanMode::Curve(FanCurve::performance())),
                cpu: CpuPolicy {
                    epp_ac: Some(0),
                    epp_dc: Some(0),
                    epp1_ac: Some(0),
                    epp1_dc: Some(0),
                    boost_policy: Some(BoostPolicy::Aggressive),
                    ..CpuPolicy::default()
                },
                gpu_policy: Some(GpuPolicyPatch {
                    ctgp: Some(true),
                    ppab: Some(true),
                    dstate: None,
                    slowdown_temp_c: None,
                }),
                power_limits: None,
                os_policy: None,
            },
        ),
        (
            "cpu-max",
            PerformanceProfile {
                description: "持续性能优先，风扇全速运行".into(),
                thermal_mode: Some(ThermalMode::Performance),
                fan: Some(FanMode::Max),
                cpu: CpuPolicy {
                    epp_ac: Some(0),
                    epp_dc: Some(0),
                    epp1_ac: Some(0),
                    epp1_dc: Some(0),
                    boost_policy: Some(BoostPolicy::Aggressive),
                    ..CpuPolicy::default()
                },
                gpu_policy: None,
                power_limits: None,
                os_policy: None,
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::policy::{CpuPowerLimits, FanLevels};

    #[test]
    fn builtins_are_present_and_stable_clean() {
        let r = ProfileRegistry::with_builtins();
        for name in ["silent", "balanced", "gaming", "cpu-max"] {
            let p = r
                .get(name)
                .unwrap_or_else(|| panic!("missing built-in {name}"));
            // No built-in may smuggle experimental fields into stable builds.
            assert!(p.power_limits.is_none(), "{name} must be stable-clean");
            assert!(
                p.cpu.power_limits.is_none(),
                "{name} cpu R8 field must stay empty"
            );
            assert!(r.is_builtin(name));
        }
    }

    #[test]
    fn builtins_keep_fan_control_in_the_application() {
        let r = ProfileRegistry::with_builtins();
        for name in ["silent", "balanced", "gaming"] {
            assert!(matches!(
                r.get(name).and_then(|p| p.fan),
                Some(FanMode::Curve(_))
            ));
        }
        assert_eq!(r.get("cpu-max").and_then(|p| p.fan), Some(FanMode::Max));
    }

    #[test]
    fn toml_round_trip_full_profile() {
        let text = r#"
description = "测试档"

thermal_mode = "performance"
fan = { manual = { left = 25, right = 30 } }

[cpu]
epp_ac = 80
boost_policy = "efficient_aggressive"
boost_ac = "aggressive"
boost_dc = "efficient_enabled"
min_perf_ac = 20
min_perf_dc = 5
max_perf_ac = 100
max_perf_dc = 80

[gpu_policy]
ctgp = false

[power_limits]
pl1_w = 45
pl2_w = 90
pl4_w = 150
cpu_gpu_concurrent_w = 0

[os_policy]
cpu_placement = "performance"
qos = "high"
process_priority = "above_normal"
memory_priority = "normal"
gpu_preference = "high_performance"
"#;
        let p: PerformanceProfile = toml::from_str(text).expect("parse");
        assert_eq!(p.description, "测试档");
        assert_eq!(p.thermal_mode, Some(ThermalMode::Performance));
        assert_eq!(p.fan, Some(FanMode::Manual(FanLevels::new(25, 30))));
        assert_eq!(p.cpu.epp_ac, Some(80));
        assert_eq!(p.cpu.boost_policy, Some(BoostPolicy::EfficientAggressive));
        assert_eq!(p.cpu.boost_policy_ac, Some(BoostPolicy::Aggressive));
        assert_eq!(p.cpu.boost_policy_dc, Some(BoostPolicy::EfficientEnabled));
        assert_eq!(p.cpu.min_performance_ac, Some(20));
        assert_eq!(p.cpu.min_performance_dc, Some(5));
        assert_eq!(p.cpu.max_performance_ac, Some(100));
        assert_eq!(p.cpu.max_performance_dc, Some(80));
        assert_eq!(p.gpu_policy.unwrap().ctgp, Some(false));
        assert_eq!(p.gpu_policy.unwrap().ppab, None);
        assert_eq!(
            p.power_limits,
            Some(CpuPowerLimits {
                pl1_w: 45,
                pl2_w: 90,
                pl4_w: 150,
                cpu_gpu_concurrent_w: 0,
            })
        );
        let os = p.os_policy.as_ref().expect("os policy");
        assert_eq!(
            os.cpu_placement,
            Some(phelper_domain::os_policy::CpuPlacement::Performance)
        );
        assert_eq!(os.qos, Some(phelper_domain::os_policy::QosLevel::High));
        assert_eq!(
            os.process_priority,
            Some(phelper_domain::os_policy::ProcessPriority::AboveNormal)
        );
        assert_eq!(
            os.gpu_preference,
            Some(phelper_domain::os_policy::GpuPreference::HighPerformance)
        );
        // And it serializes back to parseable TOML.
        let out = to_toml(&p).expect("serialize");
        assert!(out.contains("left = 25"));
        assert!(out.contains("right = 30"));
        assert!(!out.contains("cpu = 25"));
        assert!(!out.contains("gpu = 30"));
        let q: PerformanceProfile = toml::from_str(&out).expect("re-parse");
        assert_eq!(p, q);
    }

    #[test]
    fn legacy_cpu_gpu_fan_names_remain_readable() {
        let text = r#"
description = "旧配置"
fan = { manual = { cpu = 25, gpu = 30 } }
"#;
        let profile: PerformanceProfile = toml::from_str(text).expect("parse legacy fan names");
        assert_eq!(profile.fan, Some(FanMode::Manual(FanLevels::new(25, 30))));
        let out = to_toml(&profile).expect("serialize canonical fan names");
        assert!(out.contains("left = 25"));
        assert!(out.contains("right = 30"));
    }

    #[test]
    fn toml_round_trip_curve_profile() {
        let profile = PerformanceProfile {
            description: "曲线档".into(),
            fan: Some(FanMode::Curve(phelper_domain::policy::FanCurve::balanced())),
            ..Default::default()
        };
        let out = to_toml(&profile).expect("serialize");
        let parsed: PerformanceProfile = toml::from_str(&out).expect("re-parse");
        assert_eq!(parsed, profile);
    }

    #[test]
    fn toml_sparse_profile_defaults_fill() {
        let p: PerformanceProfile =
            toml::from_str("description = \"只动 EPP\"\n[cpu]\nepp_dc = 95\n").expect("parse");
        assert_eq!(p.cpu.epp_dc, Some(95));
        assert!(p.cpu.epp_ac.is_none());
        assert!(p.fan.is_none());
        assert!(p.power_limits.is_none());
    }

    #[test]
    fn toml_unknown_field_rejected() {
        let r = toml::from_str::<PerformanceProfile>("epp_ac = 80\n");
        assert!(r.is_err(), "typo fields must be caught");
        let r = toml::from_str::<PerformanceProfile>("[cpu]\nepp_q3 = 80\n");
        assert!(r.is_err(), "typos inside [cpu] must be caught");
    }

    #[test]
    fn load_dir_user_file_and_shadow_and_broken() {
        let dir =
            std::env::temp_dir().join(format!("phelper-profiles-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("mine.toml"),
            "description = \"我的\"\n[cpu]\nepp_ac = 50\n",
        )
        .unwrap();
        std::fs::write(dir.join("silent.toml"), "description = \"shadow\"\n").unwrap();
        std::fs::write(dir.join("broken.toml"), "not [valid toml\n").unwrap();
        let r = ProfileRegistry::load(&dir);
        assert!(r.get("mine").is_some());
        assert!(!r.is_builtin("mine"));
        // Built-in silent survives the shadowing attempt.
        assert!(r.is_builtin("silent"));
        assert_ne!(r.get("silent").unwrap().description, "shadow");
        assert!(r.get("broken").is_none());
        assert_eq!(
            r.warnings.len(),
            2,
            "shadow + broken warnings: {:?}",
            r.warnings
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
