//! Scenario lifecycle + reporting on a `MicrogridSite`.
//!
//! Three concerns live here:
//!
//! - The scenario journal: start / stop / record / events_since /
//!   summary / report, plus the elapsed-time accessor.
//! - CSV sink open / close — paired with scenario start / stop so
//!   the recorded files match the journal window.
//! - The scenario-report shape and its SoC-stats helper.
//!
//! All persistent state lives in `MicrogridSiteInner`; this file
//! only adds methods.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::sim::scenario::{ScenarioCheck, ScenarioEvent};
use crate::sim::scenario_csv::{CsvSink, CsvSinks};

use super::MicrogridSite;

/// Snapshot of `ScenarioJournal` lifecycle state for `/api/scenario`.
/// Excludes the events themselves — those live behind a paginated
/// `/api/scenario/events` endpoint with a `since=` cursor.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ScenarioSummary {
    pub name: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub elapsed_s: f64,
    pub event_count: usize,
    /// One past the highest event id ever recorded. Stable cursor
    /// for `/api/scenario/events?since=N` — clients pass this back
    /// unchanged to mean "anything newer than what I last saw".
    pub next_event_id: u64,
    /// Lowest event id still retained in the ring. Clients compare
    /// their `since` cursor against this: if `since < earliest_event_id`
    /// they're polling into a window that has already been evicted,
    /// so some events were missed.
    pub earliest_event_id: u64,
}

/// Snapshot of scenario-scoped metrics for `/api/scenario/report`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ScenarioReport {
    pub scenario_elapsed_s: f64,
    pub peak_main_meter_w: f64,
    pub main_meter_id: Option<u64>,
    pub total_battery_charged_wh: f64,
    pub total_battery_discharged_wh: f64,
    pub total_pv_produced_wh: f64,
    pub per_battery: Vec<PerBatteryReport>,
    pub per_pv: Vec<PerPvReport>,
    /// Stats over the *current* SoC of every registered battery.
    /// Computed lazily on each report fetch — cheap O(N) over a
    /// handful of batteries. None when no batteries are registered.
    pub soc_stats: Option<SocStats>,
    /// Per-15-minute UTC-aligned window average of main-meter
    /// active power. Sorted oldest-first.
    pub main_meter_window_averages: Vec<WindowAverageEntry>,
    /// Full-run `(scenario-expect …)` totals. Count every check
    /// even after the detail ring below starts evicting.
    pub checks_passed: u64,
    pub checks_failed: u64,
    /// Recent check results, oldest first (bounded ring).
    pub checks: Vec<ScenarioCheck>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PerBatteryReport {
    pub id: u64,
    pub charge_wh: f64,
    pub discharge_wh: f64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PerPvReport {
    pub id: u64,
    pub produced_wh: f64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SocStats {
    /// Arithmetic mean of every battery's current SoC.
    pub mean_pct: f64,
    /// Median (lower of the two middle values for an even count).
    pub median_pct: f64,
    /// Mode bucketed to integer percent. If multiple buckets tie,
    /// returns the lowest. None for an empty set.
    pub mode_pct: Option<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WindowAverageEntry {
    pub window_start: DateTime<Utc>,
    pub avg_w: f64,
}

/// Compute mean / median / integer-bucketed mode over a battery
/// SoC sample set. Returns `None` for an empty input.
fn compute_soc_stats(socs: &[f32]) -> Option<SocStats> {
    if socs.is_empty() {
        return None;
    }
    let mean_pct = socs.iter().map(|v| *v as f64).sum::<f64>() / socs.len() as f64;
    let mut sorted: Vec<f32> = socs.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_pct = sorted[sorted.len() / 2 - usize::from(sorted.len().is_multiple_of(2))] as f64;
    // Mode: integer-bucketed, lowest-bucket on tie.
    let mut histogram = [0u32; 101];
    for v in socs {
        let bucket = v.clamp(0.0, 100.0).round() as usize;
        histogram[bucket] += 1;
    }
    // Pick the lowest bucket on a count tie. `max_by_key` keeps
    // the LAST max seen; iterate ascending and update only on
    // strictly greater so the lowest bucket wins.
    let mut mode_pct: u8 = 0;
    let mut best_count: u32 = 0;
    for (idx, count) in histogram.iter().enumerate() {
        if *count > best_count {
            best_count = *count;
            mode_pct = idx as u8;
        }
    }
    Some(SocStats {
        mean_pct,
        median_pct,
        mode_pct: Some(mode_pct),
    })
}

impl MicrogridSite {
    /// Open fresh CSV sinks per registered component under `dir`:
    /// a telemetry file for every component, plus a received-setpoints
    /// and an effective-bounds file for each component that reports an
    /// active-power envelope (the ones a control app commands).
    /// Returns the total file count opened. Existing sinks are
    /// dropped first so a re-call replaces (rather than appends to)
    /// the prior recording.
    pub(crate) fn scenario_open_csv(&self, dir: &Path) -> std::io::Result<usize> {
        std::fs::create_dir_all(dir)?;
        let components = self.inner.components.read().clone();
        let mut telemetry = CsvSinks::new();
        let mut setpoints = CsvSinks::new();
        let mut bounds = CsvSinks::new();
        for c in &components {
            telemetry.insert(c.id(), CsvSink::open(dir, c.id(), c.category())?);
            if c.effective_active_bounds().is_some() {
                setpoints.insert(c.id(), CsvSink::open_setpoints(dir, c.id())?);
                bounds.insert(c.id(), CsvSink::open_bounds(dir, c.id())?);
            }
        }
        let count = telemetry.len() + setpoints.len() + bounds.len();
        *self.inner.scenario_csv.write() = telemetry;
        *self.inner.scenario_setpoints_csv.write() = setpoints;
        *self.inner.scenario_bounds_csv.write() = bounds;
        Ok(count)
    }

    /// Drop every active CSV sink (telemetry, setpoints, bounds).
    /// Each underlying `BufWriter` flushes on drop. Returns the
    /// total file count closed.
    pub(crate) fn scenario_close_csv(&self) -> usize {
        let mut count = 0;
        for sinks in [
            &self.inner.scenario_csv,
            &self.inner.scenario_setpoints_csv,
            &self.inner.scenario_bounds_csv,
        ] {
            let mut g = sinks.write();
            count += g.len();
            g.clear();
        }
        count
    }

    /// Begin a fresh scenario at `now`. Empties the event ring,
    /// clears the stop marker, sets the name. Used by
    /// `(scenario-start)`.
    pub(crate) fn scenario_start(&self, name: String, now: DateTime<Utc>) {
        self.inner.scenario.write().start(name, now);
    }

    /// Mark the scenario as ended at `now`. Also closes any active
    /// CSV sinks so the file flushes before a downstream loader
    /// might pick it up. Idempotent.
    pub(crate) fn scenario_stop(&self, now: DateTime<Utc>) {
        self.inner.scenario.write().stop(now);
        self.scenario_close_csv();
    }

    /// Append a journal event. Returns the assigned id.
    pub(crate) fn scenario_record(&self, kind: String, payload: String, now: DateTime<Utc>) -> u64 {
        self.inner.scenario.write().record(kind, payload, now)
    }

    /// Record one `(scenario-expect …)` result.
    pub(crate) fn scenario_record_check(&self, check: ScenarioCheck) {
        self.inner.scenario.write().record_check(check);
    }

    /// Wall-clock seconds since the scenario started. 0 if not
    /// running. Freezes once stopped.
    pub(crate) fn scenario_elapsed_s(&self, now: DateTime<Utc>) -> f64 {
        self.inner.scenario.read().elapsed_s(now)
    }

    /// Snapshot of scenario lifecycle for `/api/scenario`.
    pub(crate) fn scenario_summary(&self, now: DateTime<Utc>) -> ScenarioSummary {
        let g = self.inner.scenario.read();
        ScenarioSummary {
            name: g.name.clone(),
            started_at: g.started_at,
            ended_at: g.ended_at,
            elapsed_s: g.elapsed_s(now),
            event_count: g.event_count(),
            next_event_id: g.next_event_id(),
            earliest_event_id: g.earliest_event_id(),
        }
    }

    /// Pull events with id > `since`, capped at `limit`. Used by
    /// `/api/scenario/events`.
    pub(crate) fn scenario_events_since(&self, since: u64, limit: usize) -> Vec<ScenarioEvent> {
        self.inner.scenario.read().events_since(since, limit)
    }

    /// Aggregate metrics for `/api/scenario/report`. Returns a
    /// snapshot. SoC stats are computed at fetch time from each
    /// battery's current telemetry — cheap, no accumulator needed.
    pub(crate) fn scenario_report(&self, now: DateTime<Utc>) -> ScenarioReport {
        use crate::sim::Category;
        let g = self.inner.scenario.read();
        let mut total_charged = 0.0;
        let mut total_discharged = 0.0;
        let per_battery: Vec<PerBatteryReport> = g
            .per_battery()
            .iter()
            .map(|(id, b)| {
                total_charged += b.charge_wh;
                total_discharged += b.discharge_wh;
                PerBatteryReport {
                    id: *id,
                    charge_wh: b.charge_wh,
                    discharge_wh: b.discharge_wh,
                }
            })
            .collect();
        let mut total_pv = 0.0;
        let per_pv: Vec<PerPvReport> = g
            .per_pv()
            .iter()
            .map(|(id, p)| {
                total_pv += p.produced_wh;
                PerPvReport {
                    id: *id,
                    produced_wh: p.produced_wh,
                }
            })
            .collect();
        let main_meter_window_averages: Vec<WindowAverageEntry> = g
            .window_avgs()
            .iter()
            .map(|(secs, (sum, count))| WindowAverageEntry {
                window_start: DateTime::<Utc>::from_timestamp(*secs, 0).unwrap_or_else(Utc::now),
                avg_w: if *count > 0 {
                    *sum / (*count as f64)
                } else {
                    0.0
                },
            })
            .collect();
        let checks: Vec<ScenarioCheck> = g.checks().cloned().collect();
        let checks_passed = g.checks_passed();
        let checks_failed = g.checks_failed();
        drop(g);

        // SoC stats: walk every registered battery, read its
        // current SoC. Out-of-band of the journal because it's
        // current state, not an accumulator.
        let mut socs: Vec<f32> = Vec::new();
        for c in self.inner.components.read().iter() {
            if c.category() == Category::Battery
                && let Some(s) = c.telemetry(self).soc_pct
            {
                socs.push(s);
            }
        }
        let soc_stats = compute_soc_stats(&socs);

        ScenarioReport {
            scenario_elapsed_s: self.inner.scenario.read().elapsed_s(now),
            peak_main_meter_w: self.inner.scenario.read().peak_main_meter_active_w(),
            main_meter_id: *self.inner.main_meter_id.read(),
            total_battery_charged_wh: total_charged,
            total_battery_discharged_wh: total_discharged,
            total_pv_produced_wh: total_pv,
            per_battery,
            per_pv,
            soc_stats,
            main_meter_window_averages,
            checks_passed,
            checks_failed,
            checks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::compute_soc_stats;

    #[test]
    fn soc_stats_compute_on_typical_set() {
        let s = compute_soc_stats(&[20.0, 40.0, 60.0, 80.0]).unwrap();
        // Mean of 20, 40, 60, 80 = 50.
        assert!((s.mean_pct - 50.0).abs() < 1e-6);
        // Median (lower of middle two on even count) = 40.
        assert!((s.median_pct - 40.0).abs() < 1e-6);
        // No clear mode — all equal counts at distinct buckets;
        // returns the lowest tied bucket (20).
        assert_eq!(s.mode_pct, Some(20));
    }

    #[test]
    fn soc_stats_mode_picks_repeated_bucket() {
        let s = compute_soc_stats(&[50.0, 50.4, 50.6, 25.0, 80.0]).unwrap();
        // Three SoCs round to 50 (50, 50, 51 — actually 50.6
        // rounds to 51, so mode is 50 with 2 buckets, vs 51, 25,
        // 80 each at 1).
        assert_eq!(s.mode_pct, Some(50));
    }

    #[test]
    fn soc_stats_empty_returns_none() {
        assert!(compute_soc_stats(&[]).is_none());
    }
}
