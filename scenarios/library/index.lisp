;; Index of starter scenarios shipped with switchyard. config.lisp
;; loads this file at the end of boot so the seven canned
;; scenarios appear in the Scenarios mode dropdown on a fresh
;; checkout. Comment out individual loads if a sample microgrid
;; doesn't carry the component ids the scenario expects.
;;
;; The scenarios use the section wrappers (at / drive-*), so pull in
;; the scenario DSL first.
(load "sim/scenarios.lisp")

(load "scenarios/library/peak-evening.lisp")
(load "scenarios/library/pv-dropout.lisp")
(load "scenarios/library/battery-degraded.lisp")
(load "scenarios/library/flaky-network.lisp")
(load "scenarios/library/frequency-deviation.lisp")
(load "scenarios/library/cold-start.lisp")
(load "scenarios/library/off-grid-island.lisp")
