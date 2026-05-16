//! Filesystem helpers exposed to Lisp: `(file-exists-p)` (used by
//! the override-file loader's `(load-overrides)` guard) and
//! `(load-microgrid-configs)` (walks the per-mg config dir at
//! boot).

use std::path::{Path, PathBuf};

use tulisp::{Error, TulispContext};

pub(super) fn register(ctx: &mut TulispContext, load_dir: PathBuf) {
    // Path resolution mirrors tulisp's `(load PATH)`: relative paths
    // are joined onto the config file's load dir, absolutes pass
    // through. `load-overrides` gates `(load <override-file>)` with
    // a `(file-exists-p …)` check; same base path keeps both calls
    // looking at the same file regardless of the process CWD.
    let exists_dir = load_dir.clone();
    ctx.defun("file-exists-p", move |path: String| -> bool {
        let p = Path::new(&path);
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else {
            exists_dir.join(p)
        };
        resolved.exists()
    });
    // Iterate `microgrids/config.*.lisp` under the config's load
    // dir (excluding `*.overrides.lisp`) and `(load …)` each in
    // sorted order. The top-level config.lisp calls this after
    // setting enterprise-wide defaults; each per-mg file carries
    // its own `(make-microgrid …)` form so runtime-created
    // microgrids land in their own file rather than the entry
    // config.
    ctx.defun(
        "load-microgrid-configs",
        move |ctx: &mut TulispContext| -> Result<bool, Error> {
            let dir = load_dir.join("microgrids");
            let entries = match std::fs::read_dir(&dir) {
                Ok(it) => it,
                // Missing dir is a clean no-op — single-mg setups that
                // haven't migrated yet just stay on the old shape.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
                Err(e) => {
                    return Err(Error::os_error(format!(
                        "load-microgrid-configs: reading {}: {e}",
                        dir.display()
                    )));
                }
            };
            let mut paths: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                        return false;
                    };
                    name.starts_with("config.")
                        && name.ends_with(".lisp")
                        && !name.ends_with(".overrides.lisp")
                })
                .collect();
            paths.sort();
            for path in paths {
                let escaped = path
                    .to_string_lossy()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"");
                ctx.eval_string(&format!("(load \"{escaped}\")"))?;
            }
            Ok(true)
        },
    );
}
