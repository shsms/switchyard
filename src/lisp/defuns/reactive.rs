//! Reactive-power knobs: `(set-reactive-pf-limit)` and
//! `(set-reactive-apparent-va)`. Mirror what a SunSpec /
//! IEEE 1547-2018 EMS pushes via Modbus.

use tulisp::{Error, TulispContext};

use crate::sim::microgrids::SharedSiteRouter;

pub(super) fn register(ctx: &mut TulispContext, router: SharedSiteRouter) {
    // Same opt-in convention as the make-* plist args:
    //   value > 0  → that constraint is active with this magnitude
    //   value ≤ 0  → that constraint is disabled
    // Mirrors what a SunSpec / IEEE 1547-2018 EMS pushes via Modbus.
    let r = router.clone();
    ctx.defun(
        "set-reactive-pf-limit",
        move |id: i64, k: f64| -> Result<bool, Error> {
            let w = r.site();
            match w.get(id as u64) {
                Some(c) => {
                    c.set_reactive_pf_limit(if k > 0.0 { Some(k as f32) } else { None });
                    Ok(true)
                }
                None => Err(Error::invalid_argument(format!(
                    "set-reactive-pf-limit: component {id} not found"
                ))),
            }
        },
    );

    let r = router;
    ctx.defun(
        "set-reactive-apparent-va",
        move |id: i64, va: f64| -> Result<bool, Error> {
            let w = r.site();
            match w.get(id as u64) {
                Some(c) => {
                    c.set_reactive_apparent_va(if va > 0.0 { Some(va as f32) } else { None });
                    Ok(true)
                }
                None => Err(Error::invalid_argument(format!(
                    "set-reactive-apparent-va: component {id} not found"
                ))),
            }
        },
    );
}
