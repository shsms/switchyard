//! The source of "now" for scenario time (and, in headless runs,
//! physics stepping).
//!
//! Live mode reads the wall clock — identical to calling `Utc::now()`
//! directly, so the running server is unaffected. A headless sim run
//! instead reports a fixed base time plus the elapsed offset of a shared
//! [`tulisp_async::ManualClock`] — the same clock the timer queue runs
//! on — so timers, scenario time, and physics all advance together when
//! the driver advances the clock. That makes a scenario run
//! deterministically and faster than real time.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use tulisp_async::ManualClock;

/// A clonable handle yielding the current simulation time as a
/// `DateTime<Utc>`. Cheap to clone (one `Arc`).
#[derive(Clone)]
pub struct NowSource(Arc<Inner>);

enum Inner {
    /// `now()` is `Utc::now()`.
    Wall,
    /// `now()` is `base + clock.elapsed()`.
    Sim {
        base: DateTime<Utc>,
        clock: Arc<ManualClock>,
    },
}

impl NowSource {
    /// Wall-clock source — the live-server default.
    pub fn wall() -> Self {
        Self(Arc::new(Inner::Wall))
    }

    /// Simulated source tied to `clock`: `now()` advances exactly as the
    /// host advances `clock`, off a fixed `base`.
    pub fn sim(base: DateTime<Utc>, clock: Arc<ManualClock>) -> Self {
        Self(Arc::new(Inner::Sim { base, clock }))
    }

    pub fn now(&self) -> DateTime<Utc> {
        match &*self.0 {
            Inner::Wall => Utc::now(),
            Inner::Sim { base, clock } => {
                *base + chrono::Duration::from_std(clock.elapsed()).unwrap_or_default()
            }
        }
    }
}

impl Default for NowSource {
    fn default() -> Self {
        Self::wall()
    }
}

/// A fixed base instant for headless runs, so absolute timestamps are
/// reproducible across runs (scenario *elapsed* is relative and so
/// independent of the base, but journaled `ts` values become stable too).
pub fn headless_base() -> DateTime<Utc> {
    DateTime::from_timestamp(1_577_836_800, 0).expect("2020-01-01T00:00:00Z is valid")
}

/// Parse a human time offset into a [`Duration`]. Accepts a unit suffix
/// — `ms`, `s`, `sec`, `m`, `min`, `h`, `hr` — or a bare number (seconds).
/// Used for relative scenario cue times (`"500ms"`, `"60s"`, `"3min"`).
/// `None` on a malformed or negative value.
pub fn parse_offset(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    let (num, unit): (&str, &str) = match s.find(|c: char| c.is_alphabetic()) {
        Some(i) => (s[..i].trim(), s[i..].trim()),
        None => (s, "s"),
    };
    let v: f64 = num.parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    let secs = match unit {
        "ms" => v / 1000.0,
        "s" | "sec" | "secs" => v,
        "m" | "min" | "mins" => v * 60.0,
        "h" | "hr" | "hrs" => v * 3600.0,
        _ => return None,
    };
    Some(std::time::Duration::from_secs_f64(secs))
}

/// Parse an absolute wall time `"HH:MM"` (24-hour) into the offset from
/// midnight. Used for `:schedule 'absolute` cue/stage times. `None` on a
/// malformed value or out-of-range hour/minute.
pub fn parse_time_of_day(s: &str) -> Option<std::time::Duration> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u64 = h.trim().parse().ok()?;
    let m: u64 = m.trim().parse().ok()?;
    if h >= 24 || m >= 60 {
        return None;
    }
    Some(std::time::Duration::from_secs(h * 3600 + m * 60))
}

#[cfg(test)]
mod tests {
    use super::{parse_offset, parse_time_of_day};
    use std::time::Duration;

    #[test]
    fn parse_offset_units() {
        assert_eq!(parse_offset("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_offset("60s"), Some(Duration::from_secs(60)));
        assert_eq!(parse_offset("3min"), Some(Duration::from_secs(180)));
        assert_eq!(parse_offset("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_offset("90"), Some(Duration::from_secs(90))); // bare = seconds
        assert_eq!(parse_offset("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_offset("nope"), None);
        assert_eq!(parse_offset("-5s"), None);
        assert_eq!(parse_offset("5 light-years"), None);
    }

    #[test]
    fn parse_time_of_day_hhmm() {
        assert_eq!(parse_time_of_day("00:00"), Some(Duration::ZERO));
        assert_eq!(parse_time_of_day("14:30"), Some(Duration::from_secs(52200)));
        assert_eq!(parse_time_of_day("24:00"), None);
        assert_eq!(parse_time_of_day("12:60"), None);
        assert_eq!(parse_time_of_day("noon"), None);
    }
}
