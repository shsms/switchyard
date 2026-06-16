;; Consumer load ramps through the evening peak; PV is gone, the
;; battery has to discharge to cover. Modelled after a typical
;; northern-European household demand curve: low daytime baseline,
;; sharp peak, taper.
;;
;; A relative demo: ramp → peak → wind-down compressed to one
;; transition a minute so it plays in the UI without waiting for the
;; wall clock to reach the evening.

(define-scenario
 :name "peak-evening-load"
 :description "Consumer ramp → peak → wind-down, PV gone, batteries discharging"
 :schedule 'relative
 :length "3min"
 :setup (lambda ()
          (set-meter-power 100 12000.0)
          (set-solar-sunlight 200 10.0))
 :cues (list
        (at "60s" (lambda ()
                    (set-meter-power 100 25000.0)
                    (set-solar-sunlight 200 0.0)
                    (set-active-power 1001 -10000.0)))
        (at "120s" (lambda ()
                     (set-meter-power 100 6000.0)
                     (set-active-power 1001 0.0)))))
