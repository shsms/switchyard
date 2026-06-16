;; Grid frequency leans toward ±100 mHz, then releases back to the base
;; OU drift. Exercises a frequency-aware control app's deadband +
;; ramp-rate logic.
;;
;; Each cue shifts the OU process's nominal via
;; `(override-frequency-model :nominal …)` — the driver keeps
;; integrating and noise stays on, so the trace reads like a real grid
;; leaning toward the new operating point rather than snapping to a
;; constant. The final cue drops the override so the natural drift
;; around 50 Hz resumes.
;;
;; A relative demo: one lean a minute.

(define-scenario
 :name "frequency-deviation"
 :description "Grid frequency leans ±100 mHz, then released"
 :schedule 'relative
 :length "4min"
 :setup (lambda () (override-frequency-model :nominal 49.9))
 :cues (list
        (at "60s" (lambda () (override-frequency-model :nominal 50.0)))
        (at "120s" (lambda () (override-frequency-model :nominal 50.1)))
        (at "180s" (lambda () (clear-frequency-override)))))
