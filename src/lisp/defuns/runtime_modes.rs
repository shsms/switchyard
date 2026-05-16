//! `(set-component-health)`, `(set-component-telemetry-mode)`,
//! `(set-component-command-mode)` — flip a component's runtime
//! enum at REPL / scenario time so fault simulation is scriptable.
//! Plus the site-wide stream knobs `(cancel-all-streams)` and
//! `(set-sample-lag-ms)`.

use tulisp::TulispContext;

use crate::sim::microgrids::SharedSiteRouter;

pub(super) fn register(ctx: &mut TulispContext, router: SharedSiteRouter) {
    use crate::sim::runtime::{CommandMode, Health, TelemetryMode};

    let r = router.clone();
    ctx.defun("set-component-health", move |id: i64, h: Health| -> bool {
        let w = r.site();
        w.set_health(id as u64, h);
        true
    });

    let r = router.clone();
    ctx.defun(
        "set-component-telemetry-mode",
        move |id: i64, m: TelemetryMode| -> bool {
            let w = r.site();
            w.set_telemetry_mode(id as u64, m);
            true
        },
    );

    let r = router.clone();
    ctx.defun(
        "set-component-command-mode",
        move |id: i64, m: CommandMode| -> bool {
            let w = r.site();
            w.set_command_mode(id as u64, m);
            true
        },
    );

    let r = router.clone();
    ctx.defun("cancel-all-streams", move || -> bool {
        // Server-side graceful cancel of every active stream. Each
        // streaming task sees the epoch bump on its next iteration and
        // exits, sending the client an EOF/CANCELLED. Clients reconnect
        // and resume on fresh streams.
        r.site().cancel_all_streams();
        true
    });

    let r = router;
    ctx.defun("set-sample-lag-ms", move |ms: i64| -> bool {
        // Shift every outgoing telemetry sample's timestamp into the
        // past by MS milliseconds. Models a server that delivers
        // samples with a fixed timestamp lag, e.g. to test how a
        // downstream resampler copes with stale data.
        r.site().set_sample_lag_ms(ms.max(0) as u64);
        true
    });
}
