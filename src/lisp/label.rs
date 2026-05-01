//! `LispLabel`: a plist / alist value that's either a quoted symbol
//! (`'error`) or a double-quoted string (`"error"`).
//!
//! Enum-like config keys (`:health`, `:telemetry-mode`, `:command-mode`,
//! and the per-category default alists we're introducing) read more
//! naturally with bare symbols than with strings — `:health 'error`
//! beats `:health "error"`, and matches the way microsim's defaults
//! blocks are written. We still accept strings so existing configs
//! don't break.
//!
//! `LispLabel` is a thin newtype around the extracted name; downstream
//! code parses it via the existing `FromStr` impls on `Health`,
//! `TelemetryMode`, etc.

use tulisp::{Error, TulispObject};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LispLabel(String);

impl LispLabel {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LispLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<TulispObject> for LispLabel {
    type Error = Error;
    fn try_from(value: TulispObject) -> Result<Self, Self::Error> {
        // Prefer symbol; fall back to string. Both extractors return
        // the same `String` shape on success — symbol gives the symbol
        // name, string gives the literal.
        if let Ok(s) = value.as_symbol() {
            return Ok(Self(s));
        }
        if let Ok(s) = value.as_string() {
            return Ok(Self(s));
        }
        Err(
            Error::type_mismatch(format!("Expected symbol or string, got {value}"))
                .with_trace(value),
        )
    }
}

impl TryFrom<&TulispObject> for LispLabel {
    type Error = Error;
    fn try_from(value: &TulispObject) -> Result<Self, Self::Error> {
        if let Ok(s) = value.as_symbol() {
            return Ok(Self(s));
        }
        if let Ok(s) = value.as_string() {
            return Ok(Self(s));
        }
        Err(
            Error::type_mismatch(format!("Expected symbol or string, got {value}"))
                .with_trace(value.clone()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tulisp::TulispContext;

    #[test]
    fn from_symbol() {
        let mut ctx = TulispContext::new();
        let sym = ctx.intern("error");
        let l: LispLabel = sym.try_into().unwrap();
        assert_eq!(l.as_str(), "error");
    }

    #[test]
    fn from_string_literal() {
        let obj: TulispObject = "error".into();
        let l: LispLabel = obj.try_into().unwrap();
        assert_eq!(l.as_str(), "error");
    }

    #[test]
    fn rejects_other_types() {
        let obj: TulispObject = 42i64.into();
        let err = LispLabel::try_from(obj).unwrap_err();
        assert!(err.desc().contains("Expected symbol or string"));
    }
}
