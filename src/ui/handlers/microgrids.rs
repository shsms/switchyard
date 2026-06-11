//! `/api/microgrids` — list every registered microgrid + the
//! create endpoint that allocates a fresh id + port and notifies
//! the binary's registered-microgrid listener to boot the runtime.

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};

use crate::lisp::Config;

pub(in crate::ui) async fn microgrids_list(
    State(config): State<Config>,
) -> Json<Vec<crate::sim::microgrids::MicrogridView>> {
    Json(crate::sim::microgrids::snapshot(&config.microgrids()))
}

#[derive(Deserialize)]
pub(in crate::ui) struct CreateMicrogridBody {
    name: String,
    #[serde(default)]
    tso: Option<String>,
}

#[derive(Serialize)]
pub(in crate::ui) struct CreateMicrogridResp {
    id: u64,
    name: String,
    grpc_port: u16,
    tso: Option<String>,
}

/// POST /api/microgrids/create — auto-allocates id + grpc_port,
/// inserts a fresh entry in the registry, and broadcasts a
/// registered-microgrid notification. The binary's listener (see
/// `bin/switchyard.rs`) reacts by booting the runtime — physics +
/// history + Microgrid gRPC server + loopback client — so there is
/// exactly one spawn path shared with runtime `(make-microgrid …)`
/// evals, and no path can double-boot a runtime.
///
/// Empty-name requests are rejected. The new microgrid's site is
/// constructed with the shared enterprise id allocator so its
/// auto-allocated component ids stay globally unique.
pub(in crate::ui) async fn microgrids_create(
    State(config): State<Config>,
    Json(body): Json<CreateMicrogridBody>,
) -> Result<Json<CreateMicrogridResp>, (StatusCode, String)> {
    use crate::sim::microgrids::{
        MicrogridDef, MicrogridEntry, next_free_id_in, next_free_port_in,
    };
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name must be non-empty".into()));
    }
    let registry = config.microgrids();
    let site = crate::sim::MicrogridSite::with_id_allocator(config.enterprise_id_allocator());
    // Allocate id + port AND insert the entry under one lock so
    // concurrent creates can't pick the same port (the earlier
    // shape probed both before locking; two simultaneous calls
    // could land on the same grpc_port and the second tonic
    // listener would fail to bind silently inside its tokio task).
    let (id, grpc_port, def) = {
        let mut r = registry.lock();
        let id = next_free_id_in(&r);
        let grpc_port = next_free_port_in(&r);
        let def = MicrogridDef {
            id,
            name: name.clone(),
            grpc_port,
            tso: body.tso.clone(),
        };
        r.insert(
            id,
            MicrogridEntry {
                def: def.clone(),
                site: site.clone(),
            },
        );
        (id, grpc_port, def)
    };
    // Persist the per-mg config stub BEFORE spawning the runtime.
    // If the write fails the next boot would orphan the live tasks
    // (gRPC server, loopback, physics, history sampler) since the
    // stub is what re-creates the microgrid at load-time. Rolling
    // back the registry insert + bailing out keeps the failure
    // mode clean: nothing started, nothing leaked.
    if let Err(e) = write_microgrid_stub(&config, id, &name, grpc_port, body.tso.as_deref()) {
        registry.lock().remove(&id);
        return Err((StatusCode::INTERNAL_SERVER_ERROR, e));
    }
    // Notify enterprise-wide subscribers: the binary's listener boots
    // the runtime (physics + history + gRPC server + loopback), and
    // the WS event pump starts forwarding topology_changed / sample
    // events to live UI sessions. The registry insert + stub write
    // above both happen before this, so the listener's lookup finds
    // the entry. Test fixtures run no listener — the entry simply
    // gets no runtime, same as the old no-op spawner.
    config.notify_microgrid_registered(id);
    Ok(Json(CreateMicrogridResp {
        id,
        name: def.name,
        grpc_port,
        tso: def.tso,
    }))
}

/// Write `microgrids/config.<id>.lisp` for a runtime-created entry.
/// The stub carries a `(make-microgrid …)` form pinned to this id /
/// port / tso, plus an empty `:topology` lambda that just
/// `(load-overrides)`s — the UI populates the topology over time by
/// appending to the per-mg overrides file next to this stub. Errors
/// out instead of clobbering an existing file (concurrent creates
/// shouldn't fight over the same path, but the registry already
/// dedups by id so this is just paranoia).
fn write_microgrid_stub(
    config: &Config,
    id: u64,
    name: &str,
    grpc_port: u16,
    tso: Option<&str>,
) -> Result<(), String> {
    let dir = config.microgrids_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join(format!("config.{id}.lisp"));
    if path.exists() {
        return Err(format!(
            "stub file {} already exists; refusing to clobber",
            path.display()
        ));
    }
    // Escape only " and \ inside the name string. The TSO is one of
    // the four short codes ("TN" / "AM" / "HZ" / "BW") or unset, so
    // the same escape rule covers it.
    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }
    let tso_form = match tso {
        Some(t) if !t.is_empty() => format!(" :tso \"{}\"", esc(t)),
        _ => String::new(),
    };
    let content = format!(
        ";; Runtime-created microgrid (id {id}). Edit by hand or via\n\
         ;; the UI — UI edits land in config.{id}.overrides.lisp next\n\
         ;; to this file.\n\
         \n\
         (make-microgrid\n\
        \x20:id {id}\n\
        \x20:name \"{name_esc}\"\n\
        \x20:grpc-port {grpc_port}{tso_form}\n\
        \x20:topology\n\
        \x20(lambda ()\n\
        \x20  (load-overrides)))\n",
        name_esc = esc(name),
    );
    std::fs::write(&path, content).map_err(|e| format!("write {}: {e}", path.display()))
}
