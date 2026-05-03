# switchyard

A microgrid simulator for testing downstream control apps. Components
(grid, meter, battery, inverters, EV charger, CHP) are Rust types
behind a single `SimulatedComponent` trait; the topology + animation
script is Lisp via [`tulisp`](https://github.com/shsms/tulisp).

The simulator exposes three surfaces:

- **gRPC** (`[::1]:8800`) — Frequenz `Microgrid` v1alpha18 API.
  Downstream apps written against the production API talk to
  switchyard the same way they'd talk to a real microgrid.
- **Web UI** (`http://127.0.0.1:8801`) — interactive topology +
  per-component charts + scenario dashboard. Embedded into the
  binary via `rust-embed`; no build step.
- **swctl** — clap-based client that drives both surfaces from the
  shell.

## Build & run

```sh
cargo build
cargo run --bin switchyard config.lisp
```

`config.lisp` is the entry-point: it wires the topology, sets the
microgrid id, and animates the AC environment. Saving the file
hot-reloads the world. See `sim/defaults.lisp` for the per-category
default knobs and `sim/common.lisp` for the runtime helpers.

The gRPC build pulls in
[frequenz-floss/frequenz-api-microgrid](https://github.com/frequenz-floss/frequenz-api-microgrid).
The default search path is `../microsim/submodules/frequenz-api-microgrid`;
override with `SWITCHYARD_PROTO_ROOT` if it lives elsewhere.

## Scenarios

A scenario is a Lisp script that drives the simulator through stress
events (load spikes, cloud cover, random outages, silent components)
while a Rust reporter records peak / charge / discharge / SoC stats
and per-15-minute averages. See [`scenarios/README.md`](scenarios/README.md)
for the framework, [`scenarios/example.lisp`](scenarios/example.lisp)
for a runnable 30-minute sample.

```sh
swctl scenario start "demo"
swctl scenario load scenarios/example.lisp
swctl scenario report
swctl scenario events --since 0 --limit 20
swctl scenario stop
```

The Report panel in the web UI polls the same endpoints as `swctl
scenario report`.

## swctl

```sh
swctl info
swctl tree
swctl list --category battery
swctl stream 1001 --samples 5
swctl set-power 1001 -- -5000 --lifetime 30   # negative = discharge
swctl scenario report
```

`--addr` (default `http://[::1]:8800`) points the gRPC client;
`--ui-addr` (default `http://127.0.0.1:8801`) points the scenario
HTTP commands. `--json` swaps any human table for the raw JSON.

## Configuration knobs

- **`(make-meter :main t …)`** — flag the point-of-common-coupling
  meter; the scenario reporter tracks its peak.
- **`(make-meter :power N | (lambda () …) | 'symbol)`** — drive the
  meter's published power from a constant, a lambda, or a global
  symbol. Same on solar inverters via `:sunlight%`.
- **`(set-meter-power id N | (lambda () …) | 'symbol)`** — same
  polymorphism imperatively, for `(every …)` callbacks or scenario
  scripts. Numeric values collapse any prior dynamic source.

## Architecture in one paragraph

`World` owns the component registry, the physics tick loop, the
telemetry-history rings, and the scenario journal. Lisp's only jobs
are wiring the topology (`(make-grid)`, `(make-meter)`, …) and
animating the environment (`(every …)`, `(run-with-timer …)`,
`(set-meter-power)`, etc.) — every component's tick / ramp / SoC
derate stays in Rust. Inverter and battery share only an electrical
coupling: the battery's BMS clamps DC ingress, the inverter
publishes the measured aggregate, and a server-side gateway
intersects bounds for setpoint validation.

## More

- [`CLAUDE.md`](CLAUDE.md) — developer notes for this repo.
- [`todo.org`](todo.org) — roadmap + open design questions.
- [`scenarios/README.md`](scenarios/README.md) — scenario framework
  reference.
