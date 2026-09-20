//! MUX selection persists across reboot. A same-boot read never completes
//! verification. Desktop writes remain gated until both directions are proven.
use phelper_domain::{error::ControlError, policy::MuxMode};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pending {
    previous: MuxMode,
    requested: MuxMode,
    boot: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Record {
    board: String,
    bios: String,
    pending: Option<Pending>,
    verified_hybrid: bool,
    verified_discrete: bool,
}

pub(super) struct MuxLifecycle {
    path: PathBuf,
    record: Record,
    boot: Option<String>,
    pub status: String,
    pub writable: bool,
    blocked: bool,
}

impl MuxLifecycle {
    pub fn open(journal: &Path, board: &str, bios: &str, actual: Option<MuxMode>) -> Self {
        Self::open_with_boot(journal, board, bios, actual, boot_identity())
    }
    fn open_with_boot(
        journal: &Path,
        board: &str,
        bios: &str,
        actual: Option<MuxMode>,
        boot: Option<String>,
    ) -> Self {
        let path = journal.with_extension("mux.json");
        let loaded = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Record>(&text)
                .map(Some)
                .map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        };
        let mut lifecycle = Self {
            path,
            record: Record {
                board: board.into(),
                bios: bios.into(),
                ..Default::default()
            },
            boot,
            status: "尚未完成本机双向切换与重启验证；完成开发验证后再开放写入。".into(),
            writable: false,
            blocked: false,
        };
        match loaded {
            Ok(Some(record)) if record.board == board && record.bios == bios => {
                lifecycle.record = record
            }
            Ok(Some(record)) if record.pending.is_some() => {
                lifecycle.blocked = true;
                lifecycle.status = "待验证记录的主板或 BIOS 已改变，请人工核验显卡模式。".into();
            }
            Ok(_) => {}
            Err(e) => {
                lifecycle.blocked = true;
                lifecycle.status = format!("显卡切换记录无法读取：{e}");
            }
        }
        if let Some(pending) = lifecycle.record.pending.clone() {
            if lifecycle
                .boot
                .as_ref()
                .is_some_and(|boot| *boot != pending.boot)
            {
                if actual == Some(pending.requested) {
                    match pending.requested {
                        MuxMode::Hybrid => lifecycle.record.verified_hybrid = true,
                        MuxMode::Discrete => lifecycle.record.verified_discrete = true,
                        _ => {}
                    }
                    lifecycle.record.pending = None;
                    lifecycle.status = format!("已在重启后确认：{:?}", pending.requested);
                    if let Err(e) = lifecycle.save() {
                        lifecycle.blocked = true;
                        lifecycle.status = e.to_string();
                    }
                } else {
                    lifecycle.status = format!(
                        "重启后未匹配目标 {:?}；固件报告 {:?}，可申请恢复 {:?}。",
                        pending.requested, actual, pending.previous
                    );
                }
            } else {
                lifecycle.status = format!(
                    "已请求 {:?}，等待重启后核验；原选择 {:?}。",
                    pending.requested, pending.previous
                );
            }
        }
        lifecycle.writable = !lifecycle.blocked
            && lifecycle.record.verified_hybrid
            && lifecycle.record.verified_discrete;
        if lifecycle.writable && lifecycle.record.pending.is_none() {
            lifecycle.status = "本机混合/独显双向重启验证已完成。".into();
        }
        lifecycle
    }

    pub fn prepare(&mut self, current: MuxMode, target: MuxMode) -> Result<(), ControlError> {
        if self.blocked {
            return Err(ControlError::UnsafeRequest {
                reason: self.status.clone(),
            });
        }
        if !matches!(target, MuxMode::Hybrid | MuxMode::Discrete) {
            return Err(ControlError::Unsupported);
        }
        let boot = self
            .boot
            .clone()
            .ok_or_else(|| ControlError::BackendUnavailable {
                what: "无法读取 Windows 启动标识，不能验证跨重启切换".into(),
            })?;
        let previous = self.record.pending.as_ref().map_or(current, |p| p.previous);
        if !matches!(current, MuxMode::Hybrid | MuxMode::Discrete) {
            return Err(ControlError::Unsupported);
        }
        if self.record.pending.is_none() && current == target {
            return Err(ControlError::UnsafeRequest {
                reason: "固件已报告该模式，无需再次请求".into(),
            });
        }
        let old = self.record.pending.clone();
        self.record.pending = Some(Pending {
            previous,
            requested: target,
            boot,
        });
        if let Err(error) = self.save() {
            self.record.pending = old;
            return Err(error);
        }
        self.status = format!(
            "已记录 {:?} 请求；需要重启后核验。原选择 {:?}。",
            target, previous
        );
        Ok(())
    }
    fn save(&self) -> Result<(), ControlError> {
        let text = serde_json::to_string_pretty(&self.record).map_err(|e| {
            ControlError::BackendUnavailable {
                what: e.to_string(),
            }
        })?;
        crate::persistence::write_atomic(&self.path, &text).map_err(|e| {
            ControlError::BackendUnavailable {
                what: e.to_string(),
            }
        })
    }
}

#[cfg(windows)]
fn boot_identity() -> Option<String> {
    #[derive(Deserialize)]
    struct Boot {
        #[serde(rename = "LastBootUpTime")]
        time: String,
    }
    wmi::WMIConnection::new()
        .ok()?
        .raw_query::<Boot>("SELECT LastBootUpTime FROM Win32_OperatingSystem")
        .ok()?
        .into_iter()
        .next()
        .map(|b| b.time)
}
#[cfg(not(windows))]
fn boot_identity() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_boot_cannot_verify_and_both_directions_gate_desktop() {
        let dir = std::env::temp_dir().join(format!("phelper-mux-life-{}", std::process::id()));
        let path = dir.join("journal.jsonl");
        let _ = std::fs::remove_file(path.with_extension("mux.json"));
        let mut first = MuxLifecycle::open_with_boot(
            &path,
            "8BAB",
            "F.30",
            Some(MuxMode::Hybrid),
            Some("boot1".into()),
        );
        first.prepare(MuxMode::Hybrid, MuxMode::Discrete).unwrap();
        let same = MuxLifecycle::open_with_boot(
            &path,
            "8BAB",
            "F.30",
            Some(MuxMode::Discrete),
            Some("boot1".into()),
        );
        assert!(same.record.pending.is_some());
        assert!(!same.writable);
        let mut next = MuxLifecycle::open_with_boot(
            &path,
            "8BAB",
            "F.30",
            Some(MuxMode::Discrete),
            Some("boot2".into()),
        );
        assert!(next.record.pending.is_none());
        assert!(!next.writable);
        next.prepare(MuxMode::Discrete, MuxMode::Hybrid).unwrap();
        let done = MuxLifecycle::open_with_boot(
            &path,
            "8BAB",
            "F.30",
            Some(MuxMode::Hybrid),
            Some("boot3".into()),
        );
        assert!(done.writable);
        let reopened = MuxLifecycle::open_with_boot(
            &path,
            "8BAB",
            "F.30",
            Some(MuxMode::Hybrid),
            Some("boot3".into()),
        );
        assert!(reopened.writable);
        assert!(reopened.status.contains("已完成"));
        let changed = MuxLifecycle::open_with_boot(
            &path,
            "8BAB",
            "F.31",
            Some(MuxMode::Hybrid),
            Some("boot4".into()),
        );
        assert!(!changed.writable);
    }
}
