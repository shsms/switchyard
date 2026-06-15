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

;; The random-outage--* state below is hoisted to globals so each
;; timer firing sees the same parameters. With same-ctx tulisp-async
;; this is no longer required for closure visibility (timer bodies
;; funcall on the parent ctx and lexical captures survive), but the
;; globals approach is kept here for clarity — and as a consequence
;; only one random-outage chain runs at a time per process. Calling
;; random-outage again replaces the prior chain's parameters; any
;; timer it has in flight on `active-timers` will continue with the
;; new state on its next firing.

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

(defun random-outage--track (timer)
  "Track TIMER on `active-timers', dropping this chain's previous
(already-fired) handle first — without the prune a multi-day run
accumulates thousands of dead one-shot handles that only
`reset-state' ever cleared. Only one outage chain runs per process
(see above), so a single replacement slot suffices."
  (when (and (boundp 'random-outage--timer) random-outage--timer)
    (let (kept)
      (dolist (tm active-timers)
        (unless (eq tm random-outage--timer)
          (setq kept (cons tm kept))))
      (setq active-timers kept)))
  (setq random-outage--timer timer)
  (setq active-timers (cons timer active-timers)))

(defun random-outage--schedule ()
  "Schedule the next outage after a uniform-random gap."
  (let ((gap (random-uniform random-outage--min-every
                             random-outage--max-every)))
    (random-outage--track (run-with-timer gap nil 'random-outage--fire))))

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
      (random-outage--track (run-with-timer dur nil 'random-outage--restore)))))

(defun random-outage--restore ()
  "Revert the victim's health and reschedule the next outage."
  (let ((victim random-outage--current-victim))
    (when victim
      (set-component-health victim 'ok)
      (scenario-event 'restored (format "%d back" victim))
      (random-outage--schedule))))

;; -----------------------------------------------------------------------------
;; Declarative signal profiles (timeline / hold / ramp)
;; -----------------------------------------------------------------------------
;;
;; Build a piecewise-linear driver as a sequence of segments instead of
;; hand-rolling a `cond` on `(scenario-elapsed)`. `timeline` returns a
;; dynamic source (a lambda re-resolved each tick), so it plugs straight
;; into `set-meter-power` / `set-solar-sunlight`:
;;
;;   (set-meter-power 100 (timeline (hold 2000 :for 60)
;;                                  (ramp :to 50000 :over 10)
;;                                  (ramp :to 2000 :over 10)))
;;
;; Time is relative to `(scenario-start)`; before the first segment the
;; value is its start, after the last it holds the last segment's end.

(defun hold (value &rest plist)
  "Timeline segment: stay at VALUE for :for seconds."
  (list :dur (plist-get plist :for) :from value :to value))

(defun ramp (&rest plist)
  "Timeline segment: move linearly to :to over :over seconds, starting
from :from — which defaults to the previous segment's end value."
  (list :dur  (plist-get plist :over)
        :to   (plist-get plist :to)
        :from (plist-get plist :from)))

(defun timeline--at (rows lastv tt)
  "Value of the ROWS piecewise-linear profile at scenario time TT. Each
row is (tstart tend vfrom vto); past the last row the value holds LASTV."
  (let ((val lastv))
    (dolist (row rows)
      (if (and (>= tt (nth 0 row)) (< tt (nth 1 row)))
          (setq val (+ (nth 2 row)
                       (* (- (nth 3 row) (nth 2 row))
                          (/ (- tt (nth 0 row)) (- (nth 1 row) (nth 0 row))))))))
    val))

(defun timeline (&rest segments)
  "Return a dynamic source (a lambda over scenario time) walking
SEGMENTS — each a `(hold V :for S)` or `(ramp :to V :over S [:from A])`.
A ramp without :from continues from the previous segment's end value
(0 at the start); after the last segment the value holds its end."
  (let ((tstart 0.0)
        (prev 0.0)
        (rows nil))
    (dolist (seg segments)
      (let* ((dur (plist-get seg :dur))
             (from (plist-get seg :from))
             (to (plist-get seg :to))
             (vfrom (if from from prev))
             (tend (+ tstart dur)))
        (setq rows (append rows (list (list tstart tend vfrom to))))
        (setq tstart tend)
        (setq prev to)))
    (let ((lastv prev))
      (lambda () (timeline--at rows lastv (scenario-elapsed))))))
