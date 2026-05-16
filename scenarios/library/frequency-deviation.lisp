;; Grid frequency drifts toward ±100 mHz across the day, then
;; releases back to the base OU drift. Exercises a frequency-aware
;; control app's deadband + ramp-rate logic.
;;
;; Each stage shifts the OU process's nominal via
;; `(override-frequency-model :nominal …)` — the driver keeps
;; integrating and noise stays on, so the trace reads like a real
;; grid leaning toward the new operating point rather than snapping
;; to a constant. The final stage drops the override so the
;; natural drift around 50 Hz resumes.

(define-scenario
 :name "frequency-deviation"
 :description "Grid frequency leans ±100 mHz across the day, then released"
 :stages
 '((:name "low"
    :hour-from 0.0 :hour-to 4.0
    :on (lambda () (override-frequency-model :nominal 49.9)))
   (:name "nominal"
    :hour-from 4.0 :hour-to 8.0
    :on (lambda () (override-frequency-model :nominal 50.0)))
   (:name "high"
    :hour-from 8.0 :hour-to 12.0
    :on (lambda () (override-frequency-model :nominal 50.1)))
   (:name "back-to-nominal"
    :hour-from 12.0 :hour-to 24.0
    :on (lambda () (clear-frequency-override)))))
