//! Pure vocabulary for power-aware automatic scheduling.
//!
//! The domain deliberately does not contain Windows notification handles or
//! process-enumeration policy.  It only describes the power context and the
//! read model that the core can expose to the UI/CLI.

use serde::{Deserialize, Serialize};

/// The power source reported by Windows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerSource {
    #[default]
    Unknown,
    Ac,
    Battery,
}

/// The complete, cheap-to-refresh power context used by automatic policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PowerContext {
    pub source: PowerSource,
    pub battery_percent: Option<u8>,
    pub battery_saver: Option<bool>,
    /// Active Windows power-plan GUID, when PowrProf returns one.
    pub active_scheme: Option<String>,
    pub observed_at_epoch_ms: u64,
}

/// Automatic scheduling modes exposed by phelper.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomaticMode {
    /// No process policy is owned by the automatic scheduler.
    #[default]
    Off,
    /// On battery, eligible user processes receive E-core CPU Sets and
    /// EcoQoS.  On AC, the automatic scheduler owns no process policy.
    BatteryEfficiency,
}

/// Lifecycle phase of the automatic scheduler.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomaticPhase {
    #[default]
    Disabled,
    /// The mode is enabled, but the power source is not battery or is not
    /// known well enough to make a safe decision.
    Waiting,
    /// A process snapshot is being reconciled.  This is transient and should
    /// not be rendered as an error by the UI.
    Applying,
    /// The desired automatic policy is stable for the current context.
    Active,
    Error,
}

/// Layer-A power budget (docs/automatic-scheduling-architecture §7.1/§8.3):
/// the classed, hysteresis-stable view of the power context. Kept as a
/// distinct vocabulary from `PowerSource` — a battery level class is a
/// POLICY input (Phase E hardware linkage consumes it), not a raw
/// observation; unknown inputs never classify (fail closed, §25 rule 2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerBudget {
    #[default]
    Unknown,
    /// On AC — no efficiency pressure.
    Ac,
    /// On battery, level nominal.
    Dc,
    /// On battery with Windows battery saver enabled.
    DcSaver,
    /// On battery below the low-battery ENTER threshold; stays classified
    /// until the battery recharges past the (higher) EXIT threshold.
    DcLowBattery,
}

impl PowerBudget {
    /// Dual-threshold low-battery hysteresis (§8.3; §16.2 draft defaults:
    /// enter 25 %, exit 35 % — values are the CALLER's policy, not baked
    /// in). `previous` is the last stable classification:
    ///
    /// - On AC the level is irrelevant → `Ac` (saver state is an AC-side
    ///   curiosity only; Windows does not run saver on AC by default).
    /// - A reading inside the hysteresis band (enter < level < exit)
    ///   KEEPS `previous` — a dipping charge must not oscillate the class.
    ///   With no previous class the band is genuinely undecided →
    ///   `Unknown` (fail closed: no new writes, §19.1).
    /// - A saver battery classifies as `DcSaver` regardless of level —
    ///   the user's explicit intent outranks a level band.
    /// - A missing battery level on battery power is `Unknown`, never a
    ///   guessed `Dc`.
    pub fn classify(
        ctx: &PowerContext,
        previous: PowerBudget,
        enter_percent: u8,
        exit_percent: u8,
    ) -> PowerBudget {
        debug_assert!(
            enter_percent <= exit_percent,
            "enter threshold must not exceed exit"
        );
        match ctx.source {
            PowerSource::Ac => PowerBudget::Ac,
            PowerSource::Battery => match ctx.battery_saver {
                Some(true) => PowerBudget::DcSaver,
                _ => match ctx.battery_percent {
                    Some(level) if level <= enter_percent => PowerBudget::DcLowBattery,
                    Some(level) if level >= exit_percent => PowerBudget::Dc,
                    // Inside the hysteresis band: keep the previous
                    // classification, or stay undecided without one.
                    Some(_) => previous,
                    None => PowerBudget::Unknown,
                },
            },
            PowerSource::Unknown => PowerBudget::Unknown,
        }
    }
}

/// Immutable read model for the core automatic scheduler.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutomaticSchedulerSnapshot {
    pub mode: AutomaticMode,
    pub phase: AutomaticPhase,
    pub power: Option<PowerContext>,
    pub managed_processes: u32,
    pub skipped_manual: u32,
    pub last_reconcile_at_epoch_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENTER: u8 = 25;
    const EXIT: u8 = 35;

    fn ctx(source: PowerSource, level: Option<u8>, saver: Option<bool>) -> PowerContext {
        PowerContext {
            source,
            battery_percent: level,
            battery_saver: saver,
            active_scheme: None,
            observed_at_epoch_ms: 0,
        }
    }

    /// §22.1: the hysteresis ladder — enter at ≤25 %, keep the class
    /// through the band, only release at ≥35 %.
    #[test]
    fn low_battery_hysteresis_enters_once_and_releases_above_exit() {
        let prev = PowerBudget::Dc;
        assert_eq!(
            PowerBudget::classify(
                &ctx(PowerSource::Battery, Some(80), None),
                prev,
                ENTER,
                EXIT
            ),
            PowerBudget::Dc
        );
        assert_eq!(
            PowerBudget::classify(
                &ctx(PowerSource::Battery, Some(ENTER), None),
                prev,
                ENTER,
                EXIT
            ),
            PowerBudget::DcLowBattery,
            "the ENTER threshold is inclusive"
        );
        // 30 % is inside the band: the class must HOLD, not flip back.
        assert_eq!(
            PowerBudget::classify(
                &ctx(PowerSource::Battery, Some(30), None),
                PowerBudget::DcLowBattery,
                ENTER,
                EXIT
            ),
            PowerBudget::DcLowBattery,
            "inside the hysteresis band the previous class holds"
        );
        assert_eq!(
            PowerBudget::classify(
                &ctx(PowerSource::Battery, Some(EXIT), None),
                PowerBudget::DcLowBattery,
                ENTER,
                EXIT
            ),
            PowerBudget::Dc,
            "the EXIT threshold is inclusive"
        );
    }

    /// §22.1: the same band reading must not invent a class when there is
    /// no previous one (fail closed).
    #[test]
    fn band_without_history_stays_unknown() {
        assert_eq!(
            PowerBudget::classify(
                &ctx(PowerSource::Battery, Some(30), None),
                PowerBudget::Unknown,
                ENTER,
                EXIT
            ),
            PowerBudget::Unknown,
            "an undecided band must not guess Dc or DcLowBattery"
        );
    }

    /// §22.1: unknown inputs never classify (§25 rule 2).
    #[test]
    fn unknown_inputs_fail_closed() {
        assert_eq!(
            PowerBudget::classify(
                &ctx(PowerSource::Unknown, Some(30), None),
                PowerBudget::Dc,
                ENTER,
                EXIT
            ),
            PowerBudget::Unknown
        );
        assert_eq!(
            PowerBudget::classify(
                &ctx(PowerSource::Battery, None, None),
                PowerBudget::Dc,
                ENTER,
                EXIT
            ),
            PowerBudget::Unknown,
            "battery power without a level is unknown, never a guessed Dc"
        );
    }

    /// §22.1: AC ignores the level entirely; saver outranks the band on
    /// battery.
    #[test]
    fn ac_ignores_level_and_saver_outranks_the_band() {
        assert_eq!(
            PowerBudget::classify(
                &ctx(PowerSource::Ac, Some(10), None),
                PowerBudget::Dc,
                ENTER,
                EXIT
            ),
            PowerBudget::Ac
        );
        assert_eq!(
            PowerBudget::classify(
                &ctx(PowerSource::Battery, Some(10), Some(true)),
                PowerBudget::Dc,
                ENTER,
                EXIT
            ),
            PowerBudget::DcSaver,
            "saver intent outranks the level band"
        );
        // Saver turned off falls back to the level rules.
        assert_eq!(
            PowerBudget::classify(
                &ctx(PowerSource::Battery, Some(10), Some(false)),
                PowerBudget::DcSaver,
                ENTER,
                EXIT
            ),
            PowerBudget::DcLowBattery
        );
    }
}
