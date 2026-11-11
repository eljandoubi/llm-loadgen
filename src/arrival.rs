//! When requests are meant to start.
//!
//! An open-loop generator decides the start times in advance and keeps to them whatever the
//! server does. A schedule is an endless iterator of offsets from the start of the run.

use crate::rng::Rng;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Process {
    /// Exponential gaps: what independent users produce. Bursts happen.
    Poisson,
    /// Equal gaps: a steady drip.
    Constant,
}

#[derive(Clone, Debug)]
pub struct Schedule {
    process: Process,
    rate: f64,
    rng: Rng,
    index: u64,
    clock: f64,
}

impl Schedule {
    pub fn new(process: Process, rate_per_s: f64, seed: u64) -> Result<Self, String> {
        if !(rate_per_s.is_finite() && rate_per_s > 0.0) {
            return Err(format!(
                "the rate must be a positive number, got {rate_per_s}"
            ));
        }
        Ok(Self {
            process,
            rate: rate_per_s,
            rng: Rng::new(seed),
            index: 0,
            clock: 0.0,
        })
    }
}

impl Iterator for Schedule {
    type Item = Duration;

    fn next(&mut self) -> Option<Duration> {
        let at = match self.process {
            Process::Constant => self.index as f64 / self.rate,
            Process::Poisson => {
                self.clock += self.rng.exp(1.0 / self.rate);
                self.clock
            }
        };
        self.index += 1;
        Some(Duration::from_secs_f64(at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offsets(process: Process, rate: f64, seed: u64, n: usize) -> Vec<f64> {
        Schedule::new(process, rate, seed)
            .unwrap()
            .take(n)
            .map(|d| d.as_secs_f64())
            .collect()
    }

    #[test]
    fn constant_arrivals_start_at_zero_and_are_evenly_spaced() {
        let t = offsets(Process::Constant, 8.0, 1, 5);
        assert_eq!(t, vec![0.0, 0.125, 0.25, 0.375, 0.5]);
    }

    #[test]
    fn poisson_gaps_have_mean_one_over_rate_and_unit_variation() {
        let t = offsets(Process::Poisson, 50.0, 7, 100_001);
        let gaps: Vec<f64> = t.windows(2).map(|w| w[1] - w[0]).collect();
        let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
        let var = gaps.iter().map(|g| (g - mean).powi(2)).sum::<f64>() / gaps.len() as f64;
        assert!((mean - 0.02).abs() < 0.02 * 0.02, "mean gap {mean}");
        assert!(
            (var.sqrt() / mean - 1.0).abs() < 0.03,
            "cv {}",
            var.sqrt() / mean
        );
    }

    #[test]
    fn poisson_counts_per_second_have_mean_and_variance_equal_to_the_rate() {
        let rate = 40.0;
        let t = offsets(Process::Poisson, rate, 11, 20_000);
        let seconds = t.last().unwrap().floor() as usize;
        let mut counts = vec![0.0f64; seconds];
        for &x in &t {
            if (x as usize) < seconds {
                counts[x as usize] += 1.0;
            }
        }
        let mean = counts.iter().sum::<f64>() / seconds as f64;
        let var = counts.iter().map(|c| (c - mean).powi(2)).sum::<f64>() / seconds as f64;
        assert!((mean - rate).abs() < rate * 0.03, "mean {mean}");
        assert!((var - rate).abs() < rate * 0.2, "variance {var}");
    }

    #[test]
    fn offsets_never_go_backwards() {
        for process in [Process::Poisson, Process::Constant] {
            let t = offsets(process, 1000.0, 3, 10_000);
            assert!(t.windows(2).all(|w| w[0] <= w[1]));
        }
    }

    #[test]
    fn a_seed_fixes_the_schedule() {
        let a = offsets(Process::Poisson, 10.0, 5, 50);
        assert_eq!(a, offsets(Process::Poisson, 10.0, 5, 50));
        assert_ne!(a, offsets(Process::Poisson, 10.0, 6, 50));
    }

    #[test]
    fn the_seed_does_not_matter_for_a_constant_rate() {
        assert_eq!(
            offsets(Process::Constant, 3.0, 1, 10),
            offsets(Process::Constant, 3.0, 99, 10)
        );
    }

    #[test]
    fn bad_rates_are_refused() {
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let err = Schedule::new(Process::Poisson, rate, 0).unwrap_err();
            assert!(err.contains("positive"), "{err}");
        }
    }
}
