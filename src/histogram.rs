//! A latency histogram with inclusive, two-significant-digit nanosecond boundaries.
//! Zero belongs to the first bin; the last bin covers every representable delay.
use std::sync::OnceLock;

pub struct Histogram {
    bins: Vec<u64>,
    count: u64,
    sum_ns: u128,
    max_ns: u64,
}

pub fn bounds() -> &'static [u64] {
    static BOUNDS: OnceLock<Vec<u64>> = OnceLock::new();
    BOUNDS.get_or_init(|| {
        let mut bounds: Vec<u64> = (1..10).collect();
        let mut scale = 1_u64;
        loop {
            for digits in 10_u64..100 {
                let Some(bound) = digits.checked_mul(scale) else {
                    bounds.push(u64::MAX);
                    return bounds;
                };
                bounds.push(bound);
            }
            scale *= 10;
        }
    })
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            bins: vec![0; bounds().len()],
            count: 0,
            sum_ns: 0,
            max_ns: 0,
        }
    }
}

impl Histogram {
    pub fn observe(&mut self, delay_ns: u64) {
        self.bins[bounds().partition_point(|&upper| upper < delay_ns)] += 1;
        self.count += 1;
        self.sum_ns += u128::from(delay_ns);
        self.max_ns = self.max_ns.max(delay_ns);
    }

    pub fn bins(&self) -> &[u64] {
        &self.bins
    }
    pub fn count(&self) -> u64 {
        self.count
    }
    pub fn sum_ns(&self) -> u128 {
        self.sum_ns
    }
    pub fn max_ns(&self) -> u64 {
        self.max_ns
    }

    /// Replace the accumulated values while reusing the bin storage.
    pub fn copy_from(&mut self, source: &Self) {
        self.bins.copy_from_slice(&source.bins);
        self.count = source.count;
        self.sum_ns = source.sum_ns;
        self.max_ns = source.max_ns;
    }

    /// Return inclusive cumulative counts at sorted boundaries from `bounds()`.
    pub fn cumulative_counts(&self, upper_bounds: &[u64]) -> Vec<u64> {
        let mut input = bounds().iter().zip(&self.bins).peekable();
        let mut count = 0;
        upper_bounds
            .iter()
            .map(|upper| {
                while input.peek().is_some_and(|(bound, _)| **bound <= *upper) {
                    count += input.next().unwrap().1;
                }
                count
            })
            .collect()
    }

    /// Return bucket upper bounds for p90, p95, and p99, or None when empty.
    pub fn percentiles(&self) -> Option<[u64; 3]> {
        if self.count == 0 {
            return None;
        }
        let ranks = [90_u128, 95, 99].map(|p| (u128::from(self.count) * p).div_ceil(100));
        let mut result = [0; 3];
        let (mut count, mut next) = (0_u128, 0);
        for (&upper, &frequency) in bounds().iter().zip(&self.bins) {
            count += u128::from(frequency);
            while next < ranks.len() && count >= ranks[next] {
                result[next] = upper;
                next += 1;
            }
            if next == ranks.len() {
                break;
            }
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::{Histogram, bounds};

    #[test]
    fn observations_use_inclusive_upper_bounds_and_preserve_totals() {
        let values = [0, 1, 99, 100, 101, 110, 111, u64::MAX];
        let mut histogram = Histogram::default();
        for value in values {
            histogram.observe(value);
        }
        let occupied: Vec<_> = bounds()
            .iter()
            .copied()
            .zip(histogram.bins().iter().copied())
            .filter(|&(_, count)| count != 0)
            .collect();
        assert_eq!(
            occupied,
            [(1, 2), (99, 1), (100, 1), (110, 2), (120, 1), (u64::MAX, 1)]
        );
        assert_eq!(histogram.count(), values.len() as u64);
        assert_eq!(histogram.bins().iter().sum::<u64>(), histogram.count());
        assert_eq!(histogram.sum_ns(), values.into_iter().map(u128::from).sum());
        assert_eq!(histogram.max_ns(), u64::MAX);
    }

    #[test]
    fn cumulative_counts_match_direct_counts_at_shared_boundaries() {
        let values = [0, 99, 100, 101, 110, 999, 1000, 1001, u64::MAX];
        let mut histogram = Histogram::default();
        for value in values {
            histogram.observe(value);
        }
        let upper_bounds = [1, 100, 110, 1000, 2000, u64::MAX];
        let expected: Vec<_> = upper_bounds
            .iter()
            .map(|upper| values.iter().filter(|value| *value <= upper).count() as u64)
            .collect();
        assert_eq!(histogram.cumulative_counts(&upper_bounds), expected);
        assert_eq!(
            Histogram::default().cumulative_counts(&upper_bounds),
            vec![0; upper_bounds.len()]
        );
    }

    #[test]
    fn percentiles_use_ceiling_ranks_and_report_empty_histograms() {
        let mut histogram = Histogram::default();
        assert_eq!(histogram.percentiles(), None);
        histogram.observe(101);
        assert_eq!(histogram.percentiles(), Some([110; 3]));
        let mut histogram = Histogram::default();
        for value in 1..=100 {
            histogram.observe(value);
        }
        assert_eq!(histogram.percentiles(), Some([90, 95, 99]));
        histogram.observe(u64::MAX);
        assert_eq!(histogram.percentiles(), Some([91, 96, 100]));
        let mut tail = Histogram::default();
        tail.observe(u64::MAX);
        assert_eq!(tail.percentiles(), Some([u64::MAX; 3]));
    }

    #[test]
    fn copying_replaces_values_and_keeps_histograms_independent() {
        let mut source = Histogram::default();
        source.observe(101);
        let mut target = Histogram::default();
        target.observe(u64::MAX);
        target.copy_from(&source);
        source.observe(1);
        assert_eq!(target.count(), 1);
        assert_eq!(target.sum_ns(), 101);
        assert_eq!(target.max_ns(), 101);
        assert_eq!(target.percentiles(), Some([110; 3]));
    }
}
