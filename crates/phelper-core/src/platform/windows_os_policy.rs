//! Windows OS-level scheduling adapter.
//!
//! The hardware coordinator remains the single writer for HP/EC state.  This
//! adapter owns a separate, process/thread-scoped writer for Windows policy:
//! CPU Sets, legacy affinity, QoS, priorities, ideal processor and the
//! per-executable graphics preference.  Every mutation captures a baseline
//! once and restores it on explicit restore or engine shutdown.

use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::ptr::{null_mut, read_unaligned};
use std::sync::{Arc, Mutex};

use phelper_domain::error::PlatformError;
use phelper_domain::os_policy::{
    ActiveOsPolicy, AffinityMask, CpuPlacement, CpuSetInfo, CpuTopology, GpuPreference,
    MemoryPriority, OsPolicyApplyResult, OsPolicyOwner, OsPolicySnapshot, OsPolicyTarget,
    OsSchedulingPolicy, ProcessInfo, ProcessPriority, ProcessorRef, QosLevel, ThreadPriority,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_CALL_NOT_IMPLEMENTED, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER,
    FILETIME, GetLastError, HANDLE, WIN32_ERROR,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Kernel::PROCESSOR_NUMBER;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, KEY_WRITE, REG_NONE,
    REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW,
    RegQueryValueExW, RegSetValueExW,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::SystemInformation::{
    CpuSetInformation, GROUP_AFFINITY, GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION,
    SYSTEM_CPU_SET_INFORMATION_0_0,
};
use windows::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetPriorityClass,
    GetProcessAffinityMask, GetProcessDefaultCpuSets, GetProcessGroupAffinity,
    GetProcessIdOfThread, GetProcessInformation, GetProcessTimes, GetThreadGroupAffinity,
    GetThreadIdealProcessorEx, GetThreadInformation, GetThreadPriority, GetThreadSelectedCpuSets,
    HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, MEMORY_PRIORITY, MEMORY_PRIORITY_INFORMATION,
    NORMAL_PRIORITY_CLASS, OpenProcess, OpenThread, PROCESS_ACCESS_RIGHTS, PROCESS_CREATION_FLAGS,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_STATE, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_SET_QUOTA, ProcessMemoryPriority, ProcessPowerThrottling, SetPriorityClass,
    SetProcessAffinityMask, SetProcessDefaultCpuSets, SetProcessInformation,
    SetThreadGroupAffinity, SetThreadIdealProcessorEx, SetThreadInformation, SetThreadPriority,
    SetThreadSelectedCpuSets, THREAD_ACCESS_RIGHTS, THREAD_PRIORITY, THREAD_PRIORITY_ABOVE_NORMAL,
    THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_IDLE,
    THREAD_PRIORITY_LOWEST, THREAD_PRIORITY_NORMAL, THREAD_QUERY_LIMITED_INFORMATION,
    THREAD_SET_INFORMATION, THREAD_SET_LIMITED_INFORMATION, ThreadMemoryPriority,
    ThreadPowerThrottling,
};
use windows::core::{PCWSTR, PWSTR, w};

const GPU_PREF_KEY: PCWSTR = w!("Software\\Microsoft\\DirectX\\UserGpuPreferences");
const GPU_PREF_POWER_SAVING: &str = "GpuPreference=1;";
const GPU_PREF_HIGH_PERFORMANCE: &str = "GpuPreference=2;";

#[derive(Clone)]
struct Captured<T> {
    available: bool,
    value: Option<T>,
}

impl<T> Captured<T> {
    fn known(value: Option<T>) -> Self {
        Self {
            available: true,
            value,
        }
    }

    fn unavailable() -> Self {
        Self {
            available: false,
            value: None,
        }
    }
}

#[derive(Clone)]
struct ProcessBaseline {
    executable: Option<String>,
    creation_time: Option<u64>,
    affinity: Captured<AffinityMask>,
    cpu_sets: Captured<Vec<u32>>,
    qos: Captured<PROCESS_POWER_THROTTLING_STATE>,
    priority: Captured<u32>,
    memory_priority: Captured<u32>,
    gpu_preference: Captured<String>,
}

#[derive(Clone)]
struct ThreadBaseline {
    owner_pid: u32,
    owner_executable: Option<String>,
    owner_creation_time: Option<u64>,
    group_affinity: Captured<GROUP_AFFINITY>,
    cpu_sets: Captured<Vec<u32>>,
    qos: Captured<PROCESS_POWER_THROTTLING_STATE>,
    priority: Captured<i32>,
    memory_priority: Captured<u32>,
    ideal_processor: Captured<ProcessorRef>,
}

#[derive(Clone)]
enum Baseline {
    Process(ProcessBaseline),
    Thread(ThreadBaseline),
}

/// Result of an automatic apply attempt.  A manual policy always wins for a
/// target, so the automatic worker treats `SkippedManual` as an expected
/// outcome rather than an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomaticApplyResult {
    Applied,
    Unchanged,
    SkippedManual,
}

enum ApplyOwnedResult {
    Applied(OsPolicyApplyResult),
    Unchanged,
    SkippedManual,
}

struct Inner {
    topology: Option<CpuTopology>,
    baselines: HashMap<OsPolicyTarget, Baseline>,
    active: HashMap<OsPolicyTarget, ActiveOsPolicy>,
    /// GPU preference is a per-executable registry value, not a per-PID
    /// value.  Keep one original value for all phelper owners of an EXE.
    gpu_baselines: HashMap<String, Option<String>>,
    /// Targets whose last write could not be rolled back completely.  Keep
    /// them visible and retryable instead of dropping the only restore
    /// baseline after a partial Win32 failure.
    recovery_pending: HashSet<OsPolicyTarget>,
}

/// Cheap, cloneable handle.  The Windows calls are made by the caller's
/// thread; `new` performs no topology or process enumeration so it is safe to
/// put on the desktop startup path.
#[derive(Clone)]
pub struct OsPolicyHandle {
    inner: Arc<Mutex<Inner>>,
}

impl Default for OsPolicyHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl OsPolicyHandle {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                topology: None,
                baselines: HashMap::new(),
                active: HashMap::new(),
                gpu_baselines: HashMap::new(),
                recovery_pending: HashSet::new(),
            })),
        }
    }

    /// Lazy topology query.  This is deliberately not part of construction:
    /// the first screen can render before Windows CPU-set discovery finishes.
    pub fn topology(&self) -> Result<CpuTopology, PlatformError> {
        let mut inner = self.lock()?;
        if let Some(topology) = &inner.topology {
            return Ok(topology.clone());
        }
        let topology = query_topology()?;
        inner.topology = Some(topology.clone());
        Ok(topology)
    }

    /// Snapshot of policies currently owned by phelper.  It does not trigger
    /// a topology query, keeping the normal app-state tick cheap.
    pub fn snapshot(&self) -> OsPolicySnapshot {
        let Ok(inner) = self.inner.lock() else {
            return OsPolicySnapshot::default();
        };
        let mut active = inner.active.values().cloned().collect::<Vec<_>>();
        active.sort_by_key(|item| match item.target {
            OsPolicyTarget::Process { pid } => (0u8, pid),
            OsPolicyTarget::Thread { tid } => (1u8, tid),
        });
        OsPolicySnapshot {
            topology: inner.topology.clone(),
            active,
        }
    }

    /// A lightweight process picker source.  Full image paths are best effort
    /// because protected/system processes may refuse even limited query
    /// access; applying a policy still fails closed when an identity is
    /// required for a safe restore.
    pub fn list_processes(&self) -> Result<Vec<ProcessInfo>, PlatformError> {
        query_processes()
    }

    /// Apply a policy to one running process or thread.  The baseline is
    /// captured only once per target and remains until restore.
    pub fn apply(
        &self,
        target: OsPolicyTarget,
        policy: OsSchedulingPolicy,
    ) -> Result<OsPolicyApplyResult, PlatformError> {
        // A user action is an explicit override.  If the automatic worker
        // currently owns this target, restore its captured baseline first so
        // the new manual baseline starts from the real pre-phelper state.
        if self.is_owned_by(target, OsPolicyOwner::Automatic)? {
            self.restore_automatic(target)?;
        }
        match self.apply_owned(target, policy, OsPolicyOwner::Manual)? {
            ApplyOwnedResult::Applied(result) => Ok(result),
            ApplyOwnedResult::Unchanged | ApplyOwnedResult::SkippedManual => {
                unreachable!("manual apply cannot be unchanged or skipped")
            }
        }
    }

    /// Apply the automatic policy without taking over a manually controlled
    /// target.  The baseline is shared with manual writes, preventing two
    /// independent restore ledgers from fighting over the same process.
    pub fn apply_automatic(
        &self,
        target: OsPolicyTarget,
        policy: OsSchedulingPolicy,
    ) -> Result<AutomaticApplyResult, PlatformError> {
        match self.apply_owned(target, policy, OsPolicyOwner::Automatic)? {
            ApplyOwnedResult::Applied(_) => Ok(AutomaticApplyResult::Applied),
            ApplyOwnedResult::Unchanged => Ok(AutomaticApplyResult::Unchanged),
            ApplyOwnedResult::SkippedManual => Ok(AutomaticApplyResult::SkippedManual),
        }
    }

    fn apply_owned(
        &self,
        target: OsPolicyTarget,
        policy: OsSchedulingPolicy,
        owner: OsPolicyOwner,
    ) -> Result<ApplyOwnedResult, PlatformError> {
        policy.validate_for(&target).map_err(PlatformError::Os)?;
        if matches!(target, OsPolicyTarget::Process { pid } if pid == std::process::id()) {
            return Err(PlatformError::Os("拒绝修改 phelper 自身的调度策略".into()));
        }

        let mut inner = self.lock()?;
        let mut unchanged = false;
        if owner == OsPolicyOwner::Automatic
            && let Some(existing) = inner.active.get(&target)
        {
            if existing.owner == OsPolicyOwner::Manual {
                return Ok(ApplyOwnedResult::SkippedManual);
            }
            unchanged = existing.policy == policy && !inner.recovery_pending.contains(&target);
        }
        let cpu_sets = resolve_cpu_placement(&mut inner, policy.cpu_placement.as_ref())?;
        validate_ideal_processor(&mut inner, policy.ideal_processor)?;
        let mut before = capture_target(target)?;
        // Keep the exact live value for a failed-request rollback, but use a
        // shared per-executable baseline for the long-lived ledger.  The
        // DirectX preference is global to an image path, so keying it by PID
        // lets two processes overwrite each other's restore value.
        let rollback_before = before.clone();
        let gpu_key = policy
            .gpu_preference
            .is_some()
            .then(|| target_identity(&before).map(|path| gpu_path_key(&path)))
            .flatten();
        if let Some(key) = &gpu_key
            && let Some(shared) = inner.gpu_baselines.get(key).cloned()
            && let Baseline::Process(b) = &mut before
        {
            b.gpu_preference = Captured::known(shared);
        }

        if let Some(existing) = inner.baselines.get(&target)
            && !same_target_identity(existing, &before)
        {
            return Err(PlatformError::Os(
                "目标 PID/TID 已被另一个可执行文件复用，拒绝覆盖原策略".into(),
            ));
        }
        if unchanged {
            return Ok(ApplyOwnedResult::Unchanged);
        }
        require_capture_for_policy(&before, &policy)?;

        let had_baseline = inner.baselines.contains_key(&target);
        if !had_baseline {
            inner.baselines.insert(target, before.clone());
        }
        let gpu_ledger_new = if let Some(key) = &gpu_key {
            if inner.gpu_baselines.contains_key(key) {
                false
            } else if let Baseline::Process(b) = &before {
                inner
                    .gpu_baselines
                    .insert(key.clone(), b.gpu_preference.value.clone());
                true
            } else {
                false
            }
        } else {
            false
        };

        if let Err(error) = apply_target(target, &policy, cpu_sets.as_deref()) {
            // Roll back this request to the state observed immediately before
            // it.  A failed rollback is itself actionable state: keep the
            // baseline and expose a retryable active entry instead of
            // pretending that the target is clean.
            if let Err(rollback_error) = restore_captured_target(target, &rollback_before) {
                let executable = target_identity(&before);
                inner.active.insert(
                    target,
                    ActiveOsPolicy {
                        target,
                        executable: executable.clone(),
                        policy: policy.clone(),
                        gpu_requires_restart: policy.gpu_preference.is_some(),
                        owner,
                        creation_time: creation_time(&before),
                    },
                );
                inner.recovery_pending.insert(target);
                return Err(PlatformError::Os(format!(
                    "{error}; 回滚失败，已保留恢复任务：{rollback_error}"
                )));
            }
            if !had_baseline {
                inner.baselines.remove(&target);
            }
            if gpu_ledger_new {
                if let Some(key) = &gpu_key {
                    inner.gpu_baselines.remove(key);
                }
            }
            inner.recovery_pending.remove(&target);
            return Err(error);
        }

        let executable = target_identity(&before);
        let gpu_requires_restart = policy.gpu_preference.is_some();
        inner.active.insert(
            target,
            ActiveOsPolicy {
                target,
                executable: executable.clone(),
                policy,
                gpu_requires_restart,
                owner,
                creation_time: creation_time(&before),
            },
        );
        inner.recovery_pending.remove(&target);
        Ok(ApplyOwnedResult::Applied(OsPolicyApplyResult {
            target,
            executable,
            gpu_requires_restart,
        }))
    }

    /// Restore one manually-owned target.  Automatic targets are controlled
    /// by the automatic worker and are restored through the owner-specific
    /// method below.
    pub fn restore(&self, target: OsPolicyTarget) -> Result<bool, PlatformError> {
        self.restore_owned(target, Some(OsPolicyOwner::Manual))
    }

    pub fn restore_automatic(&self, target: OsPolicyTarget) -> Result<bool, PlatformError> {
        self.restore_owned(target, Some(OsPolicyOwner::Automatic))
    }

    fn restore_owned(
        &self,
        target: OsPolicyTarget,
        required_owner: Option<OsPolicyOwner>,
    ) -> Result<bool, PlatformError> {
        let mut inner = self.lock()?;
        if let Some(required_owner) = required_owner
            && inner.active.get(&target).map(|active| active.owner) != Some(required_owner)
        {
            return Ok(false);
        }
        let Some(baseline) = inner.baselines.get(&target).cloned() else {
            return Ok(false);
        };
        let owns_gpu = inner
            .active
            .get(&target)
            .is_some_and(|active| active.policy.gpu_preference.is_some());
        let gpu_key = owns_gpu
            .then(|| gpu_path_from_baseline(&baseline))
            .flatten();
        let restore_gpu = gpu_key
            .as_deref()
            .is_none_or(|key| !has_other_gpu_owner(&inner, target, key));
        restore_baseline(target, &baseline, restore_gpu)?;
        inner.baselines.remove(&target);
        inner.active.remove(&target);
        inner.recovery_pending.remove(&target);
        if restore_gpu && let Some(key) = gpu_key {
            inner.gpu_baselines.remove(&key);
        }
        Ok(true)
    }

    /// Restore every automatic target.  The caller can then restore manual
    /// targets separately without allowing an automatic failure to mask them.
    pub fn restore_automatic_all(&self) -> Result<(), PlatformError> {
        let targets = self
            .inner
            .lock()
            .map_err(|_| PlatformError::Os("OS policy lock poisoned".into()))?
            .active
            .values()
            .filter(|active| active.owner == OsPolicyOwner::Automatic)
            .map(|active| active.target)
            .collect::<Vec<_>>();
        let mut first_error = None;
        for target in targets {
            if let Err(error) = self.restore_automatic(target) {
                tracing::warn!(?target, %error, "automatic OS policy restore failed");
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Drop an automatic ledger entry only after the caller has observed that
    /// the target process exited.  No write is attempted because there is no
    /// live target left to restore.
    pub fn discard_automatic(&self, target: OsPolicyTarget) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        if inner.active.get(&target).map(|active| active.owner) != Some(OsPolicyOwner::Automatic) {
            return false;
        }
        inner.active.remove(&target);
        inner.baselines.remove(&target);
        inner.recovery_pending.remove(&target);
        true
    }

    /// Best-effort restore of every target touched by this handle.  One
    /// failure does not prevent the other targets from being restored.
    pub fn restore_all(&self) -> Result<(), PlatformError> {
        let targets = self
            .inner
            .lock()
            .map_err(|_| PlatformError::Os("OS policy lock poisoned".into()))?
            .baselines
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let mut first_error = None;
        for target in targets {
            if let Err(error) = self.restore_owned(target, None) {
                tracing::warn!(?target, %error, "OS policy restore failed");
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn is_owned_by(
        &self,
        target: OsPolicyTarget,
        owner: OsPolicyOwner,
    ) -> Result<bool, PlatformError> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| PlatformError::Os("OS policy lock poisoned".into()))?
            .active
            .get(&target)
            .is_some_and(|active| active.owner == owner))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, PlatformError> {
        self.inner
            .lock()
            .map_err(|_| PlatformError::Os("OS policy lock poisoned".into()))
    }
}

fn process_access() -> PROCESS_ACCESS_RIGHTS {
    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_INFORMATION | PROCESS_SET_QUOTA
}

fn thread_access() -> THREAD_ACCESS_RIGHTS {
    THREAD_QUERY_LIMITED_INFORMATION | THREAD_SET_INFORMATION | THREAD_SET_LIMITED_INFORMATION
}

fn os_error(operation: &str, error: windows::core::Error) -> PlatformError {
    PlatformError::Os(format!(
        "{operation}: {error} (0x{:08X})",
        error.code().0 as u32
    ))
}

fn last_error(operation: &str) -> PlatformError {
    let code = unsafe { GetLastError() };
    PlatformError::Os(format!("{operation}: Win32 {} (0x{:08X})", code.0, code.0))
}

fn close(handle: HANDLE) {
    let _ = unsafe { CloseHandle(handle) };
}

fn same_path(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn gpu_path_key(path: &str) -> String {
    path.replace('/', "\\").to_ascii_lowercase()
}

fn gpu_path_from_baseline(baseline: &Baseline) -> Option<String> {
    match baseline {
        Baseline::Process(b) => b.executable.as_deref().map(gpu_path_key),
        Baseline::Thread(_) => None,
    }
}

fn has_other_gpu_owner(inner: &Inner, target: OsPolicyTarget, key: &str) -> bool {
    inner.active.values().any(|active| {
        active.target != target
            && active.policy.gpu_preference.is_some()
            && active
                .executable
                .as_deref()
                .is_some_and(|path| gpu_path_key(path) == key)
    })
}

fn target_identity(baseline: &Baseline) -> Option<String> {
    match baseline {
        Baseline::Process(b) => b.executable.clone(),
        Baseline::Thread(b) => b.owner_executable.clone(),
    }
}

fn creation_time(baseline: &Baseline) -> Option<u64> {
    match baseline {
        Baseline::Process(b) => b.creation_time,
        Baseline::Thread(b) => b.owner_creation_time,
    }
}

fn same_target_identity(a: &Baseline, b: &Baseline) -> bool {
    match (a, b) {
        (Baseline::Process(a), Baseline::Process(b)) => match (&a.executable, &b.executable) {
            (Some(path_a), Some(path_b)) => {
                same_path(path_a, path_b)
                    && match (a.creation_time, b.creation_time) {
                        (Some(time_a), Some(time_b)) => time_a == time_b,
                        _ => true,
                    }
            }
            _ => false,
        },
        (Baseline::Thread(a), Baseline::Thread(b)) => {
            a.owner_pid == b.owner_pid
                && match (&a.owner_executable, &b.owner_executable) {
                    (Some(path_a), Some(path_b)) => {
                        same_path(path_a, path_b)
                            && match (a.owner_creation_time, b.owner_creation_time) {
                                (Some(time_a), Some(time_b)) => time_a == time_b,
                                _ => true,
                            }
                    }
                    _ => false,
                }
        }
        _ => false,
    }
}

fn require_capture_for_policy(
    baseline: &Baseline,
    policy: &OsSchedulingPolicy,
) -> Result<(), PlatformError> {
    let unavailable =
        |what: &str| PlatformError::Os(format!("无法读取 {what} 的原状态，拒绝执行不可恢复的写入"));
    match baseline {
        Baseline::Process(b) => {
            if b.executable.is_none() || b.creation_time.is_none() {
                return Err(unavailable("进程身份（路径/创建时间）"));
            }
            if policy.cpu_placement.is_some() && !b.cpu_sets.available {
                return Err(unavailable("进程 CPU Sets"));
            }
            if policy.affinity.is_some() && !b.affinity.available {
                return Err(unavailable("进程 Affinity"));
            }
            if policy.qos.is_some() && !b.qos.available {
                return Err(unavailable("进程 QoS"));
            }
            if policy.process_priority.is_some() && !b.priority.available {
                return Err(unavailable("进程优先级"));
            }
            if policy.memory_priority.is_some() && !b.memory_priority.available {
                return Err(unavailable("进程内存优先级"));
            }
            if policy.gpu_preference.is_some()
                && (b.executable.is_none() || !b.gpu_preference.available)
            {
                return Err(unavailable("可执行文件 GPU 首选项"));
            }
        }
        Baseline::Thread(b) => {
            if b.owner_executable.is_none() || b.owner_creation_time.is_none() {
                return Err(unavailable("线程所属进程身份（路径/创建时间）"));
            }
            if policy.cpu_placement.is_some() && !b.cpu_sets.available {
                return Err(unavailable("线程 CPU Sets"));
            }
            if policy.affinity.is_some() && !b.group_affinity.available {
                return Err(unavailable("线程 Group Affinity"));
            }
            if policy.qos.is_some() && !b.qos.available {
                return Err(unavailable("线程 QoS"));
            }
            if policy.thread_priority.is_some() && !b.priority.available {
                return Err(unavailable("线程优先级"));
            }
            if policy.memory_priority.is_some() && !b.memory_priority.available {
                return Err(unavailable("线程内存优先级"));
            }
            if policy.ideal_processor.is_some() && !b.ideal_processor.available {
                return Err(unavailable("线程理想处理器"));
            }
        }
    }
    Ok(())
}

fn validate_ideal_processor(
    inner: &mut Inner,
    processor: Option<ProcessorRef>,
) -> Result<(), PlatformError> {
    let Some(processor) = processor else {
        return Ok(());
    };
    if !ensure_topology(inner)?
        .cpu_sets
        .iter()
        .any(|cpu| cpu.group == processor.group && cpu.logical_processor_index == processor.number)
    {
        return Err(PlatformError::Os(format!(
            "理想处理器 G{}:{} 不存在",
            processor.group, processor.number
        )));
    }
    Ok(())
}

fn resolve_cpu_placement(
    inner: &mut Inner,
    placement: Option<&CpuPlacement>,
) -> Result<Option<Vec<u32>>, PlatformError> {
    let Some(placement) = placement else {
        return Ok(None);
    };
    let topology = ensure_topology(inner)?.clone();
    let ids = match placement {
        CpuPlacement::All => Vec::new(),
        CpuPlacement::Performance => topology.performance_ids,
        CpuPlacement::Efficiency => topology.efficiency_ids,
        CpuPlacement::Custom(ids) => {
            let known = topology
                .cpu_sets
                .iter()
                .map(|cpu| cpu.id)
                .collect::<Vec<_>>();
            if let Some(id) = ids.iter().find(|id| !known.contains(id)) {
                return Err(PlatformError::Os(format!("CPU Set ID {id} 不存在")));
            }
            ids.clone()
        }
    };
    if ids.is_empty() && !matches!(placement, CpuPlacement::All) {
        return Err(PlatformError::Os("当前系统没有可用的目标 CPU Set".into()));
    }
    Ok(Some(ids))
}

fn ensure_topology(inner: &mut Inner) -> Result<&CpuTopology, PlatformError> {
    if inner.topology.is_none() {
        inner.topology = Some(query_topology()?);
    }
    Ok(inner.topology.as_ref().expect("topology inserted"))
}

fn query_topology() -> Result<CpuTopology, PlatformError> {
    let mut bytes = 0u32;
    unsafe {
        let _ = GetSystemCpuSetInformation(None, 0, &mut bytes, None, None);
    }
    if bytes == 0 {
        let code = unsafe { GetLastError() };
        if code == ERROR_CALL_NOT_IMPLEMENTED {
            return Err(PlatformError::NotAvailable("Windows CPU Sets"));
        }
        return Err(PlatformError::Os(format!(
            "GetSystemCpuSetInformation size query: Win32 {}",
            code.0
        )));
    }

    let item_size = size_of::<SYSTEM_CPU_SET_INFORMATION>();
    let item_count = (bytes as usize).div_ceil(item_size);
    let mut buffer = vec![SYSTEM_CPU_SET_INFORMATION::default(); item_count];
    let mut returned = 0u32;
    let ok = unsafe {
        GetSystemCpuSetInformation(
            Some(buffer.as_mut_ptr()),
            (buffer.len() * item_size) as u32,
            &mut returned,
            None,
            None,
        )
    };
    if ok.0 == 0 {
        return Err(last_error("GetSystemCpuSetInformation"));
    }

    let mut cpu_sets = Vec::new();
    let mut offset = 0usize;
    while offset + size_of::<u32>() <= returned as usize {
        let info = unsafe {
            read_unaligned(
                (buffer.as_ptr() as *const u8).add(offset) as *const SYSTEM_CPU_SET_INFORMATION
            )
        };
        if info.Size == 0 {
            break;
        }
        if info.Type == CpuSetInformation {
            let cpu: SYSTEM_CPU_SET_INFORMATION_0_0 = unsafe { info.Anonymous.CpuSet };
            let flags = unsafe { cpu.Anonymous1.AllFlags };
            cpu_sets.push(CpuSetInfo {
                id: cpu.Id,
                group: cpu.Group,
                logical_processor_index: cpu.LogicalProcessorIndex,
                core_index: cpu.CoreIndex,
                efficiency_class: cpu.EfficiencyClass,
                parked: flags & 1 != 0,
            });
        }
        offset = offset.saturating_add(info.Size as usize);
    }
    if cpu_sets.is_empty() {
        return Err(PlatformError::Data("Windows returned no CPU Sets"));
    }
    cpu_sets.sort_by_key(|cpu| cpu.id);
    // Parked is a current power-management state, not a topology class.  A
    // laptop can park every E-core while on AC or during an idle sample; if
    // parked sets are discarded before classification, both user-selectable
    // groups collapse onto the same unparked P-cores.  Classify the complete
    // topology and let Windows decide when a selected set needs unparking.
    let min_efficiency = cpu_sets
        .iter()
        .map(|cpu| cpu.efficiency_class)
        .min()
        .unwrap_or(0);
    let max_efficiency = cpu_sets
        .iter()
        .map(|cpu| cpu.efficiency_class)
        .max()
        .unwrap_or(0);
    let performance_ids = cpu_sets
        .iter()
        .filter(|cpu| cpu.efficiency_class == max_efficiency)
        .map(|cpu| cpu.id)
        .collect();
    let efficiency_ids = cpu_sets
        .iter()
        .filter(|cpu| cpu.efficiency_class == min_efficiency)
        .map(|cpu| cpu.id)
        .collect();
    Ok(CpuTopology {
        cpu_sets,
        performance_ids,
        efficiency_ids,
    })
}

fn query_processes() -> Result<Vec<ProcessInfo>, PlatformError> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|e| os_error("CreateToolhelp32Snapshot", e))?;
    let result = (|| {
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        unsafe { Process32FirstW(snapshot, &mut entry) }
            .map_err(|e| os_error("Process32FirstW", e))?;
        let mut processes = Vec::new();
        loop {
            let name = utf16z(&entry.szExeFile);
            let pid = entry.th32ProcessID;
            let (executable, creation_time) = query_process_metadata(pid);
            let session_id = query_process_session(pid);
            processes.push(ProcessInfo {
                pid,
                name,
                executable,
                thread_count: entry.cntThreads,
                session_id,
                creation_time,
            });
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
        processes.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
                .then(a.pid.cmp(&b.pid))
        });
        Ok(processes)
    })();
    close(snapshot);
    result
}

fn utf16z(value: &[u16]) -> String {
    let end = value.iter().position(|c| *c == 0).unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

fn query_process_path(pid: u32) -> Result<Option<String>, PlatformError> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .map_err(|e| os_error("OpenProcess(query path)", e))?;
    let result = query_path(handle);
    close(handle);
    result
}

fn query_process_metadata(pid: u32) -> (Option<String>, Option<u64>) {
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return (None, None);
    };
    let metadata = (
        query_path(handle).ok().flatten(),
        query_creation_time(handle).ok(),
    );
    close(handle);
    metadata
}

fn query_process_session(pid: u32) -> Option<u32> {
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(pid, &mut session) }
        .is_ok()
        .then_some(session)
}

fn query_process_creation_time(pid: u32) -> Option<u64> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let value = query_creation_time(handle).ok();
    close(handle);
    value
}

fn query_path(handle: HANDLE) -> Result<Option<String>, PlatformError> {
    let mut capacity = 512usize;
    for _ in 0..5 {
        let mut buffer = vec![0u16; capacity];
        let mut length = buffer.len() as u32;
        match unsafe {
            windows::Win32::System::Threading::QueryFullProcessImageNameW(
                handle,
                windows::Win32::System::Threading::PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut length,
            )
        } {
            Ok(()) => {
                buffer.truncate(length as usize);
                return Ok(Some(String::from_utf16_lossy(&buffer)));
            }
            Err(error) if error.code().0 as u32 == ERROR_INSUFFICIENT_BUFFER.0 => {
                capacity *= 2;
            }
            Err(error) => return Err(os_error("QueryFullProcessImageNameW", error)),
        }
    }
    Err(PlatformError::Os(
        "QueryFullProcessImageNameW: path exceeds supported length".into(),
    ))
}

fn query_creation_time(handle: HANDLE) -> Result<u64, PlatformError> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }
        .map_err(|e| os_error("GetProcessTimes", e))?;
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn query_process_groups(handle: HANDLE) -> Result<Vec<u16>, PlatformError> {
    let mut count = 0u16;
    unsafe {
        let _ = GetProcessGroupAffinity(handle, &mut count, null_mut());
    }
    if count == 0 {
        return Err(last_error("GetProcessGroupAffinity size query"));
    }
    let mut groups = vec![0u16; count as usize];
    if unsafe { GetProcessGroupAffinity(handle, &mut count, groups.as_mut_ptr()) }.0 == 0 {
        return Err(last_error("GetProcessGroupAffinity"));
    }
    groups.truncate(count as usize);
    Ok(groups)
}

fn query_process_affinity(handle: HANDLE) -> Captured<AffinityMask> {
    let Ok(groups) = query_process_groups(handle) else {
        return Captured::unavailable();
    };
    if groups.len() != 1 {
        return Captured::unavailable();
    }
    let mut process_mask = 0usize;
    let mut system_mask = 0usize;
    match unsafe { GetProcessAffinityMask(handle, &mut process_mask, &mut system_mask) } {
        Ok(()) => Captured::known(Some(AffinityMask {
            group: groups[0],
            mask: process_mask as u64,
        })),
        Err(_) => Captured::unavailable(),
    }
}

fn query_cpu_sets_process(handle: HANDLE) -> Result<Option<Vec<u32>>, PlatformError> {
    let mut count = 0u32;
    unsafe {
        let _ = GetProcessDefaultCpuSets(handle, None, &mut count);
    }
    if count == 0 {
        return Ok(None);
    }
    let mut ids = vec![0u32; count as usize];
    if unsafe { GetProcessDefaultCpuSets(handle, Some(&mut ids), &mut count) }.0 == 0 {
        return Err(last_error("GetProcessDefaultCpuSets"));
    }
    ids.truncate(count as usize);
    Ok(Some(ids))
}

fn query_cpu_sets_thread(handle: HANDLE) -> Result<Option<Vec<u32>>, PlatformError> {
    let mut count = 0u32;
    unsafe {
        let _ = GetThreadSelectedCpuSets(handle, None, &mut count);
    }
    if count == 0 {
        return Ok(None);
    }
    let mut ids = vec![0u32; count as usize];
    if unsafe { GetThreadSelectedCpuSets(handle, Some(&mut ids), &mut count) }.0 == 0 {
        return Err(last_error("GetThreadSelectedCpuSets"));
    }
    ids.truncate(count as usize);
    Ok(Some(ids))
}

fn query_process_qos(handle: HANDLE) -> Result<PROCESS_POWER_THROTTLING_STATE, PlatformError> {
    // GetProcessInformation validates Version as an input field on Windows
    // 11.  Leaving the zeroed default here produces ERROR_INVALID_PARAMETER
    // (87) for otherwise accessible processes, which makes the safety layer
    // reject every EcoQoS write.
    let mut state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ..Default::default()
    };
    unsafe {
        GetProcessInformation(
            handle,
            ProcessPowerThrottling,
            (&mut state as *mut PROCESS_POWER_THROTTLING_STATE).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    }
    .map_err(|e| os_error("GetProcessInformation(ProcessPowerThrottling)", e))?;
    Ok(state)
}

fn query_thread_qos(handle: HANDLE) -> Result<PROCESS_POWER_THROTTLING_STATE, PlatformError> {
    let mut state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ..Default::default()
    };
    unsafe {
        GetThreadInformation(
            handle,
            ThreadPowerThrottling,
            (&mut state as *mut PROCESS_POWER_THROTTLING_STATE).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    }
    .map_err(|e| os_error("GetThreadInformation(ThreadPowerThrottling)", e))?;
    Ok(state)
}

fn query_process_memory_priority(handle: HANDLE) -> Result<u32, PlatformError> {
    let mut info = MEMORY_PRIORITY_INFORMATION::default();
    unsafe {
        GetProcessInformation(
            handle,
            ProcessMemoryPriority,
            (&mut info as *mut MEMORY_PRIORITY_INFORMATION).cast(),
            size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
        )
    }
    .map_err(|e| os_error("GetProcessInformation(ProcessMemoryPriority)", e))?;
    Ok(info.MemoryPriority.0)
}

fn query_thread_memory_priority(handle: HANDLE) -> Result<u32, PlatformError> {
    let mut info = MEMORY_PRIORITY_INFORMATION::default();
    unsafe {
        GetThreadInformation(
            handle,
            ThreadMemoryPriority,
            (&mut info as *mut MEMORY_PRIORITY_INFORMATION).cast(),
            size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
        )
    }
    .map_err(|e| os_error("GetThreadInformation(ThreadMemoryPriority)", e))?;
    Ok(info.MemoryPriority.0)
}

fn query_gpu_preference(path: &str) -> Result<Option<String>, PlatformError> {
    let mut key = HKEY::default();
    let code = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            GPU_PREF_KEY,
            None,
            KEY_QUERY_VALUE,
            &mut key,
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if code.0 != 0 {
        return Err(reg_error("RegOpenKeyExW", code));
    }
    let name = wide(path);
    let result = (|| {
        let mut kind = REG_NONE;
        let mut bytes = 0u32;
        let code = unsafe {
            RegQueryValueExW(
                key,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                None,
                Some(&mut bytes),
            )
        };
        if code == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if code.0 != 0 {
            return Err(reg_error("RegQueryValueExW(size)", code));
        }
        if kind != REG_SZ {
            return Err(PlatformError::Os(format!(
                "GPU preference registry value has unsupported type {}",
                kind.0
            )));
        }
        let mut data = vec![0u8; bytes as usize];
        let code = unsafe {
            RegQueryValueExW(
                key,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                Some(data.as_mut_ptr()),
                Some(&mut bytes),
            )
        };
        if code.0 != 0 {
            return Err(reg_error("RegQueryValueExW", code));
        }
        data.truncate(bytes as usize);
        let values = data
            .chunks(2)
            .filter(|chunk| chunk.len() == 2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .take_while(|c| *c != 0)
            .collect::<Vec<_>>();
        Ok(Some(String::from_utf16_lossy(&values)))
    })();
    let _ = unsafe { RegCloseKey(key) };
    result
}

fn apply_gpu_preference(path: &str, preference: GpuPreference) -> Result<(), PlatformError> {
    let mut key = HKEY::default();
    let code = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            GPU_PREF_KEY,
            None,
            w!(""),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    if code.0 != 0 {
        return Err(reg_error("RegCreateKeyExW", code));
    }
    let result = match preference {
        GpuPreference::System => {
            let code = unsafe { RegDeleteValueW(key, PCWSTR(wide(path).as_ptr())) };
            if code == ERROR_FILE_NOT_FOUND || code.0 == 0 {
                Ok(())
            } else {
                Err(reg_error("RegDeleteValueW", code))
            }
        }
        GpuPreference::PowerSaving => write_gpu_value(key, path, GPU_PREF_POWER_SAVING),
        GpuPreference::HighPerformance => write_gpu_value(key, path, GPU_PREF_HIGH_PERFORMANCE),
    };
    let _ = unsafe { RegCloseKey(key) };
    result
}

fn restore_gpu_preference(path: &str, old: Option<&String>) -> Result<(), PlatformError> {
    let mut key = HKEY::default();
    let code = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            GPU_PREF_KEY,
            None,
            w!(""),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    };
    if code.0 != 0 {
        return Err(reg_error("RegCreateKeyExW(restore)", code));
    }
    let name = wide(path);
    let result = if let Some(value) = old {
        write_gpu_value(key, path, value)
    } else {
        let code = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) };
        if code == ERROR_FILE_NOT_FOUND || code.0 == 0 {
            Ok(())
        } else {
            Err(reg_error("RegDeleteValueW(restore)", code))
        }
    };
    let _ = unsafe { RegCloseKey(key) };
    result
}

fn write_gpu_value(key: HKEY, path: &str, value: &str) -> Result<(), PlatformError> {
    let name = wide(path);
    let data = wide_bytes(value);
    let code = unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(&data)) };
    if code.0 != 0 {
        Err(reg_error("RegSetValueExW", code))
    } else {
        Ok(())
    }
}

fn reg_error(operation: &str, code: WIN32_ERROR) -> PlatformError {
    PlatformError::Os(format!("{operation}: Win32 {} (0x{:08X})", code.0, code.0))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_bytes(value: &str) -> Vec<u8> {
    wide(value).into_iter().flat_map(u16::to_le_bytes).collect()
}

fn capture_target(target: OsPolicyTarget) -> Result<Baseline, PlatformError> {
    match target {
        OsPolicyTarget::Process { pid } => capture_process(pid),
        OsPolicyTarget::Thread { tid } => capture_thread(tid),
    }
}

fn capture_process(pid: u32) -> Result<Baseline, PlatformError> {
    let handle = unsafe { OpenProcess(process_access(), false, pid) }
        .map_err(|e| os_error("OpenProcess", e))?;
    let result = {
        let executable = query_path(handle).ok().flatten();
        let creation_time = query_creation_time(handle).ok();
        let affinity = query_process_affinity(handle);
        let cpu_sets = match query_cpu_sets_process(handle) {
            Ok(value) => Captured::known(value),
            Err(error) => {
                tracing::debug!(pid, %error, "process CPU Sets readback unavailable");
                Captured::unavailable()
            }
        };
        let qos = match query_process_qos(handle) {
            Ok(value) => Captured::known(Some(value)),
            Err(error) => {
                tracing::debug!(pid, %error, "process QoS readback unavailable");
                Captured::unavailable()
            }
        };
        let raw_priority = unsafe { GetPriorityClass(handle) };
        let priority = if raw_priority != 0 {
            Captured::known(Some(raw_priority))
        } else {
            tracing::debug!(
                pid,
                "GetPriorityClass returned zero; process priority is not restorable"
            );
            Captured::unavailable()
        };
        let memory_priority = match query_process_memory_priority(handle) {
            Ok(value) => Captured::known(Some(value)),
            Err(error) => {
                tracing::debug!(pid, %error, "process memory priority readback unavailable");
                Captured::unavailable()
            }
        };
        let gpu_preference = match &executable {
            Some(path) => match query_gpu_preference(path) {
                Ok(value) => Captured::known(value),
                Err(error) => {
                    tracing::debug!(pid, %error, "GPU preference readback unavailable");
                    Captured::unavailable()
                }
            },
            None => Captured::unavailable(),
        };
        Ok(Baseline::Process(ProcessBaseline {
            executable,
            creation_time,
            affinity,
            cpu_sets,
            qos,
            priority,
            memory_priority,
            gpu_preference,
        }))
    };
    close(handle);
    result
}

fn capture_thread(tid: u32) -> Result<Baseline, PlatformError> {
    let handle = unsafe { OpenThread(thread_access(), false, tid) }
        .map_err(|e| os_error("OpenThread", e))?;
    let result = (|| {
        let owner_pid = unsafe { GetProcessIdOfThread(handle) };
        if owner_pid == 0 {
            return Err(last_error("GetProcessIdOfThread"));
        }
        let owner_executable = query_process_path(owner_pid).ok().flatten();
        let owner_creation_time = query_process_creation_time(owner_pid);
        let mut group_affinity = GROUP_AFFINITY::default();
        let group_affinity =
            if unsafe { GetThreadGroupAffinity(handle, &mut group_affinity) }.0 != 0 {
                Captured::known(Some(group_affinity))
            } else {
                Captured::unavailable()
            };
        let cpu_sets = match query_cpu_sets_thread(handle) {
            Ok(value) => Captured::known(value),
            Err(error) => {
                tracing::debug!(tid, %error, "thread CPU Sets readback unavailable");
                Captured::unavailable()
            }
        };
        let qos = match query_thread_qos(handle) {
            Ok(value) => Captured::known(Some(value)),
            Err(error) => {
                tracing::debug!(tid, %error, "thread QoS readback unavailable");
                Captured::unavailable()
            }
        };
        let raw_priority = unsafe { GetThreadPriority(handle) };
        let priority = Captured::known((raw_priority != i32::MAX).then_some(raw_priority));
        let memory_priority = match query_thread_memory_priority(handle) {
            Ok(value) => Captured::known(Some(value)),
            Err(error) => {
                tracing::debug!(tid, %error, "thread memory priority readback unavailable");
                Captured::unavailable()
            }
        };
        let mut ideal = PROCESSOR_NUMBER::default();
        let ideal_processor = match unsafe { GetThreadIdealProcessorEx(handle, &mut ideal) } {
            Ok(()) => Captured::known(Some(ProcessorRef {
                group: ideal.Group,
                number: ideal.Number,
            })),
            Err(error) => {
                tracing::debug!(tid, %error, "thread ideal processor readback unavailable");
                Captured::unavailable()
            }
        };
        Ok(Baseline::Thread(ThreadBaseline {
            owner_pid,
            owner_executable,
            owner_creation_time,
            group_affinity,
            cpu_sets,
            qos,
            priority,
            memory_priority,
            ideal_processor,
        }))
    })();
    close(handle);
    result
}

fn apply_target(
    target: OsPolicyTarget,
    policy: &OsSchedulingPolicy,
    cpu_sets: Option<&[u32]>,
) -> Result<(), PlatformError> {
    match target {
        OsPolicyTarget::Process { pid } => apply_process(pid, policy, cpu_sets),
        OsPolicyTarget::Thread { tid } => apply_thread(tid, policy, cpu_sets),
    }
}

fn apply_process(
    pid: u32,
    policy: &OsSchedulingPolicy,
    cpu_sets: Option<&[u32]>,
) -> Result<(), PlatformError> {
    let handle = unsafe { OpenProcess(process_access(), false, pid) }
        .map_err(|e| os_error("OpenProcess(apply)", e))?;
    let result = (|| {
        if let Some(ids) = cpu_sets {
            let ids = (!ids.is_empty()).then_some(ids);
            if unsafe { SetProcessDefaultCpuSets(handle, ids) }.0 == 0 {
                return Err(last_error("SetProcessDefaultCpuSets"));
            }
        }
        if let Some(affinity) = policy.affinity {
            let groups = query_process_groups(handle)?;
            if groups.len() != 1 || groups[0] != affinity.group {
                return Err(PlatformError::Os(
                    "进程 Affinity 只支持目标当前所在的单一处理器组".into(),
                ));
            }
            let mask = usize::try_from(affinity.mask)
                .map_err(|_| PlatformError::Os("Affinity mask 超出当前平台位宽".into()))?;
            unsafe { SetProcessAffinityMask(handle, mask) }
                .map_err(|e| os_error("SetProcessAffinityMask", e))?;
        }
        if let Some(qos) = policy.qos {
            set_process_qos(handle, qos)?;
        }
        if let Some(priority) = policy.process_priority {
            unsafe { SetPriorityClass(handle, process_priority(priority)) }
                .map_err(|e| os_error("SetPriorityClass", e))?;
        }
        if let Some(priority) = policy.memory_priority {
            set_process_memory_priority(handle, priority)?;
        }
        if let Some(gpu) = policy.gpu_preference {
            let path = query_path(handle)?.ok_or_else(|| {
                PlatformError::Os("无法取得目标进程路径，不能设置 GPU 首选项".into())
            })?;
            apply_gpu_preference(&path, gpu)?;
        }
        Ok(())
    })();
    close(handle);
    result
}

fn apply_thread(
    tid: u32,
    policy: &OsSchedulingPolicy,
    cpu_sets: Option<&[u32]>,
) -> Result<(), PlatformError> {
    let handle = unsafe { OpenThread(thread_access(), false, tid) }
        .map_err(|e| os_error("OpenThread(apply)", e))?;
    let result = (|| {
        if let Some(ids) = cpu_sets
            && unsafe { SetThreadSelectedCpuSets(handle, ids) }.0 == 0
        {
            return Err(last_error("SetThreadSelectedCpuSets"));
        }
        if let Some(affinity) = policy.affinity {
            let group_affinity = GROUP_AFFINITY {
                Mask: usize::try_from(affinity.mask)
                    .map_err(|_| PlatformError::Os("Affinity mask 超出当前平台位宽".into()))?,
                Group: affinity.group,
                Reserved: [0; 3],
            };
            if unsafe { SetThreadGroupAffinity(handle, &group_affinity, None) }.0 == 0 {
                return Err(last_error("SetThreadGroupAffinity"));
            }
        }
        if let Some(qos) = policy.qos {
            set_thread_qos(handle, qos)?;
        }
        if let Some(priority) = policy.thread_priority {
            unsafe { SetThreadPriority(handle, thread_priority(priority)) }
                .map_err(|e| os_error("SetThreadPriority", e))?;
        }
        if let Some(priority) = policy.memory_priority {
            set_thread_memory_priority(handle, priority)?;
        }
        if let Some(ideal) = policy.ideal_processor {
            let processor = PROCESSOR_NUMBER {
                Group: ideal.group,
                Number: ideal.number,
                Reserved: 0,
            };
            unsafe { SetThreadIdealProcessorEx(handle, &processor, None) }
                .map_err(|e| os_error("SetThreadIdealProcessorEx", e))?;
        }
        Ok(())
    })();
    close(handle);
    result
}

fn power_state(qos: QosLevel) -> PROCESS_POWER_THROTTLING_STATE {
    match qos {
        QosLevel::System => PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: 0,
            StateMask: 0,
        },
        QosLevel::High => PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0,
        },
        QosLevel::Eco => PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        },
    }
}

fn set_process_qos(handle: HANDLE, qos: QosLevel) -> Result<(), PlatformError> {
    let state = power_state(qos);
    unsafe {
        SetProcessInformation(
            handle,
            ProcessPowerThrottling,
            (&state as *const PROCESS_POWER_THROTTLING_STATE).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    }
    .map_err(|e| os_error("SetProcessInformation(ProcessPowerThrottling)", e))
}

fn set_thread_qos(handle: HANDLE, qos: QosLevel) -> Result<(), PlatformError> {
    let state = power_state(qos);
    unsafe {
        SetThreadInformation(
            handle,
            ThreadPowerThrottling,
            (&state as *const PROCESS_POWER_THROTTLING_STATE).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    }
    .map_err(|e| os_error("SetThreadInformation(ThreadPowerThrottling)", e))
}

fn process_priority(priority: ProcessPriority) -> PROCESS_CREATION_FLAGS {
    match priority {
        ProcessPriority::Idle => IDLE_PRIORITY_CLASS,
        ProcessPriority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
        ProcessPriority::Normal => NORMAL_PRIORITY_CLASS,
        ProcessPriority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
        ProcessPriority::High => HIGH_PRIORITY_CLASS,
    }
}

fn thread_priority(priority: ThreadPriority) -> THREAD_PRIORITY {
    match priority {
        ThreadPriority::Idle => THREAD_PRIORITY_IDLE,
        ThreadPriority::Lowest => THREAD_PRIORITY_LOWEST,
        ThreadPriority::BelowNormal => THREAD_PRIORITY_BELOW_NORMAL,
        ThreadPriority::Normal => THREAD_PRIORITY_NORMAL,
        ThreadPriority::AboveNormal => THREAD_PRIORITY_ABOVE_NORMAL,
        ThreadPriority::Highest => THREAD_PRIORITY_HIGHEST,
    }
}

fn memory_priority(priority: MemoryPriority) -> MEMORY_PRIORITY {
    match priority {
        MemoryPriority::VeryLow => MEMORY_PRIORITY(1),
        MemoryPriority::Low => MEMORY_PRIORITY(2),
        MemoryPriority::Medium => MEMORY_PRIORITY(3),
        MemoryPriority::BelowNormal => MEMORY_PRIORITY(4),
        MemoryPriority::Normal => MEMORY_PRIORITY(5),
    }
}

fn set_process_memory_priority(
    handle: HANDLE,
    priority: MemoryPriority,
) -> Result<(), PlatformError> {
    let info = MEMORY_PRIORITY_INFORMATION {
        MemoryPriority: memory_priority(priority),
    };
    unsafe {
        SetProcessInformation(
            handle,
            ProcessMemoryPriority,
            (&info as *const MEMORY_PRIORITY_INFORMATION).cast(),
            size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
        )
    }
    .map_err(|e| os_error("SetProcessInformation(ProcessMemoryPriority)", e))
}

fn set_thread_memory_priority(
    handle: HANDLE,
    priority: MemoryPriority,
) -> Result<(), PlatformError> {
    let info = MEMORY_PRIORITY_INFORMATION {
        MemoryPriority: memory_priority(priority),
    };
    unsafe {
        SetThreadInformation(
            handle,
            ThreadMemoryPriority,
            (&info as *const MEMORY_PRIORITY_INFORMATION).cast(),
            size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
        )
    }
    .map_err(|e| os_error("SetThreadInformation(ThreadMemoryPriority)", e))
}

fn restore_baseline(
    target: OsPolicyTarget,
    baseline: &Baseline,
    restore_gpu: bool,
) -> Result<(), PlatformError> {
    match (target, baseline) {
        (OsPolicyTarget::Process { pid }, Baseline::Process(b)) => {
            let handle = unsafe { OpenProcess(process_access(), false, pid) }
                .map_err(|e| os_error("OpenProcess(restore)", e))?;
            let result = (|| {
                verify_process_identity(handle, b.executable.as_deref(), b.creation_time)?;
                restore_process_handle(handle, b, restore_gpu)
            })();
            close(handle);
            result
        }
        (OsPolicyTarget::Thread { tid }, Baseline::Thread(b)) => {
            let handle = unsafe { OpenThread(thread_access(), false, tid) }
                .map_err(|e| os_error("OpenThread(restore)", e))?;
            let result = (|| {
                let owner = unsafe { GetProcessIdOfThread(handle) };
                if owner != b.owner_pid {
                    return Err(PlatformError::Os("线程所属进程已变化，跳过恢复".into()));
                }
                let path = query_process_path(owner)?;
                let creation = query_process_creation_time(owner);
                if !identity_matches(
                    path.as_deref(),
                    b.owner_executable.as_deref(),
                    creation,
                    b.owner_creation_time,
                ) {
                    return Err(PlatformError::Os(
                        "线程所属可执行文件已变化，跳过恢复".into(),
                    ));
                }
                restore_thread_handle(handle, b)
            })();
            close(handle);
            result
        }
        _ => Err(PlatformError::Os(
            "OS policy target 类型与基线不匹配".into(),
        )),
    }
}

fn restore_captured_target(
    target: OsPolicyTarget,
    baseline: &Baseline,
) -> Result<(), PlatformError> {
    // Rollback must use the same identity checks as an explicit restore.
    // A partial apply can race process exit/PID reuse; raw handle restoration
    // would otherwise mutate an unrelated process.
    restore_baseline(target, baseline, true)
}

fn verify_process_identity(
    handle: HANDLE,
    expected: Option<&str>,
    expected_creation_time: Option<u64>,
) -> Result<(), PlatformError> {
    let Some(expected) = expected else {
        return Err(PlatformError::Os(
            "无法验证目标进程身份，跳过恢复以避免 PID 复用误操作".into(),
        ));
    };
    let actual = query_path(handle)?.ok_or_else(|| {
        PlatformError::Os("无法读取目标进程路径，跳过恢复以避免 PID 复用误操作".into())
    })?;
    if !same_path(expected, &actual) {
        return Err(PlatformError::Os(
            "目标进程已被另一个可执行文件复用，跳过恢复".into(),
        ));
    }
    if let Some(expected) = expected_creation_time
        && query_creation_time(handle).ok() != Some(expected)
    {
        return Err(PlatformError::Os("目标进程创建身份已变化，跳过恢复".into()));
    }
    Ok(())
}

fn identity_matches(
    actual: Option<&str>,
    expected: Option<&str>,
    actual_creation_time: Option<u64>,
    expected_creation_time: Option<u64>,
) -> bool {
    match (actual, expected) {
        (Some(actual), Some(expected)) => {
            same_path(actual, expected)
                && match (actual_creation_time, expected_creation_time) {
                    (Some(actual), Some(expected)) => actual == expected,
                    (None, Some(_)) => false,
                    _ => true,
                }
        }
        _ => false,
    }
}

fn restore_process_handle(
    handle: HANDLE,
    b: &ProcessBaseline,
    restore_gpu: bool,
) -> Result<(), PlatformError> {
    let mut first_error = None;
    if b.cpu_sets.available {
        let ids = b.cpu_sets.value.as_deref().filter(|ids| !ids.is_empty());
        if unsafe { SetProcessDefaultCpuSets(handle, ids) }.0 == 0 {
            first_error.get_or_insert(last_error("SetProcessDefaultCpuSets(restore)"));
        }
    }
    if b.affinity.available
        && let Some(affinity) = b.affinity.value
    {
        if let Err(error) = unsafe { SetProcessAffinityMask(handle, affinity.mask as usize) }
            .map_err(|e| os_error("SetProcessAffinityMask(restore)", e))
        {
            first_error.get_or_insert(error);
        }
    }
    if b.qos.available
        && let Some(qos) = b.qos.value
    {
        let result = unsafe {
            SetProcessInformation(
                handle,
                ProcessPowerThrottling,
                (&qos as *const PROCESS_POWER_THROTTLING_STATE).cast(),
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        }
        .map_err(|e| os_error("SetProcessInformation(QoS restore)", e));
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    if b.priority.available
        && let Some(priority) = b.priority.value
    {
        if let Err(error) = unsafe { SetPriorityClass(handle, PROCESS_CREATION_FLAGS(priority)) }
            .map_err(|e| os_error("SetPriorityClass(restore)", e))
        {
            first_error.get_or_insert(error);
        }
    }
    if b.memory_priority.available
        && let Some(priority) = b.memory_priority.value
    {
        let info = MEMORY_PRIORITY_INFORMATION {
            MemoryPriority: MEMORY_PRIORITY(priority),
        };
        let result = unsafe {
            SetProcessInformation(
                handle,
                ProcessMemoryPriority,
                (&info as *const MEMORY_PRIORITY_INFORMATION).cast(),
                size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
            )
        }
        .map_err(|e| os_error("SetProcessInformation(memory restore)", e));
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    if restore_gpu
        && b.gpu_preference.available
        && let Some(path) = &b.executable
    {
        if let Err(error) = restore_gpu_preference(path, b.gpu_preference.value.as_ref()) {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn restore_thread_handle(handle: HANDLE, b: &ThreadBaseline) -> Result<(), PlatformError> {
    let mut first_error = None;
    if b.cpu_sets.available
        && unsafe { SetThreadSelectedCpuSets(handle, b.cpu_sets.value.as_deref().unwrap_or(&[])) }.0
            == 0
    {
        first_error.get_or_insert(last_error("SetThreadSelectedCpuSets(restore)"));
    }
    if b.group_affinity.available
        && let Some(group_affinity) = b.group_affinity.value
        && unsafe { SetThreadGroupAffinity(handle, &group_affinity, None) }.0 == 0
    {
        first_error.get_or_insert(last_error("SetThreadGroupAffinity(restore)"));
    }
    if b.qos.available
        && let Some(qos) = b.qos.value
    {
        let result = unsafe {
            SetThreadInformation(
                handle,
                ThreadPowerThrottling,
                (&qos as *const PROCESS_POWER_THROTTLING_STATE).cast(),
                size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        }
        .map_err(|e| os_error("SetThreadInformation(QoS restore)", e));
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    if b.priority.available
        && let Some(priority) = b.priority.value
    {
        if let Err(error) = unsafe { SetThreadPriority(handle, THREAD_PRIORITY(priority)) }
            .map_err(|e| os_error("SetThreadPriority(restore)", e))
        {
            first_error.get_or_insert(error);
        }
    }
    if b.memory_priority.available
        && let Some(priority) = b.memory_priority.value
    {
        let info = MEMORY_PRIORITY_INFORMATION {
            MemoryPriority: MEMORY_PRIORITY(priority),
        };
        let result = unsafe {
            SetThreadInformation(
                handle,
                ThreadMemoryPriority,
                (&info as *const MEMORY_PRIORITY_INFORMATION).cast(),
                size_of::<MEMORY_PRIORITY_INFORMATION>() as u32,
            )
        }
        .map_err(|e| os_error("SetThreadInformation(memory restore)", e));
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    if b.ideal_processor.available
        && let Some(ideal) = b.ideal_processor.value
    {
        let processor = PROCESSOR_NUMBER {
            Group: ideal.group,
            Number: ideal.number,
            Reserved: 0,
        };
        if let Err(error) = unsafe { SetThreadIdealProcessorEx(handle, &processor, None) }
            .map_err(|e| os_error("SetThreadIdealProcessorEx(restore)", e))
        {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}
