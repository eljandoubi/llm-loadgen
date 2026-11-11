//! A small seedable generator (splitmix64), so arrival schedules and prompts are reproducible.

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

    /// Uniform in [0, 1), from 53 random bits.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Exponentially distributed with the given mean.
    pub fn exp(&mut self, mean: f64) -> f64 {
        -mean * (1.0 - self.next_f64()).ln()
    }

    /// Uniform integer in `0..n`, for `n > 0`.
    pub fn below(&mut self, n: u64) -> u64 {
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_reference_splitmix64_sequence_for_seed_zero() {
        let mut rng = Rng::new(0);
        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(rng.next_u64(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn the_same_seed_gives_the_same_stream_and_another_seed_a_different_one() {
        let a: Vec<u64> = (0..5)
            .scan(Rng::new(42), |r, _| Some(r.next_u64()))
            .collect();
        let b: Vec<u64> = (0..5)
            .scan(Rng::new(42), |r, _| Some(r.next_u64()))
            .collect();
        let c: Vec<u64> = (0..5)
            .scan(Rng::new(43), |r, _| Some(r.next_u64()))
            .collect();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn floats_are_in_the_unit_interval_with_mean_one_half() {
        let mut rng = Rng::new(1);
        let draws: Vec<f64> = (0..100_000).map(|_| rng.next_f64()).collect();
        assert!(draws.iter().all(|&x| (0.0..1.0).contains(&x)));
        let mean = draws.iter().sum::<f64>() / draws.len() as f64;
        assert!((mean - 0.5).abs() < 0.01, "mean {mean}");
    }

    #[test]
    fn exponential_draws_have_the_requested_mean_and_spread() {
        let mut rng = Rng::new(2);
        let draws: Vec<f64> = (0..200_000).map(|_| rng.exp(0.25)).collect();
        let mean = draws.iter().sum::<f64>() / draws.len() as f64;
        let var = draws.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / draws.len() as f64;
        assert!((mean - 0.25).abs() < 0.25 * 0.02, "mean {mean}");
        // for an exponential distribution the standard deviation equals the mean
        assert!(
            (var.sqrt() - 0.25).abs() < 0.25 * 0.03,
            "std {}",
            var.sqrt()
        );
        assert!(draws.iter().all(|&x| x >= 0.0 && x.is_finite()));
    }

    #[test]
    fn below_stays_in_range_and_covers_it_evenly() {
        let mut rng = Rng::new(3);
        let mut seen = [0u32; 10];
        for _ in 0..100_000 {
            seen[rng.below(10) as usize] += 1;
        }
        for (digit, &count) in seen.iter().enumerate() {
            assert!(
                (9_000..11_000).contains(&count),
                "digit {digit} came up {count} times"
            );
        }
        assert_eq!(rng.below(1), 0);
    }
}
