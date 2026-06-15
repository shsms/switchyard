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
