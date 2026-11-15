//! A tiny deterministic PRNG (SplitMix64).
//!
//! The generator must produce byte-identical output on every machine and for every dependency
//! version, so it carries its own generator instead of using `rand`.

/// SplitMix64: fast, tiny and statistically good enough for test data.
#[derive(Clone, Debug)]
pub(crate) struct Rng {
    state: u64,
}

/// FNV-1a over `label`, used to derive independent streams from one seed.
fn fnv(label: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in label.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl Rng {
    /// A stream for `(seed, label, index)`; distinct labels and indices never share a stream
    /// prefix, so adding output in one area does not perturb another.
    pub(crate) fn derive(seed: u64, label: &str, index: u64) -> Self {
        let mut rng = Self {
            state: seed ^ fnv(label).rotate_left(17) ^ index.wrapping_mul(0x9e37_79b9_7f4a_7c15),
        };
        rng.next_u64();
        rng
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..bound` (`0` when `bound` is `0`).
    pub(crate) fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() % bound as u64) as usize
    }

    /// `true` with probability `p`.
    pub(crate) fn chance(&mut self, p: f64) -> bool {
        ((self.next_u64() >> 11) as f64 / (1u64 << 53) as f64) < p
    }

    /// A uniformly chosen element.
    pub(crate) fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// Fisher-Yates shuffle.
    pub(crate) fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_are_reproducible_and_independent() {
        let mut a = Rng::derive(7, "x", 0);
        let mut b = Rng::derive(7, "x", 0);
        assert_eq!(a.next_u64(), b.next_u64());
        assert_ne!(
            Rng::derive(7, "x", 0).next_u64(),
            Rng::derive(7, "y", 0).next_u64()
        );
        assert_ne!(
            Rng::derive(7, "x", 0).next_u64(),
            Rng::derive(7, "x", 1).next_u64()
        );
        assert_ne!(
            Rng::derive(7, "x", 0).next_u64(),
            Rng::derive(8, "x", 0).next_u64()
        );
    }

    #[test]
    fn below_stays_in_range() {
        let mut rng = Rng::derive(1, "r", 0);
        assert!((0..1000).all(|_| rng.below(7) < 7));
        assert_eq!(rng.below(0), 0);
    }
}
