//! Shared test fixtures for the lisp/ subtree. Every child module's
//! `#[cfg(test)] mod tests` block builds its `Config` instances
//! through `config_with`, which seeds a unique temp dir + auto-
//! wraps the test body in a `(make-microgrid …)` form so callers
//! don't have to repeat the boilerplate.

use std::sync::atomic::{AtomicU64, Ordering};

use super::Config;

static UNIQ: AtomicU64 = AtomicU64::new(0);

/// Build a Config from a tiny config.lisp body in a unique temp
/// dir; returns the Config + the dir so tests can mess with the
/// per-microgrid override path.
pub(super) fn config_with(body: &str) -> (Config, std::path::PathBuf) {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "switchyard-cfg-{}-{}",
        std::process::id(),
        UNIQ.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.lisp");
    let wrapped = wrap_test_body(body);
    std::fs::write(&path, wrapped).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let cfg = rt
        .block_on(async { Config::new(path.to_str().unwrap()) })
        .expect("config eval");
    // Drop the runtime — Config keeps its own handles to whatever
    // tulisp-async spawned during init.
    std::mem::forget(rt);
    (cfg, dir)
}

/// Auto-wrap a test body in `(make-microgrid …)` if the body doesn't
/// already register one — every config must do so post-migration, but
/// most tests don't care about the wrapper and just want their forms
/// evaluated in a microgrid scope. Tests that exercise make-microgrid
/// itself supply their own form and the wrapper is skipped.
///
/// Inline `(set-microgrid-id N)` from the pre-migration shape gets
/// stripped and its N seeds the wrapper's :id so per-mg id
/// assertions keep their original target values.
pub(super) fn wrap_test_body(body: &str) -> String {
    if body.contains("make-microgrid") {
        return body.to_string();
    }
    let (stripped, mg_id) = strip_set_microgrid_id(body);
    let inner = if stripped.trim().is_empty() {
        "nil".to_string()
    } else {
        stripped
    };
    format!("(make-microgrid :id {mg_id} :grpc-port 8800 :topology (lambda () {inner}))")
}

fn strip_set_microgrid_id(body: &str) -> (String, u64) {
    let needle = "(set-microgrid-id ";
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    let mut mg_id: u64 = 2200;
    while let Some(idx) = rest.find(needle) {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx + needle.len()..];
        if let Some(close) = tail.find(')') {
            let n_str = tail[..close].trim();
            if let Ok(v) = n_str.parse::<u64>() {
                mg_id = v;
            }
            rest = &tail[close + 1..];
        } else {
            out.push_str(&rest[idx..]);
            return (out, mg_id);
        }
    }
    out.push_str(rest);
    (out, mg_id)
}

/// Counter for tests that need their own unique temp dir without
/// going through `config_with`.
pub(super) fn next_unique() -> u64 {
    UNIQ.fetch_add(1, Ordering::Relaxed)
}
