//! Smooth bound-decay function shared by SoC-protective ramps. Direct
//! port of microsim's `bounded-exp-decay` in `sim/common.lisp`, kept in
//! its own module so multiple components (battery today, EV charger
//! tomorrow) can reuse it.

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
    use super::bounded_exp_decay;

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
    fn middle_is_monotone() {
        let a = bounded_exp_decay(80.0, 90.0, 82.0, 1.2, 0.3);
        let b = bounded_exp_decay(80.0, 90.0, 85.0, 1.2, 0.3);
        let c = bounded_exp_decay(80.0, 90.0, 88.0, 1.2, 0.3);
        assert!(a > b && b > c, "expected monotone decreasing, got {a} {b} {c}");
        assert!((0.0..=1.0).contains(&a));
        assert!((0.0..=1.0).contains(&c));
    }
}
