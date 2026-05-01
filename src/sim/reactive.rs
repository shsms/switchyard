//! Inverter reactive-power capability envelope and the per-tick
//! state machine that drives Q through a command-delay and ramp.
//!
//! Two pieces:
//! - [`ReactiveCapability`]: the envelope shape (PF-limit, kVA-limit,
//!   or both). Pure data; cheap to copy.
//! - [`ReactivePath`]: the full per-inverter state — capability +
//!   delay + slew + last-published Q. Centralises the validation and
//!   tick logic that BatteryInverter and SolarInverter both need.
//!
//! Real inverters limit Q via two composable constraints:
//!
//! 1. **PF-limit**: `|Q| ≤ k × |P|`. A power-factor floor — when `P → 0`,
//!    Q allowance also collapses to zero. Common interconnection
//!    requirement (e.g. IEEE 1547-2018 default PF ≥ 0.85 ↔ k ≈ 0.62;
//!    microsim's hardcoded 0.35 ↔ PF ≥ 0.94).
//! 2. **Apparent / kVA-limit**: `P² + Q² ≤ S_rated²`. Hardware envelope;
//!    full Q is allowed at P=0, shrinking to √(S² − P²) as P grows.
//!
//! Both are optional; either, both, or neither may be set per
//! inverter. The effective Q-bound at a given P is their
//! intersection.

use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    SetpointError,
    ramp::{CommandDelay, Ramp},
};

#[derive(Clone, Copy, Debug, Default)]
pub struct ReactiveCapability {
    /// `|Q| ≤ pf_limit × |P|`. None disables the PF cap.
    pub pf_limit: Option<f32>,
    /// `√(P² + Q²) ≤ apparent_va`. None disables the kVA cap.
    pub apparent_va: Option<f32>,
}

impl ReactiveCapability {
    /// Microsim-compatible default: ±35% of |P|, no kVA cap.
    pub fn microsim_default() -> Self {
        Self {
            pf_limit: Some(0.35),
            apparent_va: None,
        }
    }

    /// Live `(lower, upper)` Q bounds at the given active P.
    /// `(0.0, 0.0)` when P exceeds the apparent envelope (no headroom
    /// for any Q).
    pub fn q_bounds_at(&self, p: f32) -> (f32, f32) {
        let mut lo = f32::NEG_INFINITY;
        let mut hi = f32::INFINITY;

        if let Some(k) = self.pf_limit {
            let q_max = k * p.abs();
            lo = lo.max(-q_max);
            hi = hi.min(q_max);
        }

        if let Some(s) = self.apparent_va {
            if p.abs() >= s {
                return (0.0, 0.0);
            }
            let q_max = (s * s - p * p).max(0.0).sqrt();
            lo = lo.max(-q_max);
            hi = hi.min(q_max);
        }

        // No constraint configured at all → "anything goes" is too
        // dangerous; clamp to a finite default so the proto bounds
        // field doesn't carry ±∞.
        if !lo.is_finite() || !hi.is_finite() {
            lo = -p.abs();
            hi = p.abs();
        }

        if lo > hi { (0.0, 0.0) } else { (lo, hi) }
    }

    pub fn contains(&self, p: f32, q: f32) -> bool {
        let (lo, hi) = self.q_bounds_at(p);
        q >= lo && q <= hi
    }
}

/// Per-inverter reactive-power state machine. Owns the capability
/// envelope, the command-delay queue, the slew-rate-limited ramp, and
/// the last-published Q value that telemetry / parent meters read.
///
/// The lifecycle is the same for every smart inverter:
///   1. `accept_setpoint(p_live, vars)` — validate against the live
///      envelope at the current active power, enqueue into the delay
///      queue. Returns `OutOfBounds` for envelope violations.
///   2. `step(p_live, now, dt)` — promote any pending command (re-
///      clamping to the envelope at this `p_live`, in case P drifted
///      while the command was in the queue), advance the ramp by
///      `dt`, store the result as the new published value, return it.
///   3. (optional) `override_published(q)` — for inverters whose
///      downstream measurement clips the ramp value (e.g. a battery
///      that refused part of the apparent share). Solar leaves this
///      alone; the published value stays equal to the ramp output.
///   4. `published()` — read what telemetry publishes / what a
///      parent meter aggregates.
pub struct ReactivePath {
    capability: Mutex<ReactiveCapability>,
    delay: CommandDelay,
    ramp: Ramp,
    /// Last published Q (telemetry / aggregate read). Equals
    /// `ramp.actual()` after every `step` unless `override_published`
    /// is called between ticks.
    published: Mutex<f32>,
}

impl ReactivePath {
    pub fn new(
        capability: ReactiveCapability,
        command_delay: Duration,
        ramp_rate_var_per_s: f32,
    ) -> Self {
        Self {
            capability: Mutex::new(capability),
            delay: CommandDelay::new(command_delay),
            ramp: Ramp::new(ramp_rate_var_per_s, 0.0),
            published: Mutex::new(0.0),
        }
    }

    /// Validate against the live envelope at the current P, then
    /// enqueue into the command-delay queue. Returns `OutOfBounds` if
    /// the request exceeds the envelope.
    pub fn accept_setpoint(&self, p_live: f32, vars: f32) -> Result<(), SetpointError> {
        let (lo, hi) = self.capability.lock().q_bounds_at(p_live);
        if vars < lo || vars > hi {
            return Err(SetpointError::OutOfBounds {
                value: vars,
                lower: lo,
                upper: hi,
            });
        }
        self.delay.set_target(Utc::now(), vars);
        Ok(())
    }

    /// Advance one tick: promote any pending command (re-clamped to
    /// the live envelope at `p_live`), slew the ramp by `dt`, publish
    /// the result, and return it. Idempotent if there's no pending
    /// command and the ramp has already settled.
    pub fn step(&self, p_live: f32, now: DateTime<Utc>, dt: Duration) -> f32 {
        if let Some(target) = self.delay.poll(now) {
            let (lo, hi) = self.capability.lock().q_bounds_at(p_live);
            self.ramp.set_target(target.clamp(lo, hi));
        }
        let actual = self.ramp.advance(dt);
        *self.published.lock() = actual;
        actual
    }

    /// Replace the last-published value — for inverters whose
    /// downstream actually clips Q (the BMS contract on a battery
    /// today is to pass Q through unchanged, so this is a no-op
    /// there, but the API is here for the day a child refuses).
    pub fn override_published(&self, q: f32) {
        *self.published.lock() = q;
    }

    pub fn published(&self) -> f32 {
        *self.published.lock()
    }

    pub fn bounds_at(&self, p_live: f32) -> (f32, f32) {
        self.capability.lock().q_bounds_at(p_live)
    }

    pub fn set_pf_limit(&self, pf: Option<f32>) {
        self.capability.lock().pf_limit = pf;
    }

    pub fn set_apparent_va(&self, va: Option<f32>) {
        self.capability.lock().apparent_va = va;
    }

    pub fn reset(&self) {
        self.delay.reset();
        self.ramp.set_target(0.0);
        *self.published.lock() = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pf_limit_scales_with_p() {
        let cap = ReactiveCapability {
            pf_limit: Some(0.35),
            apparent_va: None,
        };
        assert_eq!(cap.q_bounds_at(0.0), (0.0, 0.0));
        let (lo, hi) = cap.q_bounds_at(10_000.0);
        assert!((lo + 3500.0).abs() < 1e-3);
        assert!((hi - 3500.0).abs() < 1e-3);
        // sign of P doesn't matter (discharge is symmetric)
        assert_eq!(cap.q_bounds_at(-10_000.0), (lo, hi));
    }

    #[test]
    fn kva_limit_is_circle() {
        let cap = ReactiveCapability {
            pf_limit: None,
            apparent_va: Some(10_000.0),
        };
        // P=0 → full Q allowance (±10 kVA)
        let (lo, hi) = cap.q_bounds_at(0.0);
        assert!((lo + 10_000.0).abs() < 1e-3);
        assert!((hi - 10_000.0).abs() < 1e-3);
        // P=6 kW, S=10 kVA → Q_max = sqrt(100-36)*1000 = 8 kVA
        let (lo, hi) = cap.q_bounds_at(6_000.0);
        assert!((lo + 8_000.0).abs() < 1e-3);
        assert!((hi - 8_000.0).abs() < 1e-3);
        // P at the rim → no Q
        assert_eq!(cap.q_bounds_at(10_000.0), (0.0, 0.0));
    }

    #[test]
    fn both_intersect() {
        let cap = ReactiveCapability {
            pf_limit: Some(0.5),
            apparent_va: Some(10_000.0),
        };
        // At P=4 kW: PF gives ±2 kVA, kVA gives ±sqrt(100-16)*1000 ≈ ±9.17 kVA → PF wins
        let (lo, hi) = cap.q_bounds_at(4_000.0);
        assert!((lo + 2_000.0).abs() < 1e-3);
        assert!((hi - 2_000.0).abs() < 1e-3);
        // At P=9 kW: PF gives ±4.5 kVA, kVA gives ±sqrt(100-81)*1000 ≈ ±4.36 kVA → kVA wins
        let (lo, _hi) = cap.q_bounds_at(9_000.0);
        assert!(lo > -4_500.0 && lo < -4_300.0, "got lo={lo}");
    }
}
