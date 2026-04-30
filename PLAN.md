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
  component handles. **No `tulisp-async`** — Lisp's only job is to
  build the component graph; once the file finishes loading, every
  subsequent decision (ticks, ramps, delays, telemetry, set-points)
  happens in Rust. Microsim uses `(every)` and `run-with-timer` for
  per-component lifecycles, but those move out of the DSL here.
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

## Open design issues

### Inverter ↔ battery coupling is currently data, not electrical

In the real world an inverter and the battery sitting on its DC bus
share **only an electrical coupling** — a pair of busbars, no data
link. The inverter does not "know" the battery's SoC-protective
limits; it just pushes current onto the bus, and the battery's BMS
clamps or trips locally. Software clients of the inverter learn about
the resulting derate by *measuring the inverter* (its actual output
power, its bounds telemetry derived from what it can deliver), not by
peeking inside the battery.

Switchyard currently violates this. The shipped implementation has
`BatteryInverter::combined_bounds(world) = self.bounds.effective() ∩
Σ children.effective_active_bounds()`, used both for setpoint
validation and to pull the ramp target back into the children's
envelope each tick. That is a synthetic data path — the inverter is
reading a Rust-level method on the battery struct that no real
inverter could read on a real battery.

What the right model looks like:

1. **Inverter is the only authority on its own AC-side bounds.** Its
   `effective_active_bounds()` is rated ∩ active augmentations; nothing
   about children enters that calculation.
2. **Battery is the only authority on its DC-side bounds.** Its tick
   already self-clamps incoming `set_dc_power`, but right now the
   inverter pre-empts that by clamping upstream. Drop the upstream
   clamp; let the battery refuse what it cannot accept.
3. **The inverter's *measured* output is the sum of what its children
   actually accepted**, not the value the ramp drove toward. The
   inverter's telemetry's `active_power_w` should track that measured
   value; the difference between commanded and measured is the
   physical signal that a real client uses to infer "the battery is
   limiting me".
4. **Bounds telemetry on the inverter is its own bounds.** A client
   that wants to know the *aggregate* envelope queries each child
   stream and sums on the client side, the same way it would in
   production.

Until that refactor lands, the current behaviour is convenient (a
single setpoint reach reports cleanly) but architecturally incorrect.
Track in a follow-up: replace `combined_bounds` with measured-output
aggregation; keep `effective_active_bounds()` on the trait (still
useful for batteries' own telemetry and for non-inverter aggregations
like a meter that reports a battery's DC bound), but stop calling it
from the inverter.

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
