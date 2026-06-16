//! Scenario registry — named, introspectable scenarios the operator
//! can run from the UI / `swctl scenario run` / a stepped CI gate.
//!
//! A scenario is one unified unit on two orthogonal axes (see
//! `scenarios/DESIGN.md`):
//!
//!  - **schedule reference** ([`Schedule`]): `relative` (offsets from
//!    start, the general case) or `absolute` (calendar / `HH:MM`).
//!  - **clock driver** ([`ClockDriver`]): `real` (wall clock) or
//!    `stepped` (the headless `ManualClock`, deterministic + fast).
//!
//! Authoring is identical across the matrix; the runner (todo §J2)
//! picks the clock and the schedule reference only changes how cue
//! times resolve. The scenario body is six optional sections —
//! `setup` / `drive` / `agents` / `cues` / `expect` / `record` — each
//! held as raw tulisp forms produced by the section wrappers
//! (`drive-solar` / `controller` / `at` / `check` / `event`, in
//! `sim/scenarios.lisp`). The runner compiles them down to the
//! primitives that already exist (`scenario-start`, `set-meter-power`,
//! `define-controller`, `run-with-timer`, `scenario-expect`, …).
//!
//! This module is pure data + the registry; parsing lives in the
//! `define-scenario` defun and running lives one layer up.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::NaiveDate;
use parking_lot::Mutex;
use serde::Serialize;
use tulisp::TulispObject;

/// How a scenario's cue / check / stage times are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Schedule {
    /// Offsets from scenario start (`"0s"`, `"60s"`, `"3min"`).
    Relative,
    /// Calendar / wall times (`"14:00"`, anchored by `:date`).
    Absolute,
}

impl Schedule {
    /// Parse the `:schedule` symbol/string. `None` for an unknown
    /// value so the defun can surface a script-level error.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "relative" => Some(Self::Relative),
            "absolute" => Some(Self::Absolute),
            _ => None,
        }
    }
}

/// How time advances while a scenario runs. The author's `:clock` is
/// the default; a runner may override it (e.g. a CI gate forcing
/// `stepped`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockDriver {
    /// Wall clock; cues fire as real time passes. Live demos.
    Real,
    /// Host-advanced sim clock (the headless `ManualClock`);
    /// deterministic and as fast as it's stepped. CI / replay.
    Stepped,
}

impl ClockDriver {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "real" => Some(Self::Real),
            "stepped" => Some(Self::Stepped),
            _ => None,
        }
    }
}

/// A registered, named scenario. Every section is optional; the
/// list-valued ones (`drive` / `agents` / `cues` / `expect`) hold the
/// raw forms the section wrappers produced, kept as `TulispObject`s so
/// the runner can funcall / interpret them later. `setup` and `record`
/// are single forms (a lambda, or `'csv` / a dir string for `record`).
#[derive(Clone, Debug)]
pub struct ScenarioDef {
    pub name: String,
    pub description: String,
    pub schedule: Schedule,
    /// Default clock driver; a runner may override.
    pub clock: ClockDriver,
    /// Run length in seconds (relative schedules). `None` runs until
    /// stopped (or, absolute, until `date` + 24h).
    pub length_s: Option<f64>,
    /// Calendar date anchoring an `absolute` schedule. `None` falls
    /// back to wallclock-today.
    pub date: Option<NaiveDate>,
    /// Optional RNG seed; with a `stepped` clock makes the run
    /// bit-for-bit reproducible.
    pub seed: Option<i64>,

    /// Runs once at start (seed RNG, install constant state).
    pub setup: Option<TulispObject>,
    /// Continuous environment sources (`drive-solar` / `drive-meter`).
    pub drive: Vec<TulispObject>,
    /// In-sim controllers reacting to live state (`controller`).
    pub agents: Vec<TulispObject>,
    /// Discrete timed actions (`at`).
    pub cues: Vec<TulispObject>,
    /// Timed assertions (`check`).
    pub expect: Vec<TulispObject>,
    /// Recording directive (`'csv` or a directory).
    pub record: Option<TulispObject>,
}

pub type SharedScenarios = Arc<Mutex<HashMap<String, ScenarioDef>>>;

pub fn new_registry() -> SharedScenarios {
    Arc::new(Mutex::new(HashMap::new()))
}

/// JSON-serialisable view of a scenario for the HTTP API + UI list.
/// Drops the non-serialisable section forms, keeping the metadata and
/// per-section counts the list / timeline need. Built fresh on every
/// API hit so it's safe to ship across threads.
#[derive(Clone, Debug, Serialize)]
pub struct ScenarioView {
    pub name: String,
    pub description: String,
    pub schedule: Schedule,
    pub clock: ClockDriver,
    pub length_s: Option<f64>,
    pub date: Option<NaiveDate>,
    pub seed: Option<i64>,
    pub has_setup: bool,
    pub n_drive: usize,
    pub n_agents: usize,
    pub n_cues: usize,
    pub n_expect: usize,
    pub records: bool,
}

impl From<&ScenarioDef> for ScenarioView {
    fn from(d: &ScenarioDef) -> Self {
        ScenarioView {
            name: d.name.clone(),
            description: d.description.clone(),
            schedule: d.schedule,
            clock: d.clock,
            length_s: d.length_s,
            date: d.date,
            seed: d.seed,
            has_setup: d.setup.is_some(),
            n_drive: d.drive.len(),
            n_agents: d.agents.len(),
            n_cues: d.cues.len(),
            n_expect: d.expect.len(),
            records: d.record.is_some(),
        }
    }
}

/// Snapshot the registry into a list of [`ScenarioView`]s, alphabetic
/// by name. Used by `GET /api/scenarios` and `swctl scenario list`.
pub fn snapshot(registry: &SharedScenarios) -> Vec<ScenarioView> {
    let mut out: Vec<_> = registry.lock().values().map(ScenarioView::from).collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_and_clock_parse_known_values() {
        assert_eq!(Schedule::parse("relative"), Some(Schedule::Relative));
        assert_eq!(Schedule::parse("absolute"), Some(Schedule::Absolute));
        assert_eq!(Schedule::parse("wallclock"), None);
        assert_eq!(ClockDriver::parse("real"), Some(ClockDriver::Real));
        assert_eq!(ClockDriver::parse("stepped"), Some(ClockDriver::Stepped));
        assert_eq!(ClockDriver::parse("instant"), None);
    }

    fn def(name: &str) -> ScenarioDef {
        ScenarioDef {
            name: name.into(),
            description: String::new(),
            schedule: Schedule::Relative,
            clock: ClockDriver::Real,
            length_s: None,
            date: None,
            seed: None,
            setup: None,
            drive: Vec::new(),
            agents: Vec::new(),
            cues: Vec::new(),
            expect: Vec::new(),
            record: None,
        }
    }

    #[test]
    fn snapshot_is_alphabetic() {
        let reg = new_registry();
        for n in ["zulu", "alpha", "mike"] {
            reg.lock().insert(n.into(), def(n));
        }
        let names: Vec<_> = snapshot(&reg).into_iter().map(|v| v.name).collect();
        assert_eq!(names, vec!["alpha", "mike", "zulu"]);
    }
}
