# Scenarios

A scenario is a Lisp file that drives switchyard through a stress
test — sudden load spikes, cloud cover, battery outages, silent
components — while a Rust **reporter** records metrics and a
**journal** records named events. At any time you can ask the
running simulator what's happened so far via
`GET /api/scenario/report` or the **Report** panel in the UI.

The framework is two layers:

- **Driver** (the script you write) — schedules events using
  `(every …)` and `(run-with-timer …)` plus a small set of
  scenario-specific defuns. See `example.lisp` in this directory.
- **Reporter** (Rust observer, always running) — accumulates peak
  main-meter power, battery charge / discharge integrals, PV
  produced energy, SoC stats, and 15-minute window peaks.
  Resets on `scenario-start`; freezes on `scenario-stop`.

## Loading a scenario

There's no auto-discovery. Load explicitly from the REPL —

```lisp
(load "scenarios/example.lisp")
```

— or chain it into `config.lisp` so it runs at startup.
`(load …)` resolves relative to the directory the running
config lives in (the same dir tulisp's load path uses), so
`scenarios/example.lisp` works from a normal `cargo run` in this
repo.

## Lifecycle defuns

| Defun                              | Purpose                                          |
|------------------------------------|--------------------------------------------------|
| `(scenario-start NAME)`            | begin a run — clears the journal + reporters    |
| `(scenario-stop)`                  | end the run — freezes elapsed + metrics + CSV   |
| `(scenario-event KIND PAYLOAD)`    | append a journaled event                        |
| `(scenario-elapsed)`               | wall-clock seconds since start (frozen on stop) |
| `(scenario-end-after MINUTES)`     | schedule `(scenario-stop)` after MINUTES        |
| `(scenario-record-csv DIR)`        | start writing one CSV per component to DIR     |
| `(scenario-stop-csv)`              | close all CSV sinks (also implicit on stop)    |

`KIND` accepts a symbol or a string. `PAYLOAD` accepts any value
— it renders through Display when stored.

## Driving the environment

These setters work outside scenarios too — they're the same
animation knobs `config.lisp` uses for its built-in load and
cloud curves — but inside a scenario they're how a script
exercises the simulator:

| Defun                                  | Effect                                                   |
|----------------------------------------|----------------------------------------------------------|
| `(set-meter-power ID VAL)`             | drive a meter's `:power` (number / lambda / `'symbol`)   |
| `(set-solar-sunlight ID VAL)`          | drive a solar inverter's `:sunlight%` (same polymorphism)|
| `(set-component-health ID K)`          | flip health to `'ok` / `'error` / `'standby`             |
| `(set-component-telemetry-mode ID K)`  | `'normal` / `'silent` / `'closed`                        |
| `(set-component-command-mode ID K)`    | `'normal` / `'timeout` / `'error`                        |
| `(set-active-power ID W &OPTIONAL MS)` | gRPC-style setpoint with request-lifetime                |

`(set-meter-power 100 (lambda () (csv-lookup …)))` and
`(set-meter-power 100 'consumer-power)` install the lambda or
the symbol as the source — the scheduler re-resolves it once per
tick. An imperative numeric `set-meter-power` collapses any prior
dynamic source back to a constant.

`(set-component-telemetry-mode 200 'silent)` plus
`(set-component-command-mode 200 'timeout)` simulates a
"flaky network" — the inverter keeps producing power and the
physics keeps simulating, but the gRPC stream goes quiet and
SetPower requests hang. Useful for exercising downstream apps
that need to cope with stale or unresponsive sources.

## Helpers in `sim/scenarios.lisp`

Loaded once per scenario via `(load "sim/scenarios.lisp")`:

- `(random-uniform LOW HIGH)` — uniform float in `[LOW, HIGH)`.
- `(random-pick LIST)` — one element of `LIST`, uniformly. `nil`
  on empty.
- `(random-outage IDS &rest opts)` — recurring random outages on
  a random pick from `IDS`. Plist opts: `:min-every` /
  `:max-every` (gap seconds), `:min-duration` / `:max-duration`
  (outage seconds), `:kind` (health symbol while down — default
  `'error`). Each transition lands as a journal event.

## Reading the report

The reporter exposes:

```sh
curl -s http://127.0.0.1:8801/api/scenario          # lifecycle
curl -s http://127.0.0.1:8801/api/scenario/events   # journal (paginated)
curl -s http://127.0.0.1:8801/api/scenario/report   # aggregate metrics
```

`/api/scenario/events` takes `?since=N&limit=M` for incremental
polling — pass back the previous response's `next_event_id`
unchanged.

`/api/scenario/report` returns:

| Field                          | Meaning                                                   |
|--------------------------------|-----------------------------------------------------------|
| `scenario_elapsed_s`           | seconds since `scenario-start`; frozen on stop            |
| `peak_main_meter_w`            | max active-power on the main meter so far                 |
| `main_meter_id`                | id of the meter flagged with `:main t`, or `null`         |
| `total_battery_charged_wh`     | sum across batteries; positive DC power → charging        |
| `total_battery_discharged_wh`  | sum across batteries; negative DC power → discharging     |
| `total_pv_produced_wh`         | sum across solar inverters; negative active P → produced  |
| `per_battery`                  | `[{ id, charge_wh, discharge_wh }]`                       |
| `per_pv`                       | `[{ id, produced_wh }]`                                    |
| `soc_stats`                    | `{ mean_pct, median_pct, mode_pct }` over current SoCs   |
| `main_meter_window_peaks`      | `[{ window_start, peak_w }]`, 15-min UTC-aligned, ≤ 96   |

For peak tracking to work, the main / point-of-common-coupling
meter must be flagged at construction:

```lisp
(make-meter :id 2 :main t :successors …)
```

The sample `config.lisp` already does this. Multiple `:main t`
flags is a config error — the second `(%make-meter)` call
returns an error.

## Recording CSVs

`(scenario-record-csv "csvs")` opens one buffered CSV per
component under `csvs/`, named `<id>-<category>.csv`, with a
uniform 5-column header (`ts_iso, active_power_w,
reactive_power_var, dc_power_w, soc_pct`) — empty cells where a
component doesn't publish that field. Rows write at the 1 Hz
history-sampler cadence. `(scenario-stop)` flushes and closes
the files; `(scenario-stop-csv)` does the same on demand
mid-scenario.
