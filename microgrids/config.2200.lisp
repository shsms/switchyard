;; Berlin demo microgrid (id 2200). One file per microgrid lives in
;; this directory; the top-level config.lisp picks them up via
;; (load-microgrid-configs). Runtime-created microgrids land here
;; too — each gets its own file with the same shape.

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

       ;; Solar branch — id 200 so the cloud-curve timer in
       ;; config.lisp can reach it.
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

       ;; Hidden consumer meter — invisible in ListComponents / tree
       ;; but aggregated into the main meter. `%make-meter` bypasses
       ;; meter-defaults so the explicit :power isn't combined with
       ;; a default :stream-jitter-pct on a hidden component. Power
       ;; follows a sine wave: peak 30 kW, trough 5 kW, one cycle
       ;; every 15 min, plus ±500 W jitter.
       (%make-meter
        :id 100 :name "consumer" :hidden t
        :power (lambda ()
                 (+ 17500.0
                    (* 12500.0 (sin (* 6.2831853 (/ (window-elapsed 900.0) 900.0))))
                    (- (random 1000) 500))))))))
   ;; Apply UI-driven edits the user has clicked Persist on. Loaded
   ;; from inside the :topology lambda so (current-microgrid-id)
   ;; resolves to 2200 and the overrides land in *this* microgrid's
   ;; site.
   (load-overrides)))
