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
;; Environment animation — driven by `every` rather than baked-in
;; constants. Lisp's job is to inject realistic per-tick noise into the
;; AC environment so streamed telemetry reflects what a real microgrid
;; sees (a slowly wandering line voltage, a frequency that drifts a
;; few mHz around 50.0).
;; -----------------------------------------------------------------------------

(every
 :milliseconds 200
 :call (lambda ()
         (set-voltage-per-phase
          (+ 229.0 (/ (random 200) 100.0))
          (+ 229.0 (/ (random 200) 100.0))
          (+ 229.0 (/ (random 200) 100.0)))
         (set-frequency
          (+ 49.99 (/ (random 4) 100.0)))))

;; -----------------------------------------------------------------------------
;; Topology
;; -----------------------------------------------------------------------------

;; Battery branch — SCADA delay + slew-rate-limited ramp + slight
;; per-stream jitter on both the inverter and the battery.
(setq inv-bat-1 (make-battery-inverter
                 :command-delay-ms 1500
                 :ramp-rate         5000.0
                 :stream-jitter-pct 8.0
                 ;; kVA-limited inverter (apparent-power circle of
                 ;; 32 kVA), no PF cap. Q allowance is full ±32 kVA at
                 ;; P=0 and shrinks as P approaches 30 kW.
                 :reactive-pf-limit 0.0          ;; 0 = disabled
                 :reactive-apparent-va 32000.0   ;; kVA-circle envelope
                 :successors (list (make-battery
                                    :initial-soc          85.0
                                    :soc-protect-margin   10.0
                                    :stream-jitter-pct    8.0))))

;; Solar branch.
(setq inv-pv-1 (make-solar-inverter
                :sunlight%         80.0
                :ramp-rate         2000.0
                :stream-jitter-pct 5.0))

;; EV charger near-full so the SoC-protect taper is observable.
(setq ev-1 (make-ev-charger
            :initial-soc        92.0
            :soc-upper          100.0
            :soc-protect-margin 10.0
            :rated-upper        22000.0
            :command-delay-ms   500
            :ramp-rate          3000.0
            :stream-jitter-pct  10.0))

;; CHP modeled as a constant -2 kW load on its meter.
(setq chp-1 (make-chp))

(setq meter-bat (make-meter :successors (list inv-bat-1)))
(setq meter-pv  (make-meter :successors (list inv-pv-1)))
(setq meter-ev  (make-meter :successors (list ev-1)
                            :stream-jitter-pct 4.0))
(setq meter-chp (make-meter :power -2000.0 :successors (list chp-1)))
;; Hidden consumer meters are driven dynamically from Lisp. Both
;; approaches are demonstrated; pick whichever fits your scenario.

;; ── (a) Function-driven consumer ────────────────────────────────────
;; Shape: low first half (1 kW), gradual ramp through the second half,
;; then a sudden spike near the end of every 15-minute window. Anything
;; expressible as a Lisp function over the elapsed window time works.
;;
;; Note: nested `if` rather than `cond` is intentional. tulisp-vm's
;; bytecode label table currently doesn't survive the ctx switch into
;; the timer body for `cond`'s jump table; `if` compiles to a simpler
;; form that does. Inline the body into the lambda — calls into a
;; (defun) with `cond` would also hit the same path.
(setq meter-load (make-meter :hidden t :power 1000.0))
(every
 :milliseconds 200
 :call (lambda ()
         (let ((t-w (window-elapsed 900.0)))
           (set-meter-power
            (component-id meter-load)
            (if (< t-w 450.0)
                1000.0                                 ;; first half: 1 kW
              (if (> t-w 870.0)
                  16000.0                              ;; spike near end
                (+ 1000.0 (* 35.0 (- t-w 450.0))))))))) ;; 7-min ramp

;; ── (b) CSV-driven consumer (alternative; uncomment to use) ─────────
;; (setq csv-data    (csv-load "sim/example_load.csv"))
;; (setq csv-anchor  (now-seconds))   ;; t=0 in the CSV maps to "now"
;; (every
;;  :milliseconds 1000
;;  :call (lambda ()
;;          (let ((rel (mod (- (now-seconds) csv-anchor) 900.0)))
;;            (set-meter-power (component-id meter-load)
;;                             (+ (csv-lookup csv-data "kitchen" rel)
;;                                (csv-lookup csv-data "bedroom" rel)
;;                                (csv-lookup csv-data "office" rel))))))

(setq main-meter (make-meter
                  :id 2
                  :interval 200
                  :successors (list meter-bat meter-pv meter-ev meter-chp meter-load)))

(make-grid
 :id 1
 :rated-fuse-current 100
 :successors (list main-meter))
