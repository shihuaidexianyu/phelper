//! Optional PresentMon console capture. No service, driver, overlay injection
//! or hardware-control path. Frame statistics are calculated per swap chain.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub const PRESENTMON_VERSION: &str = "2.5.1";
pub fn helper_path() -> PathBuf {
    // Installed builds carry the pinned console beside the application. Keep
    // the user tools path as a fallback for cargo/portable developer builds.
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        let bundled = directory.join("tools").join("PresentMon-2.5.1-x64.exe");
        if bundled.is_file() {
            return bundled;
        }
    }
    crate::persistence::data_dir()
        .join("tools")
        .join("PresentMon-2.5.1-x64.exe")
}
pub fn reports_dir() -> PathBuf {
    crate::persistence::data_dir().join("measurements")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameSummary {
    pub pid: u32,
    pub swap_chain: String,
    pub frames: usize,
    pub mean_fps: f64,
    /// Reciprocal of the mean of the slowest 1% of present intervals.
    pub one_percent_low_fps: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub other_swap_chains: usize,
    pub rejected_rows: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureReport {
    pub schema_version: u32,
    pub label: String,
    pub executable: String,
    pub process_creation_time: u64,
    pub duration_s: f64,
    pub presentmon_version: String,
    pub frames: FrameSummary,
    pub hardware: Vec<serde_json::Value>,
    pub csv_path: PathBuf,
    pub json_path: PathBuf,
    pub context: serde_json::Value,
}

/// Small CSV tokenizer, including quoted executable names and escaped quotes.
fn row(line: &str) -> Result<Vec<String>, String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.trim_end_matches('\r').chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => {
                fields.push(std::mem::take(&mut field));
            }
            _ => field.push(c),
        }
    }
    if quoted {
        return Err("CSV 引号未闭合".into());
    }
    fields.push(field);
    Ok(fields)
}

pub fn analyze_csv(path: &Path, pid: u32) -> Result<FrameSummary, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len() > 128 * 1024 * 1024 {
        return Err("采集文件超过 128 MB，请缩短采集时间".into());
    }
    analyze(BufReader::new(file), pid)
}

fn analyze(reader: impl BufRead, pid: u32) -> Result<FrameSummary, String> {
    let mut lines = reader.lines();
    let header = row(&lines
        .next()
        .ok_or("没有采集到帧数据")?
        .map_err(|e| e.to_string())?)?;
    let column = |name: &str| {
        header
            .iter()
            .position(|h| h.trim_start_matches('\u{feff}').eq_ignore_ascii_case(name))
    };
    let pid_col = column("ProcessID").ok_or("CSV 缺少 ProcessID")?;
    let chain_col = column("SwapChainAddress").ok_or("CSV 缺少 SwapChainAddress")?;
    let time_col =
        column("MsBetweenPresents").ok_or("需要 PresentMon v1_metrics 的 MsBetweenPresents 列")?;
    let mut groups: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut rejected = 0;
    for line in lines {
        let line = line.map_err(|e| e.to_string())?;
        let Ok(fields) = row(&line) else {
            rejected += 1;
            continue;
        };
        if fields.get(pid_col).and_then(|v| v.parse::<u32>().ok()) != Some(pid) {
            continue;
        }
        let Some(ms) = fields
            .get(time_col)
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
        else {
            rejected += 1;
            continue;
        };
        let Some(chain) = fields.get(chain_col) else {
            rejected += 1;
            continue;
        };
        groups.entry(chain.clone()).or_default().push(ms);
    }
    let other_swap_chains = groups.len().saturating_sub(1);
    let (swap_chain, mut frames) = groups
        .into_iter()
        .max_by_key(|(_, f)| f.len())
        .ok_or("没有有效的呈现帧；请确认目标应用正在绘制画面")?;
    if frames.len() < 100 {
        return Err("有效帧不足 100，无法生成稳定的分位统计".into());
    }
    frames.sort_by(f64::total_cmp);
    let count = frames.len();
    let percentile = |p: f64| {
        frames[((count as f64 * p).ceil() as usize)
            .saturating_sub(1)
            .min(count - 1)]
    };
    let slow_count = (count as f64 * 0.01).ceil() as usize;
    Ok(FrameSummary {
        pid,
        swap_chain,
        frames: count,
        mean_fps: 1000.0 * count as f64 / frames.iter().sum::<f64>(),
        one_percent_low_fps: 1000.0 * slow_count as f64
            / frames[count - slow_count..].iter().sum::<f64>(),
        p95_ms: percentile(0.95),
        p99_ms: percentile(0.99),
        other_swap_chains,
        rejected_rows: rejected,
    })
}

fn helper() -> Command {
    let mut command = Command::new(helper_path());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    command.stdin(Stdio::null());
    command
}

struct OwnedCapture {
    child: std::process::Child,
    session: String,
}
impl Drop for OwnedCapture {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        // Bound graceful termination too; never let a second helper hold
        // application shutdown forever. This exact session belongs to us.
        if let Ok(mut stopper) = helper()
            .args([
                "--session_name",
                &self.session,
                "--terminate_existing_session",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline && matches!(stopper.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = stopper.kill();
            let _ = stopper.wait();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn capture(
    pid: u32,
    seconds: u32,
    label: String,
    telemetry: crate::telemetry::TelemetryHandle,
    context: serde_json::Value,
    stop: Arc<AtomicBool>,
) -> Result<CaptureReport, String> {
    if !(5..=300).contains(&seconds) || pid == 0 {
        return Err("采集时间需为 5–300 秒，且 PID 必须有效".into());
    }
    if !helper_path().is_file() {
        return Err("帧时间采集组件尚未安装，请运行 scripts/install-presentmon.ps1".into());
    }
    let process = crate::os_policy::OsPolicyHandle::new()
        .list_processes()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|p| p.pid == pid)
        .ok_or("目标进程不存在")?;
    let executable = process.executable.ok_or("无法读取目标进程路径")?;
    let creation = process.creation_time.ok_or("无法核验进程创建时间")?;
    let stamp = crate::app::now_epoch_ms();
    std::fs::create_dir_all(reports_dir()).map_err(|e| e.to_string())?;
    let base = reports_dir().join(format!("{stamp}-{pid}"));
    let csv_path = base.with_extension("csv");
    let json_path = base.with_extension("json");
    let log_path = base.with_extension("log");
    let log = std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
    let session = format!("Phelper.{}.{}", std::process::id(), stamp);
    let child = helper()
        .args([
            "--process_id",
            &pid.to_string(),
            "--timed",
            &seconds.to_string(),
            "--terminate_after_timed",
            "--terminate_on_proc_exit",
            "--no_console_stats",
            "--v1_metrics",
            "--no_track_gpu",
            "--no_track_input",
            "--session_name",
            &session,
            "--output_file",
        ])
        .arg(&csv_path)
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut owned = OwnedCapture { child, session };
    let started = Instant::now();
    let mut next_sample = Instant::now();
    let mut hardware = Vec::new();
    let status = loop {
        if let Some(status) = owned.child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if stop.load(Ordering::Acquire)
            || started.elapsed() > Duration::from_secs(u64::from(seconds) + 15)
        {
            return Err("采集已停止；原始文件已保留".into());
        }
        if Instant::now() >= next_sample {
            next_sample = Instant::now() + Duration::from_millis(500);
            let snap = telemetry.snapshot();
            let samples: BTreeMap<_, _> = snap.samples.iter().map(|(id, sample)| (id.0, serde_json::json!({
                "value": sample.value.as_f64(), "age_ms": sample.timestamp.elapsed().as_millis(), "source": format!("{:?}", sample.source)
            }))).collect();
            hardware.push(serde_json::json!({"elapsed_ms": started.elapsed().as_millis(), "samples": samples}));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if !status.success() {
        return Err(format!(
            "帧采集失败 ({status})；详细原因见 {}",
            log_path.display()
        ));
    }
    let frames = analyze_csv(&csv_path, pid)?;
    let report = CaptureReport {
        schema_version: 1,
        label,
        executable,
        process_creation_time: creation,
        duration_s: started.elapsed().as_secs_f64(),
        presentmon_version: PRESENTMON_VERSION.into(),
        frames,
        hardware,
        csv_path,
        json_path: json_path.clone(),
        context,
    };
    let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    crate::persistence::write_atomic(&json_path, &json).map_err(|e| e.to_string())?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn percentiles_use_one_swap_chain_and_slowest_tail_mean() {
        let mut csv = "Application,ProcessID,SwapChainAddress,msBetweenPresents\n".to_string();
        for i in 0..1000 {
            csv.push_str(&format!(
                "\"game, a.exe\",42,A,{}\n",
                if i < 990 { 10 } else { 50 }
            ));
        }
        for _ in 0..50 {
            csv.push_str("overlay.exe,42,B,1\nother.exe,99,C,1\n");
        }
        csv.push_str("game.exe,42,A,NaN\n");
        let result = analyze(std::io::Cursor::new(csv), 42).unwrap();
        assert_eq!(result.frames, 1000);
        assert_eq!(result.swap_chain, "A");
        assert_eq!(result.one_percent_low_fps, 20.0);
        assert_eq!(result.p95_ms, 10.0);
        assert_eq!(result.p99_ms, 10.0);
        assert_eq!(result.other_swap_chains, 1);
        assert_eq!(result.rejected_rows, 1);
    }
    #[test]
    fn missing_schema_and_empty_capture_are_errors() {
        assert!(analyze(std::io::Cursor::new("FPS\n60\n"), 42).is_err());
        assert!(
            analyze(
                std::io::Cursor::new("ProcessID,SwapChainAddress,MsBetweenPresents\n"),
                42
            )
            .is_err()
        );
    }
}
