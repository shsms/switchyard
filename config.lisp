;; Switchyard sample configuration. Mirrors microsim's config.lisp but
;; with delay/ramp parameters exercised on the inverters. Reload-safe:
;; (reset-state) wipes World, then (make-*) calls rebuild it.

(reset-state)

;; Simulation cadence (ms) for the physics tick.
(set-physics-tick-ms 100)

;; AC environment.
(set-frequency 50.0)
(set-voltage-per-phase 230.0 230.0 230.0)

;; Battery branch — SCADA delay + slew-rate-limited ramp + slight
;; per-stream jitter on both the inverter and the battery so multi-
;; subscriber clients see them drifting independently. The battery
;; is started near full so the SoC-protect taper is observable.
(setq inv-bat-1 (make-battery-inverter
                 :command-delay-ms 1500
                 :ramp-rate         5000.0
                 :stream-jitter-pct 8.0
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

;; CHP branch — power literal lives on the meter.
(setq chp-1 (make-chp))

;; Per-branch meters.
(setq meter-bat (make-meter :successors (list inv-bat-1)))
(setq meter-pv  (make-meter :successors (list inv-pv-1)))
(setq meter-ev  (make-meter :successors (list ev-1)
                            :stream-jitter-pct 4.0))
(setq meter-chp (make-meter :power -2000.0 :successors (list chp-1)))
(setq meter-load (make-meter :hidden t :power 16000.0))

;; Main meter and grid connection point.
(setq main-meter (make-meter
                  :id 2
                  :interval 200
                  :successors (list meter-bat meter-pv meter-ev meter-chp meter-load)))

(make-grid
 :id 1
 :rated-fuse-current 100
 :successors (list main-meter))
