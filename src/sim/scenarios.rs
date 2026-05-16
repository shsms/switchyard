//! Scenario registry — named, multi-stage scripts the operator can
//! run from the UI / `swctl scenarios` / `tsctl scenarios`-style CLI.
//!
//! A scenario is a named DAG of *stages* covering a 24-hour day.
//! Each stage owns a wall-clock window `[hour_from, hour_to)` (the
//! configured timezone applies — see `clock.rs`) and a tulisp lambda
//! `on` that is funcalled on stage entry. The lambda is the unit of
//! action: it can `set-meter-power`, `set-solar-sunlight`, install
//! `(every …)` timers, fire faults — whatever the existing lisp
//! defuns expose. There's no separate `:off` step; the next stage's
//! `:on` typically supersedes whatever the previous one installed,
//! and a `(scenario-stop NAME)` paired with `(reset-state)` returns
//! to baseline.
//!
//! Modelled after [`tradingsim::scenarios::ScenarioDef`] —
//! switchyard's per-stage payload is simplified to a single lambda
//! (no bias / weather knobs; the lambda calls whatever it needs).
//! The lifecycle plumbing (auto-advance, manual override, prev /
//! next / jump / stop) ports over verbatim.
//!
//! Pure data + small helpers — no async, no I/O. The auto-advance
//! task ([`spawn_auto_advance`] in a follow-up commit) and the
//! HTTP endpoints sit one layer up.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use tulisp::TulispObject;

use crate::sim::clock::Clock;

#[derive(Clone, Debug)]
pub struct Stage {
    /// Display name shown in the UI ("06:00 morning ramp").
    pub name: String,
    /// Wallclock hour window in the configured timezone, half-open
    /// `[hour_from, hour_to)`. Auto-advance picks the matching stage
    /// by `local_hour`.
    pub hour_from: f64,
    pub hour_to: f64,
    /// Tulisp lambda funcalled on stage entry. Held as a raw
    /// `TulispObject` — the registry's mutex blocks Send / Sync, so
    /// the auto-advance task funcalls it on whichever thread happens
    /// to hold the interpreter lock at the time. Optional so a stage
    /// can be a pure timeline annotation with no side-effects.
    pub on: Option<TulispObject>,
}

#[derive(Clone, Debug)]
pub struct ScenarioDef {
    pub name: String,
    pub description: String,
    /// Optional calendar date pinning curves to a specific day.
    /// `None` falls back to wallclock-today. Useful for "sunny-
    /// summer" scenarios that want to anchor `(local-day-of-year)`
    /// regardless of when the scenario is actually run.
    pub date: Option<NaiveDate>,
    pub stages: Vec<Stage>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ScenarioRuntime {
    /// `None` = not running; `Some(i)` = stage `i` is current.
    pub current_stage: Option<usize>,
    pub started_at: Option<DateTime<Utc>>,
    pub stage_entered_at: Option<DateTime<Utc>>,
    /// True when the operator jumped away from the wallclock-current
    /// stage. The auto-advance task respects this and stops bumping
    /// until the operator returns to the wallclock-matching stage
    /// or restarts the scenario.
    pub manual_override: bool,
}

#[derive(Clone, Debug)]
pub struct ScenarioEntry {
    pub def: ScenarioDef,
    pub runtime: ScenarioRuntime,
}

pub type SharedScenarios = Arc<Mutex<HashMap<String, ScenarioEntry>>>;

pub fn new_registry() -> SharedScenarios {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Stage matching `hour` (local fractional hour, 0..24). Returns
/// `None` outside any defined window — typical for sparse scenarios
/// that only cover, say, 17:00–21:00 of evening peak.
pub fn wallclock_stage(def: &ScenarioDef, hour: f64) -> Option<usize> {
    def.stages
        .iter()
        .position(|s| s.hour_from <= hour && hour < s.hour_to)
}

/// JSON-serialisable view of a scenario for the HTTP API. Drops the
/// `on` TulispObject (not serialisable) but keeps everything the UI
/// timeline + stage list need to render. Built fresh on every API
/// hit so it's safe to ship across threads.
#[derive(Clone, Debug, Serialize)]
pub struct ScenarioView {
    pub name: String,
    pub description: String,
    pub date: Option<NaiveDate>,
    pub stages: Vec<StageView>,
    pub runtime: ScenarioRuntime,
}

#[derive(Clone, Debug, Serialize)]
pub struct StageView {
    pub name: String,
    pub hour_from: f64,
    pub hour_to: f64,
    /// True when the def supplied a non-nil `:on` lambda. Pure
    /// timeline annotations render unchecked in the UI; stages with
    /// `on=true` show an action chip.
    pub has_on: bool,
}

impl From<&ScenarioEntry> for ScenarioView {
    fn from(e: &ScenarioEntry) -> Self {
        ScenarioView {
            name: e.def.name.clone(),
            description: e.def.description.clone(),
            date: e.def.date,
            stages: e.def.stages.iter().map(StageView::from).collect(),
            runtime: e.runtime.clone(),
        }
    }
}

impl From<&Stage> for StageView {
    fn from(s: &Stage) -> Self {
        StageView {
            name: s.name.clone(),
            hour_from: s.hour_from,
            hour_to: s.hour_to,
            has_on: s.on.is_some(),
        }
    }
}

/// Snapshot the registry into a list of [`ScenarioView`]s, alphabetic
/// by name. Used by `GET /api/scenarios` and `swctl scenarios list`.
pub fn snapshot(registry: &SharedScenarios) -> Vec<ScenarioView> {
    let mut out: Vec<_> = registry
        .lock()
        .values()
        .map(ScenarioView::from)
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Hour the wallclock-driven `clock.local_hour(now)` would produce
/// for the given `Utc` instant. Kept as a small helper so the
/// auto-advance task and the UI's "now marker" placement agree.
pub fn local_hour(clock: &Clock, now: DateTime<Utc>) -> f64 {
    clock.local_hour(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(stages: &[(f64, f64)]) -> ScenarioDef {
        ScenarioDef {
            name: "t".into(),
            description: String::new(),
            date: None,
            stages: stages
                .iter()
                .map(|&(a, b)| Stage {
                    name: format!("{a}-{b}"),
                    hour_from: a,
                    hour_to: b,
                    on: None,
                })
                .collect(),
        }
    }

    #[test]
    fn wallclock_stage_picks_half_open_window() {
        let d = def(&[(0.0, 6.0), (6.0, 12.0), (12.0, 24.0)]);
        assert_eq!(wallclock_stage(&d, 0.0), Some(0));
        assert_eq!(wallclock_stage(&d, 5.99), Some(0));
        assert_eq!(wallclock_stage(&d, 6.0), Some(1));
        assert_eq!(wallclock_stage(&d, 23.5), Some(2));
    }

    #[test]
    fn wallclock_stage_returns_none_outside_coverage() {
        let d = def(&[(17.0, 21.0)]);
        assert_eq!(wallclock_stage(&d, 8.0), None);
        assert_eq!(wallclock_stage(&d, 17.5), Some(0));
        assert_eq!(wallclock_stage(&d, 21.0), None);
    }

    #[test]
    fn snapshot_is_alphabetic() {
        let reg = new_registry();
        for n in ["zulu", "alpha", "mike"] {
            reg.lock().insert(
                n.into(),
                ScenarioEntry {
                    def: ScenarioDef {
                        name: n.into(),
                        description: String::new(),
                        date: None,
                        stages: Vec::new(),
                    },
                    runtime: ScenarioRuntime::default(),
                },
            );
        }
        let names: Vec<_> = snapshot(&reg).into_iter().map(|v| v.name).collect();
        assert_eq!(names, vec!["alpha", "mike", "zulu"]);
    }
}
