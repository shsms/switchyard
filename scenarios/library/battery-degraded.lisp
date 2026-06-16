;; Battery 1000 cycles ok / error. A control app targeting
;; BatteryPool::power should see the pool's bounds shrink while the
;; battery is sidelined and recover when it comes back ok.
;;
;; A relative demo: a health flip every minute.

(define-scenario
 :name "battery-degraded-fleet"
 :description "Battery 1000 flips ok/error"
 :schedule 'relative
 :length "4min"
 :setup (lambda () (set-component-health 1000 'ok))
 :cues (list
        (at "60s" (lambda () (set-component-health 1000 'error)))
        (at "120s" (lambda () (set-component-health 1000 'ok)))
        (at "180s" (lambda () (set-component-health 1000 'error)))))
