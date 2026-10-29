//! UART (async serial): start bit, 5–9 data bits LSB first, optional parity, stop bit. The baud
//! rate is measured from the shortest pulses and snapped to a standard rate when close.

use serde::{Deserialize, Serialize};

use super::digital::unit_run;
use super::{Annotation, Event, Mark};

pub const STANDARD_BAUDS: [f64; 16] = [
    300.0,
    1200.0,
    2400.0,
    4800.0,
    9600.0,
    19_200.0,
    31_250.0,
    38_400.0,
    57_600.0,
    74_880.0,
    115_200.0,
    230_400.0,
    250_000.0,
    460_800.0,
    921_600.0,
    1_000_000.0,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Parity {
    #[default]
    None,
    Even,
    Odd,
}

pub(crate) struct UartResult {
    pub(crate) baud: Option<f64>,
    pub(crate) events: Vec<(Event, Vec<Annotation>)>,
}

/// `idle_high` lines are normal TTL/RS-232-after-transceiver; pass `false` for inverted lines.
/// Line format; `baud: None` measures it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Format {
    pub(crate) baud: Option<f64>,
    pub(crate) data_bits: u8,
    pub(crate) parity: Parity,
    /// `false` for inverted (idle-low) lines.
    pub(crate) idle_high: bool,
}

pub(crate) fn decode(
    line: &[bool],
    runs: &[usize],
    dt: f64,
    t0: f64,
    format: &Format,
) -> UartResult {
    let Format {
        baud,
        data_bits,
        parity,
        idle_high,
    } = *format;
    let baud = baud.or_else(|| {
        let unit = unit_run(runs)?;
        let estimate = 1.0 / (unit * dt);
        let standard = STANDARD_BAUDS.iter().copied().min_by(|a, b| {
            (a / estimate - 1.0)
                .abs()
                .total_cmp(&(b / estimate - 1.0).abs())
        })?;
        Some(if (standard / estimate - 1.0).abs() < 0.06 {
            standard
        } else {
            estimate
        })
    });
    let Some(rate) = baud else {
        return UartResult {
            baud: None,
            events: Vec::new(),
        };
    };
    let bit = 1.0 / (rate * dt);
    let time = |sample: f64| t0 + sample * dt;
    // Work on "mark" levels: true = idle.
    let mark: Vec<bool> = line.iter().map(|l| *l == idle_high).collect();
    let parity_bits = usize::from(parity != Parity::None);
    let frame_bits = 1 + usize::from(data_bits) + parity_bits + 1;
    let mut events = Vec::new();
    let mut i = 1;
    while i < mark.len() {
        if !(mark[i - 1] && !mark[i]) {
            i += 1;
            continue;
        }
        let start = i as f64 - 0.5;
        let at = |n: f64| -> Option<bool> {
            mark.get((start + (n + 0.5) * bit).round() as usize)
                .copied()
        };
        if (start + frame_bits as f64 * bit) as usize >= mark.len() {
            break;
        }
        if at(0.0) != Some(false) {
            // A glitch, not a start bit.
            i += 1;
            continue;
        }
        let mut value: u16 = 0;
        let mut ones = 0;
        for n in 0..data_bits {
            let one = at(f64::from(n) + 1.0).unwrap_or(true);
            value |= u16::from(one) << n;
            ones += u32::from(one);
        }
        let parity_error = match parity {
            Parity::None => false,
            Parity::Even | Parity::Odd => {
                let p = at(f64::from(data_bits) + 1.0).unwrap_or(true);
                let total = ones + u32::from(p);
                (parity == Parity::Even) != (total % 2 == 0)
            }
        };
        let stop_index = (frame_bits - 1) as f64;
        let framing_error = at(stop_index) != Some(true);
        let end = start + frame_bits as f64 * bit;
        let byte = (value & 0xFF) as u8;
        let printable = if byte.is_ascii_graphic() || byte == b' ' {
            format!("{}", byte as char)
        } else {
            format!("{byte:02X}")
        };
        let mark_kind = if framing_error || parity_error {
            Mark::Error
        } else {
            Mark::Data
        };
        events.push((
            Event::Uart {
                t_s: time(start),
                value: byte,
                framing_error,
                parity_error,
            },
            vec![Annotation::new(
                time(start),
                time(end),
                &printable,
                mark_kind,
            )],
        ));
        // Continue from the middle of the stop bit.
        i = ((start + (stop_index + 0.5) * bit) as usize).max(i + 1);
    }
    UartResult {
        baud: Some(rate),
        events,
    }
}

#[cfg(test)]
mod tests {
    use super::super::digital::{digitize, synth};
    use super::*;

    #[test]
    fn decodes_text_with_auto_baud() {
        let mut bits = vec![true; 30];
        for byte in b"Hi, R-Net!" {
            bits.push(false);
            for i in 0..8 {
                bits.push((byte >> i) & 1 == 1);
            }
            bits.push(true);
            bits.extend([true; 2]);
        }
        bits.extend([true; 20]);
        // 115200 baud at 1 MS/s: 8.68 samples per bit.
        let samples = synth::render(&bits, 1e6 / 115_200.0, 0.0, 3.3, 0.8);
        let logic = digitize(&samples);
        let format = Format {
            baud: None,
            data_bits: 8,
            parity: Parity::None,
            idle_high: true,
        };
        let result = decode(&logic.levels, &logic.inner_runs(), 1e-6, 0.0, &format);
        assert_eq!(result.baud, Some(115_200.0));
        let text: Vec<u8> = result
            .events
            .iter()
            .map(|(e, _)| match e {
                Event::Uart {
                    value,
                    framing_error: false,
                    ..
                } => *value,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(text, b"Hi, R-Net!");
    }
}
