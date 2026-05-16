//! `(watch-file PATH)` — register a path with the live-reload
//! notify watcher.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use tulisp::{Error, TulispContext};

/// Register `(watch-file PATH)`. Adds PATH (resolved relative to
/// the entry-point config's directory) to the set of files notify
/// watches; edits to any of them trigger the same reload as edits to
/// the entry-point config.
///
/// One-shot semantics: paths are collected during the initial config
/// eval and handed to the notify watcher in `Config::watch`. New
/// `(watch-file …)` calls during a hot-reload accumulate but won't
/// be honoured until the process restarts.
pub(in crate::lisp) fn register(
    ctx: &mut TulispContext,
    load_dir: PathBuf,
    extra_watches: Arc<Mutex<HashSet<PathBuf>>>,
) {
    ctx.defun("watch-file", move |path: String| -> Result<bool, Error> {
        let p = Path::new(&path);
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else {
            load_dir.join(p)
        };
        // Canonicalize so dedup works regardless of how the user
        // wrote the path. Failing canonicalize == file doesn't
        // exist; surface that as an error so a typo doesn't
        // silently no-op.
        let canonical = resolved.canonicalize().map_err(|e| {
            Error::invalid_argument(format!(
                "watch-file {}: {} ({})",
                resolved.display(),
                e,
                "file does not exist or is unreadable"
            ))
        })?;
        extra_watches.lock().insert(canonical);
        Ok(true)
    });
}
