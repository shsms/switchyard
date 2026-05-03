//! Scalar component inputs that may be a constant or a Lisp expression.
//!
//! A meter's `:power` and a solar inverter's `:sunlight%` are scalar
//! inputs that scenario scripts often want to drive declaratively:
//!
//! ```lisp
//! (make-meter :power (lambda () (csv-lookup curve (now-seconds))))
//! (make-meter :power 'consumer-power)              ; deref a global
//! ```
//!
//! `DynamicScalar` is the storage shape that lets a component carry
//! such an input without violating the architectural rule that
//! `tick()` must not call back into the interpreter. The World
//! scheduler holds the interpreter lock once per tick, calls
//! [`SimulatedComponent::refresh_inputs`], which re-resolves the
//! source and stores the resulting `f32` in an atomic. `tick()`
//! then reads the atomic — pure Rust, no Lisp.
//!
//! [`SimulatedComponent::refresh_inputs`]: crate::sim::component::SimulatedComponent::refresh_inputs

use std::sync::atomic::{AtomicU32, Ordering};

use tulisp::{TulispContext, TulispObject};

/// How to resolve the source expression on each refresh.
enum Source {
    /// Evaluate the expression as-is. Symbols deref to their variable
    /// value; arithmetic forms compute; numeric literals self-evaluate.
    Eval(TulispObject),
    /// Call the source with no arguments. For lambda values supplied
    /// directly as a plist value (`:power (lambda () …)`) — in tulisp
    /// such a lambda is self-evaluating, so eval would just hand back
    /// the lambda; funcall actually runs it.
    Funcall(TulispObject),
}

/// A scalar input that is either a constant or a Lisp expression.
pub struct DynamicScalar {
    cached: AtomicU32,
    source: Option<Source>,
}

impl DynamicScalar {
    /// A pure constant. `refresh` is a no-op.
    pub fn constant(v: f32) -> Self {
        Self {
            cached: AtomicU32::new(v.to_bits()),
            source: None,
        }
    }

    /// A Lisp-driven value resolved by [`TulispContext::eval`] each
    /// refresh. Use this for symbol-form sources (`'consumer-power`)
    /// or arbitrary Lisp expressions whose evaluation yields a
    /// number.
    pub fn from_eval(expr: TulispObject, fallback: f32) -> Self {
        Self {
            cached: AtomicU32::new(fallback.to_bits()),
            source: Some(Source::Eval(expr)),
        }
    }

    /// A Lisp-driven value resolved by [`TulispContext::funcall`]
    /// each refresh. Use this when `callable` is a lambda value or
    /// any other zero-arg callable handed in directly as a plist
    /// value.
    pub fn from_funcall(callable: TulispObject, fallback: f32) -> Self {
        Self {
            cached: AtomicU32::new(fallback.to_bits()),
            source: Some(Source::Funcall(callable)),
        }
    }

    /// Build the right variant by inspecting `obj`'s shape:
    ///
    /// - `nil` → `None`.
    /// - number → [`Self::constant`].
    /// - symbol, cons, string → [`Self::from_eval`] (a symbol derefs;
    ///   a cons re-evaluates each refresh).
    /// - anything else (`Lambda` / `CompiledDefun` / opaque Rust
    ///   handle) → [`Self::from_funcall`].
    ///
    /// Pass a lambda value *unquoted* in the plist —
    /// `:power (lambda () …)` — so the plist evaluator hands back the
    /// compiled function rather than the literal list.
    pub fn from_lisp(obj: &TulispObject, fallback: f32) -> Option<Self> {
        if obj.null() {
            return None;
        }
        if obj.numberp() {
            return f64::try_from(obj).ok().map(|n| Self::constant(n as f32));
        }
        if obj.symbolp() || obj.consp() || obj.stringp() {
            return Some(Self::from_eval(obj.clone(), fallback));
        }
        Some(Self::from_funcall(obj.clone(), fallback))
    }

    /// Read the cached resolved value. Cheap; never blocks.
    pub fn get(&self) -> f32 {
        f32::from_bits(self.cached.load(Ordering::Relaxed))
    }

    /// Overwrite the cached value. Used by `(set-meter-power id W)`-
    /// style external setters and by tests.
    pub fn set(&self, v: f32) {
        self.cached.store(v.to_bits(), Ordering::Relaxed);
    }

    /// True if the source is a Lisp expression rather than a static
    /// constant. Components use this to skip dynamic-source bookkeeping
    /// for the common numeric case.
    pub fn is_dynamic(&self) -> bool {
        self.source.is_some()
    }

    /// Re-resolve the source and update the cached value. No-op for
    /// constants. Errors and non-finite results (`NaN`, `±∞`) log
    /// and keep the prior cached value — a scenario shouldn't
    /// corrupt downstream telemetry if a curve transiently returns
    /// garbage.
    pub fn refresh(&self, ctx: &mut TulispContext) {
        let Some(src) = &self.source else { return };
        let (label, result) = match src {
            Source::Eval(e) => (e, ctx.eval(e)),
            Source::Funcall(f) => (f, ctx.funcall(f, &TulispObject::nil())),
        };
        match result {
            Ok(obj) => match f64::try_from(&obj) {
                Ok(v) if v.is_finite() => self.set(v as f32),
                Ok(v) => log::warn!(
                    "DynamicScalar refresh: non-finite result {} from {}; keeping prior value",
                    v,
                    label,
                ),
                Err(e) => log::warn!(
                    "DynamicScalar refresh: non-numeric result from {}: {}",
                    label,
                    e.format(ctx)
                ),
            },
            Err(e) => log::warn!(
                "DynamicScalar refresh error in {}: {}",
                label,
                e.format(ctx)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tulisp::TulispContext;

    #[test]
    fn constant_get_returns_initial_value() {
        let s = DynamicScalar::constant(123.5);
        assert_eq!(s.get(), 123.5);
    }

    #[test]
    fn constant_refresh_is_noop() {
        let s = DynamicScalar::constant(7.0);
        let mut ctx = TulispContext::new();
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 7.0);
    }

    #[test]
    fn set_overrides_cached() {
        let s = DynamicScalar::constant(0.0);
        s.set(99.5);
        assert_eq!(s.get(), 99.5);
    }

    #[test]
    fn from_eval_runs_arithmetic_each_refresh() {
        let mut ctx = TulispContext::new();
        // `'(* 3 14.0)` returns the quoted list itself; refresh evals
        // it to 42.0.
        let src = ctx.eval_string("'(* 3 14.0)").unwrap();
        let s = DynamicScalar::from_eval(src, 0.0);
        assert_eq!(s.get(), 0.0);
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 42.0);
    }

    #[test]
    fn from_eval_keeps_fallback_on_non_finite_result() {
        let mut ctx = TulispContext::new();
        // 1.0 / 0.0 = +Inf in tulisp's float arithmetic; the
        // refresh path should reject it and keep the fallback
        // rather than poison the cache.
        let src = ctx.eval_string("'(/ 1.0 0.0)").unwrap();
        let s = DynamicScalar::from_eval(src, 7.5);
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 7.5);
    }

    #[test]
    fn from_eval_keeps_fallback_on_non_numeric() {
        let mut ctx = TulispContext::new();
        let src = ctx.eval_string("'\"not a number\"").unwrap();
        let s = DynamicScalar::from_eval(src, 1.5);
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 1.5);
    }

    #[test]
    fn from_eval_derefs_a_symbol() {
        let mut ctx = TulispContext::new();
        ctx.eval_string("(setq consumer-power 1500.0)").unwrap();
        let sym = ctx.eval_string("'consumer-power").unwrap();
        let s = DynamicScalar::from_eval(sym, 0.0);
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 1500.0);
        // Mutate the bound variable; refresh picks up the new value.
        ctx.eval_string("(setq consumer-power 2750.0)").unwrap();
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 2750.0);
    }

    #[test]
    fn from_funcall_invokes_a_lambda() {
        let mut ctx = TulispContext::new();
        let lambda = ctx.eval_string("(lambda () 17.5)").unwrap();
        let s = DynamicScalar::from_funcall(lambda, 0.0);
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 17.5);
    }

    #[test]
    fn from_lisp_dispatches_on_kind() {
        let mut ctx = TulispContext::new();

        // Numeric → constant.
        let n = ctx.eval_string("42.0").unwrap();
        let s = DynamicScalar::from_lisp(&n, 0.0).unwrap();
        assert!(!s.is_dynamic());
        assert_eq!(s.get(), 42.0);

        // Lambda value (CompiledDefun after eval) → funcall.
        let l = ctx.eval_string("(lambda () 9.5)").unwrap();
        let s = DynamicScalar::from_lisp(&l, 0.0).unwrap();
        assert!(s.is_dynamic());
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 9.5);

        // Symbol → eval (deref) on refresh.
        ctx.eval_string("(setq pv-cap 8000.0)").unwrap();
        let sym = ctx.eval_string("'pv-cap").unwrap();
        let s = DynamicScalar::from_lisp(&sym, 0.0).unwrap();
        assert!(s.is_dynamic());
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 8000.0);

        // Cons cell (arbitrary Lisp expression) → eval on refresh.
        let expr = ctx.eval_string("'(* 2 21)").unwrap();
        let s = DynamicScalar::from_lisp(&expr, 0.0).unwrap();
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 42.0);

        // nil → None.
        let nil = ctx.eval_string("nil").unwrap();
        assert!(DynamicScalar::from_lisp(&nil, 0.0).is_none());
    }
}
