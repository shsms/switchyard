;; Local emacs settings for switchyard. Configures etags-regen so
;; `M-x etags-regen-mode` picks up the right command. The mode is
;; NOT auto-enabled here — run it manually when you want a fresh
;; TAGS file. Nothing deletes the old TAGS or hooks into save.
;;
;; `etags-regen-program` is flagged risky in emacs (it names an
;; executable), so the assoc-style `(var . value)` pattern would
;; prompt on every visit even after `!`. Going through `(eval . …)`
;; with `setq-local` sidesteps the risky-variable check; emacs
;; prompts once per session and `!` persists the eval form to
;; `safe-local-eval-forms` in your init file.

((lisp-mode
  . ((eval . (progn
               (setq-local etags-regen-program "cargo")
               (setq-local etags-regen-program-options
                           '("run" "-q" "--bin" "switchyard-etags"
                             "config.lisp" "scenarios/example.lisp"))
               (setq-local etags-regen-file-extensions
                           '("lisp" "rs" "el")))))))
