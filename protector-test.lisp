;; Standalone switchyard config for exercising the GCP active-power
;; limiter's live-capacity sizing. One microgrid (id 2300) on the
;; default gRPC port 8800. Run with:
;;
;;   cargo run --bin switchyard protector-test.lisp
;;
;; Two -30 kW PV inverters (ids 200, 201) at full sun produce -60 kW,
;; breaching the GCP -20 kW feed-in limit, so the limiter engages
;; immediately. A battery (auto-id) is present so the limiter can
;; subscribe to the battery pool too.

;; Run from the switchyard repo root:  cargo run --bin switchyard protector-test.lisp
(unless (boundp 'switchyard-loaded)
  (setq switchyard-loaded t)
  (load "sim/common.lisp"))
(load "sim/defaults.lisp")
(load "sim/scenarios.lisp")

(reset-state)

(set-physics-tick-ms 100)
(set-enterprise-id 1)
(set-frequency-model :nominal 50.0 :mean-rev-rate 0.05 :sigma 0.015)

(make-microgrid
 :id 2300
 :name "Protector test"
 :grpc-port 8800
 :tso "TN"
 :topology
 (lambda ()
   (make-grid-connection-point
    :id 1
    :rated-lower -20000.0          ;; feed-in limit -20 kW
    :rated-upper  50000.0          ;; consumption limit +50 kW
    :successors
    (list
     (make-meter
      :id 2 :main t
      :successors
      (list
       ;; Two PV inverters, -30 kW rated each.
       (make-meter
        :successors (list (make-solar-inverter :id 200 :rated-lower -30000.0 :sunlight% 100.0)))
       (make-meter
        :successors (list (make-solar-inverter :id 201 :rated-lower -30000.0 :sunlight% 100.0)))
       ;; Battery branch (auto-allocated inverter + battery ids).
       (make-meter
        :successors (list (make-battery-inverter
                           :successors (list (make-battery :initial-soc 50.0)))))))))))
