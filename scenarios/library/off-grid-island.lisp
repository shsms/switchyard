;; Grid connection point goes 'error mid-day, simulating a forced
;; islanding event. PV + battery have to carry the load alone for
;; the 8-hour window; the control app should respond by clamping
;; consumer setpoints and discharging the battery.

(define-scenario
 :name "off-grid-island"
 :description "Grid goes 'error for 8 h; PV + battery carry load alone"
 :stages
 '((:name "grid-up"
    :hour-from 0.0 :hour-to 8.0
    :on (lambda () (set-component-health 1 'ok)))
   (:name "islanded"
    :hour-from 8.0 :hour-to 16.0
    :on (lambda ()
          (set-component-health 1 'error)
          (set-active-power 1001 -10000.0)))
   (:name "reconnected"
    :hour-from 16.0 :hour-to 24.0
    :on (lambda ()
          (set-component-health 1 'ok)
          (set-active-power 1001 0.0)))))
