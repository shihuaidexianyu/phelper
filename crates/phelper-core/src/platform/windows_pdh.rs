//! Windows OS-level counters: PDH (CPU util, disk, network) + memory status.
//!
//! PDH rate counters are computed by the API between two CollectQueryData
//! calls — the first sample after open is expected to be empty (None).

use phelper_domain::error::PlatformError;
use phelper_domain::ports::SystemCounters;
use phelper_domain::telemetry::{ProviderStatus, SystemSample};
use windows::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhGetFormattedCounterValue, PdhOpenQueryW,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::core::PWSTR;

const CPU_PATH: &str = r"\Processor Information(_Total)\% Processor Time";
const DISK_READ_PATH: &str = r"\PhysicalDisk(_Total)\Disk Read Bytes/sec";
const DISK_WRITE_PATH: &str = r"\PhysicalDisk(_Total)\Disk Write Bytes/sec";
const NET_RX_PATH: &str = r"\Network Interface(*)\Bytes Received/sec";
const NET_TX_PATH: &str = r"\Network Interface(*)\Bytes Sent/sec";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub(crate) struct WindowsPdh {
    query: PDH_HQUERY,
    cpu: PDH_HCOUNTER,
    disk_read: PDH_HCOUNTER,
    disk_write: PDH_HCOUNTER,
    net_rx: PDH_HCOUNTER,
    net_tx: PDH_HCOUNTER,
    /// PDH rate counters need a warm-up collect.
    primed: bool,
    degraded: Option<String>,
}

impl WindowsPdh {
    pub(crate) fn new() -> Result<Self, PlatformError> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            let status = PdhOpenQueryW(None, 0, &mut query);
            if status != 0 {
                return Err(PlatformError::Os(format!("PdhOpenQuery: {status}")));
            }
            let add = |q, path: &str| -> Result<PDH_HCOUNTER, PlatformError> {
                let mut counter = PDH_HCOUNTER::default();
                let mut path_w = wide(path);
                let status = PdhAddEnglishCounterW(q, PWSTR(path_w.as_mut_ptr()), 0, &mut counter);
                if status != 0 {
                    return Err(PlatformError::Os(format!("PdhAddCounter {path}: {status}")));
                }
                Ok(counter)
            };
            let s = Self {
                query,
                cpu: add(query, CPU_PATH)?,
                disk_read: add(query, DISK_READ_PATH)?,
                disk_write: add(query, DISK_WRITE_PATH)?,
                net_rx: add(query, NET_RX_PATH)?,
                net_tx: add(query, NET_TX_PATH)?,
                primed: false,
                degraded: None,
            };
            Ok(s)
        }
    }

    fn counter_f64(&self, counter: PDH_HCOUNTER) -> Option<f64> {
        unsafe {
            let mut value = PDH_FMT_COUNTERVALUE::default();
            let status = PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, None, &mut value);
            if status != 0 {
                return None;
            }
            Some(value.Anonymous.doubleValue)
        }
    }

    /// Wildcard counter: sum all instances (loopback/pseudo interfaces are
    /// negligible at this metric's resolution; documented in §12).
    fn counter_array_sum(&self, counter: PDH_HCOUNTER) -> Option<f64> {
        unsafe {
            // First call sizes the buffer (PDH_MORE_DATA expected).
            let mut size: u32 = 0;
            let mut count: u32 = 0;
            let _ =
                PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, None);
            if size == 0 {
                return None;
            }
            let mut buf = vec![0u8; size as usize];
            let status = PdhGetFormattedCounterArrayW(
                counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                Some(buf.as_mut_ptr().cast()),
            );
            if status != 0 {
                return None;
            }
            let items = std::slice::from_raw_parts(
                buf.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W,
                count as usize,
            );
            let mut sum = 0.0;
            for item in items {
                if item.FmtValue.CStatus == 0 {
                    sum += item.FmtValue.Anonymous.doubleValue;
                }
            }
            Some(sum)
        }
    }
}

// SAFETY: PDH query/counter handles are process-global resources; PDH is
// documented thread-safe for queries used from a single thread at a time.
// The M1 coordinator keeps the collector on one thread regardless.
unsafe impl Send for WindowsPdh {}

impl Drop for WindowsPdh {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}

impl SystemCounters for WindowsPdh {
    fn sample(&mut self) -> Result<SystemSample, PlatformError> {
        unsafe {
            let status = PdhCollectQueryData(self.query);
            if status != 0 {
                let detail = format!("PdhCollectQueryData: {status}");
                self.degraded = Some(detail.clone());
                return Err(PlatformError::Os(detail));
            }
        }
        let was_primed = std::mem::replace(&mut self.primed, true);

        let mut mem = MEMORYSTATUSEX::default();
        mem.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        let (mem_total, mem_used) = unsafe {
            if let Err(error) = GlobalMemoryStatusEx(&mut mem) {
                let detail = format!("GlobalMemoryStatusEx: {error}");
                self.degraded = Some(detail.clone());
                return Err(PlatformError::Os(detail));
            }
            let total = mem.ullTotalPhys;
            (Some(total), Some(total - mem.ullAvailPhys))
        };

        let rate = |v: Option<f64>| was_primed.then_some(v).flatten();
        self.degraded = None;
        Ok(SystemSample {
            cpu_util_percent: rate(self.counter_f64(self.cpu)),
            mem_used_bytes: mem_used,
            mem_total_bytes: mem_total,
            disk_read_bps: rate(self.counter_f64(self.disk_read)),
            disk_write_bps: rate(self.counter_f64(self.disk_write)),
            net_rx_bps: rate(self.counter_array_sum(self.net_rx)),
            net_tx_bps: rate(self.counter_array_sum(self.net_tx)),
        })
    }

    fn status(&self) -> ProviderStatus {
        match &self.degraded {
            None => ProviderStatus::Ok,
            Some(d) => ProviderStatus::Degraded(d.clone()),
        }
    }
}
