//! `(set-active-power)` — apply an active-power setpoint and arm
//! a request-lifetime timeout. Mirrors gRPC's
//! `SetElectricalComponentPower`; the reset fires from the loop in
//! `Config::start_timeout_loop`.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tulisp::{Error, TulispContext};

use crate::sim::microgrids::SharedSiteRouter;

use super::super::Metadata;

/// Lower bound on a non-zero request-lifetime that
/// `(set-active-power)` can install. The timeout loop polls at
/// 100 ms and the default physics tick is 100 ms, so a sub-150 ms
/// lifetime can expire before the next physics tick observes the
/// setpoint at all — the ramp would clear without ever leaving
/// idle. `lifetime-ms = 0` is preserved as an explicit "expire
/// immediately" escape (used by tests) and bypasses the clamp.
const MIN_SET_ACTIVE_POWER_LIFETIME_MS: u64 = 150;

/// `(set-active-power ID WATTS &OPTIONAL LIFETIME-MS)` — apply an
/// active-power setpoint and arm a request-lifetime timeout, mirroring
/// what gRPC's `SetElectricalComponentPower` does. Returns `t` on
/// success; signals an error if the component doesn't exist or
/// rejects the setpoint (e.g. out-of-bounds, unsupported kind).
///
/// `LIFETIME-MS` is the duration after which the setpoint snaps back
/// to idle. Omitting it falls back to `default-request-lifetime-ms`,
/// matching the gRPC behaviour. The reset fires from the loop in
/// `Config::start_timeout_loop`.
pub(super) fn register(
    ctx: &mut TulispContext,
    router: SharedSiteRouter,
    metadata: Arc<RwLock<Metadata>>,
) {
    let r = router;
    ctx.defun(
        "set-active-power",
        move |id: i64, watts: f64, lifetime_ms: Option<i64>| -> Result<bool, Error> {
            let w = r.site();
            let component = w.get(id as u64).ok_or_else(|| {
                Error::invalid_argument(format!("set-active-power: component {id} not found"))
            })?;
            // Gateway-level envelope check, mirroring the gRPC SetPower
            // path: a command outside the inverter's own bounds
            // intersected with its children's DC bounds is rejected, not
            // silently saturated by the battery. 0 W (the fail-safe park)
            // is always allowed.
            if watts != 0.0
                && let Some(envelope) = w.active_setpoint_envelope(id as u64)
                && !envelope.contains(watts as f32)
            {
                return Err(Error::invalid_argument(format!(
                    "set-active-power: set-point {watts} W exceeds combined envelope {envelope}"
                )));
            }
            component
                .set_active_setpoint(watts as f32)
                .map_err(|e| Error::invalid_argument(format!("set-active-power: {e}")))?;
            let lifetime = lifetime_ms
                .map(|ms| {
                    let raw = ms.max(0) as u64;
                    let clamped = if raw == 0 {
                        0
                    } else {
                        raw.max(MIN_SET_ACTIVE_POWER_LIFETIME_MS)
                    };
                    Duration::from_millis(clamped)
                })
                .unwrap_or_else(|| metadata.read().default_request_lifetime);
            w.add_timeout(
                id as u64,
                crate::timeout_tracker::SetpointAxis::Active,
                lifetime,
            );
            Ok(true)
        },
    );
}

#[cfg(test)]
mod tests {
    use super::super::super::test_support::config_with;

    /// set-active-power applies a setpoint and arms the timeout tracker.
    /// We can verify both by checking that MicrogridSite registers a deadline
    /// for the targeted component after the call.
    #[test]
    fn set_active_power_applies_setpoint_and_arms_timeout() {
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (setq b1 (%make-battery :id 1 :rated-lower -5000.0 :rated-upper 5000.0))
             (%make-battery-inverter :id 2 :rated-lower -5000.0 :rated-upper 5000.0
                                       :successors (list b1))",
        );
        // 30-second lifetime — applies the setpoint and arms the
        // tracker; nothing should be expired yet.
        cfg.eval("(set-active-power 2 1500.0 30000)").unwrap();
        assert_eq!(cfg.site().drain_expired_timeouts(), Vec::new());
        // Lifetime 0 → instantly elapses; the next drain returns id.
        cfg.eval("(set-active-power 2 1500.0 0)").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(
            cfg.site().drain_expired_timeouts(),
            vec![(2, crate::timeout_tracker::SetpointAxis::Active)]
        );
    }

    /// set-active-power gates against the *intersection* of the
    /// inverter's own bounds and its battery child's bounds — not just
    /// the inverter's own — so a value the inverter alone would accept
    /// but the battery can't is rejected, not silently saturated.
    #[test]
    fn set_active_power_rejects_outside_battery_inverter_intersection() {
        let (cfg, _dir) = config_with(
            // Inverter rated ±5 kW, but its battery only ±1 kW -> the
            // combined envelope is ±1 kW.
            "(set-microgrid-id 9)
             (setq b1 (%make-battery :id 1 :rated-lower -1000.0 :rated-upper 1000.0))
             (%make-battery-inverter :id 2 :rated-lower -5000.0 :rated-upper 5000.0
                                       :successors (list b1))",
        );
        // +3 kW is inside the inverter's own ±5 kW but outside the
        // battery's ±1 kW -> rejected against the intersection.
        let res = cfg.eval("(set-active-power 2 3000.0 30000)");
        assert!(res.is_err(), "expected rejection, got {res:?}");
        assert!(
            res.as_ref().unwrap_err().contains("envelope"),
            "expected 'envelope' in error, got {res:?}"
        );
        // Discharge side mirrors it.
        assert!(cfg.eval("(set-active-power 2 -3000.0 30000)").is_err());
        // Within the ±1 kW intersection is accepted.
        cfg.eval("(set-active-power 2 800.0 30000)").unwrap();
        // 0 W (the fail-safe park) is always accepted.
        cfg.eval("(set-active-power 2 0.0 30000)").unwrap();
    }

    /// set-active-power on an unknown id surfaces an error, and a setpoint
    /// rejected by the component (e.g. unsupported kind on a meter)
    /// also propagates rather than silently no-op'ing.
    #[test]
    fn set_active_power_rejects_unknown_or_unsupported() {
        let (cfg, _dir) = config_with(
            "(set-microgrid-id 9)
             (%make-meter :id 1)",
        );
        let res = cfg.eval("(set-active-power 999 1500.0)");
        assert!(res.is_err(), "expected error, got {res:?}");
        assert!(res.unwrap_err().contains("999"));
        // Meter doesn't support active setpoints — set_active_setpoint
        // returns Unsupported, which we surface as a Lisp error.
        let res = cfg.eval("(set-active-power 1 1500.0)");
        assert!(res.is_err(), "expected error, got {res:?}");
    }
}
