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
(unless (boundp 'active-timers)
  (setq active-timers nil))

(defun reset-state ()
  "Cancel every timer registered via `every` / `run-with-timer-tracked`,
then wipe the World registry. Call this at the top of your config so a
hot-reload starts from a clean slate."
  (dolist (tm active-timers)
    (cancel-timer tm))
  (setq active-timers nil)
  (world-reset))

;; -----------------------------------------------------------------------------
;; Periodic helper
;; -----------------------------------------------------------------------------

(defun every (&rest plist)
  "Call :call every :milliseconds ms. First firing happens after the
interval has elapsed — not synchronously at load time — so a config
file can put `every` blocks anywhere relative to the topology they
reference. The handle is pushed onto `active-timers` so reset-state
can cancel it on reload."
  (let* ((ms (plist-get plist :milliseconds))
         (func (plist-get plist :call))
         (secs (/ ms 1000.0)))
    (setq active-timers
          (cons (run-with-timer secs secs func) active-timers))))
