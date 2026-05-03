//! Scenario lifecycle + event journal.
//!
//! A scenario is a Lisp script that drives the simulator through a
//! sequence of events (sudden load spikes, component outages, …)
//! while a Rust observer records what happened. `ScenarioJournal`
//! is that observer. The script calls `(scenario-start NAME)` to
//! begin, `(scenario-event KIND PAYLOAD)` to record interesting
//! moments, and `(scenario-stop)` to freeze the report so it can
//! be fetched.
//!
//! Events are append-only with a capped ring so a long-running
//! scenario doesn't grow without bound. Each event carries a stable
//! monotonic id so HTTP clients can poll the events endpoint with a
//! `since=` cursor and only see new entries.

use std::collections::VecDeque;

use chrono::{DateTime, Utc};

use crate::sim::history::Metric;

/// Cap on the in-memory event ring. A typical scenario fires
/// dozens of events per minute (component outages, setpoint
/// responses, milestone markers); 4096 entries covers ~30 minutes
/// of dense traffic before the oldest start aging out. The
/// overflow is silent; clients reading the events endpoint with a
/// `since=` cursor will see a gap.
const SCENARIO_EVENT_CAPACITY: usize = 4096;

/// Single entry in the scenario journal. `id` is monotonic and
/// stable across the journal's lifetime — it does not reset on
/// `scenario_start`. Clients use it as the `since=` cursor for
/// `/api/scenario/events`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScenarioEvent {
    pub id: u64,
    pub ts: DateTime<Utc>,
    pub kind: String,
    pub payload: String,
}

/// Scenario journal: name + lifecycle + capped event ring + the
/// metric accumulators the reporter exposes via
/// `/api/scenario/report`.
#[derive(Debug, Default)]
pub struct ScenarioJournal {
    pub name: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    events: VecDeque<ScenarioEvent>,
    next_id: u64,
    /// Maximum positive active power seen on the main meter since
    /// the scenario started. Resets on `start`, freezes — like
    /// elapsed — once `stop` lands, but keeps absorbing samples
    /// until then. Stored as f64 to match the units B3/B4 will
    /// later use for charge / discharge integrals.
    peak_main_meter_active_w: f64,
}

impl ScenarioJournal {
    /// Begin a fresh scenario. Replaces any prior name, clears the
    /// stop marker, and empties the event ring. Event ids continue
    /// monotonically — a client polling /api/scenario/events across
    /// a restart sees a gap rather than an id rewind.
    pub fn start(&mut self, name: String, now: DateTime<Utc>) {
        self.name = Some(name);
        self.started_at = Some(now);
        self.ended_at = None;
        self.events.clear();
        self.peak_main_meter_active_w = 0.0;
    }

    /// Hand a freshly-recorded telemetry sample to the reporter.
    /// Skipped before `start` and after `stop`, so the peaks reflect
    /// only the active scenario window.
    pub fn record_sample(
        &mut self,
        id: u64,
        metric: Metric,
        value: f32,
        main_meter_id: Option<u64>,
    ) {
        if self.started_at.is_none() || self.ended_at.is_some() {
            return;
        }
        if Some(id) == main_meter_id && metric == Metric::ActivePowerW {
            let v = value as f64;
            if v > self.peak_main_meter_active_w {
                self.peak_main_meter_active_w = v;
            }
        }
    }

    pub fn peak_main_meter_active_w(&self) -> f64 {
        self.peak_main_meter_active_w
    }

    /// Mark the scenario as ended. Idempotent; subsequent calls
    /// keep the first stop time.
    pub fn stop(&mut self, now: DateTime<Utc>) {
        if self.ended_at.is_none() {
            self.ended_at = Some(now);
        }
    }

    /// Append an event. Drops the oldest entry when the ring is
    /// full. Returns the new event's id.
    pub fn record(&mut self, kind: String, payload: String, ts: DateTime<Utc>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        if self.events.len() >= SCENARIO_EVENT_CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back(ScenarioEvent {
            id,
            ts,
            kind,
            payload,
        });
        id
    }

    /// Seconds since `started_at`, or 0 if the scenario hasn't
    /// started. Freezes at `ended_at` once stopped.
    pub fn elapsed_s(&self, now: DateTime<Utc>) -> f64 {
        let Some(start) = self.started_at else {
            return 0.0;
        };
        let end = self.ended_at.unwrap_or(now);
        (end - start)
            .to_std()
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }

    /// Returns events with id >= `from_id`, capped at `limit`.
    /// Oldest first. The `from_id` cursor is inclusive — clients
    /// poll `/api/scenario/events?since=N` and pass back
    /// `next_event_id` from the previous response unchanged: that's
    /// "the id of the next event that hasn't been written yet", so
    /// id >= cursor naturally returns only new entries.
    pub fn events_since(&self, from_id: u64, limit: usize) -> Vec<ScenarioEvent> {
        self.events
            .iter()
            .filter(|e| e.id >= from_id)
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    pub fn next_event_id(&self) -> u64 {
        self.next_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().unwrap()
    }

    #[test]
    fn lifecycle_sets_and_freezes_elapsed() {
        let mut j = ScenarioJournal::default();
        // Pre-start: elapsed is 0.
        assert_eq!(j.elapsed_s(ts(100)), 0.0);
        j.start("warmup".into(), ts(100));
        assert_eq!(j.name.as_deref(), Some("warmup"));
        assert!((j.elapsed_s(ts(160)) - 60.0).abs() < 1e-6);
        // Stop freezes elapsed at the stop ts.
        j.stop(ts(160));
        assert!((j.elapsed_s(ts(9999)) - 60.0).abs() < 1e-6);
        // Idempotent stop keeps the first time.
        j.stop(ts(99999));
        assert!((j.elapsed_s(ts(9999)) - 60.0).abs() < 1e-6);
    }

    #[test]
    fn record_assigns_monotonic_ids_and_filters_since() {
        let mut j = ScenarioJournal::default();
        j.start("s".into(), ts(0));
        let id0 = j.record("note".into(), "hi".into(), ts(1));
        let id1 = j.record("outage".into(), "bat-1003".into(), ts(2));
        let id2 = j.record("note".into(), "bye".into(), ts(3));
        assert_eq!((id0, id1, id2), (0, 1, 2));

        // from_id=0 (inclusive) returns everything.
        let from_0 = j.events_since(0, 10);
        assert_eq!(
            from_0.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![0, 1, 2],
        );

        // from_id=2 returns only id 2 onward.
        let from_2 = j.events_since(2, 10);
        assert_eq!(from_2.iter().map(|e| e.id).collect::<Vec<_>>(), vec![2]);

        // limit caps the result.
        let capped = j.events_since(0, 2);
        assert_eq!(capped.iter().map(|e| e.id).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn start_clears_events_but_ids_continue() {
        let mut j = ScenarioJournal::default();
        j.start("first".into(), ts(0));
        j.record("a".into(), String::new(), ts(1));
        j.record("b".into(), String::new(), ts(2));
        assert_eq!(j.event_count(), 2);

        j.start("second".into(), ts(100));
        assert_eq!(j.event_count(), 0);
        assert_eq!(j.next_event_id(), 2);

        let id = j.record("c".into(), String::new(), ts(101));
        assert_eq!(id, 2);
    }

    #[test]
    fn ring_caps_at_capacity_and_drops_oldest() {
        let mut j = ScenarioJournal::default();
        j.start("flood".into(), ts(0));
        for i in 0..(SCENARIO_EVENT_CAPACITY + 50) {
            j.record("tick".into(), i.to_string(), ts(i as i64));
        }
        assert_eq!(j.event_count(), SCENARIO_EVENT_CAPACITY);
        // Oldest 50 ids dropped.
        let earliest = j.events_since(0, 1).into_iter().next().unwrap();
        assert_eq!(earliest.id, 50);
    }
}
