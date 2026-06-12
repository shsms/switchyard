//! Tracks the deadline by which the most recent set-power request for
//! each (component, power axis) pair expires. Mirrors microsim's
//! TimeoutTracker, extended per-axis: active and reactive setpoints
//! carry independent request lifetimes, so a short-lived Q command
//! must not clear a long-lived P command when it expires (and vice
//! versa).
//!
//! Implementation is a `HashMap<(u64, SetpointAxis), Instant>` swept
//! by `remove_expired` once per timeout-loop tick (100 ms cadence).
//! Sweep is O(N) over active entries; with typical scales (tens of
//! components, occasional setpoint churn) that's a non-issue. For
//! large microgrids with thousands of active timeouts the natural
//! upgrade is a `BinaryHeap<(Instant, …)>` so the sweep pops only
//! the earliest-due entry. Defer that until the scan shows up in a
//! profile.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::Mutex;

/// Which setpoint a request lifetime governs. Active and reactive
/// commands time out independently; expiry resets only its own axis
/// (`SimulatedComponent::reset_setpoint_axis`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SetpointAxis {
    Active,
    Reactive,
}

#[derive(Clone, Default)]
pub struct TimeoutTracker {
    inner: Arc<Mutex<HashMap<(u64, SetpointAxis), Instant>>>,
}

impl TimeoutTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&self, id: u64, axis: SetpointAxis, lifetime: Duration) {
        self.inner
            .lock()
            .insert((id, axis), Instant::now() + lifetime);
    }

    pub fn remove_expired(&self) -> Vec<(u64, SetpointAxis)> {
        let now = Instant::now();
        let mut guard = self.inner.lock();
        let expired: Vec<(u64, SetpointAxis)> = guard
            .iter()
            .filter_map(|(key, deadline)| if *deadline <= now { Some(*key) } else { None })
            .collect();
        for key in &expired {
            guard.remove(key);
        }
        expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two axes hold independent deadlines for the same
    /// component: an elapsed reactive lifetime drains alone, leaving
    /// the active deadline armed.
    #[test]
    fn axes_expire_independently() {
        let t = TimeoutTracker::new();
        t.add(7, SetpointAxis::Active, Duration::from_secs(3600));
        t.add(7, SetpointAxis::Reactive, Duration::ZERO);
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(t.remove_expired(), vec![(7, SetpointAxis::Reactive)]);
        // The active deadline is untouched and still pending.
        assert_eq!(t.remove_expired(), Vec::new());
    }

    /// Latest-set-wins is per axis: re-arming the active deadline
    /// doesn't disturb the reactive one.
    #[test]
    fn rearming_one_axis_keeps_the_other() {
        let t = TimeoutTracker::new();
        t.add(7, SetpointAxis::Reactive, Duration::ZERO);
        t.add(7, SetpointAxis::Active, Duration::ZERO);
        t.add(7, SetpointAxis::Active, Duration::from_secs(3600));
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(t.remove_expired(), vec![(7, SetpointAxis::Reactive)]);
    }
}
