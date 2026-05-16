//! Grid-state knobs: per-phase voltage and the per-microgrid
//! physics tick cadence.

use tulisp::{Error, TulispContext};

use crate::sim::microgrids::SharedSiteRouter;

pub(super) fn register(ctx: &mut TulispContext, router: SharedSiteRouter) {
    let r = router.clone();
    ctx.defun(
        "set-voltage-per-phase",
        move |p1: f64, p2: f64, p3: f64| -> Result<bool, Error> {
            let w = r.site();
            let mut state = w.grid_state();
            state.voltage_per_phase = (p1 as f32, p2 as f32, p3 as f32);
            w.set_grid_state(state);
            Ok(true)
        },
    );

    let r = router;
    ctx.defun(
        "set-physics-tick-ms",
        move |ms: i64| -> Result<bool, Error> {
            let w = r.site();
            w.set_physics_tick_ms(ms.max(1) as u64);
            Ok(true)
        },
    );
}
