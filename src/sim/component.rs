use std::{fmt, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};

use crate::sim::{bounds::VecBounds, world::World};

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
    OutOfBounds { value: f32, lower: f32, upper: f32 },
    NotHealthy,
    Unsupported,
}

impl fmt::Display for SetpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfBounds {
                value,
                lower,
                upper,
            } => write!(f, "set-point {value} W out of bounds [{lower}, {upper}]"),
            Self::NotHealthy => write!(f, "component is not healthy"),
            Self::Unsupported => write!(f, "operation not supported by this component type"),
        }
    }
}

impl std::error::Error for SetpointError {}

/// Per-tick snapshot a component emits for telemetry / TUI / logging.
/// All numeric fields are SI units (W, VAR, V, A, %, Wh).
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

/// The single trait every simulated component implements. See PLAN.md
/// for the rationale; the short version is "everything you need to
/// schedule, control, and observe a component".
pub trait SimulatedComponent: Send + Sync + fmt::Display {
    fn id(&self) -> u64;
    fn category(&self) -> Category;
    fn name(&self) -> &str;

    /// Telemetry stream interval requested by the component. The world
    /// scheduler may sample more often, but gRPC streams use this.
    fn stream_interval(&self) -> Duration;

    /// Advance internal state by `dt`. Called from `World::tick`.
    fn tick(&self, world: &World, now: DateTime<Utc>, dt: Duration);

    /// Snapshot current observable state.
    fn telemetry(&self, world: &World) -> Telemetry;

    fn set_active_setpoint(&self, _power_w: f32) -> Result<(), SetpointError> {
        Err(SetpointError::Unsupported)
    }
    fn set_reactive_setpoint(&self, _vars: f32) -> Result<(), SetpointError> {
        Err(SetpointError::Unsupported)
    }
    fn reset_setpoint(&self) {}

    fn augment_active_bounds(
        &self,
        _create_ts: DateTime<Utc>,
        _bounds: VecBounds,
        _lifetime: Duration,
    ) {
    }

    /// Total real power flowing at this component, used by parents
    /// (meters, inverters) to aggregate their successors. The `world`
    /// argument is for components that need to recurse into their own
    /// children (a nested meter sums its inverter, which sums its
    /// batteries). Pure-leaf components ignore it.
    fn aggregate_power_w(&self, _world: &World) -> f32 {
        0.0
    }

    /// Push DC power onto a child component (used by inverters to
    /// drive their batteries). Default no-op so non-DC components
    /// silently ignore the call.
    fn set_dc_power(&self, _p: f32) {}

    /// Override the AC-side active power this component reports.
    /// Used by `(set-meter-power id W)` to drive a Lisp- or CSV-
    /// computed consumer load curve into a Meter from a timer
    /// callback. Default no-op — components that don't model
    /// consumer-style loads silently ignore the call.
    fn set_active_power_override(&self, _p: f32) {}

    /// Like `set_dc_power`, but conveys both active and reactive so
    /// the child can report apparent-power loading on its DC side.
    /// Default forwards the active component to `set_dc_power` and
    /// drops Q — keeps non-battery components working unchanged.
    fn set_dc_active_reactive(&self, p: f32, _q: f32) {
        self.set_dc_power(p);
    }

    /// Aggregate reactive power flowing through this component
    /// (used by parent inverters to read back what their children
    /// accepted, and by meters to sum their successors). Default 0 for
    /// components that don't carry Q.
    fn aggregate_reactive_var(&self, _world: &World) -> f32 {
        0.0
    }

    /// Static rated active-power bounds (W), if applicable. Used by
    /// `ListElectricalComponents` to populate `metric_config_bounds`.
    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        None
    }

    /// Current effective active-power bounds (W) — for batteries this
    /// is DC, for inverters AC. Differs from `rated_active_bounds` when
    /// the component derates dynamically (SoC-protective ramp on a
    /// battery, augmentations on an inverter). Default falls through
    /// to `rated_active_bounds` so simple components get the obvious
    /// behaviour for free.
    fn effective_active_bounds(&self) -> Option<VecBounds> {
        self.rated_active_bounds()
            .map(|(l, u)| VecBounds::single(l, u))
    }

    /// Rated fuse current at the grid connection point.
    fn rated_fuse_current(&self) -> Option<u32> {
        None
    }

    /// Subtype label, used by `make_component_proto` to drive the
    /// proto-level enums (e.g. `InverterType`, `BatteryType`,
    /// `EvChargerType`). Free-form so the trait doesn't depend on
    /// proto types — `proto_conv` matches on known strings and falls
    /// back to "unspecified".
    fn subtype(&self) -> Option<&'static str> {
        None
    }

    /// Hidden components are still registered (so a parent meter can
    /// look them up and aggregate their power) but excluded from the
    /// gRPC `ListElectricalComponents` / `ListConnections` responses
    /// and from any topology display. Used for synthetic load /
    /// generator meters that should appear as a "consumer" power flow
    /// to clients without showing up as discrete components.
    fn is_hidden(&self) -> bool {
        false
    }

    /// Per-emit jitter applied to the stream interval, in percent
    /// (0..100). The server picks a uniform random multiplier in
    /// `1.0 ± pct/100` for every sleep so multi-component streams do
    /// not lock-step. Default 0 keeps behaviour deterministic.
    fn stream_jitter_pct(&self) -> f32 {
        0.0
    }

    /// Current `(lower, upper)` reactive-power envelope, derived from
    /// the component's reactive capability and its current active P.
    /// `None` for components that don't model reactive power.
    fn reactive_bounds(&self) -> Option<(f32, f32)> {
        None
    }

    /// Replace the PF cap on the reactive envelope at runtime.
    /// `None` disables the PF constraint (kVA cap or unrestricted
    /// take over). Mirrors the SunSpec / IEEE 1547-2018 PF setpoint
    /// surface a real EMS pushes via Modbus. Default no-op for
    /// components that don't model reactive power.
    fn set_reactive_pf_limit(&self, _pf: Option<f32>) {}

    /// Replace the apparent-power (kVA) cap on the reactive envelope
    /// at runtime. `None` disables the kVA constraint. Default no-op.
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
