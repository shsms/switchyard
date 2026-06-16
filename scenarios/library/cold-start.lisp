;; Every commandable component starts in standby. The battery inverter
;; comes online first, then solar, then EV + CHP. A control app that
;; polls health should observe its addressable set growing.
;;
;; A relative demo: one component (group) comes online a minute.

(define-scenario
 :name "cold-start"
 :description "Every component starts standby; gradual come-online"
 :schedule 'relative
 :length "4min"
 :setup (lambda ()
          (set-component-health 1001 'standby)
          (set-component-health 200 'standby)
          (set-component-health 1006 'standby)
          (set-component-health 1004 'standby))
 :cues (list
        (at "60s" (lambda () (set-component-health 1001 'ok)))
        (at "120s" (lambda () (set-component-health 200 'ok)))
        (at "180s" (lambda ()
                     (set-component-health 1006 'ok)
                     (set-component-health 1004 'ok)))))
