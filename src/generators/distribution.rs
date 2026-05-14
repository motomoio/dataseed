//! Skew distributions for `ref()` draws.
//!
//! Inputs:
//! * `len` — population size (parent row count).
//! * `rng` — the seeded ChaCha8 stream (`SeedRng`).
//!
//! Output: a `usize` index in `0..len`.
//!
//! Determinism rules:
//! * Every random draw goes through `SeedRng` (no platform libm calls
//!   outside `libm::*` for cross-platform stability).
//! * `Gauss` and `Exponential` use `libm::log`/`libm::cos` like other
//!   geo generators — `f64::ln` is also Ryu/Grisu-stable in Rust but
//!   `libm::log` matches the existing Phase 2 precedent.

use crate::rng::SeedRng;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distribution {
    Uniform,
    Zipf,
    Gauss,
    Exponential,
}

impl Distribution {
    /// Parse a distribution name as it appears in a `.dataseed` file. Returns
    /// `None` for unknown names so callers can surface a helpful error.
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "uniform" => Some(Self::Uniform),
            "zipf" => Some(Self::Zipf),
            "gauss" => Some(Self::Gauss),
            "exponential" => Some(Self::Exponential),
            _ => None,
        }
    }

    /// Draw one index in `0..len`. `len` must be > 0 — caller guarantees.
    pub fn draw(self, rng: &mut SeedRng, len: usize) -> usize {
        match self {
            Self::Uniform => rng.pick_index(len),
            Self::Zipf => draw_zipf(rng, len),
            Self::Gauss => draw_gauss(rng, len),
            Self::Exponential => draw_exponential(rng, len),
        }
    }
}

// ----- Zipf (rank-1 inverse-CDF) -----------------------------------------

fn draw_zipf(rng: &mut SeedRng, len: usize) -> usize {
    // Zipf with exponent s=1. Rank-i probability is 1/(i * H_n) where
    // H_n is the n-th harmonic number. Inverse-CDF sampling: pick u in
    // (0, 1], walk cumulative probability until we exceed u.
    //
    // O(N) per draw. For N >> 100k callers should pick Uniform; we
    // accept the cost here to keep the implementation portable.
    let u = rng.gen_range_f64(f64::MIN_POSITIVE, 1.0);
    let h_n: f64 = (1..=len).map(|k| 1.0 / k as f64).sum();
    let mut cum = 0.0;
    for k in 1..=len {
        cum += 1.0 / (k as f64 * h_n);
        if cum >= u {
            return k - 1;
        }
    }
    len - 1
}

// ----- Gauss (bell around centre rank) -----------------------------------

fn draw_gauss(rng: &mut SeedRng, len: usize) -> usize {
    // Box-Muller; reject samples outside 0..len; map to integer index.
    // sigma = len/6 puts 99.7% of mass inside the population.
    let mid = (len as f64 - 1.0) / 2.0;
    let sigma = (len as f64) / 6.0;
    loop {
        let u1 = rng.gen_range_f64(f64::MIN_POSITIVE, 1.0);
        let u2 = rng.gen_range_f64(0.0, 1.0);
        // libm::* keeps bit-identical results across platforms.
        let z = libm::sqrt(-2.0 * libm::log(u1))
            * libm::cos(2.0 * std::f64::consts::PI * u2);
        let x = mid + z * sigma;
        if x >= 0.0 && x < len as f64 {
            return x as usize;
        }
    }
}

// ----- Exponential (first-rank biased) -----------------------------------

fn draw_exponential(rng: &mut SeedRng, len: usize) -> usize {
    // Inverse-CDF: x = -ln(u) / lambda. Pick lambda = ln(100)/len so
    // 99% of draws fall inside the population; the (rare) overflow is
    // clamped to the last index.
    let lambda = libm::log(100.0) / len as f64;
    let u = rng.gen_range_f64(f64::MIN_POSITIVE, 1.0);
    let x = -libm::log(u) / lambda;
    (x as usize).min(len - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::SeedRng;

    fn histogram(d: Distribution, n: usize, samples: usize, seed: u64) -> Vec<u64> {
        let mut rng = SeedRng::from_seed(seed);
        let mut h = vec![0u64; n];
        for _ in 0..samples {
            h[d.draw(&mut rng, n)] += 1;
        }
        h
    }

    #[test]
    fn uniform_is_roughly_flat() {
        let h = histogram(Distribution::Uniform, 10, 100_000, 1);
        let avg = 100_000 / 10;
        for &c in &h {
            assert!(
                c > (avg * 8 / 10) && c < (avg * 12 / 10),
                "{c} not within 20% of {avg}"
            );
        }
    }

    #[test]
    fn zipf_is_heavily_skewed_to_first() {
        let h = histogram(Distribution::Zipf, 100, 100_000, 1);
        assert!(h[0] > h[99] * 10, "h[0]={} h[99]={}", h[0], h[99]);
    }

    #[test]
    fn gauss_peaks_in_the_middle() {
        let h = histogram(Distribution::Gauss, 21, 100_000, 1);
        let mid = h[10];
        let edge = h[0];
        assert!(mid > edge * 2, "mid={mid} edge={edge}");
    }

    #[test]
    fn exponential_is_decaying() {
        let h = histogram(Distribution::Exponential, 50, 100_000, 1);
        assert!(h[0] > h[49] * 5, "h[0]={} h[49]={}", h[0], h[49]);
    }

    #[test]
    fn draw_is_in_range() {
        let mut rng = SeedRng::from_seed(7);
        for d in [
            Distribution::Uniform,
            Distribution::Zipf,
            Distribution::Gauss,
            Distribution::Exponential,
        ] {
            for _ in 0..1000 {
                let idx = d.draw(&mut rng, 17);
                assert!(idx < 17, "idx {idx} out of bounds for {d:?}");
            }
        }
    }

    #[test]
    fn from_name_round_trip() {
        for s in &["uniform", "zipf", "gauss", "exponential"] {
            assert!(Distribution::from_name(s).is_some(), "{s}");
        }
        assert!(Distribution::from_name("normal").is_none());
        assert!(Distribution::from_name("").is_none());
    }
}
