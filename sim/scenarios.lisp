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

;; -----------------------------------------------------------------------------
;; Section wrappers for `define-scenario`
;; -----------------------------------------------------------------------------
;;
;; Each wrapper builds an introspectable plist (or, for `event`, a
;; thunk) that a `define-scenario` section holds and the runner (§J2)
;; compiles to the existing primitives. Authoring reads directly:
;;
;;   (define-scenario :name "cloud-fade" :schedule 'relative :length "4min"
;;     :drive  (list (drive-meter 100 2000000.0)
;;                   (drive-solar 200 (timeline (hold 100 :for 120)
;;                                              (ramp :to 20 :over 27))))
;;     :agents (list (controller 'ems :every "500ms"
;;                     (lambda () (set-active-power 300 (component-bound-upper 300) "2s" t))))
;;     :cues   (list (at "60s" (event 'clouds "rolling in")))
;;     :expect (list (check "110s" :component 2 :metric 'active-power
;;                          :approx 1500000.0 :tol 300000.0)))
;;
;; Cue / check times are resolved to seconds by `resolve-time`, which
;; auto-detects a relative offset ("60s") vs a clock time ("14:00").

(defun drive-meter (id source)
  "Drive section: feed meter ID from SOURCE (a constant, a symbol, or a
dynamic source like `timeline`). Compiles to `set-meter-power`."
  (list :kind 'drive-meter :target id :source source))

(defun drive-solar (id source)
  "Drive section: feed solar inverter ID sunlight % from SOURCE (a
constant, a symbol, or a dynamic source like `timeline`). Compiles to
`set-solar-sunlight`; for several inverters use one drive-solar each."
  (list :kind 'drive-solar :target id :source source))

(defun controller (id &rest args)
  "Agents section: an in-sim controller named ID firing :every TIME
(default \"100ms\"), running the trailing LAMBDA each tick. Compiles to
`define-controller`."
  (let ((every (or (plist-get args :every) "100ms"))
        (on-tick (car (last args))))
    (list :id id
          :every-ms (* 1000 (resolve-time every))
          :on-tick on-tick)))

(defun at (tt action)
  "Cues section: run ACTION (a thunk, e.g. from `event`, or any 0-arg
lambda) at scenario time TT."
  (list :at-s (resolve-time tt) :action action))

(defun check (tt &rest expect-args)
  "Expect section: at scenario time TT, run a `scenario-expect` check
with EXPECT-ARGS (the same plist scenario-expect takes:
:component / :metric / :approx / :tol / :min / :max)."
  (list :at-s (resolve-time tt) :expect expect-args))

(defun event (kind payload)
  "Cue action: a thunk that journals a `scenario-event` when run. Use
inside `at`, e.g. (at \"60s\" (event 'clouds \"rolling in\"))."
  (lambda () (scenario-event kind payload)))

;; -----------------------------------------------------------------------------
;; Runner — compile a scenario's sections to the existing primitives
;; -----------------------------------------------------------------------------
;;
;; `scenario--run` is what both runners (todo §J2) drive: the Rust
;; entrypoint looks a scenario up in the registry and calls this with
;; its section data. It compiles down to the primitives that already
;; exist — `scenario-start`, `set-meter-power` / `set-solar-sunlight`,
;; `define-controller`, `run-with-timer`, `scenario-expect`,
;; `scenario-record-csv` — so there's no separate runner machinery:
;;
;;  - the live runner funcalls this on the wall clock; cue/check timers
;;    fire on the refresh loop.
;;  - the stepped runner funcalls this then advances the sim clock with
;;    `sim_run`; the same timers fire deterministically on sim-time.
;;
;; RECORD-DIR is resolved Rust-side ('csv -> a default dir, a string ->
;; itself, nil -> no recording) since tulisp has no stringp/symbolp to
;; branch on here.

(defun scenario--drive (d)
  "Install one drive item D — a `drive-meter` / `drive-solar` plist."
  (let ((target (plist-get d :target))
        (source (plist-get d :source)))
    (if (eq (plist-get d :kind) 'drive-solar)
        (set-solar-sunlight target source)
      (set-meter-power target source))))

(defun scenario--agent (a)
  "Install one agent A — a `controller` plist — as an in-sim controller."
  (define-controller :id (plist-get a :id)
                     :on-tick (plist-get a :on-tick)
                     :every-ms (plist-get a :every-ms)))

(defun scenario--at (secs thunk)
  "Schedule THUNK to run once at scenario time SECS (seconds)."
  (run-with-timer secs nil thunk))

(defun scenario--run (name seed setup drive agents cues expect record-dir)
  "Compile and start the scenario NAME: reset the journal, seed RNG,
run SETUP, install DRIVE sources + AGENTS controllers, schedule CUES
actions + EXPECT checks as timers, and open recording. Returns NAME."
  (scenario-start name)
  (when seed (set-random-seed seed))
  (when setup (funcall setup))
  (dolist (d drive) (scenario--drive d))
  (dolist (a agents) (scenario--agent a))
  (dolist (c cues)
    (scenario--at (plist-get c :at-s) (plist-get c :action)))
  (dolist (e expect)
    (let ((args (plist-get e :expect)))
      (scenario--at (plist-get e :at-s)
                    (lambda () (apply 'scenario-expect args)))))
  (when record-dir (scenario-record-csv record-dir))
  name)
