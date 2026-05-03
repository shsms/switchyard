;; scenarios/example.lisp — a 30-minute demo scenario.
;;
;; Run from the REPL or chain into config.lisp:
;;
;;   (load "scenarios/example.lisp")
;;
;; The scenario starts on load. Watch progress in the UI's "Report"
;; side panel, or curl the JSON endpoints:
;;
;;   curl -s http://127.0.0.1:8801/api/scenario          ;; lifecycle
;;   curl -s http://127.0.0.1:8801/api/scenario/events   ;; journal
;;   curl -s http://127.0.0.1:8801/api/scenario/report   ;; metrics
;;
;; Component ids referenced below match the sample config.lisp:
;;
;;   id 2    main meter (flagged :main t for peak tracking)
;;   id 100  hidden consumer meter (driven by consumer-curve)
;;   id 200  solar inverter
;;   id 1003 / 1006 batteries (auto-allocated; verify with `swctl tree`)

;; Random helpers — random-pick / random-uniform / random-outage.
(load "sim/scenarios.lisp")

;; ── Lifecycle ──────────────────────────────────────────────────
(scenario-start "example-30min")

;; Cap the run at 30 wall-clock minutes; (scenario-stop) fires
;; automatically and freezes elapsed + every metric accumulator.
(scenario-end-after 30)

;; ── Consumer load: end-of-window spike ─────────────────────────
;; Replaces the gentler config.lisp `consumer-curve` with a sharper
;; profile: 5 kW base for the first 13 minutes of every 15-minute
;; window, then a 25 kW spike for the last 100 seconds. This is the
;; classic "demand peak right before the billing window closes"
;; stress case.
(every
 :milliseconds 1000
 :call
 (lambda ()
   (let* ((rel (window-elapsed 900.0))
          (base 5000.0)
          (spike (if (> rel 800.0) 25000.0 0.0)))
     (set-meter-power 100 (+ base spike)))))

;; ── PV cloud cover ─────────────────────────────────────────────
;; Drop sunlight to 30 % at minute 10, restore at minute 15. The
;; solar inverter's `min-avail` clamp picks up each new sunlight%
;; on the next physics tick — observable as a visible drop in
;; available generation on the Report panel.
(run-with-timer 600.0 nil
                (lambda ()
                  (set-solar-sunlight 200 30.0)
                  (scenario-event 'cloud "covered until minute 15")))
(run-with-timer 900.0 nil
                (lambda ()
                  (set-solar-sunlight 200 100.0)
                  (scenario-event 'cloud "cleared")))

;; ── Random battery outages ─────────────────────────────────────
;; Pick a random battery from the list, knock its health to 'error
;; for 60-180 s, repeat with 5-10 minute gaps. Each transition
;; lands as a journal event so the Report panel's event log shows
;; what happened when.
;;
;; Replace the id list with your actual battery ids — `swctl tree`
;; or the topology JSON (/api/topology) is the easiest way to look
;; them up.
(random-outage '(1003)
               :min-every 300.0
               :max-every 600.0
               :min-duration 60.0
               :max-duration 180.0
               :kind 'error)

;; ── Silent-but-operational solar inverter at minute 5 ─────────
;; Models a flaky network: the inverter keeps producing power and
;; the physics keeps simulating, but its telemetry stream goes
;; quiet and SetPower requests time out. Useful for testing
;; downstream apps that need to handle stale-streaming sources.
(run-with-timer 300.0 nil
                (lambda ()
                  (set-component-telemetry-mode 200 'silent)
                  (set-component-command-mode 200 'timeout)
                  (scenario-event 'silent "solar 200 stopped streaming")))

;; ── Optional: per-component CSV recording ──────────────────────
;; Uncomment to drop one CSV per registered component into ./csvs/
;; at the 1 Hz history-sampler cadence. (scenario-stop) flushes
;; and closes the files automatically.
;;
;; (scenario-record-csv "csvs")

(scenario-event 'note "example-30min armed")
