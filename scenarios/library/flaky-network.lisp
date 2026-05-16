;; Solar inverter loses telemetry + commands time out during a
;; 2-hour midday window. Exercises a control app's resilience to
;; partial visibility — a real flaky-link experience without
;; having to actually drop packets.

(define-scenario
 :name "flaky-network"
 :description "Solar inverter goes silent + commands time out for 2 h"
 :stages
 '((:name "normal"
    :hour-from 0.0 :hour-to 10.0
    :on (lambda ()
          (set-component-telemetry-mode 200 'normal)
          (set-component-command-mode 200 'normal)))
   (:name "flaky"
    :hour-from 10.0 :hour-to 12.0
    :on (lambda ()
          (set-component-telemetry-mode 200 'silent)
          (set-component-command-mode 200 'timeout)))
   (:name "recovered"
    :hour-from 12.0 :hour-to 14.0
    :on (lambda ()
          (set-component-telemetry-mode 200 'normal)
          (set-component-command-mode 200 'normal)))))
