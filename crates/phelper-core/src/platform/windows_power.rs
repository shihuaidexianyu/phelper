//! AC/battery state via GetSystemPowerStatus (cheap, no PDH).

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::mpsc::{self, Receiver, Sender};

use phelper_domain::automatic::{PowerContext, PowerSource};
use phelper_domain::error::PlatformError;
use phelper_domain::ports::PowerStatus;
use phelper_domain::telemetry::{PowerSample, ProviderStatus};
use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
use windows::Win32::System::Power::{
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, GetSystemPowerStatus, HPOWERNOTIFY, PowerGetActiveScheme,
    PowerSettingRegisterNotification, PowerSettingUnregisterNotification, SYSTEM_POWER_STATUS,
};
use windows::Win32::System::SystemServices::{
    GUID_ACDC_POWER_SOURCE, GUID_ACTIVE_POWERSCHEME, GUID_BATTERY_PERCENTAGE_REMAINING,
    GUID_POWER_SAVING_STATUS, GUID_POWERSCHEME_PERSONALITY,
};
use windows::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_CALLBACK;

/// Read the complete power context used by automatic scheduling.  The
/// active scheme is supplementary: a PowrProf read failure does not hide a
/// valid AC/DC result from GetSystemPowerStatus.
pub(crate) fn read_power_context() -> Result<PowerContext, PlatformError> {
    let status = query_system_power_status()?;
    let source = match status.ACLineStatus {
        0 => PowerSource::Battery,
        1 => PowerSource::Ac,
        _ => PowerSource::Unknown,
    };
    let battery_percent = (status.BatteryLifePercent <= 100).then_some(status.BatteryLifePercent);
    Ok(PowerContext {
        source,
        battery_percent,
        battery_saver: Some(status.SystemStatusFlag != 0),
        active_scheme: query_active_scheme(),
        observed_at_epoch_ms: epoch_ms(),
    })
}

fn query_system_power_status() -> Result<SYSTEM_POWER_STATUS, PlatformError> {
    let mut status = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut status) }
        .map_err(|e| PlatformError::Os(format!("GetSystemPowerStatus: {e}")))?;
    Ok(status)
}

fn query_active_scheme() -> Option<String> {
    let mut raw: *mut windows::core::GUID = null_mut();
    let code = unsafe { PowerGetActiveScheme(None, &mut raw) };
    if code != ERROR_SUCCESS || raw.is_null() {
        tracing::debug!(code = code.0, "PowerGetActiveScheme unavailable");
        return None;
    }
    let guid = unsafe { *raw };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(raw.cast())));
    }
    Some(format!("{guid:?}").to_ascii_lowercase())
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

struct PowerEventContext {
    tx: Sender<()>,
}

unsafe extern "system" fn power_event_callback(
    context: *const c_void,
    _event_type: u32,
    _setting: *const c_void,
) -> u32 {
    if !context.is_null() {
        let context = unsafe { &*(context as *const PowerEventContext) };
        let _ = context.tx.send(());
    }
    0
}

/// PowrProf callback subscription.  It only emits a wakeup hint; the worker
/// always rereads `PowerContext`, so callback payload layout and transient
/// events never become policy decisions by themselves.
pub(crate) struct PowerEventSubscription {
    registrations: Vec<*mut c_void>,
    _parameters: Box<DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS>,
    _context: Box<PowerEventContext>,
}

impl PowerEventSubscription {
    pub(crate) fn new() -> Result<(Self, Receiver<()>), PlatformError> {
        let (tx, rx) = mpsc::channel();
        let context = Box::new(PowerEventContext { tx });
        let context_ptr = (&*context as *const PowerEventContext).cast_mut().cast();
        let parameters = Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(power_event_callback),
            Context: context_ptr,
        });
        let recipient = HANDLE(
            (&*parameters as *const DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS)
                .cast_mut()
                .cast(),
        );
        let guids = [
            &GUID_ACDC_POWER_SOURCE,
            &GUID_BATTERY_PERCENTAGE_REMAINING,
            &GUID_ACTIVE_POWERSCHEME,
            &GUID_POWERSCHEME_PERSONALITY,
            &GUID_POWER_SAVING_STATUS,
        ];
        let mut registrations = Vec::with_capacity(guids.len());
        for guid in guids {
            let mut registration = null_mut();
            let code = unsafe {
                PowerSettingRegisterNotification(
                    guid,
                    DEVICE_NOTIFY_CALLBACK,
                    recipient,
                    &mut registration,
                )
            };
            if code != ERROR_SUCCESS {
                let error = PlatformError::Os(format!(
                    "PowerSettingRegisterNotification: Win32 {} (0x{:08X})",
                    code.0, code.0
                ));
                let subscription = Self {
                    registrations,
                    _parameters: parameters,
                    _context: context,
                };
                drop(subscription);
                return Err(error);
            }
            registrations.push(registration);
        }
        Ok((
            Self {
                registrations,
                _parameters: parameters,
                _context: context,
            },
            rx,
        ))
    }
}

impl Drop for PowerEventSubscription {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) {
            if !registration.is_null() {
                let code = unsafe {
                    PowerSettingUnregisterNotification(HPOWERNOTIFY(registration as isize))
                };
                if code != ERROR_SUCCESS {
                    tracing::debug!(code = code.0, "PowerSettingUnregisterNotification failed");
                }
            }
        }
    }
}

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
        let s = query_system_power_status()?;
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
        } else {
            self.degraded = None;
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
