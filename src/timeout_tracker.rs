//! Tracks the deadline by which the most recent set-power request for
//! each component expires. Mirrors microsim's TimeoutTracker.
//!
//! Implementation is a `HashMap<u64, Instant>` swept by
//! `remove_expired` once per timeout-loop tick (100 ms cadence).
//! Sweep is O(N) over active entries; with typical scales (tens of
//! components, occasional setpoint churn) that's a non-issue. For
//! large microgrids with thousands of active timeouts the natural
//! upgrade is a `BinaryHeap<(Instant, u64)>` so the sweep pops only
//! the earliest-due entry. Defer that until the scan shows up in a
//! profile.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::Mutex;

#[derive(Clone, Default)]
pub struct TimeoutTracker {
    inner: Arc<Mutex<HashMap<u64, Instant>>>,
}

impl TimeoutTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&self, id: u64, lifetime: Duration) {
        self.inner.lock().insert(id, Instant::now() + lifetime);
    }

    pub fn remove_expired(&self) -> Vec<u64> {
        let now = Instant::now();
        let mut guard = self.inner.lock();
        let expired: Vec<u64> = guard
            .iter()
            .filter_map(|(id, deadline)| if *deadline <= now { Some(*id) } else { None })
            .collect();
        for id in &expired {
            guard.remove(id);
        }
        expired
    }
}
