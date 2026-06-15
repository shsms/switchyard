//! Read-side DSL primitives for in-sim controllers.
//!
//! A scenario can already *perturb* the world (`set-meter-power`,
//! `set-solar-sunlight`, …) and *actuate* a component
//! (`set-active-power`), but until now it couldn't *sense* live state,
//! so a scripted controller had to command blindly. These getters close
//! that gap: a controller reads a component's current active power and
//! its effective active-power bounds — rated ∩ live augmentations, i.e.
//! exactly what a real EMS reads off the telemetry stream, including any
//! cap a bounds-driving app (e.g. the ) has applied.
//!
//! Paired with `(set-active-power … CLAMP)` and scheduled by `(every …)`
//! / `(define-controller …)`, these turn the sim genuinely closed-loop.

use tulisp::{Error, TulispContext};

use crate::sim::microgrids::SharedSiteRouter;

enum Edge {
    Lower,
    Upper,
}

/// The outermost finite edge of a component's effective active-power
/// bounds on the requested side, in watts.
fn bound_edge(router: &SharedSiteRouter, id: i64, edge: Edge) -> Result<f64, Error> {
    let w = router.site();
    let c = w.get(id as u64).ok_or_else(|| {
        Error::invalid_argument(format!("component-bound: component {id} not found"))
    })?;
    let bounds = c.effective_active_bounds().ok_or_else(|| {
        Error::invalid_argument(format!(
            "component-bound: component {id} has no active bounds"
        ))
    })?;
    let value = match edge {
        Edge::Lower => bounds.0.iter().filter_map(|b| b.lower).reduce(f32::min),
        Edge::Upper => bounds.0.iter().filter_map(|b| b.upper).reduce(f32::max),
    };
    value.map(|v| v as f64).ok_or_else(|| {
        Error::invalid_argument(format!(
            "component-bound: component {id} has an open bound on that side"
        ))
    })
}

pub(super) fn register(ctx: &mut TulispContext, router: SharedSiteRouter) {
    let r = router.clone();
    ctx.defun(
        "component-active-power",
        move |id: i64| -> Result<f64, Error> {
            let w = r.site();
            let c = w.get(id as u64).ok_or_else(|| {
                Error::invalid_argument(format!("component-active-power: component {id} not found"))
            })?;
            Ok(c.aggregate_power_w(&w) as f64)
        },
    );

    let r = router.clone();
    ctx.defun(
        "component-bound-lower",
        move |id: i64| -> Result<f64, Error> { bound_edge(&r, id, Edge::Lower) },
    );

    let r = router;
    ctx.defun(
        "component-bound-upper",
        move |id: i64| -> Result<f64, Error> { bound_edge(&r, id, Edge::Upper) },
    );
}

#[cfg(test)]
mod tests {
    use super::super::super::test_support::config_with;
    use crate::sim::bounds::VecBounds;
    use chrono::Utc;
    use std::time::Duration;

    fn rig() -> (crate::lisp::Config, std::path::PathBuf) {
        config_with(
            "(set-microgrid-id 9)
             (setq b1 (%make-battery :id 1 :rated-lower -5000.0 :rated-upper 5000.0))
             (%make-battery-inverter :id 2 :rated-lower -4000.0 :rated-upper 4000.0
                                       :successors (list b1))",
        )
    }

    /// component-bound-lower/upper report the inverter's effective
    /// bounds, and follow a live augmentation (the cap an EMS reads).
    #[test]
    fn component_bounds_report_rated_then_track_augmentation() {
        let (cfg, _dir) = rig();
        let edge = |cfg: &crate::lisp::Config, expr: &str| -> f64 {
            cfg.eval(expr).unwrap().parse().unwrap()
        };
        // Rated ±4 kW before any cap.
        assert!((edge(&cfg, "(component-bound-upper 2)") - 4000.0).abs() < 1.0);
        assert!((edge(&cfg, "(component-bound-lower 2)") + 4000.0).abs() < 1.0);
        // The  narrows the inverter to [-4 kW, +1 kW].
        cfg.site().get(2).unwrap().augment_active_bounds(
            Utc::now(),
            VecBounds::single(-4000.0, 1000.0),
            Duration::from_secs(60),
        );
        assert!((edge(&cfg, "(component-bound-upper 2)") - 1000.0).abs() < 1.0);
        assert!((edge(&cfg, "(component-bound-lower 2)") + 4000.0).abs() < 1.0);
    }

    /// component-active-power reports the component's current power after
    /// a setpoint settles.
    #[test]
    fn component_active_power_reports_settled_power() {
        let (cfg, _dir) = rig();
        cfg.eval("(set-active-power 2 2000.0 30000)").unwrap();
        let site = cfg.site();
        site.get(2)
            .unwrap()
            .tick(&site, Utc::now(), Duration::from_millis(100));
        let p: f64 = cfg
            .eval("(component-active-power 2)")
            .unwrap()
            .parse()
            .unwrap();
        assert!((p - 2000.0).abs() < 1.0, "expected +2 kW, got {p}");
    }

    /// A scripted controller reads the live cap and actuates within it:
    /// command the upper bound (clamped), and as the cap moves, the
    /// commanded power follows — closed-loop, no errors.
    #[test]
    fn controller_tracks_the_live_cap() {
        let (cfg, _dir) = rig();
        let site = cfg.site();
        let inv = site.get(2).unwrap();
        // One controller step: charge at whatever the upper bound allows.
        let step = "(set-active-power 2 (component-bound-upper 2) 30000 t)";
        cfg.eval(step).unwrap();
        inv.tick(&site, Utc::now(), Duration::from_millis(100));
        assert!((inv.aggregate_power_w(&site) - 4000.0).abs() < 1.0);
        // Cap drops to +1 kW; the next controller step tracks it.
        inv.augment_active_bounds(
            Utc::now(),
            VecBounds::single(-4000.0, 1000.0),
            Duration::from_secs(60),
        );
        cfg.eval(step).unwrap();
        inv.tick(&site, Utc::now(), Duration::from_millis(100));
        let p = inv.aggregate_power_w(&site);
        assert!(
            (p - 1000.0).abs() < 1.0,
            "controller should track +1 kW cap, got {p}"
        );
    }
}
