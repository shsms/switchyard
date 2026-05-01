;; Switchyard sample configuration. Reload-safe: (reset-state) cancels
;; outstanding timers and wipes the World, then make-* rebuilds it.

;; Load runtime helpers (every, reset-state) once. Avoids re-defining
;; defuns on every reload — they don't change between saves and the
;; cost is non-trivial.
(unless (boundp 'switchyard-loaded)
  (setq switchyard-loaded t)
  (load "sim/common.lisp"))

(reset-state)

;; -----------------------------------------------------------------------------
;; Simulation cadence + identity
;; -----------------------------------------------------------------------------

(set-physics-tick-ms 100)
(set-microgrid-id 2200)
(set-enterprise-id 1)
(set-microgrid-name "Berlin demo")
(set-socket-addr "[::1]:8800")  ;; takes effect on next launch

;; -----------------------------------------------------------------------------
;; Time-driven animation. `every` runs the callback every :milliseconds
;; (no synchronous first call), so these blocks can sit alongside the
;; topology rather than after it.
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
         (set-meter-power 100 (consumer-curve (window-elapsed 900.0)))))

;; ── CSV-driven alternative (uncomment to swap with the function above) ──
;; (setq csv-data    (csv-load "sim/example_load.csv"))
;; (setq csv-anchor  (now-seconds))   ;; t=0 in the CSV maps to "now"
;; (every
;;  :milliseconds 1000
;;  :call (lambda ()
;;          (let ((rel (mod (- (now-seconds) csv-anchor) 900.0)))
;;            (set-meter-power 100
;;                             (+ (csv-lookup csv-data "kitchen" rel)
;;                                (csv-lookup csv-data "bedroom" rel)
;;                                (csv-lookup csv-data "office" rel))))))

;; -----------------------------------------------------------------------------
;; Per-category defaults. Each (make-*) accepts `:config <alist>`;
;; precedence is per-component plist > :config alist > Rust default.
;; Label-shaped values (`:health`, `:telemetry-mode`, `:command-mode`)
;; take a quoted symbol: `(health . ok)`, not the string form.
;; -----------------------------------------------------------------------------

(setq grid-defaults
      '((rated-fuse-current . 100)
        (stream-jitter-pct  . 1.0)))

(setq meter-defaults
      '((interval          . 200)
        (stream-jitter-pct . 4.0)))

(setq battery-defaults
      '((soc-protect-margin . 10.0)
        (stream-jitter-pct  . 8.0)
        (health             . ok)))

(setq battery-inverter-defaults
      '((command-delay-ms     . 1500)
        (ramp-rate            . 5000.0)
        (stream-jitter-pct    . 8.0)
        (reactive-pf-limit    . 0.0)        ;; 0 = disabled
        (reactive-apparent-va . 32000.0)))  ;; kVA-circle envelope

(setq solar-inverter-defaults
      '((ramp-rate         . 2000.0)
        (stream-jitter-pct . 5.0)))

(setq ev-charger-defaults
      '((soc-protect-margin . 10.0)
        (command-delay-ms   .   500)
        (ramp-rate          . 3000.0)
        (stream-jitter-pct  .   10.0)))

(setq chp-defaults
      '((stream-jitter-pct . 0.0)))

;; -----------------------------------------------------------------------------
;; Topology — nested for visual clarity. The whole graph is one
;; expression; reading top-to-bottom traces the grid → main meter →
;; per-branch meters → underlying device chain. Each (make-*) consumes
;; the matching :config defaults; per-component plist args override.
;; -----------------------------------------------------------------------------

(make-grid
 :id 1
 :config grid-defaults
 :successors
 (list
  (make-meter
   :id 2
   :config meter-defaults
   :successors
   (list
    ;; Battery branch: SCADA delay + slew-rate-limited ramp,
    ;; kVA-circle reactive envelope, slight per-stream jitter on both
    ;; the inverter and the battery underneath it. All from defaults.
    (make-meter
     :config meter-defaults
     :successors
     (list (make-battery-inverter
            :config battery-inverter-defaults
            :successors
            (list (make-battery
                   :config      battery-defaults
                   :initial-soc 85.0)))))   ; per-component override

    ;; Solar branch.
    (make-meter
     :config meter-defaults
     :successors
     (list (make-solar-inverter
            :config    solar-inverter-defaults
            :sunlight% 80.0)))               ; scenario knob

    ;; EV branch — near-full so the SoC-protect taper is observable.
    (make-meter
     :config meter-defaults
     :successors
     (list (make-ev-charger
            :config       ev-charger-defaults
            :initial-soc  92.0
            :soc-upper   100.0
            :rated-upper 22000.0)))

    ;; CHP modeled as a constant -2 kW generator on its meter.
    (make-meter
     :config meter-defaults
     :power  -2000.0
     :successors (list (make-chp :config chp-defaults)))

    ;; Hidden consumer meter — invisible in ListComponents / tree but
    ;; aggregated into the main meter. Driven dynamically by the
    ;; consumer-curve timer above via id 100.
    (make-meter :id 100 :hidden t :power 1000.0)))))
