//! Per-microgrid override file plumbing on `Config`.
//!
//! Every successful `Config::eval` appends its source to
//! `microgrids/config.<id>.overrides.lisp` so the edit survives
//! a reload. The UI's overrides dialog reads the file via
//! `persisted_overrides`, prunes entries with
//! `remove_persisted_overrides`, and the canvas undo stack
//! snapshots the whole file via `overrides_text` /
//! `replace_overrides_text`.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::Serialize;

use super::Config;

/// One top-level form found in the per-microgrid override file. The
/// `idx` is the form's 0-based position; stable until the next
/// `remove_persisted_overrides` rewrites the file. `source` is the
/// form rendered via tulisp's `Display` impl — round-trips through
/// eval but doesn't preserve the original spelling (comments
/// stripped, whitespace normalized).
#[derive(Debug, Clone, Serialize)]
pub struct PersistedOverride {
    pub idx: usize,
    pub source: String,
}

impl Config {
    /// Evaluate `src` in the interpreter and, on success, append it
    /// to the persisted override file. `eval_string` returns the
    /// final form's value; we stringify it via `Display` and
    /// return the result formatted via `Display`. Errors are
    /// formatted with full trace context the same way the reload
    /// path's logger formats them.
    ///
    /// Synchronous — acquires the interpreter write lock for the
    /// duration of the eval. Callers in async contexts must wrap in
    /// `tokio::task::spawn_blocking` to keep the executor free.
    ///
    /// On success the source is appended to the per-microgrid
    /// override file (`config.ui-overrides.<id>.lisp`) so the
    /// edit survives a reload. Errored evals are skipped — a
    /// half-applied topology change shouldn't leave a re-erroring
    /// expression on disk. Either way the MicrogridSite version bumps so
    /// UI subscribers refetch.
    ///
    /// Append uses the source verbatim — no formatter pass — to
    /// keep the per-eval cost predictable. `remove_persisted_overrides`
    /// already runs `tulisp-fmt` over the file's surviving forms
    /// when it rewrites, so the file gets re-tidied whenever the
    /// user prunes the list from the UI.
    pub fn eval(&self, src: &str) -> Result<String, String> {
        let result = {
            let mut ctx = self.ctx.borrow_mut();
            match ctx.eval_string(src) {
                Ok(v) => Ok(v.to_string()),
                Err(e) => Err(e.format(&ctx)),
            }
        };
        if result.is_ok()
            && let Err(e) = self.append_to_overrides_file(src)
        {
            let label = self
                .overrides_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<no resolvable microgrid>".to_string());
            log::error!("Failed to append override to {label}: {e}");
        }
        // Bump the version on the microgrid the eval actually
        // mutated (the one current_microgrid points at, or — if no
        // scope was set — the router's fallback) so the WS event
        // pump fires TopologyChanged on the right bus. Without this
        // the bootstrap site's version moved, but UI sessions only
        // listen to per-mg buses.
        self.router.site().bump_version();
        result
    }

    /// Read-only eval — same machinery as `eval` but the result is
    /// NOT appended to the override file and the site version does
    /// NOT bump. For UI introspection (e.g. "what's the current
    /// value of battery-defaults?") that shouldn't surface as a
    /// persisted edit.
    pub fn eval_silent(&self, src: &str) -> Result<String, String> {
        let mut ctx = self.ctx.borrow_mut();
        match ctx.eval_string(src) {
            Ok(v) => Ok(v.to_string()),
            Err(e) => Err(e.format(&ctx)),
        }
    }

    fn append_to_overrides_file(&self, src: &str) -> std::io::Result<()> {
        let Some(path) = self.overrides_path() else {
            // No resolvable microgrid scope — nothing to persist
            // against. Boot path can't reach this; a future
            // `(reset-microgrid)`-then-eval flow would.
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no resolvable microgrid scope; can't persist override",
            ));
        };
        // Per-mg overrides live under `microgrids/`; the dir might not
        // exist yet on a fresh checkout. Create lazily on the first
        // write so the user doesn't have to seed it manually.
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        // Trailing blank line keeps multi-line `let*` paste shapes
        // visually separable from the next form a future eval
        // appends.
        writeln!(file, "{src}")?;
        writeln!(file)?;
        file.flush()
    }

    /// One entry per top-level form in the per-microgrid override
    /// file (`config.ui-overrides.<microgrid-id>.lisp`), parsed
    /// via `TulispContext::parse_file`. Returns an empty vec if
    /// the file is missing or malformed — load-overrides will
    /// surface a parse error on the next reload, so we don't bother
    /// propagating it here.
    pub fn persisted_overrides(&self) -> Vec<PersistedOverride> {
        let Some(path) = self.overrides_path() else {
            return Vec::new();
        };
        if !path.exists() {
            return Vec::new();
        }
        let path_str = path.to_string_lossy();
        let mut ctx = self.ctx.borrow_mut();
        let Ok(forms) = ctx.parse_file(&path_str) else {
            return Vec::new();
        };
        forms
            .base_iter()
            .enumerate()
            .map(|(idx, form)| PersistedOverride {
                idx,
                source: form.to_string(),
            })
            .collect()
    }

    pub fn persisted_count(&self) -> usize {
        self.persisted_overrides().len()
    }

    /// Drop a set of persisted-override entries (by their
    /// file-position idx) and re-derive MicrogridSite state. Atomic: the
    /// override file is rewritten without those forms (temp +
    /// rename, with a `tulisp-fmt` pretty-print pass over the
    /// surviving forms), then `reload()` re-runs config.lisp +
    /// `load-overrides` on the new file so the deleted forms'
    /// effects vanish via the MicrogridSite reset inside reload.
    ///
    /// Returns the count of forms actually dropped — out-of-range
    /// indices are silently ignored. An IO error during rewrite
    /// leaves the site state untouched (the file was renamed
    /// atomically only on success).
    ///
    /// Bulk shape so the UI's checkbox-toolbar can prune N entries
    /// in one round trip with one reload, instead of N round trips
    /// with N reloads.
    pub fn remove_persisted_overrides(&self, indices: &[usize]) -> std::io::Result<usize> {
        let drop: HashSet<usize> = indices.iter().copied().collect();
        let entries = self.persisted_overrides();
        let kept: Vec<String> = entries
            .iter()
            .filter(|o| !drop.contains(&o.idx))
            .map(|o| o.source.clone())
            .collect();
        let dropped = entries.len() - kept.len();
        if dropped == 0 {
            return Ok(0);
        }
        let Some(path) = self.overrides_path() else {
            // persisted_overrides() returned entries above, so the
            // path was resolvable then; reach here only if the
            // current-microgrid pointer flipped to None in between.
            // Bail rather than touch the filesystem with a nonsense
            // path.
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no resolvable microgrid scope; can't rewrite overrides",
            ));
        };
        let tmp = path.with_extension("lisp.tmp");
        {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)?;
            writeln!(file, ";; ── {} ──", Utc::now().to_rfc3339())?;
            writeln!(file)?;
            // Hand each surviving form to tulisp-fmt so the file
            // stays readable. format_with_width returns the same
            // source on failure; we fall back to the raw text
            // rather than dropping a form. Blank line between
            // forms keeps multi-line `let*` paste shapes visually
            // separable.
            for src in &kept {
                let fmt =
                    tulisp_fmt::format_with_width(src, 80).unwrap_or_else(|_| format!("{src}\n"));
                file.write_all(fmt.as_bytes())?;
                writeln!(file)?;
            }
            file.flush()?;
        }
        fs::rename(&tmp, &path)?;
        // A reload error after a successful rewrite leaves the file
        // on disk and the site reset to empty — the next save
        // (or a manual `reload`) is the recovery path. Surface the
        // error as IO so the HTTP handler can return 5xx; the
        // user's already lost the broken forms either way.
        if let Err(msg) = self.reload() {
            return Err(std::io::Error::other(format!(
                "reload after rewrite failed: {msg}"
            )));
        }
        Ok(dropped)
    }

    /// Read the raw text of the active microgrid's overrides file.
    /// Empty string when the file doesn't exist yet (no edits have
    /// been persisted) or the scope can't resolve. Used by the
    /// canvas-undo handler to snapshot state before each mutation.
    pub fn overrides_text(&self) -> std::io::Result<String> {
        let Some(path) = self.overrides_path() else {
            return Ok(String::new());
        };
        match fs::read_to_string(&path) {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e),
        }
    }

    /// Replace the overrides file with `content` and reload. The
    /// canvas-undo handler restores a snapshot of the file taken
    /// before a mutation; redo replays the snapshot taken after.
    /// Atomic rewrite (temp + rename) so an interruption mid-write
    /// can't corrupt the file.
    pub fn replace_overrides_text(&self, content: &str) -> std::io::Result<()> {
        let Some(path) = self.overrides_path() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no resolvable microgrid scope; can't rewrite overrides",
            ));
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("lisp.tmp");
        {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)?;
            file.write_all(content.as_bytes())?;
            file.flush()?;
        }
        fs::rename(&tmp, &path)?;
        if let Err(msg) = self.reload() {
            return Err(std::io::Error::other(format!(
                "reload after override-text replace failed: {msg}"
            )));
        }
        Ok(())
    }

    /// Resolve the per-microgrid overrides file path. Keyed off the
    /// active microgrid id (set by /api/mg/{id}/eval and the
    /// scenarios per-mg replay), falling back to the first registry
    /// entry when nothing's selected.
    ///
    /// Returns `None` when neither source resolves — current is
    /// `None` AND the registry is empty. The boot path can't reach
    /// that case (`Config::new` rejects an empty registry), but
    /// guarding against it here keeps a future `(reset-microgrid)`-
    /// then-eval flow from writing to a meaningless
    /// `config.0.overrides.lisp`.
    pub(super) fn overrides_path(&self) -> Option<PathBuf> {
        let load_dir = Path::new(&self.filename)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let mg_id = self
            .current_microgrid
            .read()
            .or_else(|| self.microgrids.lock().keys().next().copied())?;
        Some(
            load_dir
                .join("microgrids")
                .join(format!("config.{mg_id}.overrides.lisp")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::config_with;

    /// Every successful eval appends to the override file
    /// immediately — that's how an edit survives a reload (the
    /// override file is the source of truth, not an in-memory log).
    #[test]
    fn eval_appends_each_successful_form_to_override_file() {
        let (cfg, dir) = config_with("(set-microgrid-id 9) (%make-grid-connection-point :id 1)");
        cfg.eval("(rename-component 1 \"a\")").unwrap();
        cfg.eval("(rename-component 1 \"b\")").unwrap();
        let path = dir.join("microgrids/config.9.overrides.lisp");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("(rename-component 1 \"a\")"));
        assert!(body.contains("(rename-component 1 \"b\")"));
        // Errored eval doesn't land in the file.
        assert!(cfg.eval("(undefined-fn 1)").is_err());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(!body.contains("undefined-fn"));
    }
}
