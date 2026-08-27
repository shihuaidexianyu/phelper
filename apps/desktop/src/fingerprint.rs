//! Visual fingerprint (v0.2-d): one u64 per 50 ms tick describing
//! everything the CURRENT page could paint differently. The ticker skips
//! `cx.notify()` when the fingerprint is unchanged — a paint is a full
//! element-tree rebuild + DX11 frame (v0.1 measured ≈17 ms, 4×/s = the
//! idle-CPU driver). Interactive events (clicks, drags, input edits) call
//! `cx.notify()` in their own handlers, so the skip only gates
//! telemetry/knob/journal-driven repaints. A 5 s forced-refresh backstop
//! in the ticker fails OPEN toward painting: a display value the
//! fingerprint forgot freezes for at most 5 s, never permanently.
//!
//! Time-throttled pages (Dashboard/Performance/Monitor) fold a
//! 1 s bucket into the hash — their noisy displays (RPM jitter, 1 Hz charts)
//! repaint at ≤1 Hz instead of 4 Hz.
//! Static pages (Profiles/Settings) repaint only when an
//! actual displayed value changes.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use phelper_core::telemetry::registry;
use phelper_domain::telemetry::ids;

use crate::pages::PageId;
use crate::shell::ShellView;

/// Quantize an optional f64 at the page's display precision (step =
/// 1/scale); None hashes distinctly from every number ("—" rows).
fn q(v: Option<f64>, scale: f64) -> i64 {
    match v {
        Some(x) => (x * scale).round() as i64,
        None => i64::MIN,
    }
}

impl ShellView {
    /// Fingerprint of everything the current page renders from AppState.
    /// Noisy telemetry values are NOT hashed on bucket pages — there the
    /// 1 s bucket is the display cadence (the page repaints ≤1 Hz and
    /// shows the value current at that moment); only structural changes
    /// (quality/source/stale, observed, knobs, journal) flip the hash
    /// between bucket boundaries. Static pages hash their few self-moving
    /// values directly and repaint only on real change.
    pub(crate) fn fingerprint(&self) -> u64 {
        let s = &self.state;
        let mut h = DefaultHasher::new();
        std::mem::discriminant(&self.page).hash(&mut h);

        // ---- global bits (banners / control state) ----------------------
        format!("{:?}", s.engine).hash(&mut h);
        s.desired.profile.hash(&mut h);
        format!("{:?}", s.knobs).hash(&mut h);
        s.evidence.back().map(|r| r.at_epoch_ms).hash(&mut h);
        format!("{:?}", s.caps).hash(&mut h);
        format!("{:?}", s.observed).hash(&mut h);
        format!("{:?}", s.last_saved_fan_curve).hash(&mut h);
        format!("{:?}", s.experimental).hash(&mut h);

        let snap = s.telemetry.as_deref();
        let tv = |id: phelper_domain::telemetry::MetricId| {
            snap.and_then(|x| x.samples.get(&id))
                .and_then(|x| x.value.as_f64())
        };
        // 1 s time bucket: included by pages whose displayed content moves
        // even when no value "changes" (chart scroll and noisy readouts) —
        // throttles them to ≤1 Hz repaints.
        let bucket = phelper_core::app::now_epoch_ms() / 1000;

        match self.page {
            PageId::Dashboard => {
                // Noisy values (MHz/util/temp/power bounce every 50 ms
                // tick even at display precision) are deliberately NOT
                // hashed — the 1 s bucket below is their refresh cadence:
                // the page repaints ≤1 Hz and the cards/charts show the
                // value current at that moment. Only structural changes
                // (quality/source loss, charger events, slow battery drain)
                // force an immediate repaint.
                for id in [
                    ids::CPU_PKG_TEMP_C,
                    ids::CPU_PKG_POWER_W,
                    ids::CPU_UTIL_PERCENT,
                    ids::GPU_TEMP_C,
                    ids::GPU_POWER_W,
                    ids::GPU_UTIL_PERCENT,
                    ids::FAN_CPU_RPM,
                    ids::FAN_GPU_RPM,
                    ids::FRAME_DISPLAYED_FPS,
                    ids::FRAME_ONE_PERCENT_LOW_FPS,
                ] {
                    snap.and_then(|x| x.samples.get(&id))
                        .map(|x| format!("{:?}-{:?}", x.quality, x.source))
                        .hash(&mut h);
                }
                q(tv(ids::POWER_AC_ONLINE), 1.).hash(&mut h);
                q(tv(ids::POWER_BATTERY_PERCENT), 1.).hash(&mut h);
                self.dash.temp_chart.revision().hash(&mut h);
                self.dash.power_chart.revision().hash(&mut h);
                bucket.hash(&mut h); // value + chart refresh cadence
            }
            PageId::Performance => {
                // Sliders/inputs are event-driven; fan RPM and the trend
                // charts move independently and therefore share Dashboard's
                // one-second display cadence.
                q(tv(ids::CPU_PL1_W), 1.).hash(&mut h);
                q(tv(ids::CPU_PL2_W), 1.).hash(&mut h);
                q(tv(ids::CPU_PL4_W), 1.).hash(&mut h);
                self.thermal
                    .as_ref()
                    .and_then(|thermal| thermal.fan_sliders.as_ref())
                    .is_some()
                    .hash(&mut h);
                self.thermal
                    .as_ref()
                    .map(|thermal| thermal.cpu_rpm)
                    .hash(&mut h);
                self.thermal
                    .as_ref()
                    .map(|thermal| thermal.gpu_rpm)
                    .hash(&mut h);
                // Live RPM numbers ride the 1 s bucket (jittery while
                // spinning); quality/source changes flip immediately.
                for id in [ids::FAN_CPU_RPM, ids::FAN_GPU_RPM] {
                    snap.and_then(|x| x.samples.get(&id))
                        .map(|x| format!("{:?}-{:?}", x.quality, x.source))
                        .hash(&mut h);
                }
                bucket.hash(&mut h); // live RPM labels
            }
            PageId::Profiles => {
                format!("{:?}", s.profiles).hash(&mut h);
                s.profile_warnings.hash(&mut h);
            }
            PageId::Monitor => {
                for meta in registry::all() {
                    if !crate::pages::monitor::is_monitor_metric(meta.id) {
                        continue;
                    }
                    let sample = snap.and_then(|x| x.samples.get(&meta.id));
                    meta.id.0.hash(&mut h);
                    sample
                        .map(|x| format!("{:?}-{:?}", x.quality, x.source))
                        .hash(&mut h);
                }
                bucket.hash(&mut h); // values + stale state
            }
            PageId::Settings => {}
        }
        h.finish()
    }
}
