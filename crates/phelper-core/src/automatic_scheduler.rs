//! Power-aware automatic OS scheduling.
//!
//! Phase 1 intentionally has one narrow policy: while the machine is on
//! battery, eligible user-session processes receive E-core CPU Sets and
//! EcoQoS.  The worker owns reconciliation and the shared OS-policy handle
//! owns baselines/restoration.  No hardware writes, global power-plan writes,
//! process priorities or hard affinity are performed here.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use phelper_domain::automatic::{
    AutomaticMode, AutomaticPhase, AutomaticSchedulerSnapshot, PowerContext, PowerSource,
};
use phelper_domain::os_policy::{
    CpuPlacement, OsPolicyOwner, OsPolicyTarget, OsSchedulingPolicy, ProcessInfo,
};
use tracing::{debug, info, warn};

use crate::os_policy::{AutomaticApplyResult, OsPolicyHandle};
use crate::platform::windows_power::{PowerEventSubscription, read_power_context};

const PROCESS_RECONCILE_INTERVAL: Duration = Duration::from_secs(2);
const POWER_REFRESH_INTERVAL: Duration = Duration::from_secs(10);
const POWER_CONFIRM_DELAY: Duration = Duration::from_millis(250);
const WORKER_POLL: Duration = Duration::from_millis(100);

#[derive(Debug)]
enum AutomaticMessage {
    SetMode(AutomaticMode),
    Refresh,
    Shutdown(mpsc::Sender<()>),
}

#[derive(Clone)]
struct ProcessIdentity {
    executable: String,
    creation_time: Option<u64>,
}

impl ProcessIdentity {
    fn from_process(process: &ProcessInfo) -> Option<Self> {
        Some(Self {
            executable: process.executable.clone()?,
            // Automatic mode must be able to restore every target.  A path
            // alone is not enough because Windows can reuse a PID between
            // two reconciliations.
            creation_time: Some(process.creation_time?),
        })
    }

    fn matches(&self, process: &ProcessInfo) -> bool {
        self.executable
            .eq_ignore_ascii_case(process.executable.as_deref().unwrap_or_default())
            && match (self.creation_time, process.creation_time) {
                (Some(expected), Some(actual)) => expected == actual,
                (Some(_), None) => false,
                _ => true,
            }
    }
}

/// Cheap, cloneable control/read handle for the automatic scheduler.
#[derive(Clone)]
pub struct AutomaticSchedulerHandle {
    to_worker: mpsc::Sender<AutomaticMessage>,
    state: Arc<Mutex<AutomaticSchedulerSnapshot>>,
}

impl AutomaticSchedulerHandle {
    /// Start an idle automatic scheduler around an OS-policy handle.  The
    /// worker performs no process writes until a mode is explicitly selected.
    pub fn start(os_policy: OsPolicyHandle) -> Self {
        let (to_worker, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(AutomaticSchedulerSnapshot::default()));
        let state_for_worker = Arc::clone(&state);
        std::thread::Builder::new()
            .name("automatic-scheduler".into())
            .spawn(move || automatic_worker(os_policy, rx, state_for_worker))
            .expect("spawn automatic-scheduler thread");
        Self { to_worker, state }
    }

    pub fn snapshot(&self) -> AutomaticSchedulerSnapshot {
        self.state
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_default()
    }

    pub fn set_mode(&self, mode: AutomaticMode) {
        let _ = self.to_worker.send(AutomaticMessage::SetMode(mode));
    }

    pub fn refresh(&self) {
        let _ = self.to_worker.send(AutomaticMessage::Refresh);
    }

    pub fn shutdown(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self
            .to_worker
            .send(AutomaticMessage::Shutdown(ack_tx))
            .is_ok()
        {
            // Restoration can touch many processes.  Returning after an
            // arbitrary timeout would let the CLI exit with OS policies
            // still owned by a worker that is about to be terminated.
            let _ = ack_rx.recv();
        }
    }
}

struct AutomaticWorker {
    os_policy: OsPolicyHandle,
    rx: mpsc::Receiver<AutomaticMessage>,
    state: Arc<Mutex<AutomaticSchedulerSnapshot>>,
    mode: AutomaticMode,
    phase: AutomaticPhase,
    power: Option<PowerContext>,
    stable_source: PowerSource,
    pending_source: Option<(PowerSource, Instant)>,
    managed: BTreeMap<u32, ProcessIdentity>,
    skipped_manual: u32,
    last_error: Option<String>,
    last_reconcile_at_epoch_ms: Option<u64>,
    next_power_refresh: Instant,
    next_process_reconcile: Option<Instant>,
    power_events: Option<mpsc::Receiver<()>>,
    _power_subscription: Option<PowerEventSubscription>,
}

fn automatic_worker(
    os_policy: OsPolicyHandle,
    rx: mpsc::Receiver<AutomaticMessage>,
    state: Arc<Mutex<AutomaticSchedulerSnapshot>>,
) {
    let (power_subscription, power_events) = match PowerEventSubscription::new() {
        Ok((subscription, events)) => (Some(subscription), Some(events)),
        Err(error) => {
            // Polling remains a valid fallback.  This is not a reason to
            // fail engine startup or to enable policy blindly.
            warn!(%error, "power notifications unavailable; using polling fallback");
            (None, None)
        }
    };

    let now = Instant::now();
    let mut worker = AutomaticWorker {
        os_policy,
        rx,
        state,
        mode: AutomaticMode::Off,
        phase: AutomaticPhase::Disabled,
        power: None,
        stable_source: PowerSource::Unknown,
        pending_source: None,
        managed: BTreeMap::new(),
        skipped_manual: 0,
        last_error: None,
        last_reconcile_at_epoch_ms: None,
        next_power_refresh: now,
        next_process_reconcile: None,
        power_events,
        _power_subscription: power_subscription,
    };

    worker.refresh_power(true);
    worker.publish();

    loop {
        worker.drain_power_events();
        let now = Instant::now();

        if now >= worker.next_power_refresh {
            worker.next_power_refresh = now + POWER_REFRESH_INTERVAL;
            worker.refresh_power(false);
        }

        if worker
            .pending_source
            .is_some_and(|(_, confirm_at)| now >= confirm_at)
        {
            worker.confirm_power_source();
        }

        if worker
            .next_process_reconcile
            .is_some_and(|reconcile_at| now >= reconcile_at)
        {
            worker.next_process_reconcile = Some(now + PROCESS_RECONCILE_INTERVAL);
            if worker.mode == AutomaticMode::BatteryEfficiency
                && worker.stable_source == PowerSource::Battery
            {
                worker.reconcile_processes();
            }
        }

        worker.publish();

        match worker.rx.recv_timeout(WORKER_POLL) {
            Ok(AutomaticMessage::SetMode(mode)) => worker.set_mode(mode),
            Ok(AutomaticMessage::Refresh) => {
                worker.refresh_power(true);
                worker.reconcile_if_needed();
            }
            Ok(AutomaticMessage::Shutdown(ack)) => {
                worker.restore_automatic();
                worker.mode = AutomaticMode::Off;
                worker.phase = if worker.last_error.is_some() {
                    AutomaticPhase::Error
                } else {
                    AutomaticPhase::Disabled
                };
                worker.publish();
                let _ = ack.send(());
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                worker.restore_automatic();
                return;
            }
        }
    }
}

impl AutomaticWorker {
    fn set_mode(&mut self, mode: AutomaticMode) {
        if self.mode == mode {
            if mode == AutomaticMode::Off {
                // "Off" is an idempotent restore command, not merely a UI
                // toggle.  A previous restore can fail after touching only
                // some targets; retry it even when the mode value did not
                // change.
                let restore_ok = self.restore_automatic();
                self.refresh_power(true);
                if !self.managed.is_empty() {
                    if self.last_error.is_none() {
                        self.last_error = Some(if restore_ok {
                            "仍有自动调度目标未恢复".into()
                        } else {
                            "自动调度目标恢复失败".into()
                        });
                    }
                    self.phase = AutomaticPhase::Error;
                } else {
                    self.last_error = None;
                    self.phase = AutomaticPhase::Disabled;
                }
                return;
            }
            self.refresh_power(true);
            self.reconcile_if_needed();
            return;
        }
        let restore_ok = (mode == AutomaticMode::Off).then(|| self.restore_automatic());
        self.mode = mode;
        self.pending_source = None;
        self.next_process_reconcile = None;
        if restore_ok != Some(false) {
            self.last_error = None;
        }
        self.phase = if mode == AutomaticMode::Off {
            AutomaticPhase::Disabled
        } else {
            AutomaticPhase::Waiting
        };
        self.refresh_power(true);
        if mode == AutomaticMode::Off && !self.managed.is_empty() {
            self.phase = AutomaticPhase::Error;
            if self.last_error.is_none() {
                self.last_error = Some("仍有自动调度目标未恢复".into());
            }
        }
        self.reconcile_if_needed();
        info!(?mode, "automatic scheduling mode changed");
    }

    fn refresh_power(&mut self, force_reconcile: bool) {
        match read_power_context() {
            Ok(context) => {
                let source = context.source;
                self.power = Some(context);
                self.last_error = None;

                // Leaving battery is fail-safe and immediate: there is no
                // reason to keep an efficiency override after AC is back.
                if matches!(source, PowerSource::Ac | PowerSource::Unknown)
                    && !self.managed.is_empty()
                {
                    self.restore_automatic();
                }

                if source == self.stable_source {
                    self.pending_source = None;
                } else if source != PowerSource::Unknown {
                    self.pending_source = Some((source, Instant::now() + POWER_CONFIRM_DELAY));
                }

                if force_reconcile && source == PowerSource::Battery {
                    self.stable_source = PowerSource::Battery;
                    self.pending_source = None;
                }
                self.reconcile_if_needed();
            }
            Err(error) => {
                self.power = None;
                self.last_error = Some(error.to_string());
                self.phase = if self.mode == AutomaticMode::Off {
                    AutomaticPhase::Disabled
                } else {
                    AutomaticPhase::Error
                };
                if !self.managed.is_empty() {
                    self.restore_automatic();
                }
            }
        }
    }

    fn drain_power_events(&mut self) {
        let mut changed = false;
        if let Some(events) = &self.power_events {
            while events.try_recv().is_ok() {
                changed = true;
            }
        }
        if changed {
            self.next_power_refresh = Instant::now() + POWER_REFRESH_INTERVAL;
            self.refresh_power(false);
        }
    }

    fn confirm_power_source(&mut self) {
        let Some((expected, _)) = self.pending_source else {
            return;
        };
        match read_power_context() {
            Ok(context) if context.source == expected => {
                self.power = Some(context);
                self.stable_source = expected;
                self.pending_source = None;
                self.last_error = None;
                self.reconcile_if_needed();
            }
            Ok(context) => {
                self.power = Some(context);
                self.pending_source = None;
            }
            Err(error) => {
                self.last_error = Some(error.to_string());
                self.pending_source = None;
            }
        }
    }

    fn reconcile_if_needed(&mut self) {
        if self.mode == AutomaticMode::Off {
            self.phase = if self.last_error.is_some() {
                AutomaticPhase::Error
            } else {
                AutomaticPhase::Disabled
            };
            return;
        }
        // A raw power notification is not yet a confirmed source transition.
        // Do not schedule a battery reconcile while the latest read disagrees
        // with the stable source; this also prevents an AC transition from
        // being briefly re-applied during the 250 ms confirmation window.
        if self
            .power
            .as_ref()
            .is_some_and(|power| power.source != self.stable_source)
        {
            self.phase = if self.last_error.is_some() {
                AutomaticPhase::Error
            } else {
                AutomaticPhase::Waiting
            };
            self.next_process_reconcile = None;
            return;
        }
        if self.stable_source != PowerSource::Battery {
            self.phase = if self.last_error.is_some() {
                AutomaticPhase::Error
            } else {
                AutomaticPhase::Waiting
            };
            self.next_process_reconcile = None;
            return;
        }
        self.next_process_reconcile = Some(Instant::now());
        self.phase = AutomaticPhase::Waiting;
    }

    fn reconcile_processes(&mut self) {
        self.phase = AutomaticPhase::Applying;
        self.skipped_manual = 0;
        self.last_error = None;
        let processes = match self.os_policy.list_processes() {
            Ok(processes) => processes,
            Err(error) => {
                self.last_error = Some(error.to_string());
                self.phase = AutomaticPhase::Error;
                return;
            }
        };
        let current_session = processes
            .iter()
            .find(|process| process.pid == std::process::id())
            .and_then(|process| process.session_id);
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
        let all_by_pid = processes
            .iter()
            .map(|process| (process.pid, process))
            .collect::<BTreeMap<_, _>>();
        let eligible = processes
            .iter()
            .filter(|process| is_eligible(process, current_session, &windir))
            .filter_map(|process| {
                ProcessIdentity::from_process(process).map(|identity| (process.pid, identity))
            })
            .collect::<BTreeMap<_, _>>();

        let active_automatic = self
            .os_policy
            .snapshot()
            .active
            .into_iter()
            .filter(|active| active.owner == OsPolicyOwner::Automatic)
            .filter_map(|active| match active.target {
                OsPolicyTarget::Process { pid } => Some(pid),
                OsPolicyTarget::Thread { .. } => None,
            })
            .collect::<BTreeSet<_>>();

        // Reconcile processes that exited, became ineligible, or reused a
        // PID.  Exited processes need no write; dropping their ledger avoids
        // the zombie-handle/retained-state class of bug seen in other tools.
        let managed = self.managed.clone();
        for (pid, identity) in managed {
            let target = OsPolicyTarget::Process { pid };
            let Some(process) = all_by_pid.get(&pid) else {
                self.os_policy.discard_automatic(target);
                self.managed.remove(&pid);
                continue;
            };
            if eligible
                .get(&pid)
                .is_some_and(|current| current.matches(process))
                && identity.matches(process)
                && active_automatic.contains(&pid)
            {
                continue;
            }
            if !identity.matches(process) {
                // PID reuse: never try to restore the new process with the
                // old baseline.  The old process is already gone.
                self.os_policy.discard_automatic(target);
            } else if let Err(error) = self.os_policy.restore_automatic(target) {
                self.last_error = Some(error.to_string());
                continue;
            }
            self.managed.remove(&pid);
        }

        let policy = battery_policy();
        let eligible_count = eligible.len();
        let mut target_failures = 0usize;
        for (pid, identity) in eligible {
            if self.managed.contains_key(&pid) {
                continue;
            }
            let target = OsPolicyTarget::Process { pid };
            match self.os_policy.apply_automatic(target, policy.clone()) {
                Ok(AutomaticApplyResult::Applied | AutomaticApplyResult::Unchanged) => {
                    self.managed.insert(pid, identity);
                }
                Ok(AutomaticApplyResult::SkippedManual) => {
                    self.skipped_manual = self.skipped_manual.saturating_add(1);
                }
                Err(error) => {
                    target_failures += 1;
                    debug!(pid, %error, "automatic process policy skipped");
                }
            }
        }
        // A protected process or a process that exits during the snapshot is
        // a normal per-target outcome.  It must not turn a scheduler that
        // successfully manages the rest of the session into a global Error.
        // Report Error only when every eligible target failed.
        if eligible_count > 0 && self.managed.is_empty() && target_failures > 0 {
            self.last_error = Some(format!(
                "没有可接管的进程（{target_failures} 个目标被系统拒绝或不支持）"
            ));
        }
        self.last_reconcile_at_epoch_ms = Some(epoch_ms());
        self.phase = if self.last_error.is_some() {
            AutomaticPhase::Error
        } else {
            AutomaticPhase::Active
        };
    }

    fn restore_automatic(&mut self) -> bool {
        let result = self.os_policy.restore_automatic_all();
        let remaining = self
            .os_policy
            .snapshot()
            .active
            .into_iter()
            .filter(|active| active.owner == OsPolicyOwner::Automatic)
            .filter_map(|active| match active.target {
                OsPolicyTarget::Process { pid } => Some(pid),
                OsPolicyTarget::Thread { .. } => None,
            })
            .collect::<BTreeSet<_>>();
        self.managed.retain(|pid, _| remaining.contains(pid));
        match result {
            Ok(()) if remaining.is_empty() => {
                self.last_error = None;
                true
            }
            Ok(()) => {
                self.last_error = Some("仍有自动调度目标未恢复".into());
                self.phase = AutomaticPhase::Error;
                false
            }
            Err(error) => {
                self.last_error = Some(error.to_string());
                self.phase = AutomaticPhase::Error;
                false
            }
        }
    }

    fn publish(&self) {
        let Ok(mut snapshot) = self.state.lock() else {
            return;
        };
        snapshot.mode = self.mode;
        snapshot.phase = self.phase;
        snapshot.power = self.power.clone();
        snapshot.managed_processes = self.managed.len() as u32;
        snapshot.skipped_manual = self.skipped_manual;
        snapshot.last_reconcile_at_epoch_ms = self.last_reconcile_at_epoch_ms;
        snapshot.last_error = self.last_error.clone();
    }
}

fn battery_policy() -> OsSchedulingPolicy {
    OsSchedulingPolicy {
        cpu_placement: Some(CpuPlacement::Efficiency),
        qos: Some(phelper_domain::os_policy::QosLevel::Eco),
        ..Default::default()
    }
}

fn is_eligible(process: &ProcessInfo, current_session: Option<u32>, windir: &str) -> bool {
    if process.pid == 0 || process.pid == std::process::id() {
        return false;
    }
    let Some(session) = process.session_id else {
        return false;
    };
    if current_session != Some(session) {
        return false;
    }
    let Some(path) = process.executable.as_deref() else {
        return false;
    };
    let normalized_path = path.to_ascii_lowercase().replace('/', "\\");
    let normalized_windir = windir
        .to_ascii_lowercase()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_string();
    if normalized_path == normalized_windir
        || normalized_path.starts_with(&format!("{normalized_windir}\\"))
    {
        return false;
    }
    let name = process.name.to_ascii_lowercase();
    !matches!(
        name.as_str(),
        "phelper-desktop.exe"
            | "phelper-cli.exe"
            | "phelper.exe"
            | "explorer.exe"
            | "dwm.exe"
            | "audiodg.exe"
            | "csrss.exe"
            | "wininit.exe"
            | "winlogon.exe"
            | "services.exe"
            | "lsass.exe"
            | "smss.exe"
            | "svchost.exe"
            | "fontdrvhost.exe"
            | "runtimebroker.exe"
            | "sihost.exe"
            | "searchhost.exe"
            | "startmenuexperiencehost.exe"
            | "shellexperiencehost.exe"
            | "textinputhost.exe"
            | "ctfmon.exe"
            | "conhost.exe"
            | "msmpeng.exe"
            | "wmiprvse.exe"
    )
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(name: &str, path: Option<&str>, session_id: Option<u32>) -> ProcessInfo {
        ProcessInfo {
            pid: 40_000,
            name: name.into(),
            executable: path.map(str::to_string),
            thread_count: 1,
            session_id,
            creation_time: Some(123),
        }
    }

    #[test]
    fn battery_policy_only_contains_soft_efficiency_hints() {
        let policy = battery_policy();
        assert_eq!(
            policy.cpu_placement,
            Some(CpuPlacement::Efficiency),
            "BatteryEfficiency must use E-core CPU Sets"
        );
        assert_eq!(
            policy.qos,
            Some(phelper_domain::os_policy::QosLevel::Eco),
            "BatteryEfficiency must use EcoQoS"
        );
        assert!(policy.affinity.is_none());
        assert!(policy.process_priority.is_none());
        assert!(policy.memory_priority.is_none());
    }

    #[test]
    fn eligibility_requires_current_session_and_readable_non_system_path() {
        let allowed = process(
            "editor.exe",
            Some(r"C:\Program Files\Editor\editor.exe"),
            Some(1),
        );
        assert!(is_eligible(&allowed, Some(1), r"C:\Windows"));

        let other_session = process(
            "editor.exe",
            Some(r"C:\Program Files\Editor\editor.exe"),
            Some(0),
        );
        assert!(!is_eligible(&other_session, Some(1), r"C:\Windows"));

        let system_path = process("worker.exe", Some(r"C:\Windows\worker.exe"), Some(1));
        assert!(!is_eligible(&system_path, Some(1), r"C:\Windows"));

        let no_path = process("worker.exe", None, Some(1));
        assert!(!is_eligible(&no_path, Some(1), r"C:\Windows"));
    }

    #[test]
    fn critical_processes_are_not_automatic_candidates() {
        for name in ["explorer.exe", "audiodg.exe", "svchost.exe", "dwm.exe"] {
            assert!(!is_eligible(
                &process(name, Some(r"C:\Program Files\x.exe"), Some(1)),
                Some(1),
                r"C:\Windows"
            ));
        }
    }

    #[test]
    fn process_identity_rejects_creation_time_change() {
        let identity = ProcessIdentity {
            executable: r"C:\Tools\worker.exe".into(),
            creation_time: Some(123),
        };
        let mut replacement = process("worker.exe", Some(r"C:\Tools\worker.exe"), Some(1));
        assert!(identity.matches(&replacement));
        replacement.creation_time = Some(456);
        assert!(!identity.matches(&replacement));
    }
}
