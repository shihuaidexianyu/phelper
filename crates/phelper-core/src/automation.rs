//! Application/power rules own reversible coordinator sessions. They never
//! call hardware backends or compete with manual requests.
use crate::{control::ControlHandle, os_policy::OsPolicyHandle};
use phelper_domain::{
    automatic::PowerSource,
    command::{ControlCommand, ControlOutcome, ControlStatus, Verification},
    os_policy::ProcessInfo,
};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutomationConfig {
    pub enabled: bool,
    pub battery_efficiency: bool,
    pub ac_profile: Option<String>,
    pub battery_profile: Option<String>,
    pub applications: Vec<ApplicationRule>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationRule {
    /// Exact executable path, compared case-insensitively on Windows.
    pub executable: String,
    pub profile: String,
    #[serde(default)]
    pub power: Option<PowerSource>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AutomationSnapshot {
    pub initialized: bool,
    pub config: AutomationConfig,
    pub paused_by_manual: bool,
    pub active_profile: Option<String>,
    pub power: PowerSource,
    pub message: Option<String>,
    pub process_policy: phelper_domain::automatic::AutomaticSchedulerSnapshot,
}

impl AutomationConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.applications.len() > 64 {
            return Err("应用规则最多 64 条".into());
        }
        let registry = crate::profiles::ProfileRegistry::load_default();
        for name in self
            .ac_profile
            .iter()
            .chain(self.battery_profile.iter())
            .chain(self.applications.iter().map(|r| &r.profile))
        {
            if registry.get(name).is_none() {
                return Err(format!("配置档不存在：{name}"));
            }
        }
        for rule in &self.applications {
            if !Path::new(&rule.executable).is_absolute()
                || !rule.executable.to_ascii_lowercase().ends_with(".exe")
            {
                return Err("应用规则需要完整的 .exe 路径".into());
            }
        }
        Ok(())
    }

    fn resolve(&self, power: PowerSource, processes: &[ProcessInfo]) -> Option<String> {
        if !self.enabled || power == PowerSource::Unknown {
            return None;
        }
        // Explicit row order is priority. A rule stays active while its
        // process exists, including when a game temporarily loses focus.
        for rule in &self.applications {
            if rule.power.is_some_and(|p| p != power) {
                continue;
            }
            if processes.iter().any(|p| {
                p.creation_time.is_some()
                    && p.executable.as_ref().is_some_and(|path| {
                        path.replace('/', "\\")
                            .eq_ignore_ascii_case(&rule.executable.replace('/', "\\"))
                    })
            }) {
                return Some(rule.profile.clone());
            }
        }
        match power {
            PowerSource::Ac => self.ac_profile.clone(),
            PowerSource::Battery => self.battery_profile.clone(),
            PowerSource::Unknown => None,
        }
    }
}

pub fn config_path() -> std::path::PathBuf {
    crate::persistence::data_dir().join("automation.toml")
}
pub fn load_config() -> Result<AutomationConfig, String> {
    match std::fs::read_to_string(config_path()) {
        Ok(text) => toml::from_str(&text).map_err(|e| format!("自动规则配置错误：{e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(e.to_string()),
    }
}

enum Message {
    Configure(AutomationConfig),
    Shutdown(mpsc::Sender<()>),
}
#[derive(Clone)]
pub struct AutomationHandle {
    tx: mpsc::Sender<Message>,
    state: Arc<Mutex<AutomationSnapshot>>,
}

impl AutomationHandle {
    pub fn start(control: ControlHandle) -> Self {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(AutomationSnapshot::default()));
        let shared = Arc::clone(&state);
        std::thread::Builder::new()
            .name("profile-automation".into())
            .spawn(move || worker(control, rx, shared))
            .expect("automation worker");
        Self { tx, state }
    }
    pub fn snapshot(&self) -> AutomationSnapshot {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn configure(&self, config: AutomationConfig) {
        let _ = self.tx.send(Message::Configure(config));
    }
    pub fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Message::Shutdown(tx)).is_ok() {
            let _ = rx.recv();
        }
    }
}

#[derive(Clone)]
enum Transition {
    Begin(u64, String),
    End(u64),
}
fn succeeded(outcome: &ControlOutcome) -> bool {
    matches!(&outcome.status, ControlStatus::Applied { verification } if !matches!(verification, Verification::Failed { .. }))
}

fn worker(
    control: ControlHandle,
    rx: mpsc::Receiver<Message>,
    shared: Arc<Mutex<AutomationSnapshot>>,
) {
    let mut state = AutomationSnapshot {
        initialized: true,
        ..Default::default()
    };
    match load_config().and_then(|config| {
        config.validate()?;
        Ok(config)
    }) {
        Ok(config) => state.config = config,
        Err(e) => state.message = Some(e),
    }
    let mut recovery_failed = false;
    let mut active: Option<(u64, String)> = None;
    let mut pending: Option<(Transition, mpsc::Receiver<ControlOutcome>)> = None;
    let mut generation = control.manual_generation();
    let mut next_id = 1;
    let os = OsPolicyHandle::new();
    let scheduler = crate::automatic_scheduler::AutomaticSchedulerHandle::start(os.clone());
    let mut candidate: Option<String> = None;
    let mut candidate_since = Instant::now();
    let mut next_scan = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Message::Configure(config)) => {
                let result = config
                    .validate()
                    .and_then(|()| toml::to_string_pretty(&config).map_err(|e| e.to_string()))
                    .and_then(|text| {
                        crate::persistence::write_atomic(&config_path(), &text)
                            .map_err(|e| e.to_string())
                    });
                match result {
                    Ok(()) => {
                        state.config = config;
                        recovery_failed = false;
                        state.paused_by_manual = false;
                        state.message = None;
                        generation = control.manual_generation();
                        next_scan = Instant::now();
                    }
                    Err(e) => state.message = Some(e),
                }
            }
            Ok(Message::Shutdown(ack)) => {
                scheduler.shutdown();
                // A queued restore follows any in-flight begin in the same
                // single writer. Its session ID prevents stale restoration.
                let id = active.as_ref().map(|(id, _)| *id).or_else(|| {
                    pending.as_ref().map(|(t, _)| match t {
                        Transition::Begin(id, _) | Transition::End(id) => *id,
                    })
                });
                if let Some(session_id) = id {
                    let _ = control.dispatch_blocking(
                        ControlCommand::RestoreScopedProfile { session_id },
                        Duration::from_secs(60),
                    );
                }
                let _ = ack.send(());
                return;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                scheduler.shutdown();
                if let Some((session_id, _)) = active {
                    let _ = control.dispatch(ControlCommand::RestoreScopedProfile { session_id });
                }
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let current_generation = control.manual_generation();
        if current_generation != generation {
            generation = current_generation;
            state.paused_by_manual = true;
            state.message = Some("手动设置已接管；点击启用可恢复自动规则。".into());
        }
        let scheduler_mode =
            if state.config.enabled && state.config.battery_efficiency && !state.paused_by_manual {
                phelper_domain::automatic::AutomaticMode::BatteryEfficiency
            } else {
                phelper_domain::automatic::AutomaticMode::Off
            };
        if scheduler.snapshot().mode != scheduler_mode {
            scheduler.set_mode(scheduler_mode);
        }
        state.process_policy = scheduler.snapshot();
        if let Some((transition, result_rx)) = pending.take() {
            match result_rx.try_recv() {
                Ok(outcome) => {
                    let ok = succeeded(&outcome);
                    match transition {
                        Transition::Begin(id, profile) => {
                            active = Some((id, profile));
                            if !ok {
                                state.paused_by_manual = true;
                            }
                        }
                        Transition::End(_) => {
                            if ok {
                                active = None;
                            } else {
                                state.paused_by_manual = true;
                                recovery_failed = true;
                            }
                        }
                    }
                    if !ok {
                        state.message = Some(format!(
                            "自动切换未完成：{:?}；请检查诊断并恢复。",
                            outcome.status
                        ));
                    }
                }
                Err(mpsc::TryRecvError::Empty) => pending = Some((transition, result_rx)),
                Err(mpsc::TryRecvError::Disconnected) => {
                    state.paused_by_manual = true;
                    state.message = Some("自动控制通道断开".into());
                }
            }
        }
        if Instant::now() >= next_scan
            && pending.is_none()
            && !recovery_failed
            && (state.config.enabled || active.is_some())
        {
            next_scan = Instant::now() + Duration::from_secs(1);
            state.power = crate::platform::windows_power::read_power_context()
                .map(|p| p.source)
                .unwrap_or(PowerSource::Unknown);
            let processes = if state.config.applications.is_empty() {
                Ok(Vec::new())
            } else {
                os.list_processes()
            };
            let selected = if state.paused_by_manual {
                None
            } else {
                processes
                    .as_ref()
                    .ok()
                    .and_then(|p| state.config.resolve(state.power, p))
            };
            if selected != candidate {
                candidate = selected;
                candidate_since = Instant::now();
            }
            // Confirm changing power/process context before changing hardware.
            if candidate_since.elapsed() >= Duration::from_secs(1) {
                let target = candidate.as_ref();
                let transition = match &active {
                    Some((id, profile)) if Some(profile) != target => Some(Transition::End(*id)),
                    None => target.map(|profile| {
                        let id = next_id;
                        next_id += 1;
                        Transition::Begin(id, profile.clone())
                    }),
                    _ => None,
                };
                if let Some(transition) = transition {
                    let command = match &transition {
                        Transition::Begin(session_id, profile) => {
                            ControlCommand::ApplyScopedProfile {
                                profile: profile.clone(),
                                session_id: *session_id,
                            }
                        }
                        Transition::End(session_id) => ControlCommand::RestoreScopedProfile {
                            session_id: *session_id,
                        },
                    };
                    match control.dispatch(command) {
                        Ok((_, result_rx)) => pending = Some((transition, result_rx)),
                        Err(e) => {
                            state.message = Some(e.to_string());
                            state.paused_by_manual = true;
                        }
                    }
                }
            }
        }
        state.active_profile = active.as_ref().map(|(_, p)| p.clone());
        *shared.lock().unwrap_or_else(|e| e.into_inner()) = state.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_paths_and_row_priority_require_process_identity() {
        let config = AutomationConfig {
            enabled: true,
            ac_profile: Some("balanced".into()),
            applications: vec![
                ApplicationRule {
                    executable: "C:\\Games\\a.exe".into(),
                    profile: "gaming".into(),
                    power: None,
                },
                ApplicationRule {
                    executable: "C:\\Games\\b.exe".into(),
                    profile: "silent".into(),
                    power: None,
                },
            ],
            ..Default::default()
        };
        let mut process = ProcessInfo {
            pid: 42,
            name: "a.exe".into(),
            executable: Some("C:/Games/A.exe".into()),
            thread_count: 1,
            session_id: Some(1),
            creation_time: Some(5),
        };
        assert_eq!(
            config
                .resolve(PowerSource::Ac, &[process.clone()])
                .as_deref(),
            Some("gaming")
        );
        let second = ProcessInfo {
            executable: Some("C:\\Games\\b.exe".into()),
            ..process.clone()
        };
        assert_eq!(
            config
                .resolve(PowerSource::Ac, &[second, process.clone()])
                .as_deref(),
            Some("gaming")
        );
        process.creation_time = None;
        assert_eq!(
            config
                .resolve(PowerSource::Ac, &[process.clone()])
                .as_deref(),
            Some("balanced")
        );
        process.creation_time = Some(9);
        process.executable = Some("C:\\Other\\a.exe".into());
        assert_eq!(
            config.resolve(PowerSource::Ac, &[process]).as_deref(),
            Some("balanced")
        );
    }
    #[test]
    fn unknown_power_and_disabled_rules_do_not_apply() {
        let config = AutomationConfig {
            enabled: true,
            ac_profile: Some("gaming".into()),
            ..Default::default()
        };
        assert_eq!(config.resolve(PowerSource::Unknown, &[]), None);
        assert_eq!(config.resolve(PowerSource::Ac, &[]), Some("gaming".into()));
        assert_eq!(
            AutomationConfig {
                enabled: false,
                ..config
            }
            .resolve(PowerSource::Ac, &[]),
            None
        );
    }
}
