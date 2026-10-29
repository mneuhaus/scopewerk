//! Captured s    let volts_per_div = info.map_or(f64::NAN, ChannelHeader::tip_volts_per_div);reens: per-channel voltages on a shared time axis, plus measurements computed on
//! the host so they work for any trace, including ones the scope does not measure itself.

use serde::{Deserialize, Serialize};

use crate::header::{ChannelHeader, ScreenHeader, Settings, to_f64};

/// ADC counts per vertical division in screen data. The upstream Python wrapper guessed 410
/// and 8.25; on firmware V1.5.0 the scope's own `:MEAS` min/max/pk-pk match 400 and 8 exactly.
pub const COUNTS_PER_DIV: f64 = 400.0;
/// Counts per offset pixel (offsets are given in 1/50 div).
pub const COUNTS_PER_OFFSET_PIXEL: f64 = COUNTS_PER_DIV / crate::header::PIXELS_PER_DIV;

/// One screen of both channels as the scope showed it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capture {
    /// Unix time in milliseconds when the screen was read.
    pub captured_at_ms: u128,
    pub settings: Settings,
    /// Seconds between two points.
    pub dt: f64,
    /// Time of the first point relative to the trigger (seconds).
    pub t0: f64,
    pub waveforms: Vec<Waveform>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Waveform {
    pub channel: u8,
    /// Voltage at the probe tip for every screen point.
    pub volts: Vec<f32>,
}

impl Capture {
    /// Measurements of one channel with the noise floor taken from its V/div.
    pub fn stats(&self, channel: u8) -> Option<Stats> {
        let volts_per_div = self
            .settings
            .channel(channel)
            .map_or(f64::NAN, |c| c.volts_per_div);
        self.waveform(channel)
            .map(|w| w.stats(self.dt, volts_per_div))
    }

    pub fn waveform(&self, channel: u8) -> Option<&Waveform> {
        self.waveforms.iter().find(|w| w.channel == channel)
    }

    pub fn time_at(&self, index: usize) -> f64 {
        self.t0 + to_f64(index) * self.dt
    }

    pub fn points(&self) -> usize {
        self.waveforms
            .iter()
            .map(|w| w.volts.len())
            .max()
            .unwrap_or(0)
    }

    /// CSV with a time column and one column per channel.
    pub fn to_csv(&self) -> String {
        let mut csv = String::from("time_s");
        for waveform in &self.waveforms {
            csv.push_str(&format!(",ch{}_v", waveform.channel));
        }
        csv.push('\n');
        for index in 0..self.points() {
            csv.push_str(&format!("{:.9e}", self.time_at(index)));
            for waveform in &self.waveforms {
                match waveform.volts.get(index) {
                    Some(v) => csv.push_str(&format!(",{v:.5}")),
                    None => csv.push(','),
                }
            }
            csv.push('\n');
        }
        csv
    }
}

/// Convert raw little-endian i16 screen or memory data of one channel.
pub(crate) fn decode_channel(channel: u8, payload: &[u8], header: &ScreenHeader) -> Waveform {
    let info = header.channels.get(usize::from(channel) - 1);
    let volts_per_div = info.map_or(f64::NAN, ChannelHeader::tip_volts_per_div);
    let offset_counts = to_f64(info.map_or(0, |c| c.offset)) * COUNTS_PER_OFFSET_PIXEL;
    let mut volts = Vec::with_capacity(payload.len() / 2);
    for pair in payload.chunks_exact(2) {
        let counts = f64::from(i16::from_le_bytes([pair[0], pair[1]]));
        volts.push(narrow(
            volts_per_div * (counts - offset_counts) / COUNTS_PER_DIV,
        ));
    }
    Waveform { channel, volts }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "f32 is plenty for an 8-bit ADC"
)]
fn narrow(value: f64) -> f32 {
    value as f32
}

/// Measurements over one trace, all in SI units. `None` when the signal does not have the
/// property (no edges → no frequency).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub min_v: f64,
    pub max_v: f64,
    pub pk_pk_v: f64,
    pub mean_v: f64,
    pub rms_v: f64,
    /// Levels at the 5th and 95th percentile: robust "low" and "high" of a digital signal.
    pub base_v: f64,
    pub top_v: f64,
    pub frequency_hz: Option<f64>,
    pub period_s: Option<f64>,
    pub duty_cycle_pct: Option<f64>,
    pub rise_time_s: Option<f64>,
    pub fall_time_s: Option<f64>,
    pub rising_edges: usize,
    pub falling_edges: usize,
}

impl Waveform {
    /// Measurements of this trace. `volts_per_div` sets the noise floor: swings below a quarter
    /// division (about six ADC steps) are not treated as edges, as on the scope itself.
    pub fn stats(&self, dt: f64, volts_per_div: f64) -> Stats {
        let volts: Vec<f64> = self.volts.iter().map(|v| f64::from(*v)).collect();
        if volts.is_empty() {
            return Stats::default();
        }
        let n = to_f64(volts.len());
        let min_v = volts.iter().copied().fold(f64::INFINITY, f64::min);
        let max_v = volts.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mean_v = volts.iter().sum::<f64>() / n;
        let rms_v = (volts.iter().map(|v| v * v).sum::<f64>() / n).sqrt();
        let mut sorted = volts.clone();
        sorted.sort_by(f64::total_cmp);
        let base_v = percentile(&sorted, 0.05);
        let top_v = percentile(&sorted, 0.95);
        let mut stats = Stats {
            min_v,
            max_v,
            pk_pk_v: max_v - min_v,
            mean_v,
            rms_v,
            base_v,
            top_v,
            ..Stats::default()
        };

        let amplitude = top_v - base_v;
        // A trace that only shows noise has no edges worth timing.
        let noise_floor = if volts_per_div.is_finite() {
            volts_per_div * 0.25
        } else {
            1e-3
        };
        if amplitude < noise_floor || amplitude < (max_v - min_v) * 0.3 {
            return stats;
        }
        let mid = base_v + amplitude * 0.5;
        let hysteresis = amplitude * 0.1;
        let edges = crossings(&volts, mid, hysteresis);
        stats.rising_edges = edges.iter().filter(|e| e.rising).count();
        stats.falling_edges = edges.len() - stats.rising_edges;

        let rising: Vec<f64> = edges.iter().filter(|e| e.rising).map(|e| e.at).collect();
        if rising.len() >= 2 {
            let period = (rising[rising.len() - 1] - rising[0]) / to_f64(rising.len() - 1) * dt;
            stats.period_s = Some(period);
            stats.frequency_hz = Some(1.0 / period);
            // Duty: high time inside whole periods.
            let (first, last) = (rising[0], rising[rising.len() - 1]);
            let mut high = 0.0;
            let mut since = None;
            for edge in edges.iter().filter(|e| e.at >= first && e.at <= last) {
                match (edge.rising, since) {
                    (true, _) => since = Some(edge.at),
                    (false, Some(start)) => {
                        high += edge.at - start;
                        since = None;
                    }
                    (false, None) => {}
                }
            }
            stats.duty_cycle_pct = Some(100.0 * high / (last - first));
        }
        let low10 = base_v + amplitude * 0.1;
        let high90 = base_v + amplitude * 0.9;
        stats.rise_time_s = transition_time(&volts, &edges, true, low10, high90).map(|s| s * dt);
        stats.fall_time_s = transition_time(&volts, &edges, false, low10, high90).map(|s| s * dt);
        stats
    }
}

struct Edge {
    /// Interpolated sample position of the mid-level crossing.
    at: f64,
    rising: bool,
}

fn crossings(volts: &[f64], mid: f64, hysteresis: f64) -> Vec<Edge> {
    let mut edges = Vec::new();
    let mut state = None;
    for (index, pair) in volts.windows(2).enumerate() {
        let (a, b) = (pair[0], pair[1]);
        if state.is_none() {
            if a > mid + hysteresis {
                state = Some(true);
            } else if a < mid - hysteresis {
                state = Some(false);
            }
            continue;
        }
        let high = state == Some(true);
        let crossed = if high {
            b < mid - hysteresis
        } else {
            b > mid + hysteresis
        };
        if crossed {
            // Walk back to the sample pair that straddles mid for the interpolated time.
            let mut i = index;
            while i > 0 && (volts[i] - mid).signum() == (b - mid).signum() {
                i -= 1;
            }
            let (va, vb) = (volts[i], volts[i + 1]);
            let fraction = if (vb - va).abs() > f64::EPSILON {
                (mid - va) / (vb - va)
            } else {
                0.0
            };
            edges.push(Edge {
                at: to_f64(i) + fraction.clamp(0.0, 1.0),
                rising: !high,
            });
            state = Some(!high);
        }
    }
    edges
}

/// Median 10–90 % transition time in samples for edges of one direction.
fn transition_time(
    volts: &[f64],
    edges: &[Edge],
    rising: bool,
    low: f64,
    high: f64,
) -> Option<f64> {
    let mut times: Vec<f64> = edges
        .iter()
        .filter(|e| e.rising == rising)
        .filter_map(|edge| {
            let centre = edge.at.floor();
            let centre = usize::try_from(centre as i64).ok()?;
            let (start_level, end_level) = if rising { (low, high) } else { (high, low) };
            let before = |v: f64| {
                if rising {
                    v <= start_level
                } else {
                    v >= start_level
                }
            };
            let after = |v: f64| {
                if rising {
                    v >= end_level
                } else {
                    v <= end_level
                }
            };
            let start = (0..=centre).rev().find(|&i| before(volts[i]))?;
            let end = (centre..volts.len()).find(|&i| after(volts[i]))?;
            Some(to_f64(end - start))
        })
        .collect();
    if times.is_empty() {
        return None;
    }
    times.sort_by(f64::total_cmp);
    Some(times[times.len() / 2])
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    let position = fraction * to_f64(sorted.len() - 1);
    sorted[usize::try_from(position.round() as i64)
        .unwrap_or(0)
        .min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(points: usize, period: usize, high: f32) -> Waveform {
        Waveform {
            channel: 1,
            volts: (0..points)
                .map(|i| if i % period < period / 2 { high } else { 0.0 })
                .collect(),
        }
    }

    #[test]
    fn measures_a_square_wave() {
        // 1 kHz square at 10 µs per point: 100 points per period.
        let stats = square(1520, 100, 5.0).stats(1e-5, 1.0);
        assert!((stats.frequency_hz.expect("frequency") - 1000.0).abs() < 1.0);
        assert!((stats.duty_cycle_pct.expect("duty") - 50.0).abs() < 1.5);
        assert!((stats.pk_pk_v - 5.0).abs() < 1e-6);
        assert_eq!(stats.rising_edges, 15);
    }

    #[test]
    fn flat_line_has_no_frequency() {
        let flat = Waveform {
            channel: 1,
            volts: vec![2.5; 1520],
        };
        let stats = flat.stats(1e-5, 1.0);
        assert_eq!(stats.frequency_hz, None);
        assert!((stats.mean_v - 2.5).abs() < 1e-6);
    }
}
