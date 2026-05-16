;; Every commandable component starts in standby. The battery
;; inverter comes online first, then solar, then EV + CHP. A
;; control app that polls health should observe its addressable
;; set growing across the boot hour.

(define-scenario
 :name "cold-start"
 :description "Every component starts standby; gradual come-online"
 :stages
 '((:name "all-standby"
    :hour-from 0.0 :hour-to 1.0
    :on (lambda ()
          (set-component-health 1001 'standby)
          (set-component-health 200 'standby)
          (set-component-health 1006 'standby)
          (set-component-health 1004 'standby)))
   (:name "battery-online"
    :hour-from 1.0 :hour-to 2.0
    :on (lambda () (set-component-health 1001 'ok)))
   (:name "solar-online"
    :hour-from 2.0 :hour-to 3.0
    :on (lambda () (set-component-health 200 'ok)))
   (:name "all-online"
    :hour-from 3.0 :hour-to 24.0
    :on (lambda ()
          (set-component-health 1006 'ok)
          (set-component-health 1004 'ok)))))
