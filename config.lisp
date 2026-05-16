;; Switchyard sample configuration. Reload-safe: (reset-state) cancels
;; outstanding timers and wipes every microgrid's site, then the
;; (make-microgrid …) form below rebuilds the topology.

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

;; Per-tick noise on the AC environment: a slowly wandering line
;; voltage and a frequency that drifts a few mHz around 50 Hz.
(every
 :milliseconds 200
 :call (lambda ()
         (set-voltage-per-phase
          (+ 229.0 (/ (random 200) 100.0))
          (+ 229.0 (/ (random 200) 100.0))
          (+ 229.0 (/ (random 200) 100.0)))
         (set-frequency
          (+ 49.99 (/ (random 4) 100.0)))))

;; Consumer-load curve over a 15-minute window, driving meter id 100
;; (the hidden consumer meter declared in the topology below).
;; Shape: low first half (1 kW), 7-min ramp 1 → ~16 kW, sudden 16 kW
;; spike near the end. Replace with (csv-lookup …) for a profile
;; recorded from real data; the setter doesn't care where the value
;; comes from.
(defun consumer-curve (t-window)
  (cond ((< t-window 450.0) 1000.0)
        ((> t-window 870.0) 16000.0)
        (t (+ 1000.0 (* 35.0 (- t-window 450.0))))))

(every
 :milliseconds 200
 :call (lambda ()
         ;; (set-meter-power 100 (consumer-curve (window-elapsed 900.0)))
         (set-meter-power 100 0)))

;; PV cloud-cover schedule over a 10-minute window, driving the solar
;; inverter (id 200 below). Sunny first 3 min (80%), 2-min ramp into
;; clouds (→ 20%), 2 min cloudy, 2-min ramp back to clear. The
;; per-tick min-avail clamp on the solar inverter picks up each new
;; sunlight% on the next tick.
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
;; Microgrid — one (make-microgrid …) form per microgrid. The
;; :topology lambda is what scopes nested make-* calls into this
;; microgrid's site; the lambda's body reads top-to-bottom from
;; grid-connection-point → main meter → per-branch meters →
;; underlying device chain.
;; -----------------------------------------------------------------------------

(make-microgrid
 :id 2200
 :name "Berlin demo"
 :grpc-port 8800
 :tso "TN"
 :topology
 (lambda ()
   (make-grid-connection-point
    :id 1
    :rated-lower -90000.0
    :rated-upper  100000.0
    :successors
    (list
     (make-meter
      :id 2
      :main t                       ;; flagged for scenario-report peak tracking
      :successors
      (list
       ;; Battery branch — every knob (SCADA delay, ramp, jitter,
       ;; kVA-circle reactive envelope) comes from battery-inverter-
       ;; defaults / battery-defaults.
       (make-meter
        :successors
        (list (make-battery-inverter
               :successors
               (list (make-battery :initial-soc 85.0)))))   ; per-component override

       ;; Solar branch — id 200 so the cloud-curve timer above can reach it.
       (make-meter
        :successors
        (list (make-solar-inverter :id 200 :sunlight% 80.0)))   ; scenario starting point

       ;; EV branch — near-full so the SoC-protect taper is observable.
       (make-meter
        :successors
        (list (make-ev-charger
               :initial-soc  92.0
               :soc-upper   100.0
               :rated-upper 22000.0)))

       ;; CHP modeled as a constant -2 kW generator on its meter.
       (make-meter :power -2000.0 :successors (list (make-chp)))

       ;; Hidden consumer meter — invisible in ListComponents / tree but
       ;; aggregated into the main meter. Driven dynamically by the
       ;; consumer-curve timer above via id 100. `%make-meter` bypasses
       ;; meter-defaults so the explicit :power isn't combined with a
       ;; default :stream-jitter-pct on a hidden component.
       (%make-meter :id 100 :name "consumer" :hidden t :power 1000.0)))))))

;; Load the starter scenarios library — seven multi-stage canned
;; scenarios appear in the Scenarios mode dropdown on a fresh
;; checkout. Edit scenarios/library/index.lisp to drop / add
;; entries; each file is self-contained `(define-scenario …)`.
(load "scenarios/library/index.lisp")

;; Apply UI-driven edits the user has clicked Persist on. The override
;; filename is parameterised by microgrid-id so each microgrid in the
;; enterprise gets its own. No-op when the file doesn't exist.
(load-overrides)
