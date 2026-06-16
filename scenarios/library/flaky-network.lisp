;; Solar inverter loses telemetry + commands time out during a midday
;; window. Exercises a control app's resilience to partial visibility —
;; a real flaky-link experience without having to actually drop packets.
;;
;; A relative demo: goes flaky at +60s, recovers at +120s.

(define-scenario
 :name "flaky-network"
 :description "Solar inverter goes silent + commands time out, then recovers"
 :schedule 'relative
 :length "3min"
 :setup (lambda ()
          (set-component-telemetry-mode 200 'normal)
          (set-component-command-mode 200 'normal))
 :cues (list
        (at "60s" (lambda ()
                    (set-component-telemetry-mode 200 'silent)
                    (set-component-command-mode 200 'timeout)))
        (at "120s" (lambda ()
                     (set-component-telemetry-mode 200 'normal)
                     (set-component-command-mode 200 'normal)))))
