;; Grid connection point goes 'error, simulating a forced islanding
;; event. PV + battery have to carry the load alone; the control app
;; should respond by clamping consumer setpoints and discharging the
;; battery.
;;
;; A relative demo: islands at +60s, reconnects at +120s.

(define-scenario
 :name "off-grid-island"
 :description "Grid goes 'error; PV + battery carry load alone, then reconnect"
 :schedule 'relative
 :length "3min"
 :setup (lambda () (set-component-health 1 'ok))
 :cues (list
        (at "60s" (lambda ()
                    (set-component-health 1 'error)
                    (set-active-power 1001 -10000.0)))
        (at "120s" (lambda ()
                     (set-component-health 1 'ok)
                     (set-active-power 1001 0.0)))))
