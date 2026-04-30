//! Two orthogonal control-loop primitives shared by inverters, EV
//! chargers, anything else that does not respond instantly to a
//! set-point command.
//!
//! A real inverter takes some time to acknowledge a SCADA command (the
//! `CommandDelay`) and then ramps power toward the target at a slew
//! rate (the `Ramp`) — exceeding the slew rate would damage capacitors,
//! breakers, or the battery itself. Tests for both live next to the
//! implementations.

use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

/// Holds a pending set-point that becomes "armed" only after `delay`
/// has elapsed since `set` was called.
#[derive(Debug)]
pub struct CommandDelay {
    state: Mutex<State>,
    delay: Duration,
}

#[derive(Debug, Clone)]
struct State {
    pending: Option<(DateTime<Utc>, f32)>,
    armed: Option<f32>,
}

impl CommandDelay {
    pub fn new(delay: Duration) -> Self {
        Self {
            state: Mutex::new(State {
                pending: None,
                armed: None,
            }),
            delay,
        }
    }

    pub fn delay(&self) -> Duration {
        self.delay
    }

    pub fn set_target(&self, now: DateTime<Utc>, value: f32) {
        let mut s = self.state.lock();
        if self.delay.is_zero() {
            s.armed = Some(value);
            s.pending = None;
        } else {
            s.pending = Some((now, value));
        }
    }

    /// Promote pending → armed if its delay has elapsed. Returns the
    /// currently armed value (None until the first command finishes).
    pub fn poll(&self, now: DateTime<Utc>) -> Option<f32> {
        let mut s = self.state.lock();
        if let Some((set_at, v)) = s.pending {
            let due =
                set_at + chrono::Duration::from_std(self.delay).unwrap_or(chrono::Duration::zero());
            if now >= due {
                s.armed = Some(v);
                s.pending = None;
            }
        }
        s.armed
    }

    pub fn reset(&self) {
        let mut s = self.state.lock();
        s.armed = None;
        s.pending = None;
    }

    /// Inspect the armed value without advancing the delay clock.
    pub fn armed(&self) -> Option<f32> {
        self.state.lock().armed
    }
}

/// Slew-rate-limited tracker: `actual` moves toward `target` at most
/// `rate_w_per_s` per second.
///
/// Use `rate = f32::INFINITY` to make the tracker pass-through (the
/// behaviour of microsim's inverters today).
#[derive(Debug)]
pub struct Ramp {
    state: Mutex<RampState>,
    rate_w_per_s: f32,
}

#[derive(Debug, Clone)]
struct RampState {
    actual: f32,
    target: f32,
}

impl Ramp {
    pub fn new(rate_w_per_s: f32, initial: f32) -> Self {
        Self {
            state: Mutex::new(RampState {
                actual: initial,
                target: initial,
            }),
            rate_w_per_s,
        }
    }

    pub fn rate(&self) -> f32 {
        self.rate_w_per_s
    }

    pub fn set_target(&self, target: f32) {
        self.state.lock().target = target;
    }

    pub fn snap_to(&self, value: f32) {
        let mut s = self.state.lock();
        s.target = value;
        s.actual = value;
    }

    pub fn actual(&self) -> f32 {
        self.state.lock().actual
    }

    pub fn target(&self) -> f32 {
        self.state.lock().target
    }

    /// Advance `actual` by the most it is allowed to move in `dt`.
    pub fn advance(&self, dt: Duration) -> f32 {
        let mut s = self.state.lock();
        if !self.rate_w_per_s.is_finite() {
            s.actual = s.target;
            return s.actual;
        }
        let max_step = self.rate_w_per_s * dt.as_secs_f32();
        let diff = s.target - s.actual;
        if diff.abs() <= max_step {
            s.actual = s.target;
        } else {
            s.actual += diff.signum() * max_step;
        }
        s.actual
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_delay_zero_arms_immediately() {
        let cd = CommandDelay::new(Duration::ZERO);
        cd.set_target(Utc::now(), 5000.0);
        assert_eq!(cd.armed(), Some(5000.0));
    }

    #[test]
    fn command_delay_blocks_until_due() {
        let t0 = Utc::now();
        let cd = CommandDelay::new(Duration::from_secs(2));
        cd.set_target(t0, 5000.0);
        assert_eq!(cd.poll(t0 + chrono::Duration::seconds(1)), None);
        assert_eq!(cd.poll(t0 + chrono::Duration::seconds(2)), Some(5000.0));
    }

    #[test]
    fn ramp_step_limit() {
        let r = Ramp::new(1000.0, 0.0);
        r.set_target(5000.0);
        assert_eq!(r.advance(Duration::from_secs(1)), 1000.0);
        assert_eq!(r.advance(Duration::from_secs(1)), 2000.0);
        // Big jump → step caps it
        assert_eq!(r.advance(Duration::from_secs(2)), 4000.0);
    }

    #[test]
    fn ramp_pass_through_when_infinite() {
        let r = Ramp::new(f32::INFINITY, 0.0);
        r.set_target(5000.0);
        assert_eq!(r.advance(Duration::from_millis(1)), 5000.0);
    }
}
