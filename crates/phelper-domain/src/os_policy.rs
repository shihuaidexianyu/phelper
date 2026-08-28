//! Windows process/thread scheduling policy vocabulary.
//!
//! This module deliberately contains no Windows types.  It describes the
//! knobs that the core can apply to a running process or thread; the Windows
//! adapter is responsible for translating them to CPU Sets, affinity, QoS,
//! priorities and the per-executable graphics preference.

use serde::{Deserialize, Serialize};

/// Which Windows CPU Sets a target may run on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuPlacement {
    /// Remove an explicit CPU-set restriction and let Windows choose.
    #[default]
    All,
    /// CPUs with the highest efficiency class (normally P-cores).
    Performance,
    /// CPUs with the lowest efficiency class (normally E-cores).
    Efficiency,
    /// Explicit CPU Set IDs returned by the Windows topology query.
    Custom(Vec<u32>),
}

/// Legacy group affinity.  This is intentionally separate from CPU Sets:
/// affinity is a hard bit mask inside one processor group and is less
/// portable on hybrid and multi-group machines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AffinityMask {
    pub group: u16,
    pub mask: u64,
}

/// Windows process/thread power QoS.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QosLevel {
    /// Clear the phelper QoS override; Windows manages execution speed.
    #[default]
    System,
    /// Prefer full execution speed (the normal performance-oriented state).
    High,
    /// Enable EcoQoS / execution-speed throttling.
    Eco,
}

/// Safe subset of process priority classes.  Realtime is intentionally not
/// exposed because it can starve input, storage and thermal safety work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessPriority {
    Idle,
    BelowNormal,
    #[default]
    Normal,
    AboveNormal,
    High,
}

/// Thread priority without the dangerous realtime/time-critical class.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadPriority {
    Idle,
    Lowest,
    BelowNormal,
    #[default]
    Normal,
    AboveNormal,
    Highest,
}

/// Memory trimming priority.  Lower values make a process more reclaimable;
/// this does not change its CPU priority.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPriority {
    VeryLow,
    Low,
    Medium,
    BelowNormal,
    #[default]
    Normal,
}

/// A processor used as a scheduler hint for one thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessorRef {
    pub group: u16,
    pub number: u8,
}

/// Windows Graphics Settings preference written for an executable.  It is a
/// launch preference, not a retroactive GPU migration mechanism.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuPreference {
    #[default]
    System,
    PowerSaving,
    HighPerformance,
}

/// Process- or thread-level software scheduling policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OsSchedulingPolicy {
    /// Process: default CPU Sets.  Thread: selected CPU Sets.
    pub cpu_placement: Option<CpuPlacement>,
    /// Optional legacy hard affinity.  Do not combine it with a custom CPU
    /// placement: the intersection is easy to misunderstand and can be
    /// empty on a hybrid machine.
    pub affinity: Option<AffinityMask>,
    pub qos: Option<QosLevel>,
    pub process_priority: Option<ProcessPriority>,
    pub thread_priority: Option<ThreadPriority>,
    pub memory_priority: Option<MemoryPriority>,
    pub ideal_processor: Option<ProcessorRef>,
    /// Process target only.  Takes effect for the next launch of the EXE.
    pub gpu_preference: Option<GpuPreference>,
}

impl OsSchedulingPolicy {
    pub fn is_empty(&self) -> bool {
        self.cpu_placement.is_none()
            && self.affinity.is_none()
            && self.qos.is_none()
            && self.process_priority.is_none()
            && self.thread_priority.is_none()
            && self.memory_priority.is_none()
            && self.ideal_processor.is_none()
            && self.gpu_preference.is_none()
    }

    /// Pure validation shared by CLI, UI and the Windows adapter.
    pub fn validate_for(&self, target: &OsPolicyTarget) -> Result<(), String> {
        if self.is_empty() {
            return Err("至少指定一个 OS 调度参数".into());
        }
        if let Some(CpuPlacement::Custom(ids)) = &self.cpu_placement {
            if ids.is_empty() {
                return Err("自定义 CPU Set 不能为空".into());
            }
            if ids.len() > 256 {
                return Err("自定义 CPU Set 最多 256 个 ID".into());
            }
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            sorted.dedup();
            if sorted.len() != ids.len() {
                return Err("自定义 CPU Set 不能包含重复 ID".into());
            }
        }
        if let Some(affinity) = self.affinity
            && affinity.mask == 0
        {
            return Err("Affinity mask 不能为 0".into());
        }
        if matches!(self.cpu_placement, Some(CpuPlacement::Custom(_))) && self.affinity.is_some() {
            return Err("自定义 CPU Sets 与 Affinity 不能同时指定".into());
        }
        match target {
            OsPolicyTarget::Process { pid } => {
                if *pid == 0 {
                    return Err("进程 PID 不能为 0".into());
                }
                if self.thread_priority.is_some() || self.ideal_processor.is_some() {
                    return Err("线程优先级和理想处理器只能用于线程目标".into());
                }
            }
            OsPolicyTarget::Thread { tid } => {
                if *tid == 0 {
                    return Err("线程 TID 不能为 0".into());
                }
                if self.process_priority.is_some() || self.gpu_preference.is_some() {
                    return Err("进程优先级和 GPU 首选项只能用于进程目标".into());
                }
            }
        }
        if let Some(cpu) = self.ideal_processor
            && cpu.number >= 64
        {
            return Err("理想处理器编号必须小于 64".into());
        }
        Ok(())
    }
}

/// A process or thread to which a policy is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OsPolicyTarget {
    Process { pid: u32 },
    Thread { tid: u32 },
}

impl OsPolicyTarget {
    pub fn id(self) -> u32 {
        match self {
            Self::Process { pid } => pid,
            Self::Thread { tid } => tid,
        }
    }
}

/// One logical processor reported by GetSystemCpuSetInformation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuSetInfo {
    pub id: u32,
    pub group: u16,
    pub logical_processor_index: u8,
    pub core_index: u8,
    pub efficiency_class: u8,
    pub parked: bool,
}

/// Topology used to turn “P 核 / E 核” into stable Windows CPU Set IDs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuTopology {
    pub cpu_sets: Vec<CpuSetInfo>,
    pub performance_ids: Vec<u32>,
    pub efficiency_ids: Vec<u32>,
}

/// A process row for the application picker.  The list is a snapshot; a PID
/// can disappear or be reused before an apply, so the core verifies the image
/// path whenever it restores a captured baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub executable: Option<String>,
    pub thread_count: u32,
    /// Windows session containing the process.  `None` means the query was
    /// denied or the process disappeared during enumeration.
    #[serde(default)]
    pub session_id: Option<u32>,
    /// Process creation time as a Windows FILETIME integer, when readable.
    /// It is an identity guard against PID reuse; it is not a user-facing
    /// timestamp.
    #[serde(default)]
    pub creation_time: Option<u64>,
}

/// Which phelper subsystem currently owns an OS policy target.
///
/// Manual and automatic writes share one backend and one baseline ledger.
/// Keeping the owner explicit prevents an automatic reconcile from silently
/// overwriting a user-selected policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsPolicyOwner {
    #[default]
    Manual,
    Automatic,
}

/// One policy currently owned by phelper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveOsPolicy {
    pub target: OsPolicyTarget,
    pub executable: Option<String>,
    pub policy: OsSchedulingPolicy,
    pub gpu_requires_restart: bool,
    #[serde(default)]
    pub owner: OsPolicyOwner,
    /// Process creation identity captured with the baseline, when available.
    #[serde(default)]
    pub creation_time: Option<u64>,
}

/// Read model exposed to the UI and API clients.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsPolicySnapshot {
    pub topology: Option<CpuTopology>,
    pub active: Vec<ActiveOsPolicy>,
}

/// Result of a successful apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsPolicyApplyResult {
    pub target: OsPolicyTarget,
    pub executable: Option<String>,
    pub gpu_requires_restart: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_rejects_ambiguous_cpu_controls() {
        let policy = OsSchedulingPolicy {
            cpu_placement: Some(CpuPlacement::Custom(vec![1, 2])),
            affinity: Some(AffinityMask { group: 0, mask: 1 }),
            ..Default::default()
        };
        assert!(
            policy
                .validate_for(&OsPolicyTarget::Process { pid: 42 })
                .is_err()
        );
    }

    #[test]
    fn policy_rejects_process_knobs_on_thread() {
        let policy = OsSchedulingPolicy {
            process_priority: Some(ProcessPriority::High),
            ..Default::default()
        };
        assert!(
            policy
                .validate_for(&OsPolicyTarget::Thread { tid: 42 })
                .is_err()
        );
    }
}
