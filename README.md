# switchyard

A modular microgrid simulator. Components are Rust types that implement
a single `SimulatedComponent` trait; the configuration DSL is Lisp via
[`tulisp`](https://github.com/shsms/tulisp), where `(make-grid)`,
`(make-meter)`, `(make-battery)`, … return typed handles assembled into
a topology.

- `World` owns the registry, physics-tick scheduler, and shared AC
  environment (per-phase voltage, frequency).
- Components opt into `CommandDelay` (SCADA round-trip latency) and
  `Ramp` (slew-rate limit) so inverters can model real-world response
  curves and battery-protective tapering.
- The `make-*` functions are typed `defun`s built with tulisp's
  `AsPlist!` macro — the lisp side is a pure config DSL.

## Status

Phase 1 (scaffold) is in. The crate compiles; `cargo run` loads
`config.lisp`, builds the topology in `World`, and starts the physics
loop. The gRPC surface is the next milestone.

## Build

The proto definitions are reused from microsim until switchyard adds
its own submodule:

```sh
cargo build
```

Override the proto path with `SWITCHYARD_PROTO_ROOT` if microsim is not
adjacent.
