;; switchyard runtime helpers — load this from your config file before
;; anything else:
;;
;;   (unless (boundp 'switchyard-loaded)
;;     (setq switchyard-loaded t)
;;     (load "sim/common.lisp"))
;;
;; Built on tulisp-async's `run-with-timer` / `cancel-timer`. Lisp's
;; job is to wire the topology and animate the *environment* (per-tick
;; voltage / frequency perturbations, scheduled events). Component
;; physics — ramps, SoC derating, AC/DC conversion — lives in Rust and
;; is reached via the `make-*` constructors and the gRPC API.

;; -----------------------------------------------------------------------------
;; Timer bookkeeping
;; -----------------------------------------------------------------------------

;; Live timer handles created by `every` and friends. Tracked so that
;; reset-state can cancel them on a config reload — otherwise the old
;; callbacks keep firing into a fresh, unrelated World.
;;
;; Process-global by design. If two distinct scenario scripts ever
;; share a process (rare with the current single-Config design),
;; reset-state would cancel both. Switchyard runs one Config per
;; process, so today there's no namespace conflict; revisit if a
;; multi-config layout shows up.
(unless (boundp 'active-timers)
  (setq active-timers nil))

(defun reset-state ()
  "Cancel every timer registered via `every` / `run-with-timer-tracked`,
then wipe the active microgrid's components. Call this at the top of
your config so a hot-reload starts from a clean slate."
  (dolist (tm active-timers)
    (cancel-timer tm))
  (setq active-timers nil)
  (reset-microgrid))

;; -----------------------------------------------------------------------------
;; Periodic helper
;; -----------------------------------------------------------------------------

(defun every (&rest plist)
  "Call :call every :milliseconds ms. First firing happens after the
interval has elapsed — not synchronously at load time — so a config
file can put `every` blocks anywhere relative to the topology they
reference.

Optional :args is a list passed as positional arguments to the
callback on every firing — `(every :call 'fire :args (list 1001))`
calls `(fire 1001)` each tick, saving a closing lambda. Defaults
to no extra args.

The handle is pushed onto `active-timers` so reset-state can
cancel it on reload."
  (let* ((ms (plist-get plist :milliseconds))
         (func (plist-get plist :call))
         (args (plist-get plist :args))
         (secs (/ ms 1000.0)))
    (setq active-timers
          (cons (apply 'run-with-timer secs secs func args)
                active-timers))))

;; -----------------------------------------------------------------------------
;; UI override file loader
;; -----------------------------------------------------------------------------

(defun overrides-path ()
  "Path of the per-microgrid UI overrides file, relative to the
config's load directory. Mirrors what the UI's /api/persist endpoint
writes to. Reads `(current-microgrid-id)`, which inside a
make-microgrid `:topology` lambda resolves to the entry being built.
The file sits next to the per-mg config under microgrids/."
  (format "microgrids/config.%d.overrides.lisp" (current-microgrid-id)))

(defun load-overrides ()
  "Load the persisted UI overrides for this microgrid if they exist.
No-op on a fresh checkout. Call from inside a make-microgrid
:topology lambda so the load happens with the per-mg current-microgrid
context active."
  (let ((path (overrides-path)))
    (when (file-exists-p path)
      (load path))))

;; -----------------------------------------------------------------------------
;; Scenario helpers
;; -----------------------------------------------------------------------------

(defun scenario-end-after (minutes)
  "Schedule a single-shot timer that runs (scenario-stop) after
MINUTES wall-clock MINUTES (not seconds — most other DSL ops are
seconds or milliseconds; this one is minutes because the use case
is fixed-duration runs sized in minutes, e.g.
`(scenario-end-after 60)` for a one-hour cap). The handle goes
through `every`'s tracker so a reload cancels it."
  (let ((secs (* minutes 60.0)))
    (setq active-timers
          (cons (run-with-timer secs nil 'scenario-stop)
                active-timers))))
