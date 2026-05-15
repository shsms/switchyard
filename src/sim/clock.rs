//! Simulation clock. Owns a [`chrono_tz::Tz`] the UI uses to render
//! timestamps in the configured civil zone. The gRPC + physics
//! boundaries stay UTC (every `Telemetry` timestamp is `DateTime<Utc>`);
//! the clock is purely a display-side affordance so a developer
//! running a Berlin-anchored sim from EDT doesn't see every
//! timestamp shifted by 6 h.
//!
//! `(set-timezone "Europe/Berlin")` in config.lisp redirects.
//! Mutable behind an `RwLock` so a hot reload can change the zone
//! without a process restart — the UI's TZ toggle picks up the new
//! name on its next /api/clock poll.

use std::sync::Arc;

use chrono_tz::Tz;
use parking_lot::RwLock;

/// Default zone — matches tradingsim's default. Europe/Berlin
/// follows CET/CEST DST transitions via the IANA database, which
/// is what scenario stages keyed by hour-of-day expect.
pub const DEFAULT_TZ: Tz = chrono_tz::Europe::Berlin;

#[derive(Clone, Debug)]
pub struct Clock {
    pub tz: Tz,
}

impl Clock {
    pub fn new(tz: Tz) -> Self {
        Self { tz }
    }

    /// IANA name, e.g. "Europe/Berlin". The UI's TZ toggle uses
    /// this as the second option for `Intl.DateTimeFormat`'s
    /// `timeZone`; "UTC" is the other.
    pub fn tz_name(&self) -> &'static str {
        self.tz.name()
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new(DEFAULT_TZ)
    }
}

pub type SharedClock = Arc<RwLock<Clock>>;

pub fn new_clock() -> SharedClock {
    Arc::new(RwLock::new(Clock::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_europe_berlin() {
        let c = Clock::default();
        assert_eq!(c.tz_name(), "Europe/Berlin");
    }

    #[test]
    fn tz_name_round_trips_parse() {
        let tz: Tz = "America/New_York"
            .parse()
            .expect("America/New_York should parse");
        let c = Clock::new(tz);
        assert_eq!(c.tz_name(), "America/New_York");
    }

    #[test]
    fn invalid_zone_fails_parse() {
        // Validates that the parse path Tz uses is strict — a
        // typo at config-load time should surface as an error
        // rather than silently fall through to UTC.
        let result: Result<Tz, _> = "Europe/Bogus".parse();
        assert!(result.is_err());
    }
}
