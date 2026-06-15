# switchyard

A Rust microgrid simulator with a Lisp-driven config DSL. Reimplementation of
[microsim](../microsim) where component physics lives in Rust and Lisp's job
is wiring the topology + animating the environment.

## Layout

- `src/lib.rs` — module roots
- `src/sim/` — components + scheduler
  - `component.rs` — `SimulatedComponent` trait, `ComponentHandle`, `Telemetry`
  - `microgrid_site/` — per-microgrid registry, physics tick, grid state,
    topology (+ `history.rs` sampler, `scenarios.rs` event log)
  - `microgrids.rs` — enterprise registry + per-mg routing
  - `dispatch.rs` — enterprise dispatch store (per-`microgrid_id`, id
    allocator, lifecycle broadcast); backs the dispatch gRPC + UI
  - `bounds.rs` — `VecBounds`, `ComponentBounds` (rated + TTL augmentations)
  - `ramp.rs` — `CommandDelay` + `Ramp`
  - `decay.rs` — `bounded_exp_decay` + `soc_protected_bounds`
  - `battery.rs`, `meter.rs`, `grid.rs`, `chp.rs`, `ev_charger.rs`,
    `inverter/{battery,solar}_inverter.rs`
- `src/lisp/` — config DSL glue
  - `mod.rs` — `Config` (fields, accessors, reload)
  - `boot.rs` — `Config::new`: interpreter setup, defun registration,
    tulisp-async wiring, background loops
  - `defuns/` — every `register_*` installer, one file per topic
    (clock, scenarios, microgrids, metadata, runtime_modes, …)
  - `overrides.rs` / `snapshots.rs` — per-mg override-file persistence
    + snapshot save/load on `Config`
  - `make.rs` — `(make-*)` constructors via `AsPlist!`
  - `handle.rs` — `ComponentHandle` ↔ `Shared<dyn TulispAny>` round trip
- `src/ui/` — embedded web UI server
  - `mod.rs` — axum router + serve entry points
  - `handlers/` — HTTP handlers, one file per topic (topology, eval,
    scenarios, dispatches, …)
  - `state.rs` / `loopback.rs` / `events_ws.rs` — loopback client cache,
    gRPC loopback supervisor, WS event push
- `ui-assets/` — the SPA as hand-rolled ES modules (`app.js` is the
  entry; `topology.js`, `dashboard.js`, `inspect.js`, `repl.js`,
  `routing.js`, `dialogs.js`, `editor.js`, … own one concern each)
- `src/server.rs` — `Microgrid` gRPC service
- `src/assets_server.rs` — `PlatformAssets` gRPC service (shared port)
- `src/dispatch_server.rs` — `MicrogridDispatchService` gRPC service
  (store-and-serve dispatch API; CRUD + stream over `sim::dispatch`)
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
  own SoC-derated bounds; the inverter publishes the value it *commanded*
  its healthy children to take (zero when tripped or when no healthy child
  accepted the push), and the battery separately publishes what it
  accepted — a client wanting to see saturation reads both streams
  (todo.org d5 tracks per-source attribution). The API gateway
  (server.rs) intersects bounds for
  setpoint validation — components never read each other's bounds.
- **Single physics tick, registration order = tick order.** `MicrogridSite::spawn_physics`
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

Each registered microgrid binds its own gRPC port; the first
defaults to `[::1]:8800` and subsequent microgrids step by ten
(`:8810`, `:8820`, …). Override via `:grpc-port` on
`(make-microgrid …)`. swctl's `--addr` points the gRPC client at
the first microgrid by default; pass `--addr http://[::1]:8810`
etc. to reach others. The UI server binds `127.0.0.1:8801`
(hardcoded for now — `--ui-bind` / `--ui-port` is on the roadmap).

`PlatformAssets` and `MicrogridDispatchService` each bind a single
shared listener (they're enterprise-wide, keyed by `microgrid_id` per
request): assets on `[::1]:9900`, dispatch on `[::1]:8900`. Override
via `(set-assets-socket-addr …)` / `(set-dispatch-socket-addr …)`.
Point the dispatch CLI at it with
`--url 'grpc://[::1]:8900?ssl=false' --auth-key any` (auth is ignored),
or use `swctl dispatch {list,create,pause,resume,delete,get}`. The
per-microgrid Dispatches UI sub-tab (`/api/mg/{id}/dispatches`) lists
them and can create / pause / resume / delete; all three write paths
(gRPC, UI, swctl) funnel through `DispatchStore::{create,set_active}`,
so construction + validation stay identical.

## Dependencies

- `tulisp = { git = "https://github.com/shsms/tulisp", features =
  ["sync", "etags"] }` — tracks the main branch for AsPlist!, etags,
  and other downstream-touching features. Don't downgrade or swap
  branches without coordination.
- `tulisp-async = { git = "https://github.com/tulisp/tulisp-async" }`
  — same-ctx timer primitives (`run-with-timer`, `cancel-timer`,
  `sleep-for`). `TokioExecutor::new` calls `Handle::current()`, so
  `Config::new` must be invoked inside a running tokio runtime.
  `register` returns a `Handle`; the pre-tick hook owns one clone
  and ticks it each physics step — without that, no timer body
  ever runs (the same-ctx model has no background firing thread).
- Proto roots are vendored under `submodules/`:
  - `submodules/frequenz-api-microgrid` (pinned at v0.18.0) — override
    with `SWITCHYARD_PROTO_ROOT` for a private mirror.
  - `submodules/frequenz-api-assets` (pinned at v0.1.0).
  - `submodules/frequenz-api-dispatch` (pinned at v1.0.0) — dispatch
    v1; imports the same vendored common v1alpha8, so no common of
    its own.

## Adding a component type

1. New file under `src/sim/` implementing `SimulatedComponent`.
2. Add to `src/sim/mod.rs` re-exports.
3. Add a `%make-foo` defun in `src/lisp/make.rs` with `AsPlist!`-derived
   args, calling `site.register(...)`. Note the leading `%` —
   user-facing topology code calls `make-foo`, which dispatches here.
4. Add a `foo-defaults` plist + `(defun make-foo …)` wrapper to
   `sim/defaults.lisp`. The wrapper `apply`s `%make-foo` to the
   defaults plist `append`-ed in front of the caller's args; AsPlist's
   last-occurrence-wins resolution lets per-component plist values
   override the defaults.
5. (Optional) Override `subtype()` if proto needs `InverterType::Foo` / etc.

## Sample-config DSL convention

Two-layer split:
- `%make-*` — Rust primitives in `src/lisp/make.rs`. Pure plist
  parsing; every field arrives as a plist key, no defaults.
- `make-*` — Lisp wrappers in `sim/defaults.lisp` that prepend a
  `<cat>-defaults` plist and dispatch to `%make-*`.

Topology code uses `make-*` (defaults applied). To opt out of
defaults entirely for one call, invoke `%make-*` directly.
Per-component plist args win without any special handling — AsPlist!
takes the last occurrence of each key and the wrapper's defaults
appear first in the merged plist.

`config.lisp` loads `sim/defaults.lisp` outside its boundp guard so
edits re-apply on reload, and registers it via `(watch-file …)` so
saving defaults.lisp triggers the reload watcher just like saving
config.lisp does.

## Lisp value adapters

- Runtime mode enums (`Health`, `TelemetryMode`, `CommandMode`) take
  their lisp-side `TryFrom<TulispObject>` + `TulispConvertible` impls
  in `src/lisp/runtime_modes.rs`. **Symbols only** — `:health 'error`
  works, `:health "error"` errors with a type mismatch.
- `LispValue` (`src/lisp/value.rs`) — passthrough wrapper that lets a
  raw `TulispObject` ride through `AsPlist!` (works around the
  blanket-`From<T> for T` `Infallible` mismatch). Used for `:power`
  and `:sunlight%`, where the make-* dispatcher inspects the raw
  shape to pick between a constant and a `DynamicScalar`.

## Lisp gotchas (current tulisp-vm)

- **Timer bodies run on the calling ctx.** Same-ctx tulisp-async
  funcalls bodies on the parent `TulispContext`, so a lambda's
  lexical captures (`let*`-bound state, the surrounding closure
  environment) are preserved across firings. defuns/defvars/global
  setq results are visible as you'd expect.
- **`(every …)` callbacks fire on `Config`'s dedicated refresh
  loop, not on the physics tick.** `Config::spawn_lisp_refresh_loop`
  ticks on its own 100 ms grid, takes the interpreter lock once
  per pass, refreshes every microgrid's dynamic-scalar inputs,
  then drains the tulisp-async pending-firings mailbox. So a
  `(run-with-timer 0.05 …)` waits up to 100 ms before firing,
  and a zero-delay one fires on the next refresh pass. Tests
  that need a fire without spinning the loop call
  `cfg.refresh_once()` (synchronous wrapper for the same work).
  Physics ticks themselves are pure Rust now — they read the
  atomic scalars the refresh loop has cached and never touch the
  interpreter, so a long `/api/eval` no longer freezes the
  microgrid's beat.

## Adding a runtime knob

1. Field on the component config struct + plist arg in `src/lisp/make.rs`.
2. (If runtime-mutable) trait method override + `MicrogridSite` setter + Lisp defun
   in the matching `src/lisp/defuns/` file. Use `(every …)` or
   `(run-with-timer …)` from the config to script behaviour over time.
3. Demonstrate via a new line in `config.lisp` and verify via swctl.

## Testing an external bounds-driving app (e.g. the )

Switchyard is used to test apps whose job is to push
`AugmentElectricalComponentBounds` and watch `power_bounds` react (the
`fz-microgrid-` GCP active-power limiter is the motivating
case; `-test.lisp` + `scenarios/-cases.lisp`).

- **Both battery and solar inverters curtail to their effective
  (rated ∩ augmentation) bounds every tick.** `CommandDelay::poll`
  returns the armed setpoint on every tick, and `tick()` re-clamps it
  to the live envelope — so an external app narrowing a bound actually
  slews the inverter down at `ramp_rate`, and it recovers when the
  augmentation relaxes (tests: `late_augmentation_re_clamps_an_armed_setpoint`;
  `solar_inverter::tick`). Curtail-to-bounds already exists — don't
  reimplement it. A controller commands a setpoint **once**; it need
  not re-send, since the armed value persists and keeps curtailing.
- `set_active_setpoint` **hard-errors** a command outside the live
  (augmentation-narrowed) envelope — faithful to the real API gateway
  gating out-of-envelope setpoints. An EMS wanting "max within the cap"
  reads the bounds and commands within them.
- An inverter set to `:health 'error` (or `'standby`) **trips offline
  to zero output** *and* is dropped from the healthy `power_bounds`
  aggregate. A battery inverter clears its setpoint and awaits
  re-dispatch on recovery; a PV inverter resumes from sunlight.
- Drive sim state ad-hoc by POSTing lisp to
  `http://127.0.0.1:8801/api/eval`, e.g.
  `--data "(set-component-health 201 'error)"` → `{"ok":true,…}`.
- The limiter app itself reads **`NITROGEN_LOG_LEVEL`** (not
  `RUST_LOG`) for tracing: `info` shows the per-breach plan,
  `fz_microgrid_=trace` the per-inverter caps it pushes.
- Switchyard's physics supports closed-loop bound tests today; the
  remaining gaps are ergonomic, not physical — scenario assertions,
  an in-sim controller/actor that reacts to live bounds, declarative
  signal profiles, and deterministic sim-time. See `todo.org` §I.

## Roadmap and deferred work

See `todo.org` for the forward-looking roadmap (scenario framework,
reactive plist values, integration tests, CI) and known open design
questions.
