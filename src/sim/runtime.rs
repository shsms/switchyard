//! Per-component runtime knobs that simulate device-side faults
//! independent of the physics layer.
//!
//! These three orthogonal flags let a config (or a runtime caller via
//! `(set-component-* …)` defuns) drive faulty behaviour without
//! touching the simulated state. They live in the World, not on the
//! component, because the things they control (gRPC stream pacing,
//! request handling) are server-facing concerns the components
//! shouldn't know about.

use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Health {
    /// Component reports the physics-derived state code (ready /
    /// charging / discharging / …) and accepts setpoints normally.
    #[default]
    Ok,
    /// Component reports `ERROR`; setpoint requests are rejected at
    /// the gRPC layer with `FailedPrecondition`.
    Error,
    /// Component reports `STANDBY`; setpoint requests are rejected.
    Standby,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TelemetryMode {
    /// Emit samples at the component's stream interval (the default).
    #[default]
    Normal,
    /// Connection stays open but no samples are sent. Models a device
    /// that's reachable on the wire but has lost its data link to the
    /// metering / telemetry pipeline.
    Silent,
    /// Stream task exits as soon as it sees this. Existing clients see
    /// EOF; new connections terminate immediately. Models an
    /// unreachable device.
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CommandMode {
    /// Validate against bounds and apply.
    #[default]
    Normal,
    /// Hang the request indefinitely. The client will time out
    /// according to its own deadline. Models a device whose control
    /// channel is alive but stuck.
    Timeout,
    /// Reply immediately with `Unavailable`. Models a device whose
    /// control channel is down.
    Error,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ComponentRuntime {
    pub health: Health,
    pub telemetry: TelemetryMode,
    pub command: CommandMode,
}

impl FromStr for Health {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "ok" | "ready" => Ok(Self::Ok),
            "error" => Ok(Self::Error),
            "standby" => Ok(Self::Standby),
            _ => Err(()),
        }
    }
}

impl FromStr for TelemetryMode {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "normal" => Ok(Self::Normal),
            "silent" => Ok(Self::Silent),
            "closed" => Ok(Self::Closed),
            _ => Err(()),
        }
    }
}

impl FromStr for CommandMode {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "normal" => Ok(Self::Normal),
            "timeout" => Ok(Self::Timeout),
            "error" => Ok(Self::Error),
            _ => Err(()),
        }
    }
}

impl Health {
    /// Proto state-code label corresponding to this health state.
    /// Returned to clients as part of every telemetry sample's
    /// state_snapshot when health is non-Ok; physics-derived labels
    /// take over when Ok.
    pub fn state_label(self) -> Option<&'static str> {
        match self {
            Self::Ok => None,
            Self::Error => Some("error"),
            Self::Standby => Some("standby"),
        }
    }
}
