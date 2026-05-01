# switchyard — design plan

A reimagining of [`microsim`](../microsim/) where the simulation core lives
in Rust and the `(make-grid …)` / `(make-meter …)` / … functions in the
config DSL build typed Rust objects implementing a single
`SimulatedComponent` trait. Lisp keeps its role as a hot-reloadable
configuration language, but no longer carries simulation state.

The name "switchyard" refers to the high-voltage switching area between
generation and the transmission grid — a fitting metaphor for a microgrid
simulator that wires sources, loads and storage together.

## Why a rewrite

The current `microsim` keeps every component's state in dynamically-named
Lisp symbols (`component-power-1003`, `component-bounds-symbol-1003`, …)
and bolts behavior onto those symbols via closures captured in alists.
This works, but:

1. There is no type — every component is "an alist with the right keys".
2. There is no central scheduler; each `make-*` registers `every` /
   `run-with-timer` callbacks that mutate the global Lisp context.
3. Inverters react instantaneously to set-points. There is no model for
   command latency or ramping (a real inverter takes hundreds of ms to
   acknowledge a command, then ramps power at a slew-rate-limited pace
   to protect cells, breakers and contactors).
4. Adding a new component type means knowing the full suite of
   `*-symbol-from-id` helpers and reproducing the lifecycle dance.

switchyard inverts the relationship: components are Rust types with state
and behavior, the `make-*` functions are thin constructors registered with
[`tulisp`](https://github.com/shsms/tulisp)'s `AsPlist!`, and a single
`World` owns the registry and tick scheduler.

## Dependencies

- **tulisp** at branch `vm` (remote, not the local working copy):
  `tulisp = { git = "https://github.com/shsms/tulisp", branch = "vm",
  features = ["sync"] }`. Used for the `AsPlist!` macro, `Plist<T>`
  argument deserialization, and `Shared<dyn TulispAny>` for opaque
  component handles.
- **tulisp-async** (path-dep, sibling crate). Lisp's role is split
  into two complementary jobs: (a) **wire the topology** at startup
  via `make-*` constructors, and (b) **animate the environment**
  (per-tick voltage / frequency perturbations, scheduled events) via
  `(every …)` and `(run-with-timer …)` from tulisp-async. Component
  *physics* — ramps, SoC derate, AC↔DC conversion, set-point
  validation — stays entirely in Rust. The boundary is firm: a
  microsim-style per-component `every` lifecycle is not how
  switchyard models batteries or inverters.
- **tonic + prost** for the gRPC surface, against the same
  `frequenz-api-microgrid` proto submodule microsim uses.
- **tokio** for the gRPC runtime + the single-tick scheduler, **chrono**
  for timestamps, **parking_lot** for cheap locks.

## Crate layout

```
switchyard/
├── Cargo.toml
├── PLAN.md            (this file)
├── README.md
├── build.rs
├── config.lisp        (sample config — equivalent of microsim's)
├── examples/
├── sim/               (lisp helpers loaded by config.lisp, like microsim's sim/)
├── proto-submodules/  (git submodule pointing at frequenz-api-microgrid)
├── src/
│   ├── lib.rs
│   ├── proto.rs
│   ├── server.rs                 gRPC service impl
│   ├── timeout_tracker.rs
│   ├── tui_log.rs
│   ├── bin/
│   │   ├── switchyard.rs         headless server
│   │   └── switchyard-tui.rs     server + TUI
│   ├── lisp/
│   │   ├── mod.rs                Config (parses config.lisp, holds World)
│   │   ├── make.rs               (make-grid) (make-meter) … via AsPlist
│   │   ├── handle.rs             ComponentHandle ↔ TulispAny conversions
│   │   ├── time.rs               dt:now, parse-iso-timestamp, …
│   │   └── bounds.rs             bounds/make-container, bounds/add, …
│   └── sim/
│       ├── mod.rs                re-exports
│       ├── component.rs          SimulatedComponent trait, Category, Telemetry
│       ├── world.rs              World registry, scheduler, clock
│       ├── ramp.rs               CommandDelay + Ramp primitives
│       ├── bounds.rs             VecBounds with TTL entries (port of microsim)
│       ├── grid.rs
│       ├── meter.rs
│       ├── battery.rs
│       ├── ev_charger.rs
│       ├── chp.rs
│       └── inverter/
│           ├── mod.rs
│           ├── battery_inverter.rs
│           └── solar_inverter.rs
```

## Core abstractions

### `SimulatedComponent`

```rust
pub trait SimulatedComponent: Send + Sync + std::fmt::Display {
    fn id(&self) -> u64;
    fn category(&self) -> Category;
    fn name(&self) -> &str;

    /// Advance internal state by `dt`. Called by World on every tick.
    fn tick(&self, world: &World, now: DateTime<Utc>, dt: Duration);

    /// Materialize a telemetry snapshot for streaming.
    fn telemetry(&self, world: &World) -> Telemetry;

    /// Apply a control set-point. Implementations may stage it (with
    /// `CommandDelay`) and ramp toward it (`Ramp`). Returns Err if the
    /// set-point is out of configured bounds.
    fn set_active_setpoint(&self, _power_w: f32) -> Result<(), SetpointError> {
        Err(SetpointError::Unsupported)
    }
    fn set_reactive_setpoint(&self, _vars: f32) -> Result<(), SetpointError> {
        Err(SetpointError::Unsupported)
    }
    fn reset_setpoint(&self) {}

    /// Augment active-power bounds (AugmentElectricalComponentBounds RPC).
    fn augment_active_bounds(
        &self,
        _create_ts: DateTime<Utc>,
        _bounds: VecBounds,
        _lifetime: Duration,
    ) {
    }
}
```

Components are stored as `Arc<dyn SimulatedComponent>` and given to Lisp
as `Shared<dyn TulispAny>`. `ComponentHandle(Arc<dyn SimulatedComponent>)`
is the wrapper that satisfies `Clone + Display + Send + Sync` so it
qualifies for `TulispAny`.

### `World`

The single source of truth at runtime:

```rust
pub struct World {
    inner: Arc<WorldInner>,
}
struct WorldInner {
    components: RwLock<HashMap<u64, Arc<dyn SimulatedComponent>>>,
    connections: RwLock<Vec<(u64, u64)>>,  // (from, to) — display graph
    clock: Clock,                          // monotonic + wall-clock
    next_id: AtomicU64,
    physics_tick_ms: AtomicU64,
    grid_state: RwLock<GridState>,         // voltage_per_phase, ac_frequency
}
```

Responsibilities:

- Issue auto-incrementing component IDs (matching microsim's `1000+`).
- Register every component built by a `make-*` constructor.
- Record the parent→child topology used by both telemetry meters and
  the gRPC `ListConnections` endpoint.
- Run a single tokio task at `physics_tick_ms` that calls `tick()` on
  every registered component in topology order (sources → loads → meters).
- Expose the AC grid state (frequency, per-phase voltage) that
  components query when computing AC values from DC, etc.

### Delays and ramping

Two orthogonal primitives compose to model real-world inverter behavior:

```rust
/// Holds a pending set-point that becomes the "armed target" only after
/// `delay` has elapsed since `set` was called. Models the SCADA / firmware
/// round-trip on a real inverter.
pub struct CommandDelay {
    state: Mutex<CommandDelayState>,
    delay: Duration,
}

pub struct Ramp {
    state: Mutex<RampState>,
    rate_w_per_s: f32,    // slew rate; f32::INFINITY for "no ramp"
}
```

A typical battery inverter's `set_active_setpoint(p)` becomes:

```
     set_active_setpoint(p)
              |
              v
       CommandDelay.set(p)
              |
              v   tick():
       (after delay)        Ramp.target = CommandDelay.armed_target()
              |             actual_power = Ramp.advance(dt)
              v
       distribute actual_power across battery successors
```

Both primitives are **opt-in** per component, configured through plist
keys `:command-delay-ms` (default 0) and `:ramp-rate-w-per-s` (default
∞). A battery inverter whose batteries are near full SOC can also
publish a derated upper ramp limit so the inverter tapers smoothly into
the bounds — this is what "ramping to protect batteries" looks like in
practice.

### Bounds

The TTL-bounds container in `microsim`'s `lisp/bounds.rs` is a clean
abstraction worth keeping. Port it as `sim::bounds::VecBounds` and let
both `Battery` (DC bounds varying with SoC) and `BatteryInverter`
(AC bounds, augmentable from gRPC) reuse it.

## `AsPlist!` usage

Each `make-*` function is registered as a typed `defun`. Example:

```rust
use tulisp::{AsPlist, Plist, TulispContext};

AsPlist! {
    pub struct BatteryInverterArgs {
        id: Option<i64> {= None},
        interval_ms<":interval">: Option<i64> {= None},
        config: Option<TulispObject> {= None},
        successors: Vec<ComponentHandle> {= Vec::new()},
        command_delay_ms<":command-delay-ms">: Option<i64> {= None},
        ramp_rate_w_per_s<":ramp-rate">: Option<f64> {= None},
    }
}

pub fn register(ctx: &mut TulispContext, world: World) {
    let w = world.clone();
    ctx.defun("make-battery-inverter", move |args: Plist<BatteryInverterArgs>| {
        let inv = BatteryInverter::new(&w, args.into_inner())?;
        Ok::<_, Error>(ComponentHandle::new(inv))   // → Shared<dyn TulispAny>
    });
    // …
}
```

The shape of `config.lisp` stays the same as microsim's, so people
migrating know what they are looking at:

```lisp
(make-grid
 :id 1
 :rated-fuse-current 100
 :successors
 (list
  (make-meter
   :successors
   (list
    (make-battery-inverter
     :command-delay-ms 1500          ;; SCADA round-trip
     :ramp-rate         5000.0       ;; W/s
     :successors (list
                  (make-battery
                   :soc-charge-protect       t
                   :soc-charge-protect-margin 10.0))))))
```

## Modularity goals

- A new component type is a single file under `src/sim/` plus one
  `register` line. Trait + `AsPlist!` give a uniform shape.
- The scheduler (single tick loop) is decoupled from telemetry cadence
  (per-stream interval) — components only implement `tick()` and
  `telemetry()`.
- `Ramp` and `CommandDelay` live in `sim::ramp` and are reused by any
  component that wants either; they are not specific to inverters.
- Lisp side has no per-component closures or symbol-name conventions.
  All it does is call constructors and assemble a tree.

## gRPC mapping

Same surface as microsim today (so existing SDK clients work):

| RPC                                                | Backed by                                              |
|----------------------------------------------------|--------------------------------------------------------|
| `GetMicrogrid`                                     | `World::metadata()` (read from a top-level `metadata`) |
| `ListElectricalComponents`                         | `World` registry → `ElectricalComponent` proto         |
| `ListElectricalComponentConnections`               | `World` connections                                    |
| `ReceiveElectricalComponentTelemetryStream`        | per-component `telemetry()` on a per-stream interval   |
| `SetElectricalComponentPower {Active, Reactive}`   | `set_active_setpoint` / `set_reactive_setpoint`        |
| `AugmentElectricalComponentBounds`                 | `augment_active_bounds`                                |

A `TimeoutTracker` (ported from microsim) calls `reset_setpoint()` on
the component when no fresh request has arrived in `request_lifetime`.

## Phasing

Phase markers correspond to the existing TaskList items.

1. **Scaffold** — Cargo.toml against remote tulisp `vm` branch, build.rs,
   proto submodule, lib.rs skeleton, two binaries.
2. **Core sim** — `SimulatedComponent`, `World`, `Clock`, `Ramp`,
   `CommandDelay`, `VecBounds`, time helpers, `ComponentHandle`.
3. **Grid + Meter** — proves the `AsPlist!` round-trip and topology
   recording.
4. **Battery + BatteryInverter** — first non-trivial component pair;
   exercises `Ramp` and `CommandDelay` end-to-end.
5. **SolarInverter, EvCharger, CHP** — ports the rest.
6. **gRPC server** — the streaming + control surface.
7. **Sample config + smoke** — a switchyard-equivalent `config.lisp`
   loads cleanly and `cargo build` is warning-free.
8. **(future)** TUI, full proto coverage of state codes / phases beyond
   what microsim currently fills in, integration tests against the
   Frequenz SDK.

## Out of scope (initial)

- TUI parity with microsim. The TUI module is large and orthogonal;
  switchyard ships headless first.
- DC bus / power-flow physics beyond what microsim already does (real
  power, naive voltage drop). The `gpt-5 discussion …` note in microsim
  is interesting follow-up material but not part of v1.
- `ListSensors` / sensor telemetry — microsim leaves this `todo!()` and
  switchyard will too.

## Inverter ↔ battery coupling

A real inverter and the battery sitting on its DC bus share **only an
electrical coupling** — a pair of busbars, no data link. The inverter
does not "know" the battery's SoC-protective limits; it pushes current
onto the bus, and the battery's BMS clamps locally. Clients learn
about the resulting derate by measuring the inverter (its actual
output power, its own bounds telemetry) and by separately measuring
the battery — never by peeking inside one component from the other.

Switchyard models that:

- `Battery::set_dc_power` clamps incoming DC power to the SoC-derated
  effective bounds. The BMS contract.
- `BatteryInverter::tick` distributes the commanded share, reads back
  what each battery actually accepted (`aggregate_power_w()`), and
  publishes that **measured** aggregate as its AC-side telemetry.
  Mismatch between commanded ramp value and measured output is the
  physical signal a client uses to infer downstream limiting.
- `BatteryInverter::telemetry.active_power_bounds` reports the
  inverter's own bounds (rated ∩ active augmentations) — no
  children's data leaks into that field.

Setpoint validation has two layers, mirroring how a real microgrid
deployment is built:

1. **Component-local backstop.** `set_active_setpoint` on the
   component validates against its own bounds. Catches programming
   errors that bypass the gateway.
2. **API gateway envelope.** `server.rs` intersects
   `component.effective_active_bounds()` with
   `World::aggregate_child_bounds(id)` and rejects requests that fall
   outside that intersection. This is what a production microgrid API
   would do — it sees both telemetry streams and gives clients a
   single envelope to plan against, even though no in-system data link
   ties the inverter and battery together.

`SimulatedComponent::effective_active_bounds()` stays on the trait
because it is still meaningful for the component's own telemetry and
for non-inverter aggregations (e.g. a meter that wants to publish the
battery DC envelope). The constraint is that the inverter does not
call it on its children.

## Microsim parity matrix

A snapshot of where switchyard sits relative to microsim. Items marked
"in progress" are being actively closed; the rest are open follow-ups.

### Microsim has, switchyard doesn't

| Feature | Notes |
|---|---|
| **TUI** | Microsim ships a 350-line ratatui-based terminal UI for live inspection (`--tui`). Switchyard is headless; clients use `swctl` or grpcurl. |
| **`dt:now` / `dt:milliseconds` / `dt:minutes`** | Microsim exposes time helpers for scheduling absolute-time events. Switchyard has none. |
| **`microsim-etags`** | Editor jump-to-definition support for the Lisp config. Switchyard has no equivalent binary. |
| **PV inverter min-power cross-talk** | Microsim solar inverters share a `min-available-power` symbol an external timer can drive (e.g. cloud schedule). Switchyard's solar inverter reads `sunlight%` once at construction. |

### Switchyard has, microsim doesn't

| Feature | Notes |
|---|---|
| **`SimulatedComponent` trait + modular Rust architecture** | Adding a new component type is one file under `src/sim/` plus one `register` line. |
| **Health / telemetry-mode / command-mode runtime knobs** | Three orthogonal flags (`:health`, `:telemetry-mode`, `:command-mode` at construction; `set-component-*` defuns at runtime) simulate device-side faults: `Error` / `Standby` health rejects setpoints with `FailedPrecondition`; `Silent` keeps the stream open with no data; `Closed` ends the stream; `Timeout` hangs SetPower; `Error` returns `Unavailable`. Microsim approximated some of this via huge intervals + `:config '((component-state . error))`. |
| **`AsPlist!` typed plist args** | Compile-time-checked argument structs for every `make-*` instead of runtime `plist-get`. |
| **`:command-delay-ms`** | Inverters and EV chargers model SCADA round-trip latency before honouring a setpoint. |
| **`:ramp-rate`** | Slew-rate-limited power tracking (W/s) — actual power moves toward the target at a configurable rate, like a real device protecting cells / breakers. |
| **`:soc-protect-margin`** | Explicit knob for the SoC-protective derate window on Battery and EvCharger (microsim's curve has a hardcoded 10%). |
| **`:stream-jitter-pct`** | Per-component random jitter on telemetry stream cadence so multi-subscriber clients see streams drifting independently. |
| **Two-layer setpoint validation** | API gateway intersects `inverter.bounds ∩ Σ children.bounds` and rejects out-of-envelope setpoints with the explicit envelope in the error message. |
| **Decoupled inverter ↔ battery** | Inverter publishes measured output and its own bounds only; battery self-clamps on ingress. No data coupling between the two devices, matching real hardware. |
| **Composable reactive capability** | Two orthogonal optional constraints per inverter: PF cap (`\|Q\| ≤ k×\|P\|`) and apparent / kVA cap (`P² + Q² ≤ S²`). Microsim hardcodes the PF case at 0.35; switchyard exposes both as `:reactive-pf-limit` and `:reactive-apparent-va`, recomputes the live envelope every tick, and publishes it on the `AC_POWER_REACTIVE` metric sample. |
| **Lisp-driven consumer-load profiles** | `(set-meter-power id W)` setter + `(now-seconds)` / `(window-elapsed N)` clock helpers + `(csv-load)` / `(csv-lookup)` CSV interpolation, used together inside `(every)` callbacks to drive arbitrary load shapes (function-defined or CSV-recorded). Microsim's `:per-phase-power 'symbol` indirection covered one specific case; switchyard's split is more flexible (the curve can come from any Lisp expression or interpolated file). |
| **Apparent DC telemetry** | Battery reports `dc_power_w = sign(P) × √(P² + Q²)` and `dc_current_a = dc_power_w / V_dc`, reflecting the conductor / IGBT loading a real DC instrument would read. SoC integration stays on active P only. |
| **`swctl` CLI** | clap-based client (`info` / `list` / `tree` / `stream` / `set-power` / `augment-bounds`). Microsim users went via grpcurl or the Frequenz SDK. |
| **`World::aggregate_child_bounds`** | Public Rust API for walking the topology and summing children's bounds. Microsim's equivalent lives entirely in Lisp. |
| **MxN inverter ↔ battery topology** | Battery accumulates pushes additively per-tick; one inverter→N batteries equal-share; N inverters→one battery sums; M×N nests both. Failed batteries (`Health::Error/Standby`) are filtered out at distribution and the surviving siblings absorb the full commanded value. Microsim's distribution is hard-wired 1-inverter-to-N-batteries with no failed-child handling. |
| **Redundant / parallel meters** | Two meters can declare the same subtree as their `:successors` and both edges land in the connections graph; each independently aggregates the shared child. `swctl tree` renders the shared subtree under each parent (DAG → tree projection). Closing one meter via `(set-component-telemetry-mode "closed")` leaves the redundant peer unaffected. |

### Functional parity

Both have: hot-reload via `notify`; gRPC `Microgrid` v1alpha18 surface
(Get / List / Stream / SetPower / AugmentBounds); `TimeoutTracker`
resetting stale set-points; anchored telemetry timestamps with no
per-iteration drift; `(every)` / `(run-with-timer)` for environment
scripting; config-driven topology assembly.

## Backlog

### Per-phase reactive setpoints

The proto's `SetElectricalComponentPower` takes a single scalar
`power` value, not per-phase. Real inverters can be commanded to
dispatch unbalanced reactive across phases for voltage support. Out
of scope for v1; would need a proto extension or a side-channel
defun. Track here so we don't forget the gap.

### Per-source attribution under MxN

Today an inverter publishes its **commanded** AC output as
`measured`; the battery separately publishes the accepted (clamped)
value. A SCADA client that wants to see "inverter 1 produced 7.5 kW
of the 13 kW total flowing into the shared battery" has to read both
streams and reconcile. A future upgrade: track per-source pushes on
the battery's accumulator (`Vec<(parent_id, p, q)>`), distribute the
clamp ratio across sources, and let each inverter read its own
`accepted_from(self.id)` to compute fair-share measured. Minor
trade-off in trait surface.

## Risk notes

- **Locking**: tulisp `vm` branch's `Shared<dyn TulispAny>` is
  reference-counted shared state; component constructors mutate `World`
  while the Lisp interpreter holds its own context lock. Keep World
  internals on `parking_lot::RwLock` and never call back into Lisp from
  inside a component `tick()`.
- **Tick ordering**: a meter that aggregates from successors must tick
  *after* those successors. World will sort components in reverse
  topological order at registration time; `register()` is called by
  constructors in child-first order already (because Lisp evaluates
  `:successors` before the `make-*` call), so the natural insertion
  order is correct.
- **Hot-reload**: when `config.lisp` is re-evaluated, World must be
  reset (cancel scheduler, clear registry) before the constructors run
  again. A `(reset-state)` defun does this from Lisp, mirroring
  microsim.
