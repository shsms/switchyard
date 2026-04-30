//! Bridge `ComponentHandle` ↔ `Shared<dyn TulispAny>` so handles round-
//! trip through Lisp without losing their concrete type.
//!
//! `AsPlist!` requires both directions of conversion via the standard
//! `TryFrom<TulispObject>` / `From<T> for TulispObject` traits, so we
//! implement those in addition to `TulispConvertible` (the latter is
//! used by direct `defun` parameters).

use tulisp::{Error, Shared, TulispContext, TulispConvertible, TulispObject};

use crate::sim::ComponentHandle;

impl TulispConvertible for ComponentHandle {
    fn from_tulisp(value: &TulispObject) -> Result<Self, Error> {
        let any = value
            .as_any()
            .map_err(|e| e.with_trace(value.clone()))?;
        any.downcast_ref::<ComponentHandle>()
            .cloned()
            .ok_or_else(|| {
                Error::type_mismatch(format!("Expected ComponentHandle, got {value}"))
            })
    }
    fn into_tulisp(self) -> TulispObject {
        Shared::new(self).into()
    }
}

impl TryFrom<TulispObject> for ComponentHandle {
    type Error = Error;
    fn try_from(value: TulispObject) -> Result<Self, Self::Error> {
        let any = value.as_any().map_err(|e| e.with_trace(value.clone()))?;
        any.downcast_ref::<ComponentHandle>()
            .cloned()
            .ok_or_else(|| {
                Error::type_mismatch(format!("Expected ComponentHandle, got {value}"))
            })
    }
}

impl TryFrom<&TulispObject> for ComponentHandle {
    type Error = Error;
    fn try_from(value: &TulispObject) -> Result<Self, Self::Error> {
        let any = value.as_any().map_err(|e| e.with_trace(value.clone()))?;
        any.downcast_ref::<ComponentHandle>()
            .cloned()
            .ok_or_else(|| {
                Error::type_mismatch(format!("Expected ComponentHandle, got {value}"))
            })
    }
}

impl From<ComponentHandle> for TulispObject {
    fn from(h: ComponentHandle) -> Self {
        Shared::new(h).into()
    }
}

/// Helpers some of the make-* fns expose so config code can introspect a
/// handle (e.g. extract `id`).
pub fn register(ctx: &mut TulispContext) {
    ctx.defun("component-id", |h: ComponentHandle| -> i64 {
        h.id() as i64
    });
}
