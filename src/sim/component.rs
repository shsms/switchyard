use std::{fmt, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use tulisp::TulispContext;

use crate::sim::{bounds::VecBounds, dynamic_scalar::DynamicScalar, microgrid_site::MicrogridSite};

/// High-level kind of a component, mirroring the proto category enum but
/// kept Rust-side so non-gRPC code does not need to depend on protobuf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Grid,
    Meter,
    Inverter,
    Battery,
    EvCharger,
    Chp,
}

#[derive(Debug, Clone)]
pub enum SetpointError {
    /// `envelope` is the *effective* envelope (rated ∩ live
    /// augmentations, possibly ∩ reactive cap) — not the rated
    /// envelope. A client whose request was rejected because they
    /// just augmented the bounds tighter needs to see the narrowed
    /// envelope in the error, otherwise the message reads "out of
    /// [-30000, 30000]" while the actual rejection was on the
    /// [-10000, 10000] window they themselves set up.
    OutOfBounds { value: f32, envelope: VecBounds },
    /// The component type doesn't accept this operation (e.g. a
    /// meter being asked for an active-power setpoint). Maps to
    /// `tonic::Status::unimplemented` server-side.
    ///
    /// Health-based rejection is *not* in here: the server's
    /// `do_set_power` gates on `runtime.health != Health::Ok`
    /// before reaching the component, returning its own
    /// `failed_precondition` status. Adding a component-side
    /// variant would just be a second source of the same error.
    Unsupported,
}

impl fmt::Display for SetpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfBounds { value, envelope } => {
                write!(f, "set-point {value} W out of bounds {envelope}")
            }
            Self::Unsupported => write!(f, "operation not supported by this component type"),
        }
    }
}

impl std::error::Error for SetpointError {}

/// Per-tick snapshot a component emits for the gRPC telemetry
/// stream and the UI's history sampler. All numeric fields are
/// SI units (W, VAR, V, A, %, Wh).
///
/// Optional fields stay `None` for component types that do not expose
/// them — a meter has no SoC; a battery has no AC voltage; etc.
#[derive(Debug, Default, Clone)]
pub struct Telemetry {
    pub id: u64,
    pub category: Option<Category>,

    pub active_power_w: Option<f32>,
    pub reactive_power_var: Option<f32>,

    pub per_phase_active_w: Option<(f32, f32, f32)>,
    pub per_phase_reactive_var: Option<(f32, f32, f32)>,
    pub per_phase_voltage_v: Option<(f32, f32, f32)>,
    pub per_phase_current_a: Option<(f32, f32, f32)>,

    pub frequency_hz: Option<f32>,

    pub soc_pct: Option<f32>,
    pub soc_lower_pct: Option<f32>,
    pub soc_upper_pct: Option<f32>,
    pub capacity_wh: Option<f32>,
    pub dc_voltage_v: Option<f32>,
    pub dc_current_a: Option<f32>,
    pub dc_power_w: Option<f32>,

    pub active_power_bounds: Option<VecBounds>,
    /// Live reactive-power envelope at the current P. Single-bucket
    /// `(lower, upper)`, expressed in VAR. Set on inverters that
    /// implement `reactive_bounds()`; left None for batteries / meters
    /// / EV chargers / CHP.
    pub reactive_power_bounds: Option<(f32, f32)>,

    pub component_state: Option<&'static str>,
    pub relay_state: Option<&'static str>,
    pub cable_state: Option<&'static str>,
}

impl Telemetry {
    /// Read a single metric off this snapshot. `None` when the
    /// component doesn't publish it. The active-bounds metrics
    /// report the *envelope extremes* — first segment's lower, last
    /// segment's upper — unlike the chart history, which collapses a
    /// multi-segment `VecBounds` to its first segment: an assertion
    /// against "the upper bound" means the outermost reachable
    /// value, not the edge of an arbitrary inner segment.
    pub fn metric_value(&self, metric: crate::sim::history::Metric) -> Option<f32> {
        use crate::sim::history::Metric;
        match metric {
            Metric::ActivePowerW => self.active_power_w,
            Metric::ReactivePowerVar => self.reactive_power_var,
            Metric::FrequencyHz => self.frequency_hz,
            Metric::SocPct => self.soc_pct,
            Metric::DcPowerW => self.dc_power_w,
            Metric::ActivePowerLowerBoundW => self
                .active_power_bounds
                .as_ref()
                .and_then(|b| b.0.first())
                .and_then(|b| b.lower),
            Metric::ActivePowerUpperBoundW => self
                .active_power_bounds
                .as_ref()
                .and_then(|b| b.0.last())
                .and_then(|b| b.upper),
            Metric::ReactivePowerLowerBoundVar => self.reactive_power_bounds.map(|(l, _)| l),
            Metric::ReactivePowerUpperBoundVar => self.reactive_power_bounds.map(|(_, u)| u),
        }
    }
}

/// The single trait every simulated component implements.
///
/// Reading order:
///   - **Identity**: id, category, name, subtype, is_hidden.
///   - **Lifecycle**: stream_interval, stream_jitter_pct, tick, telemetry.
///   - **Setpoints**: set_active_setpoint, set_reactive_setpoint,
///     reset_setpoint, augment_active_bounds, set_active_power_override.
///   - **Bounds**: rated_active_bounds, effective_active_bounds,
///     reactive_bounds, rated_fuse_current.
///   - **Aggregation** (parent → child): aggregate_power_w,
///     aggregate_reactive_var.
///   - **Inverter ↔ child wiring**: set_dc_power, set_dc_active_reactive.
///   - **Runtime knobs**: set_reactive_pf_limit, set_reactive_apparent_va.
///
/// Every method except the four required ones (`id`, `category`,
/// `name`, `stream_interval`, `tick`, `telemetry`) has a sane default
/// — components implement only the surface they need.
pub trait SimulatedComponent: Send + Sync + fmt::Display {
    // ── identity ─────────────────────────────────────────────────────

    fn id(&self) -> u64;
    fn category(&self) -> Category;
    fn name(&self) -> &str;

    /// Free-form subtype label (e.g. `"solar"`, `"li-ion"`, `"ac"`).
    /// Drives the `InverterType` / `BatteryType` / `EvChargerType`
    /// proto enums in `make_component_proto`. Free-form so the trait
    /// doesn't depend on proto types — `proto_conv` matches on known
    /// strings and falls back to "unspecified".
    fn subtype(&self) -> Option<&'static str> {
        None
    }

    /// Hidden components are still registered (so a parent meter can
    /// look them up and aggregate their power) but excluded from the
    /// gRPC `ListElectricalComponents` / `ListConnections` responses
    /// and from `swctl tree`. Used for synthetic load / generator
    /// meters that should appear as a power flow without being a
    /// discrete addressable component.
    fn is_hidden(&self) -> bool {
        false
    }

    // ── lifecycle ────────────────────────────────────────────────────

    /// Telemetry stream interval requested by the component. The
    /// physics tick may run more often; gRPC streams sample at this
    /// cadence (subject to `stream_jitter_pct`).
    fn stream_interval(&self) -> Duration;

    /// Per-emit jitter applied to the stream interval, in percent
    /// (0..100). Each subscriber's task picks a uniform random
    /// multiplier in `1.0 ± pct/100` for every sleep so multi-stream
    /// clients see streams drifting independently. Default 0.
    fn stream_jitter_pct(&self) -> f32 {
        0.0
    }

    /// Refresh externally-driven inputs from Lisp. The MicrogridSite
    /// scheduler holds the interpreter lock and calls this on every
    /// component, in registration order, *before* the tick pass.
    /// Components carrying a [`DynamicScalar`] (lambda- or symbol-
    /// bound `:power`, `:sunlight%`, …) re-evaluate it here and
    /// stash the resolved scalar in an atomic that `tick` then reads.
    /// Default no-op.
    ///
    /// Must not register defuns or otherwise mutate global state —
    /// the lock is held for every component in turn and the loop's
    /// total cost is bounded by the slowest implementor.
    ///
    /// [`DynamicScalar`]: crate::sim::dynamic_scalar::DynamicScalar
    fn refresh_inputs(&self, _ctx: &mut TulispContext) {}

    /// Advance internal state by `dt`. Called once per physics tick
    /// from `MicrogridSite::tick_once` in registration order (children before
    /// parents). Components that aggregate from successors read them
    /// here via `site.get(child_id)`. Must not call back into the
    /// Lisp interpreter — see [`Self::refresh_inputs`] for that.
    fn tick(&self, site: &MicrogridSite, now: DateTime<Utc>, dt: Duration);

    /// Snapshot the component's observable state for streaming. Pure
    /// — should not mutate. `site` is for components that read AC
    /// environment (per-phase voltage, frequency) at sample time.
    fn telemetry(&self, site: &MicrogridSite) -> Telemetry;

    // ── setpoints (control surface) ──────────────────────────────────

    /// Apply an active-power setpoint. Default returns `Unsupported`
    /// for components that don't accept commands (Battery, Meter,
    /// Grid, …).
    fn set_active_setpoint(&self, _power_w: f32) -> Result<(), SetpointError> {
        Err(SetpointError::Unsupported)
    }

    /// Apply a reactive-power setpoint. Default returns `Unsupported`.
    fn set_reactive_setpoint(&self, _vars: f32) -> Result<(), SetpointError> {
        Err(SetpointError::Unsupported)
    }

    /// Clear any pending / armed setpoint — BOTH axes — and snap back
    /// to the component's idle value (0 for inverters, sunlight-driven
    /// power for solar). The full fail-safe reset.
    fn reset_setpoint(&self) {}

    /// Clear one power axis's setpoint, leaving the other running.
    /// Called by the `TimeoutTracker` when that axis's request
    /// lifetime elapses without a refresh — a short-lived Q command
    /// expiring must not clear a long-lived P command.
    ///
    /// The default falls back to the full reset, which is exact for
    /// single-axis components (their "everything" IS that axis).
    /// Components that accept BOTH active and reactive setpoints must
    /// override, or an expiry on one axis wipes the other.
    fn reset_setpoint_axis(&self, _axis: crate::timeout_tracker::SetpointAxis) {
        self.reset_setpoint();
    }

    /// Add a time-limited active-power bounds augmentation, narrowing
    /// the rated envelope. Backs the `AugmentElectricalComponentBounds`
    /// gRPC method.
    fn augment_active_bounds(
        &self,
        _create_ts: DateTime<Utc>,
        _bounds: VecBounds,
        _lifetime: Duration,
    ) {
    }

    /// Override the active-power value a meter publishes with a
    /// constant. Used by `(set-meter-power id W)` when called with a
    /// numeric argument. Default no-op.
    fn set_active_power_override(&self, _p: f32) {}

    /// Replace the meter's `:power` source with a Lisp expression
    /// that the scheduler's `refresh_inputs` pass re-resolves each
    /// tick. Used by `(set-meter-power id (lambda () …))` and by
    /// the UI when a user types a Lisp form into the `:power` input.
    /// Default no-op for non-meter components.
    fn set_active_power_source(&self, _scalar: DynamicScalar) {}

    /// Update the live cloud-cover percentage on a solar inverter.
    /// Used by `(set-solar-sunlight id PCT)` with a numeric
    /// argument. Default no-op for non-solar components.
    fn set_sunlight_pct(&self, _pct: f32) {}

    /// Replace the solar inverter's `:sunlight%` source with a Lisp
    /// expression. PV analogue of [`Self::set_active_power_source`];
    /// used by `(set-solar-sunlight id (lambda () …))`. Default
    /// no-op for non-solar components.
    fn set_sunlight_source(&self, _scalar: DynamicScalar) {}

    // ── bounds telemetry ─────────────────────────────────────────────

    /// Static rated active-power bounds (W). Used by
    /// `ListElectricalComponents` to populate `metric_config_bounds`.
    /// Doesn't change at runtime.
    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        None
    }

    /// Current effective active-power envelope (W) — for batteries
    /// this is DC, for inverters AC. Differs from rated when the
    /// component derates dynamically (SoC-protective ramp on a
    /// battery, augmentations on an inverter). Default falls through
    /// to `rated_active_bounds` so simple components get the obvious
    /// behaviour for free.
    fn effective_active_bounds(&self) -> Option<VecBounds> {
        self.rated_active_bounds()
            .map(|(l, u)| VecBounds::single(l, u))
    }

    /// Current `(lower, upper)` reactive-power envelope at the
    /// component's current P. `None` for components that don't model
    /// reactive power.
    fn reactive_bounds(&self) -> Option<(f32, f32)> {
        None
    }

    /// Rated fuse current at the grid connection point.
    fn rated_fuse_current(&self) -> Option<u32> {
        None
    }

    // ── aggregation (parent reads from child) ────────────────────────

    /// Total real power flowing at this component. Parents (meters,
    /// inverters) sum this across their successors. `site` lets
    /// nesting components recurse — a nested meter calls into its
    /// inverter, which reads from its batteries.
    fn aggregate_power_w(&self, _world: &MicrogridSite) -> f32 {
        0.0
    }

    /// Total reactive power flowing at this component.
    fn aggregate_reactive_var(&self, _world: &MicrogridSite) -> f32 {
        0.0
    }

    // ── inverter → child push (DC bus) ───────────────────────────────

    /// Push DC active power onto a child. Inverters call this on each
    /// of their batteries every tick. Default no-op.
    fn set_dc_power(&self, _p: f32) {}

    /// Like `set_dc_power`, but conveys both active and reactive so
    /// the child can model apparent-power loading on its DC side.
    /// Default forwards `p` to `set_dc_power` and drops `q`.
    fn set_dc_active_reactive(&self, p: f32, _q: f32) {
        self.set_dc_power(p);
    }

    // ── runtime reactive-capability knobs ────────────────────────────

    /// Replace the PF cap on the reactive envelope at runtime.
    /// `None` disables the PF constraint. Mirrors the SunSpec /
    /// IEEE 1547-2018 PF setpoint surface a real EMS pushes via
    /// Modbus.
    fn set_reactive_pf_limit(&self, _pf: Option<f32>) {}

    /// Replace the apparent-power (kVA) cap on the reactive envelope
    /// at runtime. `None` disables the kVA constraint.
    fn set_reactive_apparent_va(&self, _va: Option<f32>) {}
}

/// Cloneable handle that we hand to Lisp via `Shared<dyn TulispAny>`.
/// Wrapping in a newtype lets us hang `Display`, `Clone`, conversion
/// trait impls, and a stable `TypeId` off it.
#[derive(Clone)]
pub struct ComponentHandle(pub Arc<dyn SimulatedComponent>);

impl ComponentHandle {
    pub fn new<C: SimulatedComponent + 'static>(c: C) -> Self {
        Self(Arc::new(c))
    }

    pub fn from_arc(c: Arc<dyn SimulatedComponent>) -> Self {
        Self(c)
    }

    pub fn id(&self) -> u64 {
        self.0.id()
    }

    pub fn is_hidden(&self) -> bool {
        self.0.is_hidden()
    }
}

impl fmt::Display for ComponentHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} #{}>", self.0.name(), self.0.id())
    }
}

/// First auto-allocated component ID. Microsim picks 1000 so explicit
/// IDs (1, 2, …) on roots/main-meters don't collide; switchyard
/// matches the convention so test fixtures stay portable.
pub const FIRST_AUTO_ID: u64 = 1000;

#[cfg(test)]
mod tests {
    use super::Telemetry;
    use crate::proto::common::metrics::Bounds;
    use crate::sim::bounds::VecBounds;
    use crate::sim::history::Metric;

    /// Bounds metrics read the envelope extremes: a two-segment
    /// VecBounds (disjoint augmentation) reports the first segment's
    /// lower and the LAST segment's upper, not the first's.
    #[test]
    fn metric_value_bounds_use_envelope_extremes() {
        let snap = Telemetry {
            active_power_w: Some(2500.0),
            active_power_bounds: Some(VecBounds(vec![
                Bounds {
                    lower: Some(-10000.0),
                    upper: Some(-2000.0),
                },
                Bounds {
                    lower: Some(2000.0),
                    upper: Some(10000.0),
                },
            ])),
            ..Default::default()
        };
        assert_eq!(snap.metric_value(Metric::ActivePowerW), Some(2500.0));
        assert_eq!(
            snap.metric_value(Metric::ActivePowerLowerBoundW),
            Some(-10000.0)
        );
        assert_eq!(
            snap.metric_value(Metric::ActivePowerUpperBoundW),
            Some(10000.0)
        );
        // Unpublished metrics read None.
        assert_eq!(snap.metric_value(Metric::SocPct), None);
        assert_eq!(snap.metric_value(Metric::ReactivePowerLowerBoundVar), None);
    }
}
