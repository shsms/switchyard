//! Smooth bound-decay function shared by SoC-protective ramps and the
//! `soc_protected_bounds` helper that uses it. Ported from microsim's
//! `bounded-exp-decay` in `sim/common.lisp`. Both Battery and EvCharger
//! pull from this module so the curve shape stays consistent.

/// Tunable thresholds + safety margins for `soc_protected_bounds`.
#[derive(Clone, Copy, Debug)]
pub struct SocProtect {
    pub soc_lower_pct: f32,
    pub soc_upper_pct: f32,
    /// Width (in % points) of the band where rated bounds taper toward
    /// zero. `0.0` disables the taper entirely.
    pub margin_pct: f32,
}

/// Apply the smooth taper near both SoC limits to a `(rated_lower,
/// rated_upper)` pair, returning the SoC-protected bounds at the
/// current `soc`. Charge (positive) tapers near `soc_upper`; discharge
/// (negative) tapers near `soc_lower`. With margin = 0 the rated pair
/// is returned verbatim.
pub fn soc_protected_bounds(
    rated_lower: f32,
    rated_upper: f32,
    soc: f32,
    p: SocProtect,
) -> (f32, f32) {
    if p.margin_pct <= 0.0 {
        return (rated_lower, rated_upper);
    }

    let upper = if p.soc_upper_pct - soc < p.margin_pct {
        rated_upper
            * bounded_exp_decay(
                p.soc_upper_pct - p.margin_pct,
                p.soc_upper_pct,
                soc,
                1.2,
                0.3,
            )
    } else {
        rated_upper
    };

    let lower = if soc - p.soc_lower_pct < p.margin_pct {
        rated_lower
            * bounded_exp_decay(
                p.soc_lower_pct + p.margin_pct,
                p.soc_lower_pct,
                soc,
                1.2,
                0.3,
            )
    } else {
        rated_lower
    };

    (lower, upper)
}

/// Returns a multiplier in `[0, 1]` that smoothly tapers from 1 at
/// `start` to ~`min_val` at `stop`, then snaps to 0 at and beyond
/// `stop`. `base` controls how aggressive the curve is (higher = more
/// linear, lower = more bowed); microsim's defaults are `base = 1.2`,
/// `min_val = 0.3`.
pub fn bounded_exp_decay(start: f32, stop: f32, val: f32, base: f32, min_val: f32) -> f32 {
    if start == stop {
        return if val < start { 1.0 } else { 0.0 };
    }
    let base = base.max(1.1);
    let factor = 10.0 / (stop - start);
    let stop_s = start + (stop - start) * factor;
    let val_s = start + (val - start) * factor;
    let shift = min_val - base.powf(start - stop_s - 1.0);

    if val_s >= stop_s {
        0.0
    } else if val_s < start {
        1.0
    } else {
        shift + (1.0 - shift) * base.powf(start - val_s)
    }
}

#[cfg(test)]
mod tests {
    use super::{SocProtect, bounded_exp_decay, soc_protected_bounds};

    fn approx(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn at_start_returns_one() {
        let v = bounded_exp_decay(80.0, 90.0, 80.0, 1.2, 0.3);
        assert!(approx(v, 1.0, 1e-3), "expected 1.0, got {v}");
    }

    #[test]
    fn at_or_past_stop_returns_zero() {
        assert_eq!(bounded_exp_decay(80.0, 90.0, 90.0, 1.2, 0.3), 0.0);
        assert_eq!(bounded_exp_decay(80.0, 90.0, 95.0, 1.2, 0.3), 0.0);
    }

    #[test]
    fn before_start_returns_one() {
        assert_eq!(bounded_exp_decay(80.0, 90.0, 50.0, 1.2, 0.3), 1.0);
    }

    #[test]
    fn soc_protect_tapers_upper_only_near_high_soc() {
        let p = SocProtect {
            soc_lower_pct: 10.0,
            soc_upper_pct: 90.0,
            margin_pct: 10.0,
        };
        // Mid-band: full rated.
        assert_eq!(
            soc_protected_bounds(-30000.0, 30000.0, 50.0, p),
            (-30000.0, 30000.0)
        );
        // Near upper: upper derates, lower untouched.
        let (lo, hi) = soc_protected_bounds(-30000.0, 30000.0, 85.0, p);
        assert_eq!(lo, -30000.0);
        assert!(hi < 30000.0 && hi > 0.0, "expected derated upper, got {hi}");
        // Near lower: lower derates (less negative), upper untouched.
        let (lo, hi) = soc_protected_bounds(-30000.0, 30000.0, 15.0, p);
        assert_eq!(hi, 30000.0);
        assert!(
            lo > -30000.0 && lo < 0.0,
            "expected derated lower, got {lo}"
        );
    }

    #[test]
    fn middle_is_monotone() {
        let a = bounded_exp_decay(80.0, 90.0, 82.0, 1.2, 0.3);
        let b = bounded_exp_decay(80.0, 90.0, 85.0, 1.2, 0.3);
        let c = bounded_exp_decay(80.0, 90.0, 88.0, 1.2, 0.3);
        assert!(
            a > b && b > c,
            "expected monotone decreasing, got {a} {b} {c}"
        );
        assert!((0.0..=1.0).contains(&a));
        assert!((0.0..=1.0).contains(&c));
    }
}
