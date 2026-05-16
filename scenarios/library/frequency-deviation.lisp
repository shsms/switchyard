;; Grid frequency drifts ±100 mHz across the day. Exercises a
;; frequency-aware control app's deadband + ramp-rate logic.

(define-scenario
 :name "frequency-deviation"
 :description "Grid frequency drifts ±100 mHz"
 :stages
 '((:name "low"
    :hour-from 0.0 :hour-to 4.0
    :on (lambda () (set-frequency 49.9)))
   (:name "nominal"
    :hour-from 4.0 :hour-to 8.0
    :on (lambda () (set-frequency 50.0)))
   (:name "high"
    :hour-from 8.0 :hour-to 12.0
    :on (lambda () (set-frequency 50.1)))
   (:name "back-to-nominal"
    :hour-from 12.0 :hour-to 24.0
    :on (lambda () (set-frequency 50.0)))))
