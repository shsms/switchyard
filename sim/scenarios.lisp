;; Scenario helpers — load from a scenario script before using
;; `random-outage` etc.
;;
;;   (load "sim/scenarios.lisp")
;;
;; Built on `run-with-timer` (tulisp-async) and the existing
;; `set-component-health` defun. Adds random-pick / random-uniform
;; helpers for scenario authors who want stochastic events.

(defun random-uniform (low high)
  "Pseudo-random float in [LOW, HIGH). Composed from the integer
`(random N)` primitive — scales a 0..N draw to the requested
range. The `1.0 *` coerces the integer division to float."
  (let ((scale 1000000))
    (+ low (/ (* 1.0 (- high low) (random scale)) scale))))

(defun random-pick (items)
  "Return one element of ITEMS chosen uniformly at random. Returns
nil if ITEMS is empty."
  (when items
    (nth (random (length items)) items)))

;; Timer callbacks run in a fresh tulisp ctx — they can see global
;; symbols + defuns but NOT let-bound state from the surrounding
;; scope. The random-outage--* state below is therefore deliberately
;; hoisted to globals so each timer firing can read it. As a
;; consequence only one random-outage chain runs at a time per
;; process; calling random-outage again replaces the prior chain's
;; parameters (and any timer it has in flight from active-timers
;; will continue with the new state on its next firing).

(defun random-outage (ids &rest opts)
  "Schedule recurring random outages on a random pick from IDS.

Plist OPTS:
  :min-every    Lower bound on the gap between outages, seconds.
  :max-every    Upper bound on the gap, seconds.
  :min-duration Outage duration lower bound, seconds.
  :max-duration Outage duration upper bound.
  :kind         Health symbol while down (default 'error).

Each cycle picks a random id, schedules a `(set-component-health
ID KIND)` after a uniform-random gap, reverts to 'ok after a
uniform-random duration, and reschedules — so a single
`(random-outage ...)` call drives outages for the lifetime of the
scenario (or until the timer is cancelled by `reset-state`)."
  (setq random-outage--ids ids)
  (setq random-outage--min-every    (or (plist-get opts :min-every)    60.0))
  (setq random-outage--max-every    (or (plist-get opts :max-every)    300.0))
  (setq random-outage--min-duration (or (plist-get opts :min-duration) 30.0))
  (setq random-outage--max-duration (or (plist-get opts :max-duration) 90.0))
  (setq random-outage--kind         (or (plist-get opts :kind)         'error))
  (random-outage--schedule))

(defun random-outage--schedule ()
  "Schedule the next outage after a uniform-random gap."
  (let ((gap (random-uniform random-outage--min-every
                             random-outage--max-every)))
    (setq active-timers
          (cons (run-with-timer gap nil 'random-outage--fire)
                active-timers))))

(defun random-outage--fire ()
  "Pick a victim, knock out for a uniform-random duration, then
schedule the restore callback."
  (let ((victim (random-pick random-outage--ids))
        (dur (random-uniform random-outage--min-duration
                             random-outage--max-duration)))
    (when victim
      (setq random-outage--current-victim victim)
      (set-component-health victim random-outage--kind)
      (scenario-event 'outage
                      (format "%d down for %.0f s" victim dur))
      (setq active-timers
            (cons (run-with-timer dur nil 'random-outage--restore)
                  active-timers)))))

(defun random-outage--restore ()
  "Revert the victim's health and reschedule the next outage."
  (let ((victim random-outage--current-victim))
    (when victim
      (set-component-health victim 'ok)
      (scenario-event 'restored (format "%d back" victim))
      (random-outage--schedule))))
