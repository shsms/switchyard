//! `/api/defaults` — read every `*-defaults` plist out of the
//! running interpreter, pretty-printed for the side-panel editor.

use axum::{Json, extract::State};
use serde::Serialize;

use crate::lisp::Config;

/// One per `*-defaults` alist defined in `sim/defaults.lisp`. The
/// `var_name` is the actual Lisp variable; `value` is its current
/// printed form (a stringified alist), readable / editable as raw
/// Lisp by the UI.
#[derive(Serialize)]
pub(in crate::ui) struct DefaultsEntry {
    category: &'static str,
    var_name: String,
    value: String,
}

#[derive(Serialize)]
pub(in crate::ui) struct DefaultsResponse {
    entries: Vec<DefaultsEntry>,
}

/// Category names the defaults endpoint walks to fetch each
/// `*-defaults` alist out of the running interpreter. Order is
/// stable so the UI's defaults editor renders the same sections
/// every time. New component categories need to be added here AND
/// to the corresponding `(setq foo-defaults '((...)))` block in
/// `sim/defaults.lisp` (otherwise the endpoint silently drops the
/// new category — `eval_silent` on an unbound symbol fails and
/// the entry is skipped).
const DEFAULT_CATEGORIES: &[&str] = &[
    "grid",
    "meter",
    "battery",
    "battery-inverter",
    "solar-inverter",
    "ev-charger",
    "chp",
];

pub(in crate::ui) async fn defaults(State(config): State<Config>) -> Json<DefaultsResponse> {
    // Read each *-defaults variable via eval_silent so reading the
    // current state doesn't itself look like an edit. spawn_blocking
    // because eval acquires the std-RwLock-backed ctx.
    let entries = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        for cat in DEFAULT_CATEGORIES {
            let var = format!("{cat}-defaults");
            // Pretty-print via tulisp-fmt so the textarea shows
            // one (key . value) pair per line at a narrow width
            // — fits the side panel without horizontal scroll.
            // Falls back to the raw Display form if the printed
            // value isn't parseable (shouldn't happen for an alist
            // read back from the interpreter). Variables that
            // aren't bound just get skipped.
            if let Ok(value) = config.eval_silent(&var) {
                let formatted = tulisp_fmt::format_with_width(&value, 50)
                    .map(|f| f.trim_end().to_string())
                    .unwrap_or(value);
                out.push(DefaultsEntry {
                    category: cat,
                    var_name: var,
                    value: formatted,
                });
            }
        }
        out
    })
    .await
    .unwrap_or_default();
    Json(DefaultsResponse { entries })
}
