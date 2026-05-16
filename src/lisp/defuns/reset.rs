//! `(reset-microgrid)` — clear the active site's components.

use tulisp::{Error, TulispContext};

use crate::sim::microgrids::SharedSiteRouter;

pub(super) fn register(ctx: &mut TulispContext, router: SharedSiteRouter) {
    // Rust-side: clear the active MicrogridSite's components. The
    // Lisp-side `reset-state` (in sim/common.lisp) wraps this and
    // also cancels any outstanding tulisp-async timers so the next
    // config load doesn't double-fire `every` callbacks.
    ctx.defun("reset-microgrid", move || -> Result<bool, Error> {
        router.site().reset();
        Ok(true)
    });
}
