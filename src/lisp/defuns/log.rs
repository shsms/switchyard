//! `log.*` printers + math + RNG helpers used by ported microsim
//! configs.

use tulisp::TulispContext;

pub(super) fn register(ctx: &mut TulispContext) {
    use rand::Rng;
    ctx.defun("log.info", |msg: String| log::info!("{msg}"))
        .defun("log.warn", |msg: String| log::warn!("{msg}"))
        .defun("log.error", |msg: String| log::error!("{msg}"))
        .defun("log.debug", |msg: String| log::debug!("{msg}"))
        .defun("log.trace", |msg: String| log::trace!("{msg}"))
        // Math + RNG helpers used by ported microsim configs.
        .defun("ceiling", |n: f64| n.ceil() as i64)
        .defun("floor", |n: f64| n.floor() as i64)
        .defun("sin", |n: f64| n.sin())
        .defun("cos", |n: f64| n.cos())
        .defun("random", |limit: Option<i64>| {
            if let Some(limit) = limit {
                // `gen_range(0..n)` panics on an empty/inverted range, so a
                // non-positive limit (e.g. `(random (length '()))`) would
                // abort the eval; clamp so `(random n<=0)` yields 0.
                rand::thread_rng().gen_range(0..limit.max(1))
            } else {
                rand::thread_rng().r#gen()
            }
        });
}
