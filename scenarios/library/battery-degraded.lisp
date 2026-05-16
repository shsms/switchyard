;; Battery 1000 cycles ok / error in 4-hour blocks. A control app
;; targeting BatteryPool::power should see the pool's bounds
;; shrink while the battery is sidelined and recover when it
;; comes back ok.

(define-scenario
 :name "battery-degraded-fleet"
 :description "Battery 1000 flips ok/error every 4 hours"
 :stages
 '((:name "ok"
    :hour-from 0.0 :hour-to 4.0
    :on (lambda () (set-component-health 1000 'ok)))
   (:name "error"
    :hour-from 4.0 :hour-to 8.0
    :on (lambda () (set-component-health 1000 'error)))
   (:name "ok-again"
    :hour-from 8.0 :hour-to 12.0
    :on (lambda () (set-component-health 1000 'ok)))
   (:name "error-again"
    :hour-from 12.0 :hour-to 16.0
    :on (lambda () (set-component-health 1000 'error)))))
