//! Per-component telemetry history — bounded ring buffers feeding the
//! UI's time-series charts. Sampled by an independent task per
//! component (see `World::spawn_history_samplers` in the next commit)
//! at the component's `stream_interval`, so the UI works even with
//! zero gRPC subscribers.
//!
//! Storage is sparse per metric — a `Battery` doesn't carry AC
//! voltage, a `Meter` doesn't carry SoC, so we only allocate a buffer
//! for metrics each component actually publishes.
//!
//! Pure data structures — no async, no I/O. Integration with the
//! World registry lands in the next commit.

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};

use crate::sim::Telemetry;

/// Single metric we track over time. Trimmed to the values that show
/// up in v1 charts; per-phase / DC metrics can join later if they
/// turn out to be load-bearing for control-app evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Metric {
    ActivePowerW,
    ReactivePowerVar,
    FrequencyHz,
    SocPct,
    ActivePowerLowerBoundW,
    ActivePowerUpperBoundW,
    ReactivePowerLowerBoundVar,
    ReactivePowerUpperBoundVar,
}

impl Metric {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ActivePowerW => "active_power_w",
            Self::ReactivePowerVar => "reactive_power_var",
            Self::FrequencyHz => "frequency_hz",
            Self::SocPct => "soc_pct",
            Self::ActivePowerLowerBoundW => "active_power_lower_bound_w",
            Self::ActivePowerUpperBoundW => "active_power_upper_bound_w",
            Self::ReactivePowerLowerBoundVar => "reactive_power_lower_bound_var",
            Self::ReactivePowerUpperBoundVar => "reactive_power_upper_bound_var",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub ts: DateTime<Utc>,
    pub value: f32,
}

/// Bounded ring buffer of samples for a single metric. Pushes past
/// `capacity` evict the oldest sample.
#[derive(Debug)]
pub struct History {
    capacity: usize,
    ring: VecDeque<Sample>,
}

impl History {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            ring: VecDeque::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, ts: DateTime<Utc>, value: f32) {
        if self.ring.len() == self.capacity {
            self.ring.pop_front();
        }
        self.ring.push_back(Sample { ts, value });
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// Iterate samples whose timestamp is `>= since`. Order is oldest
    /// → newest. Useful when a UI client wants only the recent N
    /// minutes from a buffer that holds longer history.
    pub fn iter_window(&self, since: DateTime<Utc>) -> impl Iterator<Item = &Sample> {
        // Ring is monotonic-time-ordered (samples push at the tail
        // with ts == "now"), so partition_point finds the first
        // sample at-or-after `since` without scanning the whole ring.
        let cut = self.ring.partition_point(|s| s.ts < since);
        self.ring.iter().skip(cut)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Sample> {
        self.ring.iter()
    }
}

/// Per-component metric histories. Sparse — only metrics this
/// component actually publishes get a buffer.
#[derive(Debug)]
pub struct ComponentHistory {
    capacity: usize,
    series: HashMap<Metric, History>,
}

impl ComponentHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            series: HashMap::new(),
        }
    }

    /// Record everything in `snapshot` that maps to a tracked Metric.
    /// Missing fields (Option::None on the snapshot) are skipped, so a
    /// Meter's tick produces 1–2 metric pushes; a BatteryInverter's
    /// produces 5–6.
    ///
    /// Returns the list of `(metric, value)` pairs that were actually
    /// recorded — the caller (typically `World::record_history_snapshot`)
    /// uses this to fan out per-sample broadcast events without
    /// re-walking the snapshot.
    pub fn push_snapshot(
        &mut self,
        ts: DateTime<Utc>,
        snapshot: &Telemetry,
    ) -> Vec<(Metric, f32)> {
        let mut pushed = Vec::new();
        let mut record = |this: &mut Self, m: Metric, v: f32| {
            this.push(ts, m, v);
            pushed.push((m, v));
        };
        if let Some(v) = snapshot.active_power_w {
            record(self, Metric::ActivePowerW, v);
        }
        if let Some(v) = snapshot.reactive_power_var {
            record(self, Metric::ReactivePowerVar, v);
        }
        if let Some(v) = snapshot.frequency_hz {
            record(self, Metric::FrequencyHz, v);
        }
        if let Some(v) = snapshot.soc_pct {
            record(self, Metric::SocPct, v);
        }
        if let Some(b) = &snapshot.active_power_bounds {
            // Charts plot a single envelope band, so take the first
            // bounds segment. Components that emit multi-segment
            // VecBounds (split by a forbidden gap) lose the inner
            // detail in the chart view; live values still go through
            // the gRPC stream un-collapsed.
            if let Some(first) = b.0.first() {
                if let Some(v) = first.lower {
                    record(self, Metric::ActivePowerLowerBoundW, v);
                }
                if let Some(v) = first.upper {
                    record(self, Metric::ActivePowerUpperBoundW, v);
                }
            }
        }
        if let Some((l, u)) = snapshot.reactive_power_bounds {
            record(self, Metric::ReactivePowerLowerBoundVar, l);
            record(self, Metric::ReactivePowerUpperBoundVar, u);
        }
        pushed
    }

    fn push(&mut self, ts: DateTime<Utc>, metric: Metric, value: f32) {
        self.series
            .entry(metric)
            .or_insert_with(|| History::new(self.capacity))
            .push(ts, value);
    }

    pub fn get(&self, metric: Metric) -> Option<&History> {
        self.series.get(&metric)
    }

    pub fn metrics(&self) -> impl Iterator<Item = Metric> + '_ {
        self.series.keys().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    #[test]
    fn ring_evicts_oldest_when_full() {
        let mut h = History::new(3);
        h.push(t(1), 10.0);
        h.push(t(2), 20.0);
        h.push(t(3), 30.0);
        h.push(t(4), 40.0); // evicts t=1

        let values: Vec<_> = h.iter().map(|s| s.value).collect();
        assert_eq!(values, vec![20.0, 30.0, 40.0]);
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn iter_window_skips_older_samples() {
        let mut h = History::new(5);
        for i in 1..=5 {
            h.push(t(i), i as f32 * 10.0);
        }
        let in_window: Vec<_> = h.iter_window(t(3)).map(|s| s.value).collect();
        assert_eq!(in_window, vec![30.0, 40.0, 50.0]);
    }

    #[test]
    fn component_history_records_only_published_metrics() {
        let mut ch = ComponentHistory::new(10);
        // Meter-style snapshot — has P/Q + frequency, no SoC.
        let snap = Telemetry {
            active_power_w: Some(1500.0),
            reactive_power_var: Some(200.0),
            frequency_hz: Some(50.01),
            ..Default::default()
        };
        ch.push_snapshot(t(1), &snap);
        let metrics: std::collections::HashSet<_> = ch.metrics().collect();
        assert!(metrics.contains(&Metric::ActivePowerW));
        assert!(metrics.contains(&Metric::ReactivePowerVar));
        assert!(metrics.contains(&Metric::FrequencyHz));
        assert!(!metrics.contains(&Metric::SocPct));
        assert_eq!(ch.get(Metric::ActivePowerW).unwrap().len(), 1);
    }

    #[test]
    fn component_history_extracts_active_bounds_envelope() {
        use crate::sim::bounds::VecBounds;
        let mut ch = ComponentHistory::new(10);
        let snap = Telemetry {
            active_power_w: Some(0.0),
            active_power_bounds: Some(VecBounds::single(-5000.0, 5000.0)),
            ..Default::default()
        };
        ch.push_snapshot(t(1), &snap);
        let lo = ch.get(Metric::ActivePowerLowerBoundW).unwrap();
        let hi = ch.get(Metric::ActivePowerUpperBoundW).unwrap();
        assert_eq!(lo.iter().next().unwrap().value, -5000.0);
        assert_eq!(hi.iter().next().unwrap().value, 5000.0);
    }
}
