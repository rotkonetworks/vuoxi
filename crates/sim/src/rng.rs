//! A tiny seeded PRNG (splitmix64): the whole simulation is a pure function of
//! one `u64`, so a failing seed is a complete bug report.

#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform-enough value in `0..n` (`n > 0`); modulo bias is irrelevant here.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    pub fn index(&mut self, len: usize) -> usize {
        self.below(len as u64) as usize
    }

    /// Two distinct indices in `0..len` (`len >= 2`).
    pub fn pair(&mut self, len: usize) -> (usize, usize) {
        let a = self.index(len);
        let b = (a + 1 + self.index(len - 1)) % len;
        (a, b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        assert!((0..100).all(|_| a.next_u64() == b.next_u64()));
        assert_ne!(Rng::new(7).next_u64(), Rng::new(8).next_u64());
    }

    #[test]
    fn pair_is_distinct_and_in_range() {
        let mut r = Rng::new(1);
        for _ in 0..1000 {
            let (a, b) = r.pair(3);
            assert!(a < 3 && b < 3 && a != b);
        }
    }
}
