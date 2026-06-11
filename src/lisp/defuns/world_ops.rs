//! Topology-mutation defuns: `(connect)`, `(disconnect)`,
//! `(remove-component)`, `(rename-component)`. The UI editor and
//! REPL both hit these to reshape the running site.

use tulisp::{Error, TulispContext, TulispObject};

use crate::sim::microgrids::SharedSiteRouter;

/// Mutation defuns the UI editor (and power-user REPL) call to
/// reshape the running MicrogridSite — remove a component, drop an edge,
/// rename for display.
///
/// Component arguments accept either a raw integer id or a
/// `ComponentHandle` (as returned by `make-*` calls), so paste
/// templates can pass bindings directly without an outer
/// `(component-id …)` wrapper.
pub(super) fn register(ctx: &mut TulispContext, router: SharedSiteRouter) {
    let r = router.clone();
    ctx.defun(
        "connect",
        move |parent: TulispObject, child: TulispObject| -> Result<bool, Error> {
            let parent = arg_to_component_id(&parent)?;
            let child = arg_to_component_id(&child)?;
            let w = r.site();
            if !w.connect(parent, child) {
                return Err(Error::invalid_argument(format!(
                    "connect {parent} -> {child} would create a cycle; \
                     power aggregation requires an acyclic topology"
                )));
            }
            Ok(true)
        },
    );
    let r = router.clone();
    ctx.defun(
        "remove-component",
        move |id: TulispObject| -> Result<bool, Error> {
            let id = arg_to_component_id(&id)?;
            Ok(r.site().remove_component(id))
        },
    );
    let r = router.clone();
    ctx.defun(
        "disconnect",
        move |parent: TulispObject, child: TulispObject| -> Result<bool, Error> {
            let parent = arg_to_component_id(&parent)?;
            let child = arg_to_component_id(&child)?;
            Ok(r.site().disconnect(parent, child))
        },
    );
    let r = router;
    ctx.defun(
        "rename-component",
        move |id: TulispObject, name: String| -> Result<bool, Error> {
            let id = arg_to_component_id(&id)?;
            r.site().rename(id, name);
            Ok(true)
        },
    );
}

/// Resolve a `connect` / `disconnect` / `remove-component` /
/// `rename-component` argument to a component id. Accepts a raw
/// integer (for REPL convenience) or a `ComponentHandle` (so pasted
/// `(let* ((m1 (make-…))) (connect m1 m2))` bodies don't need to
/// wrap each binding in `(component-id …)`).
fn arg_to_component_id(v: &TulispObject) -> Result<u64, Error> {
    use crate::sim::ComponentHandle;
    if let Ok(h) = ComponentHandle::try_from(v) {
        return Ok(h.id());
    }
    if let Ok(n) = v.as_int() {
        return Ok(n as u64);
    }
    Err(Error::type_mismatch(format!(
        "expected component id (integer) or handle, got {v}"
    )))
}
