;; Cloud bank crosses the array: full sun, dropout to near-overcast,
;; recovery. Useful for verifying a control app tracks the PV envelope
;; down + back up without overshooting.
;;
;; A relative demo: one transition a minute.

(define-scenario
 :name "pv-dropout"
 :description "Clear → cloud bank → clear"
 :schedule 'relative
 :length "3min"
 :setup (lambda () (set-solar-sunlight 200 80.0))
 :cues (list
        (at "60s" (lambda () (set-solar-sunlight 200 15.0)))
        (at "120s" (lambda () (set-solar-sunlight 200 80.0)))))
