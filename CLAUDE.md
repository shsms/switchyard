# switchyard

A Rust microgrid simulator with a Lisp-driven config DSL. Reimplementation of
[microsim](../microsim) where component physics lives in Rust and Lisp's job
is wiring the topology + animating the environment.

## Layout

- `src/lib.rs` — module roots
- `src/sim/` — components + scheduler
  - `component.rs` — `SimulatedComponent` trait, `ComponentHandle`, `Telemetry`
  - `world.rs` — registry, physics tick, grid state, topology
  - `bounds.rs` — `VecBounds`, `ComponentBounds` (rated + TTL augmentations)
  - `ramp.rs` — `CommandDelay` + `Ramp`
  - `decay.rs` — `bounded_exp_decay` + `soc_protected_bounds`
  - `battery.rs`, `meter.rs`, `grid.rs`, `chp.rs`, `ev_charger.rs`,
    `inverter/{battery,solar}_inverter.rs`
- `src/lisp/` — config DSL glue
  - `mod.rs` — `Config`, runtime defuns, tulisp-async wiring
  - `make.rs` — `(make-*)` constructors via `AsPlist!`
  - `handle.rs` — `ComponentHandle` ↔ `Shared<dyn TulispAny>` round trip
- `src/server.rs` — `Microgrid` gRPC service
- `src/proto.rs` + `src/proto_conv.rs` — proto include + `Telemetry` →
  `MetricSample`s
- `src/timeout_tracker.rs` — request lifetime → `reset_setpoint` expiry
- `src/bin/switchyard.rs` — headless server
- `src/bin/swctl.rs` — clap-based client CLI
- `sim/common.lisp` — Lisp helpers (`every`, `reset-state`)
- `config.lisp` — sample topology + environment animation

## Architectural rules

- **Lisp wires + animates the environment, Rust does physics.** Every
  component's tick / ramp / SoC derate is in Rust. Lisp's only verbs are
  `(make-*)` to build the graph and `(every …)` / `(run-with-timer …)` to
  perturb grid state or flip runtime knobs over time.
- **No data coupling between inverter and battery.** A real inverter and
  battery share only an electrical bus. `Battery::set_dc_power` clamps to its
  own SoC-derated bounds; the inverter publishes the measured aggregate it
  actually delivered. The API gateway (server.rs) intersects bounds for
  setpoint validation — components never read each other's bounds.
- **Single physics tick, registration order = tick order.** `World::spawn_physics`
  runs one `tokio::time::interval` at `physics_tick_ms` and calls `tick()` on
  every component in registration order. Children register first because Lisp
  evaluates `:successors` before the surrounding `make-*`.
- **Telemetry stream cadence is anchored to a target timestamp.** `next_due +=
  step` then `sleep until next_due`; re-anchor only when behind. Per-stream
  `:stream-jitter-pct` perturbs each step; mean is exactly the configured
  interval.

## Build / run / test

```sh
cargo build
cargo test                                # unit tests for bounds/ramp/decay
cargo run --bin switchyard config.lisp
cargo run --bin swctl -- info
cargo run --bin swctl -- tree
cargo run --bin swctl -- stream 1001 --samples 5
cargo run --bin swctl -- set-power 1001 5000
```

The server binds `[::1]:8800` by default (override via `(set-socket-addr …)`
in config.lisp). swctl points there by default; override with `--addr`.

## Dependencies

- `tulisp = { git = "https://github.com/shsms/tulisp", branch = "vm",
  features = ["sync"] }` — uses `AsPlist!` from the `vm` branch. Don't
  downgrade or switch to a path dep without coordination.
- `tulisp-async = { path = "../tulisp-async" }` — local sibling. Provides
  `run-with-timer`, `cancel-timer`, `sleep-for`. `TokioExecutor::new` calls
  `Handle::current()`, so `Config::new` must be invoked inside a running
  tokio runtime.
- Proto root reused from `../microsim/submodules/frequenz-api-microgrid`
  (override with `SWITCHYARD_PROTO_ROOT`).

## Adding a component type

1. New file under `src/sim/` implementing `SimulatedComponent`.
2. Add to `src/sim/mod.rs` re-exports.
3. Add a `make-foo` defun in `src/lisp/make.rs` with `AsPlist!`-derived args,
   calling `world.register(...)`.
4. Mirror the args struct as a sibling `AsAlist!`-derived `FooDefaults`
   (without `id` / `successors` / other per-component fields). Take the
   alist via `:config<Option<LispValue>>` on the args struct and merge
   in the defun with `a.field.or(d.field)` (helper: `parse_defaults`).
5. (Optional) Override `subtype()` if proto needs `InverterType::Foo` / etc.

## Lisp value adapters

- Runtime mode enums (`Health`, `TelemetryMode`, `CommandMode`) take
  their lisp-side `TryFrom<TulispObject>` + `TulispConvertible` impls
  in `src/lisp/runtime_modes.rs`. **Symbols only** — `:health 'error`
  works, `:health "error"` errors with a type mismatch.
- `LispValue` (`src/lisp/value.rs`) — passthrough wrapper that lets a
  raw `TulispObject` ride through `AsPlist!` (works around the
  blanket-`From<T> for T` `Infallible` mismatch). Used for the
  `:config <alist>` per-category defaults plumbing.

## Lisp gotchas (current tulisp-vm)

- **Timer bodies see global symbols / defuns but get a fresh ctx.**
  `setq` and `defun` results survive into the timer's ctx because
  tulisp symbols own their global bindings ctx-independently. Do not
  rely on `let*`-bound state to leak across firings.

## Adding a runtime knob

1. Field on the component config struct + plist arg in `src/lisp/make.rs`.
2. (If runtime-mutable) trait method override + `World` setter + Lisp defun
   in `src/lisp/mod.rs`. Use `(every …)` or `(run-with-timer …)` from the
   config to script behaviour over time.
3. Demonstrate via a new line in `config.lisp` and verify via swctl.

## Microsim parity gaps

See PLAN.md for the up-to-date comparison of features microsim has and
switchyard doesn't (and vice versa).
