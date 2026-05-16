;; Cloud bank crosses the array mid-morning: full sun, dropout to
;; near-overcast, recovery. Useful for verifying a control app
;; tracks the PV envelope down + back up without overshooting.

(define-scenario
 :name "pv-dropout"
 :description "Clear → cloud bank → clear over the morning"
 :stages
 '((:name "sunny-morning"
    :hour-from 8.0 :hour-to 10.0
    :on (lambda () (set-solar-sunlight 200 80.0)))
   (:name "cloud-cover"
    :hour-from 10.0 :hour-to 12.0
    :on (lambda () (set-solar-sunlight 200 15.0)))
   (:name "sun-returns"
    :hour-from 12.0 :hour-to 14.0
    :on (lambda () (set-solar-sunlight 200 80.0)))))
