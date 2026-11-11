//! Latency histogram with a bounded relative error.
//!
//! Values are `u64` (microseconds in this crate). The first 256 values each get their own bucket;
//! above that, every power of two is split into 128 equal buckets, so a bucket is never wider
//! than 1/128 (0.78 percent) of the values it holds. That is the idea behind `HdrHistogram`, in a
//! few lines. Recording is constant time and a histogram is about 60 KB, whatever it holds.

const MANTISSA_BITS: u32 = 8;
const HALF: u64 = 1 << (MANTISSA_BITS - 1); // 128
const EXACT_BELOW: u64 = 1 << MANTISSA_BITS; // 256
                                             // the largest value (u64::MAX) has 64 bits, so shift 56 and mantissa 255
const BUCKETS: usize = (56 * HALF + EXACT_BELOW - 1) as usize + 1;

#[derive(Clone, Debug)]
pub struct Histogram {
    counts: Vec<u64>,
    total: u64,
    min: u64,
    max: u64,
    sum: u128,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

fn index_of(value: u64) -> usize {
    let bits = 64 - value.leading_zeros();
    let shift = bits.saturating_sub(MANTISSA_BITS);
    ((u64::from(shift) * HALF) + (value >> shift)) as usize
}

/// The lowest and highest value that land in bucket `index`.
fn bounds_of(index: usize) -> (u64, u64) {
    let index = index as u64;
    if index < EXACT_BELOW {
        return (index, index);
    }
    let shift = (index / HALF - 1) as u32;
    let mantissa = index - u64::from(shift) * HALF;
    let low = mantissa << shift;
    let high = low + ((1u64 << shift) - 1);
    (low, high)
}

impl Histogram {
    pub fn new() -> Self {
        Self {
            counts: vec![0; BUCKETS],
            total: 0,
            min: u64::MAX,
            max: 0,
            sum: 0,
        }
    }

    pub fn record(&mut self, value: u64) {
        self.record_n(value, 1);
    }

    pub fn record_n(&mut self, value: u64, n: u64) {
        if n == 0 {
            return;
        }
        self.counts[index_of(value)] += n;
        self.total += n;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.sum += u128::from(value) * u128::from(n);
    }

    /// How many values were recorded.
    pub fn len(&self) -> u64 {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Smallest value recorded, exactly; 0 for an empty histogram.
    pub fn min(&self) -> u64 {
        if self.is_empty() {
            0
        } else {
            self.min
        }
    }

    /// Largest value recorded, exactly.
    pub fn max(&self) -> u64 {
        self.max
    }

    pub fn mean(&self) -> f64 {
        if self.is_empty() {
            0.0
        } else {
            self.sum as f64 / self.total as f64
        }
    }

    /// The value below which a fraction `q` (0.0 to 1.0) of the recordings fall. It is the top of
    /// the bucket that holds that rank, so it can be too high by at most 0.78 percent and never
    /// too low; q = 0 and q = 1 give the exact minimum and maximum.
    pub fn value_at_quantile(&self, q: f64) -> u64 {
        if self.is_empty() {
            return 0;
        }
        if q <= 0.0 {
            return self.min;
        }
        if q >= 1.0 {
            return self.max;
        }
        let rank = ((q * self.total as f64).ceil() as u64).clamp(1, self.total);
        let mut seen = 0;
        for (index, &count) in self.counts.iter().enumerate() {
            seen += count;
            if seen >= rank {
                return bounds_of(index).1.clamp(self.min, self.max);
            }
        }
        self.max
    }

    pub fn merge(&mut self, other: &Histogram) {
        for (mine, theirs) in self.counts.iter_mut().zip(&other.counts) {
            *mine += theirs;
        }
        self.total += other.total;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.sum += other.sum;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small deterministic generator, so these tests need nothing else from the crate.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state >> 11
    }

    #[test]
    fn small_values_are_exact() {
        for v in 0..EXACT_BELOW {
            let i = index_of(v);
            assert_eq!(bounds_of(i), (v, v));
        }
    }

    #[test]
    fn every_value_is_inside_its_bucket_and_the_error_is_bounded() {
        let mut state = 7;
        let mut values = vec![
            0,
            1,
            255,
            256,
            257,
            511,
            512,
            u64::MAX,
            u64::MAX - 1,
            1 << 40,
        ];
        for _ in 0..20_000 {
            let bits = lcg(&mut state) % 64;
            values.push(lcg(&mut state) >> (63 - bits).min(63));
        }
        for v in values {
            let (low, high) = bounds_of(index_of(v));
            assert!(low <= v && v <= high, "{v} not in {low}..={high}");
            if v >= EXACT_BELOW {
                let width = high - low + 1;
                assert!(
                    width as f64 / low as f64 <= 1.0 / 128.0 + 1e-12,
                    "{v}: {low}..={high}"
                );
            }
        }
    }

    #[test]
    fn buckets_are_ordered_and_leave_no_gap() {
        let mut previous_high = None;
        for i in 0..BUCKETS {
            let (low, high) = bounds_of(i);
            assert!(low <= high);
            if let Some(p) = previous_high {
                assert_eq!(low, p + 1, "gap or overlap before bucket {i}");
            }
            previous_high = Some(high);
        }
        assert_eq!(previous_high, Some(u64::MAX));
    }

    #[test]
    fn the_largest_value_does_not_panic() {
        let mut h = Histogram::new();
        h.record(u64::MAX);
        assert_eq!(h.max(), u64::MAX);
        assert_eq!(h.value_at_quantile(0.5), u64::MAX);
    }

    #[test]
    fn an_empty_histogram_answers_zero() {
        let h = Histogram::new();
        assert_eq!((h.len(), h.min(), h.max()), (0, 0, 0));
        assert_eq!(h.value_at_quantile(0.99), 0);
        assert_eq!(h.mean(), 0.0);
        assert!(h.is_empty());
    }

    #[test]
    fn min_max_mean_and_count_are_exact() {
        let mut h = Histogram::new();
        for v in [5, 1_000_003, 42] {
            h.record(v);
        }
        assert_eq!((h.len(), h.min(), h.max()), (3, 5, 1_000_003));
        assert!((h.mean() - f64::from(5u32 + 1_000_003 + 42) / 3.0).abs() < 1e-9);
    }

    #[test]
    fn quantiles_of_a_uniform_range_are_within_the_error_bound() {
        let mut h = Histogram::new();
        for v in 1..=100_000u64 {
            h.record(v);
        }
        for (q, truth) in [
            (0.5, 50_000.0),
            (0.9, 90_000.0),
            (0.99, 99_000.0),
            (0.999, 99_900.0),
        ] {
            let got = h.value_at_quantile(q) as f64;
            assert!(got >= truth, "q={q}: {got} is below the true {truth}");
            assert!(
                got <= truth * (1.0 + 1.0 / 128.0),
                "q={q}: {got} too far above {truth}"
            );
        }
        assert_eq!(h.value_at_quantile(0.0), 1);
        assert_eq!(h.value_at_quantile(1.0), 100_000);
    }

    #[test]
    fn quantiles_never_decrease() {
        let mut h = Histogram::new();
        let mut state = 3;
        for _ in 0..5_000 {
            h.record(lcg(&mut state) % 1_000_000);
        }
        let mut last = 0;
        for i in 0..=1000 {
            let v = h.value_at_quantile(f64::from(i) / 1000.0);
            assert!(v >= last);
            last = v;
        }
    }

    #[test]
    fn a_quantile_is_never_above_the_maximum_or_below_the_minimum() {
        let mut h = Histogram::new();
        h.record(1_000_001);
        for q in [0.0, 0.3, 0.5, 1.0] {
            assert_eq!(h.value_at_quantile(q), 1_000_001);
        }
    }

    #[test]
    fn record_n_is_the_same_as_n_records() {
        let mut a = Histogram::new();
        let mut b = Histogram::new();
        a.record_n(777, 5);
        for _ in 0..5 {
            b.record(777);
        }
        a.record_n(9, 0);
        assert_eq!(a.counts, b.counts);
        assert_eq!((a.len(), a.min(), a.max()), (b.len(), b.min(), b.max()));
        assert_eq!(a.mean(), b.mean());
    }

    #[test]
    fn merging_is_the_same_as_recording_everything_in_one() {
        let mut state = 11;
        let (mut left, mut right, mut all) = (Histogram::new(), Histogram::new(), Histogram::new());
        for i in 0..4_000 {
            let v = lcg(&mut state) % 3_000_000;
            all.record(v);
            let side = if i % 3 == 0 { &mut left } else { &mut right };
            side.record(v);
        }
        left.merge(&right);
        assert_eq!(left.counts, all.counts);
        assert_eq!(
            (left.len(), left.min(), left.max()),
            (all.len(), all.min(), all.max())
        );
        assert_eq!(left.value_at_quantile(0.99), all.value_at_quantile(0.99));
    }

    #[test]
    fn merging_an_empty_histogram_changes_nothing() {
        let mut h = Histogram::new();
        h.record(10);
        h.merge(&Histogram::new());
        assert_eq!((h.len(), h.min(), h.max()), (1, 10, 10));
        let mut empty = Histogram::new();
        empty.merge(&h);
        assert_eq!((empty.len(), empty.min(), empty.max()), (1, 10, 10));
    }
}
