//! Symbol-only `TulispObject` conversion for the three runtime mode
//! enums (`Health`, `TelemetryMode`, `CommandMode`).
//!
//! Each enum gets four impls:
//!   1. `TryFrom<TulispObject>`        — for `AsPlist!` / `AsAlist!` field types
//!   2. `TryFrom<&TulispObject>`       — same, for the borrowed-arg path
//!   3. `TulispConvertible`            — for direct `defun` parameters
//!   4. `From<T> for TulispObject`     — required by the macros' into-lisp direction
//!
//! Strings are explicitly rejected — `:health 'error` works,
//! `:health "error"` errors with a type mismatch. This is a deliberate
//! tightening from the earlier symbol-or-string `LispLabel` pass: the
//! enum-of-symbols form is the idiomatic Lisp shape and we prefer one
//! correct way over two.

use std::str::FromStr;

use tulisp::{TulispConvertible, TulispObject};

use crate::sim::runtime::{CommandMode, Health, TelemetryMode};

/// Generates the four conversion impls listed in the module docs from
/// an enum's existing `FromStr` + `Display` impls.
macro_rules! impl_lisp_symbol_enum {
    ($enum:ty, expected = $expected:literal, label = $label:literal) => {
        impl TryFrom<TulispObject> for $enum {
            type Error = ::tulisp::Error;
            fn try_from(value: TulispObject) -> Result<Self, ::tulisp::Error> {
                <Self as TryFrom<&TulispObject>>::try_from(&value)
            }
        }

        impl TryFrom<&TulispObject> for $enum {
            type Error = ::tulisp::Error;
            fn try_from(value: &TulispObject) -> Result<Self, ::tulisp::Error> {
                let sym = value.as_symbol().map_err(|_| {
                    ::tulisp::Error::type_mismatch(format!(
                        "Expected symbol for :{}, got {}",
                        $label, value
                    ))
                    .with_trace(value.clone())
                })?;
                Self::from_str(&sym).map_err(|_| {
                    ::tulisp::Error::invalid_argument(format!(
                        "unknown :{} '{sym}; expected one of {}",
                        $label, $expected
                    ))
                })
            }
        }

        impl TulispConvertible for $enum {
            fn from_tulisp(value: &TulispObject) -> Result<Self, ::tulisp::Error> {
                Self::try_from(value)
            }
            fn into_tulisp(self) -> TulispObject {
                self.into()
            }
        }

        impl From<$enum> for TulispObject {
            /// Round-trip back to Lisp as a string of the variant
            /// name. Required by `AsPlist!`'s `into_plist` direction;
            /// in normal use we never call it (make-* fns only consume
            /// the args struct).
            fn from(v: $enum) -> Self {
                v.to_string().into()
            }
        }
    };
}

impl_lisp_symbol_enum!(Health, expected = "ok / error / standby", label = "health");
impl_lisp_symbol_enum!(
    TelemetryMode,
    expected = "normal / silent / closed",
    label = "telemetry-mode"
);
impl_lisp_symbol_enum!(
    CommandMode,
    expected = "normal / timeout / error",
    label = "command-mode"
);

#[cfg(test)]
mod tests {
    use super::*;
    use tulisp::TulispContext;

    #[test]
    fn health_from_symbol() {
        let mut ctx = TulispContext::new();
        let sym = ctx.intern("error");
        let h: Health = sym.try_into().unwrap();
        assert_eq!(h, Health::Error);
    }

    #[test]
    fn health_rejects_string() {
        let obj: TulispObject = "error".into();
        let err = Health::try_from(obj).unwrap_err();
        assert!(err.desc().contains("Expected symbol for :health"));
    }

    #[test]
    fn health_unknown_symbol_errors() {
        let mut ctx = TulispContext::new();
        let sym = ctx.intern("borked");
        let err = Health::try_from(sym).unwrap_err();
        let msg = err.desc();
        assert!(msg.contains("unknown :health"));
        assert!(msg.contains("ok / error / standby"));
    }

    #[test]
    fn telemetry_mode_from_symbol() {
        let mut ctx = TulispContext::new();
        let sym = ctx.intern("silent");
        let m: TelemetryMode = sym.try_into().unwrap();
        assert_eq!(m, TelemetryMode::Silent);
    }

    #[test]
    fn command_mode_from_symbol() {
        let mut ctx = TulispContext::new();
        let sym = ctx.intern("timeout");
        let m: CommandMode = sym.try_into().unwrap();
        assert_eq!(m, CommandMode::Timeout);
    }
}
