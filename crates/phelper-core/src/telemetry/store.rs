//! Telemetry store: bounded per-metric ring buffers + latest snapshot.
//! Owned by the coordinator; read through TelemetryHandle (RwLock).

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::time::Duration;

use phelper_domain::telemetry::{
    MetricId, MetricSample, ProviderStatus, TelemetrySnapshot, WindowStats,
};

/// Per-metric capacity. Worst cadence is 250 ms → 30 min ≈ 7200 samples;
/// 8192 covers it with headroom and bounds memory (~30 metrics × ~48 B).
const RING_CAPACITY: usize = 8192;

#[derive(Default)]
pub(crate) struct TelemetryStore {
    rings: BTreeMap<MetricId, VecDeque<MetricSample>>,
    providers: BTreeMap<&'static str, ProviderStatus>,
    /// Worst scheduling lateness per collector since start (M1 acceptance:
    /// 250 ms domain jitter < 50 ms).
    max_jitter: BTreeMap<&'static str, Duration>,
}

impl TelemetryStore {
    pub(crate) fn push(&mut self, sample: MetricSample) {
        let ring = self.rings.entry(sample.id).or_default();
        if ring.len() == RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(sample);
    }

    pub(crate) fn set_provider(&mut self, name: &'static str, status: ProviderStatus) {
        self.providers.insert(name, status);
    }

    pub(crate) fn note_jitter(&mut self, name: &'static str, lateness: Duration) {
        let entry = self.max_jitter.entry(name).or_default();
        if lateness > *entry {
            *entry = lateness;
        }
    }

    pub(crate) fn scheduler_jitter(&self) -> &BTreeMap<&'static str, Duration> {
        &self.max_jitter
    }

    pub(crate) fn snapshot(&self) -> TelemetrySnapshot {
        let mut samples = BTreeMap::new();
        let mut newest = None;
        for (id, ring) in &self.rings {
            if let Some(last) = ring.back() {
                if newest.is_none_or(|t| last.timestamp > t) {
                    newest = Some(last.timestamp);
                }
                samples.insert(*id, last.clone());
            }
        }
        TelemetrySnapshot {
            samples,
            providers: self.providers.clone(),
            at: newest,
        }
    }

    pub(crate) fn history(&self, id: MetricId, window: Duration) -> Vec<MetricSample> {
        let Some(ring) = self.rings.get(&id) else {
            return Vec::new();
        };
        let Some(&newest) = ring.back().map(|s| &s.timestamp) else {
            return Vec::new();
        };
        ring.iter()
            .filter(|s| newest.duration_since(s.timestamp) <= window)
            .cloned()
            .collect()
    }

    pub(crate) fn stats(&self, id: MetricId, window: Duration) -> Option<WindowStats> {
        let samples = self.history(id, window);
        let mut count = 0usize;
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut sum = 0.0;
        for s in &samples {
            if let Some(v) = s.value.as_f64() {
                count += 1;
                sum += v;
                min = min.min(v);
                max = max.max(v);
            }
        }
        (count > 0).then_some(WindowStats {
            min,
            max,
            avg: sum / count as f64,
            count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::telemetry::{MetricSource, MetricValue, ids};

    fn sample(v: f64) -> MetricSample {
        MetricSample::fresh(
            ids::CPU_PKG_TEMP_C,
            MetricValue::F64(v),
            MetricSource::PawnIoMsr,
        )
    }

    #[test]
    fn ring_bounds_memory() {
        let mut store = TelemetryStore::default();
        for i in 0..(RING_CAPACITY + 100) {
            store.push(sample(i as f64));
        }
        assert_eq!(store.rings[&ids::CPU_PKG_TEMP_C].len(), RING_CAPACITY);
    }

    #[test]
    fn stats_math() {
        let mut store = TelemetryStore::default();
        for v in [10.0, 20.0, 30.0] {
            store.push(sample(v));
        }
        let st = store
            .stats(ids::CPU_PKG_TEMP_C, Duration::from_secs(60))
            .expect("stats");
        assert_eq!(st.count, 3);
        assert!((st.avg - 20.0).abs() < f64::EPSILON);
        assert!((st.min - 10.0).abs() < f64::EPSILON);
        assert!((st.max - 30.0).abs() < f64::EPSILON);
    }
}
