//! Enterprise-scoped metadata setters: `(set-enterprise-id)`,
//! `(set-assets-socket-addr)`, `(set-dispatch-socket-addr)`,
//! `(set-default-request-lifetime-ms)`.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tulisp::{Error, TulispContext};

use super::super::Metadata;

pub(super) fn register(ctx: &mut TulispContext, metadata: Arc<RwLock<Metadata>>) {
    let m = metadata.clone();
    ctx.defun("set-enterprise-id", move |id: i64| -> Result<bool, Error> {
        m.write().enterprise_id = id as u64;
        Ok(true)
    });
    let m = metadata.clone();
    ctx.defun(
        "set-assets-socket-addr",
        move |addr: String| -> Result<bool, Error> {
            m.write().assets_socket_addr = addr;
            Ok(true)
        },
    );
    let m = metadata.clone();
    ctx.defun(
        "set-dispatch-socket-addr",
        move |addr: String| -> Result<bool, Error> {
            m.write().dispatch_socket_addr = addr;
            Ok(true)
        },
    );
    ctx.defun(
        "set-default-request-lifetime-ms",
        move |ms: i64| -> Result<bool, Error> {
            metadata.write().default_request_lifetime = Duration::from_millis(ms.max(0) as u64);
            Ok(true)
        },
    );
}
