;; Switchyard sample configuration. Reload-safe: (reset-state) cancels
;; outstanding timers and wipes every microgrid's site, then the
;; per-microgrid (make-microgrid …) forms under microgrids/ rebuild
;; the topology.

;; Load runtime helpers (every, reset-state) once. Avoids re-defining
;; defuns on every reload — they don't change between saves and the
;; cost is non-trivial.
(unless (boundp 'switchyard-loaded)
  (setq switchyard-loaded t)
  (load "sim/common.lisp"))

;; Per-category defaults + bare-name DSL (`grid`, `meter`, `battery`,
;; …). Loaded outside the boundp guard so edits to defaults.lisp are
;; visible after reload, and registered with the notify watcher so
;; saving defaults.lisp itself triggers the reload.
(load "sim/defaults.lisp")
(watch-file "sim/defaults.lisp")

(reset-state)

;; -----------------------------------------------------------------------------
;; Enterprise-level identity + simulation cadence
;; -----------------------------------------------------------------------------

(set-physics-tick-ms 100)
(set-enterprise-id 1)

;; -----------------------------------------------------------------------------
;; Time-driven animation. `every` runs the callback every :milliseconds
;; (no synchronous first call), so these blocks can sit alongside the
;; topology rather than after it. Setters that don't carry a component
;; id (set-voltage-per-phase, set-frequency) apply to the active
;; microgrid — the scenarios per-microgrid replay fans them out.
;; -----------------------------------------------------------------------------

;; Per-tick noise on the AC line voltage — a slow random wander a
;; few hundred mV either side of nominal. Grid frequency lives on
;; its own Ornstein-Uhlenbeck driver (one for the whole enterprise,
;; one per AC grid by physics) — see `(set-frequency-model …)` and
;; `(set-frequency-override …)` below if you want to tune its
;; defaults or script an event.
(every
 :milliseconds 200
 :call (lambda ()
         (set-voltage-per-phase
          (+ 229.0 (/ (random 200) 100.0))
          (+ 229.0 (/ (random 200) 100.0))
          (+ 229.0 (/ (random 200) 100.0)))))

;; PV cloud-cover schedule over a 10-minute window, driving the solar
;; inverter (id 200 in the Berlin demo microgrid). Sunny first 3 min
;; (80%), 2-min ramp into clouds (→ 20%), 2 min cloudy, 2-min ramp
;; back to clear. The per-tick min-avail clamp on the solar inverter
;; picks up each new sunlight% on the next tick.
(defun cloud-curve (t-window)
  (cond ((< t-window 180.0) 80.0)
        ((< t-window 300.0) (- 80.0 (* 0.5 (- t-window 180.0))))
        ((< t-window 420.0) 20.0)
        (t (min 80.0 (+ 20.0 (* 0.5 (- t-window 420.0)))))))

(every
 :milliseconds 1000
 :call (lambda ()
         (set-solar-sunlight 200 (cloud-curve (window-elapsed 600.0)))))

;; -----------------------------------------------------------------------------
;; Microgrids — one file per microgrid under microgrids/. Each file
;; holds a single (make-microgrid …) form and loads its own
;; persisted UI overrides from inside the :topology lambda.
;; Runtime-created microgrids (via /api/microgrids/create) get a
;; stub file written here too, so they survive process restarts.
;; -----------------------------------------------------------------------------

(load-microgrid-configs)

;; Load the starter scenarios library — seven multi-stage canned
;; scenarios appear in the Scenarios mode dropdown on a fresh
;; checkout. Edit scenarios/library/index.lisp to drop / add
;; entries; each file is self-contained `(define-scenario …)`.
(load "scenarios/library/index.lisp")
