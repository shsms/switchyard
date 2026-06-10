//! Scenarios: `(define-scenario …)` for the multi-stage registry +
//! the per-microgrid lifecycle defuns (`scenario-start`,
//! `-stop`, `-event`, `-record-csv`, `-stop-csv`, `-elapsed`).
//!
//! Both surfaces share data via `MicrogridSite`'s scenario journal;
//! keeping them in one file makes the read-write story obvious.

use chrono::Utc;
use tulisp::{Error, TulispContext, TulispObject};

use crate::sim::microgrids::SharedSiteRouter;

/// Newtype around `TulispObject` so `Vec<RawStage>` satisfies the
/// AsPlist field bound (which needs `TryFrom<TulispObject, Error =
/// tulisp::Error>`; the blanket impl on `TulispObject` is `Error =
/// Infallible`). Mirrors tradingsim's same-named helper.
pub struct RawStage(tulisp::TulispObject);

impl TryFrom<tulisp::TulispObject> for RawStage {
    type Error = tulisp::Error;
    fn try_from(v: tulisp::TulispObject) -> Result<Self, tulisp::Error> {
        Ok(RawStage(v))
    }
}

impl From<RawStage> for tulisp::TulispObject {
    fn from(v: RawStage) -> tulisp::TulispObject {
        v.0
    }
}

tulisp::AsPlist! {
    pub struct DefineScenarioArgs {
        name: String,
        description: Option<String> {= None},
        /// Calendar date the scenario is treated as taking place
        /// on, ISO `YYYY-MM-DD`. Optional — `None` falls back to
        /// wallclock-today.
        date: Option<String> {= None},
        stages: Vec<RawStage>,
    }
}

tulisp::AsPlist! {
    pub struct StageArgs {
        name: String,
        hour_from<":hour-from">: f64,
        hour_to<":hour-to">: f64,
        /// Optional tulisp lambda funcalled on stage entry by the
        /// auto-advance task. Receives no args; side-effects via
        /// the existing setter defuns (`set-active-power`,
        /// `set-meter-power`, `(every …)`, …) drive whatever the
        /// stage represents. Wrapped via `LispValue` so the raw
        /// lambda rides through `AsPlist!` (the bare `TulispObject`
        /// has `TryFrom::Error = Infallible`, which doesn't fit the
        /// macro's expected error shape).
        on: Option<crate::lisp::value::LispValue> {= None},
    }
}

/// `(define-scenario)` populates the multi-stage registry shared
/// with the UI Scenarios panel + the auto-advance task.
pub(in crate::lisp) fn register_registry(
    ctx: &mut TulispContext,
    scenarios: crate::sim::scenarios::SharedScenarios,
) {
    use crate::sim::scenarios::{ScenarioDef, ScenarioEntry, ScenarioRuntime, Stage};
    use tulisp::Plistable as _;
    ctx.defun(
        "define-scenario",
        move |ctx: &mut TulispContext,
              args: tulisp::Plist<DefineScenarioArgs>|
              -> Result<String, tulisp::Error> {
            let a = args.into_inner();
            let mut stages = Vec::new();
            for raw in a.stages {
                let s = StageArgs::from_plist(ctx, &raw.0)?;
                let on =
                    s.on.map(crate::lisp::value::LispValue::into_inner)
                        .filter(|o| !o.null());
                stages.push(Stage {
                    name: s.name,
                    hour_from: s.hour_from,
                    hour_to: s.hour_to,
                    on,
                });
            }
            let date = match a.date.as_deref() {
                None => None,
                Some(s) => Some(
                    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|e| {
                        tulisp::Error::os_error(format!(
                            "define-scenario: :date must be YYYY-MM-DD; got {s:?} ({e})"
                        ))
                    })?,
                ),
            };
            let def = ScenarioDef {
                name: a.name.clone(),
                description: a.description.unwrap_or_default(),
                date,
                stages,
            };
            scenarios.lock().insert(
                a.name.clone(),
                ScenarioEntry {
                    def,
                    runtime: ScenarioRuntime::default(),
                },
            );
            Ok(a.name)
        },
    );
}

/// Scenario lifecycle defuns. Scripts call `(scenario-start NAME)`
/// to mark the beginning, drop `(scenario-event KIND PAYLOAD)` markers
/// at interesting moments, and `(scenario-stop)` when finished. The
/// underlying journal lives on `MicrogridSite` and is read by the
/// `/api/scenario` and `/api/scenario/events` endpoints.
pub(super) fn register_lifecycle(ctx: &mut TulispContext, router: SharedSiteRouter) {
    let r = router.clone();
    ctx.defun(
        "scenario-start",
        move |name: String| -> Result<bool, Error> {
            let w = r.site();
            w.scenario_start(name, Utc::now());
            Ok(true)
        },
    );

    let r = router.clone();
    ctx.defun("scenario-stop", move || -> Result<bool, Error> {
        let w = r.site();
        w.scenario_stop(Utc::now());
        Ok(true)
    });

    let r = router.clone();
    ctx.defun(
        "scenario-event",
        move |kind: TulispObject, payload: TulispObject| -> Result<i64, Error> {
            let w = r.site();
            // Accept either a string or a symbol for `kind` so
            // scripts can write `(scenario-event 'outage "bat-1003")`
            // alongside `(scenario-event "note" "warming up")`.
            // Payload renders via Display so any Lisp value works.
            let kind_str = if kind.symbolp() {
                kind.to_string()
            } else {
                String::try_from(kind)?
            };
            let payload_str = payload.to_string();
            let id = w.scenario_record(kind_str, payload_str, Utc::now());
            Ok(id as i64)
        },
    );

    let r = router.clone();
    ctx.defun(
        "scenario-record-csv",
        move |dir: String| -> Result<i64, Error> {
            let w = r.site();
            let path = std::path::PathBuf::from(dir);
            w.scenario_open_csv(&path)
                .map(|n| n as i64)
                .map_err(|e| Error::os_error(format!("scenario-record-csv: {e}")))
        },
    );

    let r = router.clone();
    ctx.defun("scenario-stop-csv", move || -> Result<i64, Error> {
        let w = r.site();
        Ok(w.scenario_close_csv() as i64)
    });

    let r = router;
    ctx.defun("scenario-elapsed", move || -> Result<f64, Error> {
        let w = r.site();
        Ok(w.scenario_elapsed_s(Utc::now()))
    });
}

#[cfg(test)]
mod tests {
    use super::super::super::test_support::config_with;

    /// `(scenario-record-csv DIR)` opens one telemetry CSV per
    /// registered component — plus a setpoints + bounds CSV per
    /// envelope-bearing component; record_history_snapshot writes a
    /// telemetry row per pass; `(scenario-stop-csv)` flushes and
    /// closes them. Test asserts the file exists and contains a
    /// header + N rows.
    #[test]
    fn scenario_csv_records_per_component_files() {
        use chrono::Utc;
        let (cfg, dir) = config_with(
            "(set-microgrid-id 9)
             (%make-meter :id 1)
             (%make-battery :id 2)",
        );
        let csv_dir = dir.join("csvs");
        cfg.eval("(scenario-start \"csv\")").unwrap();
        let opened: i64 = cfg
            .eval(&format!(
                "(scenario-record-csv {:?})",
                csv_dir.to_str().unwrap()
            ))
            .unwrap()
            .parse()
            .unwrap();
        // Two telemetry files + the battery's setpoints + bounds
        // files; the meter reports no envelope so it gets neither.
        assert_eq!(opened, 4);
        assert!(!csv_dir.join("1-setpoints.csv").exists());
        assert!(!csv_dir.join("1-bounds.csv").exists());
        // Three snapshots → three rows + header.
        for _ in 0..3 {
            cfg.site().record_history_snapshot(Utc::now());
        }
        cfg.eval("(scenario-stop-csv)").unwrap();

        let meter_csv = std::fs::read_to_string(csv_dir.join("1-meter.csv")).unwrap();
        let battery_csv = std::fs::read_to_string(csv_dir.join("2-battery.csv")).unwrap();
        // Header line + 3 data rows = 4 lines (last one ends in
        // newline so split gives 5 elements with trailing empty).
        assert_eq!(meter_csv.lines().count(), 4, "meter csv: {meter_csv}");
        assert_eq!(battery_csv.lines().count(), 4, "battery csv: {battery_csv}");
        assert!(meter_csv.starts_with("ts_iso,active_power_w"));
        // Battery rows have an empty active_power_w cell (it
        // publishes dc_power_w instead) — the column shape stays
        // uniform.
        let first_data = battery_csv.lines().nth(1).unwrap();
        assert!(
            first_data.starts_with("20") && first_data.contains(",,"),
            "expected empty active_power cell, got {first_data}"
        );
    }

    /// The setpoints CSV gets one row per `log_setpoint` (event-
    /// driven), the bounds CSV one row per snapshot pass (sampled) —
    /// the inputs a bound-setting control app pushed plus the
    /// envelope it produced, replayable without a log scrape.
    #[test]
    fn scenario_csv_records_setpoints_and_bounds() {
        use crate::sim::setpoints::{SetpointEvent, SetpointKind, SetpointOutcome};
        use chrono::Utc;
        let (cfg, dir) = config_with(
            "(set-microgrid-id 9)
             (%make-battery :id 2
                            :capacity 100000.0
                            :rated-lower -10000.0
                            :rated-upper 10000.0)",
        );
        let csv_dir = dir.join("csvs");
        cfg.eval("(scenario-start \"io\")").unwrap();
        cfg.eval(&format!(
            "(scenario-record-csv {:?})",
            csv_dir.to_str().unwrap()
        ))
        .unwrap();

        let now = Utc::now();
        cfg.site().log_setpoint(
            2,
            SetpointEvent {
                ts: now,
                kind: SetpointKind::ActivePower,
                value: 1500.0,
                ttl_s: Some(60),
                outcome: SetpointOutcome::Accepted {
                    effective_value: Some(1500.0),
                },
            },
        );
        cfg.site().log_setpoint(
            2,
            SetpointEvent {
                ts: now,
                kind: SetpointKind::AugmentBounds,
                value: 0.0,
                ttl_s: Some(30),
                outcome: SetpointOutcome::Rejected {
                    reason: "augmentation bound [a, b] is inverted".into(),
                },
            },
        );
        // Two sampling passes → two bounds rows.
        cfg.site().record_history_snapshot(now);
        cfg.site().record_history_snapshot(now);
        cfg.eval("(scenario-stop-csv)").unwrap();

        let setpoints = std::fs::read_to_string(csv_dir.join("2-setpoints.csv")).unwrap();
        let mut lines = setpoints.lines();
        assert_eq!(
            lines.next().unwrap(),
            "ts_iso,kind,value,ttl_s,accepted,effective_value,reason"
        );
        let accepted = lines.next().unwrap();
        assert!(
            accepted.contains(",active_power,1500,60,true,1500,"),
            "accepted row: {accepted}"
        );
        let rejected = lines.next().unwrap();
        // The reason holds a comma, so it must arrive CSV-quoted.
        assert!(
            rejected
                .contains(",augment_bounds,0,30,false,,\"augmentation bound [a, b] is inverted\""),
            "rejected row: {rejected}"
        );
        assert!(lines.next().is_none());

        let bounds = std::fs::read_to_string(csv_dir.join("2-bounds.csv")).unwrap();
        let rows: Vec<&str> = bounds.lines().collect();
        assert_eq!(rows[0], "ts_iso,lower_w,upper_w,bands");
        assert_eq!(rows.len(), 3, "bounds csv: {bounds}");
        // Fresh battery at default SoC — effective == rated.
        assert!(
            rows[1].ends_with(",-10000,10000,-10000:10000"),
            "bounds row: {}",
            rows[1]
        );
    }

    /// sim/scenarios.lisp loads cleanly and the random-* helpers
    /// produce values in their stated range.
    #[test]
    fn scenarios_helpers_load_and_run() {
        let (cfg, dir) = config_with("(set-microgrid-id 9)");
        // Copy sim/scenarios.lisp into the test's load dir so
        // (load "sim/scenarios.lisp") finds it.
        let src = std::path::Path::new("sim/scenarios.lisp");
        let dst_dir = dir.join("sim");
        std::fs::create_dir_all(&dst_dir).unwrap();
        std::fs::copy(src, dst_dir.join("scenarios.lisp")).unwrap();
        cfg.eval("(load \"sim/scenarios.lisp\")").unwrap();
        // 100 draws of random-uniform should all land in [10, 20).
        for _ in 0..100 {
            let v: f64 = cfg
                .eval("(random-uniform 10.0 20.0)")
                .unwrap()
                .parse()
                .unwrap();
            assert!((10.0..20.0).contains(&v), "out-of-range {v}");
        }
        // random-pick over a 3-element list always returns one of
        // them.
        for _ in 0..100 {
            let v = cfg.eval("(random-pick '(11 22 33))").unwrap();
            assert!(["11", "22", "33"].contains(&v.as_str()), "got {v}");
        }
        // random-pick on empty list returns nil.
        assert_eq!(cfg.eval("(random-pick '())").unwrap(), "nil");
    }

    /// `(scenario-start)` opens a scenario, `(scenario-event)`
    /// appends to the journal, `(scenario-elapsed)` returns wall-
    /// clock seconds since start, `(scenario-stop)` freezes it.
    #[test]
    fn scenario_lifecycle_round_trips_through_lisp() {
        let (cfg, _dir) = config_with("(set-microgrid-id 9)");
        cfg.eval("(scenario-start \"warmup\")").unwrap();
        let summary = cfg.site().scenario_summary(chrono::Utc::now());
        assert_eq!(summary.name.as_deref(), Some("warmup"));
        assert!(summary.started_at.is_some());
        assert!(summary.ended_at.is_none());
        assert_eq!(summary.event_count, 0);

        // First event id is 0.
        cfg.eval("(scenario-event 'outage \"bat-1003\")").unwrap();
        cfg.eval("(scenario-event \"note\" \"warming up\")")
            .unwrap();
        let summary = cfg.site().scenario_summary(chrono::Utc::now());
        assert_eq!(summary.event_count, 2);
        assert_eq!(summary.next_event_id, 2);

        let events = cfg.site().scenario_events_since(0, 100);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "outage");
        assert_eq!(events[1].kind, "note");

        // Stop freezes elapsed; a subsequent (scenario-elapsed)
        // returns the frozen value rather than continuing to grow.
        cfg.eval("(scenario-stop)").unwrap();
        let frozen = cfg.site().scenario_summary(chrono::Utc::now());
        std::thread::sleep(std::time::Duration::from_millis(20));
        let later = cfg.site().scenario_summary(chrono::Utc::now());
        assert_eq!(frozen.elapsed_s, later.elapsed_s);
        assert!(frozen.ended_at.is_some());
    }

    /// `(define-scenario)` parses a multi-stage definition into the
    /// shared registry. Stage windows + the optional :on lambda
    /// round-trip; missing :on leaves `Stage::on = None` so the
    /// auto-advance task knows to skip the funcall step.
    #[test]
    fn define_scenario_registers_with_stages() {
        let (cfg, _dir) = config_with("(set-microgrid-id 9)");
        cfg.eval(
            r#"
            (define-scenario
              :name "evening-peak"
              :description "Consumer ramp 17:00 → 21:00"
              :date "2026-01-15"
              :stages
              '((:name "ramp" :hour-from 17 :hour-to 18
                 :on (lambda () (set-active-power 1001 5000)))
                (:name "peak" :hour-from 18 :hour-to 20
                 :on (lambda () (set-active-power 1001 25000)))
                (:name "wind-down" :hour-from 20 :hour-to 21)))
            "#,
        )
        .unwrap();
        let regs = cfg.scenarios();
        let r = regs.lock();
        let e = r.get("evening-peak").expect("registered");
        assert_eq!(e.def.description, "Consumer ramp 17:00 → 21:00");
        assert_eq!(
            e.def.date,
            Some(chrono::NaiveDate::from_ymd_opt(2026, 1, 15).unwrap())
        );
        assert_eq!(e.def.stages.len(), 3);
        assert_eq!(e.def.stages[0].name, "ramp");
        assert_eq!(e.def.stages[0].hour_from, 17.0);
        assert_eq!(e.def.stages[0].hour_to, 18.0);
        assert!(e.def.stages[0].on.is_some());
        assert!(e.def.stages[1].on.is_some());
        // Third stage has no :on -> Stage::on stays None.
        assert!(e.def.stages[2].on.is_none());
    }

    /// Battery DC power integrates into the journal's per-battery
    /// charge / discharge integrals. Drive a battery via its
    /// inverter, advance physics + sampling, and assert the totals.
    #[test]
    fn battery_charge_discharge_integrates_through_snapshot() {
        use chrono::{Duration as ChronoDuration, Utc};
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (setq b (%make-battery :id 100
                                    :capacity 100000.0
                                    :rated-lower -10000.0
                                    :rated-upper 10000.0))
             (%make-battery-inverter :id 200
                                     :rated-lower -10000.0
                                     :rated-upper 10000.0
                                     :successors (list b))",
        );
        cfg.eval("(scenario-start \"integrate\")").unwrap();
        // Push a charge setpoint of +3600 W for 10 sim-seconds.
        cfg.eval("(set-active-power 200 3600.0 60000)").unwrap();
        // Advance physics enough to settle the ramp; default ramp
        // is infinity so one tick is enough.
        let mut now = Utc::now();
        cfg.site()
            .tick_once(now, std::time::Duration::from_millis(100));
        // Snapshot pass at t0 — first one just seeds the cursor
        // (dt from start is small but non-zero — ignore the result).
        cfg.site().record_history_snapshot(now);
        now += ChronoDuration::seconds(10);
        cfg.site()
            .tick_once(now, std::time::Duration::from_secs(10));
        cfg.site().record_history_snapshot(now);
        let r = cfg.site().scenario_report(now);
        // 3600 W for 10 s = 10 Wh. Allow some slop for the seed
        // sample's dt at start.
        assert!(
            r.total_battery_charged_wh > 8.0 && r.total_battery_charged_wh < 12.0,
            "expected ~10 Wh charged, got {}",
            r.total_battery_charged_wh,
        );
        assert_eq!(r.total_battery_discharged_wh, 0.0);

        // Now flip to discharging.
        cfg.eval("(set-active-power 200 -7200.0 60000)").unwrap();
        cfg.site()
            .tick_once(now, std::time::Duration::from_millis(100));
        now += ChronoDuration::seconds(5);
        cfg.site().tick_once(now, std::time::Duration::from_secs(5));
        cfg.site().record_history_snapshot(now);
        let r = cfg.site().scenario_report(now);
        // 7200 W * 5 s / 3600 = 10 Wh discharged.
        assert!(
            r.total_battery_discharged_wh > 8.0 && r.total_battery_discharged_wh < 12.0,
            "expected ~10 Wh discharged, got {}",
            r.total_battery_discharged_wh,
        );
        assert_eq!(r.per_battery.len(), 1);
        assert_eq!(r.per_battery[0].id, 100);
    }

    /// `:main t` on a meter wires it as the scenario reporter's
    /// peak source. record_history_snapshot updates the journal's
    /// peak each tick; scenario_start resets it.
    #[test]
    fn main_meter_peak_tracks_active_power() {
        use chrono::Utc;
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (%make-meter :id 1 :main t :power 1000.0)",
        );
        // Pre-start, sampling shouldn't update the peak — the
        // scenario hasn't begun.
        cfg.site().record_history_snapshot(Utc::now());
        assert_eq!(
            cfg.site().scenario_report(Utc::now()).peak_main_meter_w,
            0.0,
        );

        cfg.eval("(scenario-start \"power\")").unwrap();
        cfg.eval("(set-meter-power 1 2500.0)").unwrap();
        cfg.site().record_history_snapshot(Utc::now());
        let r = cfg.site().scenario_report(Utc::now());
        assert!((r.peak_main_meter_w - 2500.0).abs() < 1e-3);

        // A higher value lifts the peak; a later lower one
        // doesn't.
        cfg.eval("(set-meter-power 1 7800.0)").unwrap();
        cfg.site().record_history_snapshot(Utc::now());
        cfg.eval("(set-meter-power 1 1100.0)").unwrap();
        cfg.site().record_history_snapshot(Utc::now());
        let r = cfg.site().scenario_report(Utc::now());
        assert!((r.peak_main_meter_w - 7800.0).abs() < 1e-3);

        // scenario-start resets the peak.
        cfg.eval("(scenario-start \"again\")").unwrap();
        cfg.eval("(set-meter-power 1 500.0)").unwrap();
        cfg.site().record_history_snapshot(Utc::now());
        assert!((cfg.site().scenario_report(Utc::now()).peak_main_meter_w - 500.0).abs() < 1e-3,);
    }

    /// A second `(scenario-start)` clears the previous run's events
    /// but keeps the monotonic id counter so polling clients with a
    /// `since=` cursor see new events immediately rather than
    /// rewinding through stale ids.
    #[test]
    fn scenario_restart_clears_events_keeps_ids_monotonic() {
        let (cfg, _dir) = config_with("(set-microgrid-id 9)");
        cfg.eval("(scenario-start \"first\")").unwrap();
        cfg.eval("(scenario-event 'a \"\")").unwrap();
        cfg.eval("(scenario-event 'b \"\")").unwrap();
        assert_eq!(
            cfg.site()
                .scenario_summary(chrono::Utc::now())
                .next_event_id,
            2
        );
        cfg.eval("(scenario-start \"second\")").unwrap();
        let summary = cfg.site().scenario_summary(chrono::Utc::now());
        assert_eq!(summary.event_count, 0);
        assert_eq!(summary.next_event_id, 2);
        let id = cfg
            .eval("(scenario-event 'c \"\")")
            .unwrap()
            .parse::<i64>()
            .unwrap();
        assert_eq!(id, 2);
    }
}
