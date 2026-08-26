//! AC/battery state via GetSystemPowerStatus (cheap, no PDH).

use phelper_domain::error::PlatformError;
use phelper_domain::ports::PowerStatus;
use phelper_domain::telemetry::{PowerSample, ProviderStatus};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

pub(crate) struct WindowsPower {
    degraded: Option<String>,
}

impl WindowsPower {
    pub(crate) fn new() -> Self {
        Self { degraded: None }
    }
}

impl PowerStatus for WindowsPower {
    fn sample(&mut self) -> Result<PowerSample, PlatformError> {
        let mut s = SYSTEM_POWER_STATUS::default();
        unsafe { GetSystemPowerStatus(&mut s) }
            .map_err(|e| PlatformError::Os(format!("GetSystemPowerStatus: {e}")))?;
        // ACLineStatus: 0 offline, 1 online, 255 unknown.
        let ac_online = match s.ACLineStatus {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        };
        // BatteryLifePercent: 0..=100, 255 unknown.
        let battery_percent = (s.BatteryLifePercent <= 100).then_some(s.BatteryLifePercent as f64);
        if ac_online.is_none() || battery_percent.is_none() {
            self.degraded = Some("AC/battery field unknown".into());
        }
        Ok(PowerSample {
            ac_online,
            battery_percent,
        })
    }

    fn status(&self) -> ProviderStatus {
        match &self.degraded {
            None => ProviderStatus::Ok,
            Some(d) => ProviderStatus::Degraded(d.clone()),
        }
    }
}
