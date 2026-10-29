//! From analog samples to logic levels: an automatic two-level threshold with hysteresis
//! (a Schmitt trigger), and helpers for edges and pulse widths.

/// One input turned into logic levels.
#[derive(Debug, Clone)]
pub(crate) struct Logic {
    /// `true` = above the threshold.
    pub(crate) levels: Vec<bool>,
    pub(crate) low_v: f64,
    pub(crate) high_v: f64,
    pub(crate) threshold_v: f64,
}

impl Logic {
    pub(crate) fn swing(&self) -> f64 {
        self.high_v - self.low_v
    }

    /// Lengths (in samples) of every run of equal levels, except the first and last (they are
    /// cut by the capture window).
    pub(crate) fn inner_runs(&self) -> Vec<usize> {
        let mut runs = Vec::new();
        let mut start = 0;
        for i in 1..self.levels.len() {
            if self.levels[i] != self.levels[i - 1] {
                if start > 0 {
                    runs.push(i - start);
                }
                start = i;
            }
        }
        runs
    }

    pub(crate) fn transitions(&self) -> usize {
        self.levels.windows(2).filter(|w| w[0] != w[1]).count()
    }
}

/// Split the samples into a low and a high cluster (1-D two-means, robust when one level is
/// rare, e.g. a mostly idle bus) and digitize with ±10 % hysteresis around the midpoint.
pub(crate) fn digitize(volts: &[f32]) -> Logic {
    let values: Vec<f64> = volts.iter().map(|v| f64::from(*v)).collect();
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let (mut low, mut high) = (min, max);
    for _ in 0..24 {
        let mid = f64::midpoint(low, high);
        let (mut sum_low, mut n_low, mut sum_high, mut n_high) = (0.0, 0.0, 0.0, 0.0);
        for v in &values {
            if *v < mid {
                sum_low += v;
                n_low += 1.0;
            } else {
                sum_high += v;
                n_high += 1.0;
            }
        }
        let (next_low, next_high) = (
            if n_low > 0.0 { sum_low / n_low } else { low },
            if n_high > 0.0 {
                sum_high / n_high
            } else {
                high
            },
        );
        if (next_low - low).abs() < 1e-9 && (next_high - high).abs() < 1e-9 {
            break;
        }
        (low, high) = (next_low, next_high);
    }
    let threshold = f64::midpoint(low, high);
    let hysteresis = (high - low) * 0.1;
    let mut state = values.first().is_some_and(|v| *v >= threshold);
    let levels = values
        .iter()
        .map(|v| {
            if state && *v < threshold - hysteresis {
                state = false;
            } else if !state && *v > threshold + hysteresis {
                state = true;
            }
            state
        })
        .collect();
    Logic {
        levels,
        low_v: if low.is_finite() { low } else { 0.0 },
        high_v: if high.is_finite() { high } else { 0.0 },
        threshold_v: if threshold.is_finite() {
            threshold
        } else {
            0.0
        },
    }
}

/// A typical one-bit run length: the 15th percentile of inner runs (protocols with bit
/// stuffing or random data have plenty of single-bit runs; longer runs are multiples).
pub(crate) fn unit_run(runs: &[usize]) -> Option<f64> {
    if runs.len() < 3 {
        return None;
    }
    let mut sorted = runs.to_vec();
    sorted.sort_unstable();
    let pick = sorted[sorted.len() * 15 / 100];
    // Average the runs within ±30 % of the pick to beat quantization.
    let near: Vec<f64> = sorted
        .iter()
        .filter(|r| (**r as f64 - pick as f64).abs() <= pick as f64 * 0.3 + 1.0)
        .map(|r| *r as f64)
        .collect();
    Some(near.iter().sum::<f64>() / near.len() as f64)
}

#[cfg(test)]
pub(crate) mod synth {
    //! Realistic test signals: finite edges, noise and 8-bit quantization like the scope.

    /// Turn a bit sequence into samples: `samples_per_bit` (fractional), levels `low`/`high`,
    /// an RC edge with `tau` samples, deterministic noise and 1/25-division quantization.
    pub(crate) fn render(
        bits: &[bool],
        samples_per_bit: f64,
        low: f64,
        high: f64,
        tau: f64,
    ) -> Vec<f32> {
        let total = (bits.len() as f64 * samples_per_bit) as usize;
        let mut out = Vec::with_capacity(total);
        let mut v = if bits.first().copied().unwrap_or(true) {
            high
        } else {
            low
        };
        let mut seed: u32 = 0x1234_5678;
        let step = (high - low).abs() / 25.0;
        for i in 0..total {
            let bit = bits[((i as f64) / samples_per_bit) as usize];
            let target = if bit { high } else { low };
            v += (target - v) * (1.0 - (-1.0 / tau.max(0.01)).exp());
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let noise = (f64::from(seed % 1000) / 1000.0 - 0.5) * step * 1.5;
            let quantized = ((v + noise) / step).round() * step;
            out.push(quantized as f32);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_levels_of_a_mostly_idle_line() {
        // 95 % high, 5 % low: percentiles would miss the low level, two-means does not.
        let mut bits = vec![true; 950];
        bits.extend(vec![false; 50]);
        let samples = synth::render(&bits, 1.0, 0.2, 3.3, 0.5);
        let logic = digitize(&samples);
        assert!((logic.high_v - 3.3).abs() < 0.2, "{logic:?}");
        assert!((logic.low_v - 0.2).abs() < 0.3);
        assert_eq!(logic.transitions(), 1);
    }
}
