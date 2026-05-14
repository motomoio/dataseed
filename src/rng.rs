//! Seeded PRNG wrapper.
//!
//! Uses `ChaCha8Rng` rather than `rand::rngs::StdRng` because `StdRng` is
//! explicitly not stable across `rand` minor versions — the byte-for-byte
//! determinism guarantee we promise in the CLI (`--seed N`) requires a
//! frozen algorithm. ChaCha8 is portable across platforms and versions.

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

/// Deterministic random source. All generators consume from a single
/// instance so the call order in `schema { ... }` × row index fully
/// determines the byte stream the PRNG yields.
pub struct SeedRng {
    inner: ChaCha8Rng,
    seed: u64,
}

impl SeedRng {
    /// Construct from an explicit seed (the `--seed` flag).
    pub fn from_seed(seed: u64) -> Self {
        Self {
            inner: ChaCha8Rng::seed_from_u64(seed),
            seed,
        }
    }

    /// Construct with an entropy-derived seed. Used when the user doesn't
    /// pass `--seed`. The chosen seed is captured so it can be printed
    /// (helpful for reproducing one-off runs).
    pub fn from_entropy() -> Self {
        let seed = rand::random::<u64>();
        Self::from_seed(seed)
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Borrow the inner RNG for use with `rand`'s distributions and traits.
    pub fn rng_mut(&mut self) -> &mut ChaCha8Rng {
        &mut self.inner
    }

    pub fn gen_range_i64(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(lo <= hi, "gen_range_i64: lo > hi");
        self.inner.gen_range(lo..=hi)
    }

    pub fn gen_range_f64(&mut self, lo: f64, hi: f64) -> f64 {
        debug_assert!(lo <= hi, "gen_range_f64: lo > hi");
        self.inner.gen_range(lo..=hi)
    }

    pub fn gen_bool(&mut self, p: f64) -> bool {
        let p = p.clamp(0.0, 1.0);
        self.inner.gen_bool(p)
    }

    /// Uniformly pick an index in `0..len`. Panics if `len == 0`.
    pub fn pick_index(&mut self, len: usize) -> usize {
        assert!(len > 0, "pick_index: empty slice");
        self.inner.gen_range(0..len)
    }
}
