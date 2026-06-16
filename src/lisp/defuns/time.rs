//! `(now-seconds)` / `(window-elapsed)` — wall-clock helpers used
//! by time-driven load profiles — plus `(parse-offset STR)` /
//! `(parse-time-of-day STR)`, the human-time readers the scenario
//! section wrappers use to resolve cue/check times to seconds.

use tulisp::{Error, TulispContext, TulispObject};

use crate::sim::sim_clock::{parse_offset, parse_time_of_day};

pub(super) fn register(ctx: &mut TulispContext) {
    // chrono::Utc::now goes through the same clock_gettime(CLOCK_REALTIME)
    // syscall as std::time::SystemTime::now (both elide leap seconds the
    // same way the kernel does), but using chrono keeps these helpers
    // consistent with the rest of switchyard's time handling and lets us
    // extend with calendar-aware variants (seconds-since-midnight, etc.)
    // without swapping API later.

    // Wall-clock seconds since the Unix epoch as a float. Free-running
    // clock for time-driven load profiles.
    ctx.defun("now-seconds", || -> f64 {
        let now = chrono::Utc::now();
        now.timestamp() as f64 + now.timestamp_subsec_nanos() as f64 * 1e-9
    });

    // Seconds since the start of the most recent `window-secs`-aligned
    // window (anchored to the Unix epoch). For window-secs = 900,
    // returns 0..900 — equivalent to (mod (now-seconds) 900) but
    // expresses intent at the call site.
    ctx.defun("window-elapsed", |window_secs: f64| -> f64 {
        if window_secs <= 0.0 {
            return 0.0;
        }
        let now = chrono::Utc::now();
        let t = now.timestamp() as f64 + now.timestamp_subsec_nanos() as f64 * 1e-9;
        t.rem_euclid(window_secs)
    });

    // Resolve a relative human time offset ("500ms", "60s", "3min",
    // "2h") to seconds as a float. A number rides straight through, so
    // the section wrappers (`at` / `check`) can pass either a human
    // string or a bare numeric offset. A malformed string is a
    // scenario-authoring bug, so it errors rather than returning nil.
    ctx.defun("parse-offset", |t: TulispObject| -> Result<f64, Error> {
        time_to_secs(&t, parse_offset, "parse-offset")
    });

    // Resolve an absolute wall time "HH:MM" (24-hour) to seconds since
    // midnight. Used by absolute-schedule cue/check times; a number
    // (seconds since midnight) rides straight through.
    ctx.defun("parse-time-of-day", |t: TulispObject| -> Result<f64, Error> {
        time_to_secs(&t, parse_time_of_day, "parse-time-of-day")
    });
}

/// Shared body for the two time-parsing defuns: a number is returned
/// verbatim as seconds; a string is run through `parse` (`parse_offset`
/// or `parse_time_of_day`) and errors on a malformed value.
fn time_to_secs(
    t: &TulispObject,
    parse: fn(&str) -> Option<std::time::Duration>,
    who: &str,
) -> Result<f64, Error> {
    if t.numberp() {
        return f64::try_from(t.clone());
    }
    let s = String::try_from(t.clone())?;
    parse(&s)
        .map(|d| d.as_secs_f64())
        .ok_or_else(|| Error::os_error(format!("{who}: malformed time {s:?}")))
}

#[cfg(test)]
mod tests {
    use super::super::super::test_support::config_with;

    fn secs(cfg: &crate::lisp::Config, expr: &str) -> f64 {
        cfg.eval(expr).unwrap().parse().unwrap()
    }

    #[test]
    fn parse_offset_defun_handles_strings_and_numbers() {
        let (cfg, _dir) = config_with("(set-microgrid-id 9)");
        assert_eq!(secs(&cfg, "(parse-offset \"500ms\")"), 0.5);
        assert_eq!(secs(&cfg, "(parse-offset \"3min\")"), 180.0);
        // A bare number rides through as seconds.
        assert_eq!(secs(&cfg, "(parse-offset 90)"), 90.0);
        assert_eq!(secs(&cfg, "(parse-offset 1.5)"), 1.5);
        // Malformed strings error (a scenario-authoring bug).
        assert!(cfg.eval("(parse-offset \"nope\")").is_err());
        assert!(cfg.eval("(parse-offset \"-5s\")").is_err());
    }

    #[test]
    fn parse_time_of_day_defun_resolves_hhmm() {
        let (cfg, _dir) = config_with("(set-microgrid-id 9)");
        assert_eq!(secs(&cfg, "(parse-time-of-day \"00:00\")"), 0.0);
        assert_eq!(secs(&cfg, "(parse-time-of-day \"14:30\")"), 52200.0);
        assert!(cfg.eval("(parse-time-of-day \"24:00\")").is_err());
        assert!(cfg.eval("(parse-time-of-day \"noon\")").is_err());
    }
}
