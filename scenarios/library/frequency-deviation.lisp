;; Grid frequency steps ±100 mHz across the day, then releases back
;; to the OU drift. Exercises a frequency-aware control app's
;; deadband + ramp-rate logic. Pin / release uses
;; `(set-frequency-override …)` so the driver stops integrating
;; while the scenario stage holds the value; the final stage
;; clears the override so the natural OU drift resumes.

(define-scenario
 :name "frequency-deviation"
 :description "Grid frequency held at ±100 mHz across the day, then released"
 :stages
 '((:name "low"
    :hour-from 0.0 :hour-to 4.0
    :on (lambda () (set-frequency-override 49.9)))
   (:name "nominal"
    :hour-from 4.0 :hour-to 8.0
    :on (lambda () (set-frequency-override 50.0)))
   (:name "high"
    :hour-from 8.0 :hour-to 12.0
    :on (lambda () (set-frequency-override 50.1)))
   (:name "back-to-nominal"
    :hour-from 12.0 :hour-to 24.0
    :on (lambda () (clear-frequency-override)))))
