//! `log.*` printers + math + RNG helpers used by ported microsim
//! configs.

use std::sync::Mutex;

use tulisp::TulispContext;

/// Process-global seedable generator backing `(random)`. `None` (the
/// default) means draw from the OS thread RNG; once `(set-random-seed
/// N)` installs a [`SplitMix64`], every `(random)` — including the
/// stochastic scenario helpers (`random-outage`, `random-uniform`) — is
/// reproducible, so a scenario run can be replayed bit-for-bit (e.g. in
/// CI). One sim per process, so a single global seed is the right grain.
static SEEDED_RNG: Mutex<Option<SplitMix64>> = Mutex::new(None);

/// SplitMix64 — a tiny, fast, well-distributed seedable PRNG. Rolled by
/// hand so seeding doesn't depend on a particular `rand` feature being
/// enabled; the unseeded path still uses `rand::thread_rng()`.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// One `(random LIMIT)` draw, from the seeded generator when one is
/// installed and the OS RNG otherwise. `LIMIT` absent → a full-range
/// i64; present → `[0, LIMIT)` with `LIMIT<=0` clamped to 1 (so
/// `(random (length '()))` yields 0 rather than panicking).
fn random_draw(limit: Option<i64>) -> i64 {
    use rand::Rng;
    let mut guard = SEEDED_RNG.lock().expect("rng mutex");
    match (guard.as_mut(), limit) {
        (Some(rng), Some(l)) => (rng.next_u64() % l.max(1) as u64) as i64,
        (Some(rng), None) => rng.next_u64() as i64,
        (None, Some(l)) => rand::thread_rng().gen_range(0..l.max(1)),
        (None, None) => rand::thread_rng().r#gen(),
    }
}

pub(super) fn register(ctx: &mut TulispContext) {
    ctx.defun("log.info", |msg: String| log::info!("{msg}"))
        .defun("log.warn", |msg: String| log::warn!("{msg}"))
        .defun("log.error", |msg: String| log::error!("{msg}"))
        .defun("log.debug", |msg: String| log::debug!("{msg}"))
        .defun("log.trace", |msg: String| log::trace!("{msg}"))
        // Math + RNG helpers used by ported microsim configs.
        .defun("ceiling", |n: f64| n.ceil() as i64)
        .defun("floor", |n: f64| n.floor() as i64)
        .defun("sin", |n: f64| n.sin())
        .defun("cos", |n: f64| n.cos())
        .defun("random", |limit: Option<i64>| random_draw(limit))
        // Seed the generator for a reproducible run; `(clear-random-seed)`
        // reverts to the OS RNG.
        .defun("set-random-seed", |seed: i64| {
            *SEEDED_RNG.lock().expect("rng mutex") = Some(SplitMix64(seed as u64));
            true
        })
        .defun("clear-random-seed", || {
            *SEEDED_RNG.lock().expect("rng mutex") = None;
            true
        });
}

#[cfg(test)]
mod tests {
    use super::SplitMix64;

    /// Same seed → same stream; different seeds diverge. This is the
    /// property the scenario-reproducibility story rests on.
    #[test]
    fn splitmix64_is_deterministic_per_seed() {
        let draws = |seed: u64| {
            let mut r = SplitMix64(seed);
            [r.next_u64(), r.next_u64(), r.next_u64()]
        };
        assert_eq!(draws(42), draws(42));
        assert_ne!(draws(42), draws(43));
    }
}
