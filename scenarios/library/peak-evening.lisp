;; Consumer load ramps through the evening peak; PV is gone, the
;; battery has to discharge to cover. Modelled after a typical
;; northern-European household demand curve: low daytime baseline,
;; sharp 18:00–20:00 peak, taper by 21:00.

(define-scenario
 :name "peak-evening-load"
 :description "Consumer ramp 17:00 → 21:00, PV gone, batteries discharging"
 :stages
 '((:name "ramp"
    :hour-from 17.0 :hour-to 18.0
    :on (lambda ()
          (set-meter-power 100 12000.0)
          (set-solar-sunlight 200 10.0)))
   (:name "peak"
    :hour-from 18.0 :hour-to 20.0
    :on (lambda ()
          (set-meter-power 100 25000.0)
          (set-solar-sunlight 200 0.0)
          (set-active-power 1001 -10000.0)))
   (:name "wind-down"
    :hour-from 20.0 :hour-to 21.0
    :on (lambda ()
          (set-meter-power 100 6000.0)
          (set-active-power 1001 0.0)))))
