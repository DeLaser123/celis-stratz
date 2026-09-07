//! Deterministic PRNG for Monte Carlo analysis (spec §24).
//!
//! SplitMix64: a well-known, fully specified 64-bit PRNG (Steele et al.).
//! Implemented in-crate so results can never drift with a dependency update
//! and so the exact algorithm is auditable. The only randomness in the entire
//! engine lives behind an explicit seed recorded in experiment metadata.

#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform f64 in [0, 1) with 53 bits of mantissa.
    pub fn next_f64(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64) * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform index in [0, n) via widening multiply (Lemire; no modulo bias).
    pub fn next_index(&mut self, n: usize) -> usize {
        debug_assert!(n > 0);
        let n = n as u128;
        let x = self.next_u64() as u128;
        ((x * n) >> 64) as usize
    }

    /// In-place Fisher–Yates shuffle (deterministic given the seed).
    pub fn shuffle<T>(&mut self, data: &mut [T]) {
        for i in (1..data.len()).rev() {
            let j = self.next_index(i + 1);
            data.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_sequence_is_reproducible() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let mut c = SplitMix64::new(43);
        assert_ne!(a.next_u64(), c.next_u64());
    }

    #[test]
    fn f64_in_range_and_deterministic() {
        let mut r = SplitMix64::new(7);
        for _ in 0..1000 {
            let v = r.next_f64();
            assert!((0.0..1.0).contains(&v));
        }
    }

    #[test]
    fn shuffle_is_permutation_and_reproducible() {
        let original: Vec<u32> = (0..64).collect();
        let mut a = original.clone();
        let mut b = a.clone();
        SplitMix64::new(1).shuffle(&mut a);
        SplitMix64::new(1).shuffle(&mut b);
        assert_eq!(a, b, "same seed must reproduce the same permutation");
        let mut sorted = a.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, original, "shuffle must be a permutation");
        let mut d = a.clone();
        SplitMix64::new(2).shuffle(&mut d);
        assert_ne!(a, d, "different seeds should (overwhelmingly) differ");
    }
}
