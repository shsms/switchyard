;; switchyard per-category defaults + bare-name shorthand DSL.
;;
;; Loaded by config.lisp; edits to either half re-trigger the sim
;; reload (config.lisp registers this file via `(watch-file …)`).
;;
;; Two halves:
;;
;;   1. `*-defaults` alists hold the category-wide knobs. Edit a
;;      value here to retune every component of that category in one
;;      place. Per-component plist args still win on each call.
;;
;;   2. Bare-name wrappers (`grid`, `meter`, `battery`, …) are thin
;;      `defuns` that call the matching `make-*` with `:config <cat>-
;;      defaults` prepended. The `make-*` Rust primitives are still
;;      available for callers that want zero defaults.

;; -----------------------------------------------------------------------------
;; Per-category defaults
;; -----------------------------------------------------------------------------

(setq grid-defaults
      '((rated-fuse-current . 100)
        (stream-jitter-pct  . 1.0)))

(setq meter-defaults
      '((interval          . 200)
        (stream-jitter-pct . 4.0)))

(setq battery-defaults
      '((soc-protect-margin . 10.0)
        (stream-jitter-pct  . 8.0)
        (health             . ok)))

(setq battery-inverter-defaults
      '((command-delay-ms     . 1500)
        (ramp-rate            . 5000.0)
        (stream-jitter-pct    . 8.0)
        (reactive-pf-limit    . 0.0)        ;; 0 = disabled
        (reactive-apparent-va . 32000.0)))  ;; kVA-circle envelope

(setq solar-inverter-defaults
      '((ramp-rate         . 2000.0)
        (stream-jitter-pct . 5.0)))

(setq ev-charger-defaults
      '((soc-protect-margin . 10.0)
        (command-delay-ms   .   500)
        (ramp-rate          . 3000.0)
        (stream-jitter-pct  .   10.0)))

(setq chp-defaults
      '((stream-jitter-pct . 0.0)))

;; -----------------------------------------------------------------------------
;; Bare-name shorthand wrappers
;; -----------------------------------------------------------------------------
;;
;; Each wrapper prepends `:config <cat>-defaults` to the user's plist
;; before calling the underlying make-*. Per-component plist keys
;; appear after the wrapper's `:config` key, so AsPlist!'s last-wins
;; key resolution lets the per-component plist override category
;; defaults. To swap defaults entirely for one call, pass another
;; `:config` key — same last-wins rule:
;;
;;   (battery :id 100 :config special-battery-defaults)

(defun grid             (&rest p) (apply 'make-grid             :config grid-defaults             p))
(defun meter            (&rest p) (apply 'make-meter            :config meter-defaults            p))
(defun battery          (&rest p) (apply 'make-battery          :config battery-defaults          p))
(defun battery-inverter (&rest p) (apply 'make-battery-inverter :config battery-inverter-defaults p))
(defun solar-inverter   (&rest p) (apply 'make-solar-inverter   :config solar-inverter-defaults   p))
(defun ev-charger       (&rest p) (apply 'make-ev-charger       :config ev-charger-defaults       p))
(defun chp              (&rest p) (apply 'make-chp              :config chp-defaults              p))
