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
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use tulisp::{SharedMut, TulispContext, TulispObject};

use crate::sim::MicrogridSite;
use crate::sim::clock::{Clock, SharedClock};
use crate::sim::microgrids::{CurrentMicrogrid, SharedMicrogrids, with_microgrid};

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

/// Funcall a tulisp lambda with no arguments. Holds the interpreter
/// write lock for the duration of the call so the pre-tick hook
/// blocks until the lambda completes — mirrors `Config::eval`'s
/// lock discipline.
fn funcall_lambda(ctx: &SharedMut<TulispContext>, lam: &TulispObject) -> Result<(), String> {
    let mut c = ctx.borrow_mut();
    c.funcall(lam, &TulispObject::nil())
        .map(|_| ())
        .map_err(|e| e.format(&c))
}

/// Snapshot every (id, site) pair in the registry. Used by the
/// per-microgrid scenario replay: scenario stage transitions
/// journal + funcall the `:on` lambda once per microgrid, so
/// scripts that reference globally-unique component ids land
/// their side-effects on whichever microgrid owns the id, while
/// id-less actions (e.g. `(set-frequency 50.0)`) apply across
/// the enterprise.
fn registered_sites(reg: &SharedMicrogrids) -> Vec<(u64, MicrogridSite)> {
    reg.lock()
        .iter()
        .map(|(id, e)| (*id, e.site.clone()))
        .collect()
}

/// Start a scenario at the wallclock-current stage. Clears any
/// manual override and runs the entered stage's `:on` lambda
/// once per registered microgrid (with `current_microgrid`
/// flipped to that microgrid's id for the duration). Records a
/// `'scenario-start` event in every microgrid's journal so the
/// existing Report panel + setpoints / journal feed reflect the
/// transition enterprise-wide.
pub fn start(
    reg: &SharedScenarios,
    ctx: &SharedMut<TulispContext>,
    microgrids: &SharedMicrogrids,
    current: &CurrentMicrogrid,
    clock: &Clock,
    name: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let hour = clock.local_hour(now);
    let on = {
        let mut r = reg.lock();
        let e = r
            .get_mut(name)
            .ok_or_else(|| format!("no scenario named {name:?}"))?;
        let idx = wallclock_stage(&e.def, hour).unwrap_or(0);
        if e.def.stages.is_empty() {
            return Err(format!("scenario {name:?} has no stages"));
        }
        e.runtime.current_stage = Some(idx);
        e.runtime.started_at = Some(now);
        e.runtime.stage_entered_at = Some(now);
        e.runtime.manual_override = false;
        e.def.stages.get(idx).and_then(|s| s.on.clone())
    };
    for (mg_id, site) in registered_sites(microgrids) {
        site.scenario_start(name.to_owned(), now);
        if let Some(ref lam) = on {
            with_microgrid(current, mg_id, || funcall_lambda(ctx, lam))?;
        }
    }
    Ok(())
}

/// Stop the scenario. Clears runtime state and records a
/// `scenario-stop` event in every microgrid's journal. The
/// underlying world state (component setpoints, timers installed
/// by previous stage lambdas) is NOT rolled back — callers that
/// need a clean slate follow this with `(reset-state)` or load a
/// fresh snapshot.
pub fn stop(
    reg: &SharedScenarios,
    microgrids: &SharedMicrogrids,
    name: &str,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let mut r = reg.lock();
    let e = r
        .get_mut(name)
        .ok_or_else(|| format!("no scenario named {name:?}"))?;
    e.runtime.current_stage = None;
    e.runtime.stage_entered_at = None;
    e.runtime.manual_override = false;
    drop(r);
    for (_, site) in registered_sites(microgrids) {
        site.scenario_stop(now);
    }
    Ok(())
}

/// Jump to stage `idx`, setting `manual_override = true` so the
/// auto-advance task leaves the scenario alone until the operator
/// either restarts it or the wallclock catches up to that stage.
pub fn jump(
    reg: &SharedScenarios,
    ctx: &SharedMut<TulispContext>,
    microgrids: &SharedMicrogrids,
    current: &CurrentMicrogrid,
    name: &str,
    idx: usize,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let on = {
        let mut r = reg.lock();
        let e = r
            .get_mut(name)
            .ok_or_else(|| format!("no scenario named {name:?}"))?;
        if idx >= e.def.stages.len() {
            return Err(format!(
                "stage index {idx} out of range (0..{})",
                e.def.stages.len()
            ));
        }
        e.runtime.current_stage = Some(idx);
        e.runtime.stage_entered_at = Some(now);
        e.runtime.manual_override = true;
        if e.runtime.started_at.is_none() {
            e.runtime.started_at = Some(now);
        }
        e.def.stages[idx].on.clone()
    };
    let payload = format!("{name}:{idx}");
    for (mg_id, site) in registered_sites(microgrids) {
        site.scenario_record("stage-jump".to_owned(), payload.clone(), now);
        if let Some(ref lam) = on {
            with_microgrid(current, mg_id, || funcall_lambda(ctx, lam))?;
        }
    }
    Ok(())
}

/// `prev` and `next` are jumps by ±1 relative to the current stage.
/// `None` for `current_stage` resolves as "start at 0".
pub fn step(
    reg: &SharedScenarios,
    ctx: &SharedMut<TulispContext>,
    microgrids: &SharedMicrogrids,
    current: &CurrentMicrogrid,
    name: &str,
    delta: isize,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let target = {
        let r = reg.lock();
        let e = r
            .get(name)
            .ok_or_else(|| format!("no scenario named {name:?}"))?;
        let n = e.def.stages.len() as isize;
        if n == 0 {
            return Err(format!("scenario {name:?} has no stages"));
        }
        let cur = e.runtime.current_stage.map(|i| i as isize).unwrap_or(-1);
        ((cur + delta).max(0).min(n - 1)) as usize
    };
    jump(reg, ctx, microgrids, current, name, target, now)
}

/// One pass of the auto-advance loop: for every running scenario
/// whose `manual_override` is false, transition to the wallclock-
/// current stage if it differs from the recorded one. Returns the
/// names that transitioned, mostly so tests can assert without
/// reaching into the registry. Funcalls happen outside the registry
/// lock so a long `:on` lambda doesn't block readers, and each
/// stage transition replays the `:on` lambda once per microgrid
/// with `current_microgrid` flipped to that microgrid's id.
pub fn auto_advance_tick(
    reg: &SharedScenarios,
    ctx: &SharedMut<TulispContext>,
    microgrids: &SharedMicrogrids,
    current: &CurrentMicrogrid,
    clock: &Clock,
    now: DateTime<Utc>,
) -> Vec<String> {
    let hour = clock.local_hour(now);
    let to_run: Vec<(String, usize, Option<TulispObject>)> = {
        let mut r = reg.lock();
        r.iter_mut()
            .filter_map(|(name, e)| {
                if e.runtime.manual_override {
                    return None;
                }
                let cur = e.runtime.current_stage?;
                let want = wallclock_stage(&e.def, hour)?;
                if want == cur {
                    return None;
                }
                e.runtime.current_stage = Some(want);
                e.runtime.stage_entered_at = Some(now);
                let lam = e.def.stages.get(want).and_then(|s| s.on.clone());
                Some((name.clone(), want, lam))
            })
            .collect()
    };
    let sites = registered_sites(microgrids);
    let mut out = Vec::with_capacity(to_run.len());
    for (name, idx, lam) in to_run {
        let payload = format!("{name}:{idx}");
        for (mg_id, site) in &sites {
            site.scenario_record("stage-advance".to_owned(), payload.clone(), now);
            if let Some(ref l) = lam
                && let Err(e) = with_microgrid(current, *mg_id, || funcall_lambda(ctx, l))
            {
                log::warn!("scenario {name} stage {idx} mg #{mg_id} :on errored: {e}");
            }
        }
        out.push(name);
    }
    out
}

/// Spawn the auto-advance loop. Polls at 2 s — fast enough to catch
/// minute-level stage boundaries promptly without saturating the
/// interpreter lock the physics tick depends on. Lifetime of the
/// task is the process; there's no cancel handle since the
/// registry + world live forever.
pub fn spawn_auto_advance(
    reg: SharedScenarios,
    ctx: SharedMut<TulispContext>,
    microgrids: SharedMicrogrids,
    current: CurrentMicrogrid,
    clock: SharedClock,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let now = Utc::now();
            let c = clock.read().clone();
            auto_advance_tick(&reg, &ctx, &microgrids, &current, &c, now);
        }
    });
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
    fn auto_advance_skips_non_running_and_manual_overrides() {
        // Build a scenario manually (no lisp lambda needed for this
        // test — auto_advance_tick still moves the current_stage
        // pointer on the runtime side).
        let reg = new_registry();
        reg.lock().insert(
            "two-stage".into(),
            ScenarioEntry {
                def: ScenarioDef {
                    name: "two-stage".into(),
                    description: String::new(),
                    date: None,
                    stages: vec![
                        Stage {
                            name: "morning".into(),
                            hour_from: 6.0,
                            hour_to: 12.0,
                            on: None,
                        },
                        Stage {
                            name: "afternoon".into(),
                            hour_from: 12.0,
                            hour_to: 18.0,
                            on: None,
                        },
                    ],
                },
                runtime: ScenarioRuntime {
                    current_stage: Some(0),
                    ..Default::default()
                },
            },
        );
        let ctx = SharedMut::new(TulispContext::new());
        let site = MicrogridSite::new();
        let microgrids = crate::sim::microgrids::new_registry();
        microgrids.lock().insert(
            2200,
            crate::sim::microgrids::MicrogridEntry {
                def: crate::sim::microgrids::MicrogridDef {
                    id: 2200,
                    name: "default".into(),
                    grpc_port: 8800,
                    tso: None,
                },
                site,
            },
        );
        let current = crate::sim::microgrids::new_current_microgrid();
        let clock = Clock::default();
        // 15:30 local. Wallclock-current stage = 1; we should
        // transition.
        let now = chrono::TimeZone::with_ymd_and_hms(
            &chrono_tz::Europe::Berlin,
            2026,
            1,
            15,
            15,
            30,
            0,
        )
        .single()
        .unwrap()
        .with_timezone(&chrono::Utc);
        let moved = auto_advance_tick(&reg, &ctx, &microgrids, &current, &clock, now);
        assert_eq!(moved, vec!["two-stage".to_string()]);
        assert_eq!(reg.lock()["two-stage"].runtime.current_stage, Some(1));

        // Flip manual_override on; a second tick should be a no-op
        // even though wallclock still wants stage 1 (and stays at 1).
        reg.lock().get_mut("two-stage").unwrap().runtime.manual_override = true;
        let moved2 = auto_advance_tick(&reg, &ctx, &microgrids, &current, &clock, now);
        assert!(moved2.is_empty());
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
