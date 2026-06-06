//! Snapshot save / load on `Config`.
//!
//! A snapshot is a frozen copy of the per-microgrid overrides file.
//! `save_snapshot("name")` writes `snapshots/name.lisp`; `load_snapshot`
//! copies it back over the overrides file and triggers a reload.
//! Live physics state (mid-flight setpoints, current SoC, ramps)
//! isn't captured — the site re-spins from baseline once the
//! snapshotted topology is back in place.

use std::fs;
use std::path::{Path, PathBuf};

use super::Config;

impl Config {
    /// Directory snapshots are stored in: a `snapshots/` subdirectory
    /// next to the loaded config file. Lazily created on the first
    /// `save_snapshot` call.
    pub fn snapshots_dir(&self) -> PathBuf {
        let load_dir = Path::new(&self.filename)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        load_dir.join("snapshots")
    }

    /// Copy the current per-microgrid overrides file to
    /// `snapshots/<name>.lisp`. The snapshot is a frozen-in-time copy
    /// of the user's accumulated edits — replaying it (via
    /// `load_snapshot`) recovers the same dashboard / topology shape
    /// without re-running the manual click stream. Live physics state
    /// (ramps, mid-flight setpoints, current SoC, etc.) is NOT
    /// captured here — those derive from the snapshotted topology
    /// once the site re-spins from baseline.
    ///
    /// Returns the absolute path of the snapshot file on success.
    /// Errors if the name resolves to anything outside `snapshots/`
    /// (path-traversal guard) or if the overrides file isn't readable.
    pub fn save_snapshot(&self, name: &str) -> std::io::Result<PathBuf> {
        let dir = self.snapshots_dir();
        let dest = sanitise_snapshot_path(&dir, name)?;
        fs::create_dir_all(&dir)?;
        // Empty (no overrides for this mg yet) is a valid snapshot —
        // the user just hasn't edited anything. Treat a missing
        // resolvable scope the same way: write an empty snapshot so
        // load_snapshot can replay it. Reading a path that doesn't
        // exist falls through to the same empty-file write.
        match self.overrides_path() {
            Some(src) if src.exists() => {
                fs::copy(&src, &dest)?;
            }
            _ => {
                fs::write(&dest, "")?;
            }
        }
        Ok(dest)
    }

    /// Replace the current overrides file with `snapshots/<name>.lisp`
    /// and reload, so the site derives from base config.lisp +
    /// the snapshotted overrides.
    pub fn load_snapshot(&self, name: &str) -> Result<(), String> {
        let dir = self.snapshots_dir();
        let src = sanitise_snapshot_path(&dir, name)
            .map_err(|e| format!("invalid snapshot name: {e}"))?;
        if !src.exists() {
            return Err(format!("snapshot {name:?} not found"));
        }
        let dest = self
            .overrides_path()
            .ok_or_else(|| "no resolvable microgrid scope; can't pick a destination".to_string())?;
        // Atomic replace (temp + rename), mirroring the other
        // overrides-file rewrite paths — a copy interrupted midway
        // must not leave a truncated overrides file behind. The
        // microgrids/ dir may not exist yet if nothing was persisted
        // before the first snapshot load.
        if let Some(dir) = dest.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        let tmp = dest.with_extension("lisp.tmp");
        fs::copy(&src, &tmp).map_err(|e| format!("copy snapshot failed: {e}"))?;
        fs::rename(&tmp, &dest).map_err(|e| format!("replace overrides failed: {e}"))?;
        self.reload()
    }

    /// Names of every `*.lisp` file in `snapshots/`, sorted lex.
    pub fn list_snapshots(&self) -> Vec<String> {
        list_snapshots_in(&self.snapshots_dir())
    }
}

fn sanitise_snapshot_path(dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    // Reject anything that could escape the snapshots dir via `..`,
    // an absolute path, or path separators. We only accept a single
    // file-name component.
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.starts_with('.')
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid snapshot name {name:?}"),
        ));
    }
    Ok(dir.join(format!("{name}.lisp")))
}

fn list_snapshots_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("lisp") {
                return None;
            }
            p.file_stem().and_then(|s| s.to_str()).map(|s| s.to_owned())
        })
        .collect();
    out.sort();
    out
}
