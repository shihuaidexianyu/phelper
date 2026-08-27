//! PresentMonAPI2 adapter (read-only frame telemetry).
//!
//! PresentMon is intentionally an optional runtime dependency.  The adapter
//! loads `PresentMonAPI2.dll` dynamically, attaches only to the PID supplied
//! by `PHELPER_PRESENTMON_PID`, and turns frame-query records into small
//! typed batches.  A missing PID, DLL, service, or target process is an
//! ordinary provider-unavailable/degraded state; it never prevents the rest
//! of the engine from starting.

#![allow(clippy::missing_transmute_annotations)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use phelper_domain::error::PlatformError;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::PCWSTR;

const PM_STATUS_SUCCESS: i32 = 0;
const PM_STATUS_SERVICE_ERROR: i32 = 4;
const PM_STATUS_INVALID_PID: i32 = 6;
const PM_STATUS_PIPE_ERROR: i32 = 12;
const PM_STATUS_SESSION_NOT_OPEN: i32 = 13;
const PM_STATUS_MIDDLEWARE_MISSING_ENDPOINT: i32 = 17;
const PM_STATUS_MIDDLEWARE_SERVICE_MISMATCH: i32 = 20;

const PM_METRIC_CPU_BUSY: i32 = 9;
const PM_METRIC_GPU_TIME: i32 = 13;
const PM_METRIC_DISPLAY_LATENCY: i32 = 24;
const PM_METRIC_FRAME_TYPE: i32 = 63;
const PM_METRIC_DISPLAYED_FRAME_TIME: i32 = 85;

const PM_STAT_NONE: i32 = 0;
const PM_FRAME_TYPE_APPLICATION: i32 = 2;
const PM_FRAME_TYPE_REPEATED: i32 = 3;

const MAX_FRAMES_PER_BATCH: u32 = 256;
const MAX_DRAIN_BATCHES: usize = 4;
const MAX_FRAME_BLOB_SIZE: usize = 1024 * 1024;

type PmSessionHandle = *mut c_void;
type PmFrameQueryHandle = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PmQueryElement {
    metric: i32,
    stat: i32,
    device_id: u32,
    array_index: u32,
    data_offset: u64,
    data_size: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PmVersion {
    major: u16,
    minor: u16,
    patch: u16,
    tag: [u8; 22],
    hash: [u8; 8],
    config: [u8; 4],
}

type PmOpenSession = unsafe extern "C" fn(*mut PmSessionHandle) -> i32;
type PmCloseSession = unsafe extern "C" fn(PmSessionHandle) -> i32;
type PmStartTrackingProcess = unsafe extern "C" fn(PmSessionHandle, u32) -> i32;
type PmStopTrackingProcess = unsafe extern "C" fn(PmSessionHandle, u32) -> i32;
type PmRegisterFrameQuery = unsafe extern "C" fn(
    PmSessionHandle,
    *mut PmFrameQueryHandle,
    *mut PmQueryElement,
    u64,
    *mut u32,
) -> i32;
type PmFreeFrameQuery = unsafe extern "C" fn(PmFrameQueryHandle) -> i32;
type PmConsumeFrames = unsafe extern "C" fn(PmFrameQueryHandle, u32, *mut u8, *mut u32) -> i32;
type PmGetApiVersion = unsafe extern "C" fn(*mut PmVersion) -> i32;

struct PresentMonApi {
    // Keep the DLL loaded for as long as any function pointer can be used.
    _lib: HMODULE,
    open_session: PmOpenSession,
    close_session: PmCloseSession,
    start_tracking_process: PmStartTrackingProcess,
    stop_tracking_process: PmStopTrackingProcess,
    register_frame_query: PmRegisterFrameQuery,
    free_frame_query: PmFreeFrameQuery,
    consume_frames: PmConsumeFrames,
    get_api_version: PmGetApiVersion,
}

// SAFETY: the PresentMon C API handles are opaque and all calls are
// serialized by the telemetry coordinator.  The DLL remains loaded in this
// process for the lifetime of the adapter, matching the other hand-rolled
// Windows FFI adapters in this crate.
unsafe impl Send for PresentMonApi {}

impl PresentMonApi {
    fn load() -> Result<Self, PlatformError> {
        let lib = load_library()?;
        unsafe {
            macro_rules! required {
                ($name:literal, $ty:ty, $what:literal) => {{
                    let p = GetProcAddress(lib, windows::core::s!($name))
                        .ok_or(PlatformError::NotAvailable($what))?;
                    std::mem::transmute::<_, $ty>(p)
                }};
            }

            let api = Self {
                _lib: lib,
                open_session: required!("pmOpenSession", PmOpenSession, "pmOpenSession missing"),
                close_session: required!(
                    "pmCloseSession",
                    PmCloseSession,
                    "pmCloseSession missing"
                ),
                start_tracking_process: required!(
                    "pmStartTrackingProcess",
                    PmStartTrackingProcess,
                    "pmStartTrackingProcess missing"
                ),
                stop_tracking_process: required!(
                    "pmStopTrackingProcess",
                    PmStopTrackingProcess,
                    "pmStopTrackingProcess missing"
                ),
                register_frame_query: required!(
                    "pmRegisterFrameQuery",
                    PmRegisterFrameQuery,
                    "pmRegisterFrameQuery missing"
                ),
                free_frame_query: required!(
                    "pmFreeFrameQuery",
                    PmFreeFrameQuery,
                    "pmFreeFrameQuery missing"
                ),
                consume_frames: required!(
                    "pmConsumeFrames",
                    PmConsumeFrames,
                    "pmConsumeFrames missing"
                ),
                get_api_version: required!(
                    "pmGetApiVersion",
                    PmGetApiVersion,
                    "pmGetApiVersion missing"
                ),
            };

            let mut version = PmVersion::default();
            check_status((api.get_api_version)(&mut version), "pmGetApiVersion")?;
            if version.major < 3 {
                return Err(PlatformError::NotAvailable(
                    "PresentMon API v3 or newer required",
                ));
            }
            Ok(api)
        }
    }
}

fn load_library() -> Result<HMODULE, PlatformError> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("PHELPER_PRESENTMON_DLL") {
        candidates.push(PathBuf::from(path));
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        let root = PathBuf::from(program_files)
            .join("Intel")
            .join("PresentMon");
        candidates.push(root.join("PresentMonAPI2.dll"));
        candidates.push(root.join("SDK").join("PresentMonAPI2.dll"));
    }
    // The final name lets Windows use its normal DLL search path, including a
    // service installation that placed the API beside the current process.
    candidates.push(PathBuf::from("PresentMonAPI2.dll"));

    for path in candidates {
        let wide = wide_path(&path);
        // SAFETY: `wide` is NUL-terminated and lives through the call.
        if let Ok(lib) = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) } {
            return Ok(lib);
        }
    }
    Err(PlatformError::NotAvailable(
        "PresentMonAPI2.dll not found (install the PresentMon service or set PHELPER_PRESENTMON_DLL)",
    ))
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn check_status(status: i32, operation: &'static str) -> Result<(), PlatformError> {
    if status == PM_STATUS_SUCCESS {
        return Ok(());
    }
    match status {
        PM_STATUS_SERVICE_ERROR
        | PM_STATUS_PIPE_ERROR
        | PM_STATUS_SESSION_NOT_OPEN
        | PM_STATUS_MIDDLEWARE_MISSING_ENDPOINT
        | PM_STATUS_MIDDLEWARE_SERVICE_MISMATCH => Err(PlatformError::NotAvailable(
            "PresentMon service unavailable",
        )),
        PM_STATUS_INVALID_PID => Err(PlatformError::NotAvailable(
            "PresentMon target PID is invalid or has exited",
        )),
        _ => Err(PlatformError::Driver(format!(
            "PresentMon {operation} failed (PM_STATUS={status})"
        ))),
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FrameEvent {
    pub frame_type: Option<i32>,
    pub cpu_busy_ms: Option<f64>,
    pub gpu_time_ms: Option<f64>,
    pub display_latency_ms: Option<f64>,
    pub displayed_frame_time_ms: Option<f64>,
}

#[derive(Debug)]
pub(crate) struct FrameBatch {
    pub elapsed: Duration,
    pub events: Vec<FrameEvent>,
}

#[derive(Clone, Copy)]
struct Field {
    offset: usize,
    size: usize,
}

struct FrameLayout {
    frame_type: Option<Field>,
    cpu_busy: Option<Field>,
    gpu_time: Option<Field>,
    display_latency: Option<Field>,
    displayed_frame_time: Option<Field>,
    blob_size: usize,
}

impl FrameLayout {
    fn from_elements(elements: &[PmQueryElement], blob_size: u32) -> Result<Self, PlatformError> {
        let field = |metric: i32| -> Result<Field, PlatformError> {
            let element = elements
                .iter()
                .find(|element| element.metric == metric)
                .ok_or(PlatformError::Data(
                    "PresentMon query layout missing a required field",
                ))?;
            let offset = usize::try_from(element.data_offset)
                .map_err(|_| PlatformError::Data("PresentMon query offset overflow"))?;
            let size = usize::try_from(element.data_size)
                .map_err(|_| PlatformError::Data("PresentMon query size overflow"))?;
            if size == 0 {
                return Err(PlatformError::Data(
                    "PresentMon query returned an empty field",
                ));
            }
            Ok(Field { offset, size })
        };
        let optional_field = |metric: i32| {
            elements
                .iter()
                .find(|element| element.metric == metric)
                .and_then(|element| {
                    let offset = usize::try_from(element.data_offset).ok()?;
                    let size = usize::try_from(element.data_size).ok()?;
                    (size > 0).then_some(Field { offset, size })
                })
        };

        let blob_size = usize::try_from(blob_size)
            .map_err(|_| PlatformError::Data("PresentMon blob size overflow"))?;
        if blob_size == 0 {
            return Err(PlatformError::Data("PresentMon returned a zero-sized blob"));
        }
        if blob_size > MAX_FRAME_BLOB_SIZE {
            return Err(PlatformError::Data("PresentMon returned an oversized blob"));
        }
        Ok(Self {
            frame_type: optional_field(PM_METRIC_FRAME_TYPE),
            cpu_busy: optional_field(PM_METRIC_CPU_BUSY),
            gpu_time: optional_field(PM_METRIC_GPU_TIME),
            display_latency: optional_field(PM_METRIC_DISPLAY_LATENCY),
            displayed_frame_time: Some(field(PM_METRIC_DISPLAYED_FRAME_TIME)?),
            blob_size,
        })
    }
}

pub(crate) struct PresentMonSource {
    api: PresentMonApi,
    session: PmSessionHandle,
    query: PmFrameQueryHandle,
    pid: u32,
    layout: FrameLayout,
    last_poll: Option<Instant>,
}

// SAFETY: this value is moved into, and used only by, the telemetry
// coordinator thread.  PresentMon calls are never made concurrently here.
unsafe impl Send for PresentMonSource {}

impl PresentMonSource {
    pub(crate) fn open() -> Result<Self, PlatformError> {
        let pid = std::env::var("PHELPER_PRESENTMON_PID")
            .map_err(|_| PlatformError::NotAvailable("PHELPER_PRESENTMON_PID is not set"))?
            .parse::<u32>()
            .map_err(|_| {
                PlatformError::Driver("PHELPER_PRESENTMON_PID must be a positive integer".into())
            })?;
        if pid == 0 {
            return Err(PlatformError::Driver(
                "PHELPER_PRESENTMON_PID must be a positive integer".into(),
            ));
        }

        let api = PresentMonApi::load()?;
        unsafe {
            let mut session = std::ptr::null_mut();
            check_status((api.open_session)(&mut session), "pmOpenSession")?;
            if session.is_null() {
                return Err(PlatformError::Data("PresentMon returned a null session"));
            }

            if let Err(e) = check_status(
                (api.start_tracking_process)(session, pid),
                "pmStartTrackingProcess",
            ) {
                let _ = (api.close_session)(session);
                return Err(e);
            }

            let query = match register_best_query(&api, session) {
                Ok(query) => query,
                Err(e) => {
                    let _ = (api.stop_tracking_process)(session, pid);
                    let _ = (api.close_session)(session);
                    return Err(e);
                }
            };

            Ok(Self {
                api,
                session,
                query: query.handle,
                pid,
                layout: query.layout,
                last_poll: None,
            })
        }
    }

    pub(crate) fn poll(&mut self) -> Result<FrameBatch, PlatformError> {
        let now = Instant::now();
        let elapsed = self
            .last_poll
            .replace(now)
            .map(|previous| now.saturating_duration_since(previous))
            .unwrap_or_default();
        let mut events = Vec::new();
        let blob_len = self
            .layout
            .blob_size
            .checked_mul(MAX_FRAMES_PER_BATCH as usize)
            .ok_or(PlatformError::Data("PresentMon frame buffer size overflow"))?;
        let mut blob = vec![0u8; blob_len];

        for _ in 0..MAX_DRAIN_BATCHES {
            let mut count = MAX_FRAMES_PER_BATCH;
            // SAFETY: the query belongs to this source, `blob` has capacity
            // for `count` records of the registered size, and all calls are
            // serialized on the coordinator thread.
            unsafe {
                check_status(
                    (self.api.consume_frames)(self.query, self.pid, blob.as_mut_ptr(), &mut count),
                    "pmConsumeFrames",
                )?;
            }
            if count == 0 {
                break;
            }
            let count = usize::try_from(count)
                .map_err(|_| PlatformError::Data("PresentMon frame count overflow"))?;
            for index in 0..count {
                let start = index
                    .checked_mul(self.layout.blob_size)
                    .ok_or(PlatformError::Data("PresentMon frame offset overflow"))?;
                let end = start
                    .checked_add(self.layout.blob_size)
                    .ok_or(PlatformError::Data("PresentMon frame size overflow"))?;
                let record = blob
                    .get(start..end)
                    .ok_or(PlatformError::Data("PresentMon frame record out of bounds"))?;
                events.push(self.decode_event(record)?);
            }
            if count < MAX_FRAMES_PER_BATCH as usize {
                break;
            }
        }

        Ok(FrameBatch { elapsed, events })
    }

    fn decode_event(&self, record: &[u8]) -> Result<FrameEvent, PlatformError> {
        let read_f64 = |field: Option<Field>| -> Result<Option<f64>, PlatformError> {
            let Some(field) = field else { return Ok(None) };
            if field.size < size_of::<f64>() {
                return Err(PlatformError::Data("PresentMon f64 field is too small"));
            }
            let end = field
                .offset
                .checked_add(size_of::<f64>())
                .ok_or(PlatformError::Data("PresentMon f64 field offset overflow"))?;
            let bytes = record
                .get(field.offset..end)
                .ok_or(PlatformError::Data("PresentMon f64 field out of bounds"))?;
            let mut value = [0u8; size_of::<f64>()];
            value.copy_from_slice(bytes);
            Ok(Some(f64::from_ne_bytes(value)))
        };
        let frame_type = if let Some(field) = self.layout.frame_type {
            if field.size < size_of::<i32>() {
                return Err(PlatformError::Data(
                    "PresentMon frame-type field is too small",
                ));
            }
            let end = field
                .offset
                .checked_add(size_of::<i32>())
                .ok_or(PlatformError::Data("PresentMon frame-type offset overflow"))?;
            let bytes = record.get(field.offset..end).ok_or(PlatformError::Data(
                "PresentMon frame-type field out of bounds",
            ))?;
            let mut value = [0u8; size_of::<i32>()];
            value.copy_from_slice(bytes);
            Some(i32::from_ne_bytes(value))
        } else {
            None
        };
        Ok(FrameEvent {
            frame_type,
            cpu_busy_ms: read_f64(self.layout.cpu_busy)?,
            gpu_time_ms: read_f64(self.layout.gpu_time)?,
            display_latency_ms: read_f64(self.layout.display_latency)?,
            displayed_frame_time_ms: read_f64(self.layout.displayed_frame_time)?,
        })
    }
}

impl Drop for PresentMonSource {
    fn drop(&mut self) {
        // SAFETY: handles and function pointers were created by the same API
        // instance; teardown is serialized because the source is owned by the
        // coordinator thread during normal shutdown.
        unsafe {
            if !self.query.is_null() {
                let _ = (self.api.free_frame_query)(self.query);
            }
            if !self.session.is_null() {
                let _ = (self.api.stop_tracking_process)(self.session, self.pid);
                let _ = (self.api.close_session)(self.session);
            }
        }
    }
}

struct RegisteredQuery {
    handle: PmFrameQueryHandle,
    layout: FrameLayout,
}

fn register_best_query(
    api: &PresentMonApi,
    session: PmSessionHandle,
) -> Result<RegisteredQuery, PlatformError> {
    // Frame type and the optional timing metrics are progressively removed so
    // an older/service-configured PresentMon still provides the core frame
    // time/FPS signal.  `DISPLAYED_FRAME_TIME` is the one required metric.
    let candidates: &[&[i32]] = &[
        &[
            PM_METRIC_FRAME_TYPE,
            PM_METRIC_CPU_BUSY,
            PM_METRIC_GPU_TIME,
            PM_METRIC_DISPLAY_LATENCY,
            PM_METRIC_DISPLAYED_FRAME_TIME,
        ],
        &[
            PM_METRIC_CPU_BUSY,
            PM_METRIC_GPU_TIME,
            PM_METRIC_DISPLAY_LATENCY,
            PM_METRIC_DISPLAYED_FRAME_TIME,
        ],
        &[
            PM_METRIC_CPU_BUSY,
            PM_METRIC_GPU_TIME,
            PM_METRIC_DISPLAYED_FRAME_TIME,
        ],
        &[PM_METRIC_DISPLAYED_FRAME_TIME],
    ];
    let mut last_error = None;

    for metrics in candidates {
        let mut elements: Vec<PmQueryElement> = metrics
            .iter()
            .map(|&metric| PmQueryElement {
                metric,
                stat: PM_STAT_NONE,
                ..Default::default()
            })
            .collect();
        let mut handle = std::ptr::null_mut();
        let mut blob_size = 0u32;
        let status = unsafe {
            (api.register_frame_query)(
                session,
                &mut handle,
                elements.as_mut_ptr(),
                elements.len() as u64,
                &mut blob_size,
            )
        };
        if status != PM_STATUS_SUCCESS {
            if !handle.is_null() {
                unsafe {
                    let _ = (api.free_frame_query)(handle);
                }
            }
            last_error = Some(status);
            continue;
        }
        if handle.is_null() {
            last_error = Some(-1);
            continue;
        }
        match FrameLayout::from_elements(&elements, blob_size) {
            Ok(layout) => {
                return Ok(RegisteredQuery { handle, layout });
            }
            Err(e) => {
                unsafe {
                    let _ = (api.free_frame_query)(handle);
                }
                return Err(e);
            }
        }
    }

    Err(last_error
        .map(|status| {
            PlatformError::Driver(format!(
                "PresentMon could not register a frame query (PM_STATUS={status})"
            ))
        })
        .unwrap_or(PlatformError::Data(
            "PresentMon could not register a frame query",
        )))
}

/// A one-second rolling frame-time window used for the derived 1% low FPS
/// metric.  Events are timestamped at collection time; PresentMon's raw
/// frame-time values remain the authoritative per-frame values.
#[derive(Default)]
pub(crate) struct FrameWindow {
    values: VecDeque<(Instant, f64)>,
}

impl FrameWindow {
    pub(crate) fn push_batch(&mut self, now: Instant, events: &[FrameEvent]) {
        for event in events {
            if !is_application_frame(event.frame_type) {
                continue;
            }
            if let Some(ms) = event.displayed_frame_time_ms
                && ms.is_finite()
                && ms > 0.0
            {
                self.values.push_back((now, ms));
            }
        }
        while self
            .values
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > Duration::from_secs(1))
        {
            self.values.pop_front();
        }
    }

    pub(crate) fn one_percent_low_fps(&self) -> Option<f64> {
        if self.values.len() < 10 {
            return None;
        }
        let mut frame_times: Vec<f64> = self.values.iter().map(|(_, ms)| *ms).collect();
        frame_times.sort_by(f64::total_cmp);
        let index = ((frame_times.len() as f64 * 0.99).ceil() as usize)
            .saturating_sub(1)
            .min(frame_times.len() - 1);
        let worst_ms = frame_times[index];
        (worst_ms > 0.0).then_some(1000.0 / worst_ms)
    }
}

pub(crate) fn is_application_frame(frame_type: Option<i32>) -> bool {
    !matches!(frame_type, Some(PM_FRAME_TYPE_REPEATED))
        && frame_type.is_none_or(|kind| kind == PM_FRAME_TYPE_APPLICATION || kind == 0 || kind == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(frame_type: Option<i32>, frame_time_ms: f64) -> FrameEvent {
        FrameEvent {
            frame_type,
            displayed_frame_time_ms: Some(frame_time_ms),
            ..Default::default()
        }
    }

    #[test]
    fn repeated_frames_are_excluded_from_low_fps_window() {
        let now = Instant::now();
        let mut window = FrameWindow::default();
        let mut events = vec![event(Some(PM_FRAME_TYPE_REPEATED), 100.0)];
        events.extend((0..10).map(|_| event(Some(PM_FRAME_TYPE_APPLICATION), 10.0)));
        window.push_batch(now, &events);
        assert_eq!(window.values.len(), 10);
        assert!((window.one_percent_low_fps().expect("low fps") - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn unknown_frame_types_fail_closed_for_application_metrics() {
        assert!(is_application_frame(None));
        assert!(is_application_frame(Some(0)));
        assert!(is_application_frame(Some(1)));
        assert!(is_application_frame(Some(PM_FRAME_TYPE_APPLICATION)));
        assert!(!is_application_frame(Some(PM_FRAME_TYPE_REPEATED)));
        assert!(!is_application_frame(Some(50)));
    }
}
