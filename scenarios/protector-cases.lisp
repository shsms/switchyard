;; scenarios/protector-cases.lisp — drives the GCP active-power limiter
;; through its live-capacity edge cases against the protector-test
;; microgrid (id 2300). Load into a running switchyard with:
;;
;;   (load "/vagrant/switchyard/scenarios/protector-cases.lisp")
;;
;; or  swctl scenario load scenarios/protector-cases.lisp
;;
;; PV inverter ids 200 and 201 (each -30 kW rated). Both at full sun
;; produce -60 kW, breaching the -20 kW GCP feed-in limit, so the
;; limiter is already engaged. Watch the limiter's own logs at each
;; staged event below.

(load "sim/scenarios.lisp")

(scenario-start "protector-live-capacity")

;; Stage 1 (t=0): both PV healthy -> live PV capacity = -60 kW.
(scenario-event 'stage "1: both PV healthy, feed-in breach")

;; Stage 2 (t=20s): down PV 201. Live PV capacity should drop to one
;; inverter (-30 kW); the healthy 200 must be capped off -30 kW, not
;; -60 kW, so it is not over-throttled.
(run-with-timer 20.0 nil
  (lambda ()
    (set-component-health 201 'error)
    (scenario-event 'down "2: PV 201 downed -> live PV capacity halves")))

;; Stage 3 (t=40s): restore 201 -> live capacity back to -60 kW.
(run-with-timer 40.0 nil
  (lambda ()
    (set-component-health 201 'ok)
    (scenario-event 'up "3: PV 201 restored")))

;; Stage 4 (t=60s): down BOTH PV -> empty PV power_bounds (characterises
;; the empty-snapshot path).
(run-with-timer 60.0 nil
  (lambda ()
    (set-component-health 200 'error)
    (set-component-health 201 'error)
    (scenario-event 'down "4: both PV downed -> empty PV capacity")))

;; Stage 5 (t=80s): restore both.
(run-with-timer 80.0 nil
  (lambda ()
    (set-component-health 200 'ok)
    (set-component-health 201 'ok)
    (scenario-event 'up "5: both PV restored")))

(scenario-end-after 2)
(scenario-event 'note "protector-live-capacity armed")
