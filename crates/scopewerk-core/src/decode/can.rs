//! Classic CAN (2.0A/B) from one analog line: hard sync on start of frame, resync on every
//! edge, bit destuffing, CRC-15 check, ACK slot. Bitrate and polarity are found automatically by
//! decoding with every candidate and keeping the one with the most CRC-valid frames.

use super::digital::{Logic, unit_run};
use super::{Annotation, Event, Mark};

/// Bitrates tried when none is given, fastest first.
pub const STANDARD_BITRATES: [f64; 12] = [
    1_000_000.0,
    800_000.0,
    500_000.0,
    250_000.0,
    125_000.0,
    100_000.0,
    83_333.0,
    62_500.0,
    50_000.0,
    33_333.0,
    20_000.0,
    10_000.0,
];

/// Where in the bit the level is read (fraction of the bit time after the edge).
const SAMPLE_POINT: f64 = 0.65;
/// Recessive bits that count as an idle bus before a start of frame.
const IDLE_BITS: f64 = 6.5;

pub(crate) struct CanResult {
    pub(crate) bitrate: f64,
    pub(crate) dominant_high: bool,
    pub(crate) events: Vec<(Event, Vec<Annotation>)>,
    /// Frames with a valid CRC.
    #[cfg_attr(not(test), expect(dead_code, reason = "read by tests"))]
    pub(crate) valid: usize,
}

/// Decode with the given or every candidate bitrate and polarity; the best result wins.
pub(crate) fn decode(
    logic: &Logic,
    dt: f64,
    t0: f64,
    bitrate: Option<f64>,
    dominant_high: Option<bool>,
) -> CanResult {
    let mut bitrates: Vec<f64> = match bitrate {
        Some(rate) => vec![rate],
        None => STANDARD_BITRATES.to_vec(),
    };
    // Try the bitrates closest to the measured unit pulse first (only matters for ties).
    if let Some(unit) = unit_run(&logic.inner_runs()) {
        let estimate = 1.0 / (unit * dt);
        bitrates.sort_by(|a, b| {
            (a.ln() - estimate.ln())
                .abs()
                .total_cmp(&(b.ln() - estimate.ln()).abs())
        });
    }
    let polarities: Vec<bool> = match dominant_high {
        Some(p) => vec![p],
        // The bus idles recessive: the level seen most often is recessive.
        None => {
            let high = logic.levels.iter().filter(|l| **l).count();
            let idle_high = high * 2 >= logic.levels.len();
            vec![!idle_high, idle_high]
        }
    };
    let mut best: Option<(i64, CanResult)> = None;
    for &dominant_high in &polarities {
        let dominant: Vec<bool> = logic.levels.iter().map(|l| *l == dominant_high).collect();
        for &rate in &bitrates {
            let samples_per_bit = 1.0 / (rate * dt);
            if samples_per_bit < 2.5 {
                continue;
            }
            let events = decode_at(&dominant, samples_per_bit, dt, t0);
            let valid = events
                .iter()
                .filter(|(e, _)| matches!(e, Event::CanFrame { crc_ok: true, .. }))
                .count();
            let errors = events.len() - valid;
            let score =
                i64::try_from(valid).unwrap_or(i64::MAX) * 100 - i64::try_from(errors).unwrap_or(0);
            if best.as_ref().is_none_or(|(s, _)| score > *s) {
                best = Some((
                    score,
                    CanResult {
                        bitrate: rate,
                        dominant_high,
                        events,
                        valid,
                    },
                ));
            }
        }
    }
    best.map(|(_, result)| result).unwrap_or(CanResult {
        bitrate: bitrate.unwrap_or(f64::NAN),
        dominant_high: dominant_high.unwrap_or(true),
        events: Vec::new(),
        valid: 0,
    })
}

/// Reads raw bits from the dominant/recessive samples with edge resynchronisation.
struct BitClock<'a> {
    dominant: &'a [bool],
    bit: f64,
    /// Start of the current bit, in (fractional) samples.
    start: f64,
}

impl BitClock<'_> {
    /// The next raw bit as (`dominant`, start sample, end sample).
    fn next(&mut self) -> Option<(bool, f64, f64)> {
        let point = self.start + SAMPLE_POINT * self.bit;
        let index = point.round() as usize;
        let &value = self.dominant.get(index)?;
        let begin = self.start;
        let nominal = self.start + self.bit;
        // Resync: an edge within ±40 % of the nominal boundary moves the bit grid onto it.
        let from = (nominal - 0.4 * self.bit).max(point + 1.0) as usize;
        let to = ((nominal + 0.4 * self.bit) as usize).min(self.dominant.len());
        let edge = (from..to).find(|&j| self.dominant[j] != value);
        self.start = edge.map_or(nominal, |j| j as f64 - 0.5);
        Some((value, begin, self.start))
    }
}

/// Bit destuffing on top of the clock. CAN bit values: dominant = 0, recessive = 1.
struct Destuffer {
    last: Option<bool>,
    run: u32,
}

enum FrameError {
    Stuff(f64),
    Truncated,
}

impl Destuffer {
    fn next(&mut self, clock: &mut BitClock<'_>) -> Result<(bool, f64, f64), FrameError> {
        loop {
            let (dominant, start, end) = clock.next().ok_or(FrameError::Truncated)?;
            let bit = !dominant;
            if self.run == 5 {
                if Some(bit) == self.last {
                    return Err(FrameError::Stuff(start));
                }
                self.last = Some(bit);
                self.run = 1;
                continue;
            }
            if Some(bit) == self.last {
                self.run += 1;
            } else {
                self.last = Some(bit);
                self.run = 1;
            }
            return Ok((bit, start, end));
        }
    }
}

/// CRC-15/CAN over the given bits.
pub(crate) fn crc15(bits: &[bool]) -> u16 {
    let mut crc: u16 = 0;
    for &bit in bits {
        let next = bit ^ ((crc >> 14) & 1 == 1);
        crc = (crc << 1) & 0x7FFF;
        if next {
            crc ^= 0x4599;
        }
    }
    crc
}

fn decode_at(dominant: &[bool], bit: f64, dt: f64, t0: f64) -> Vec<(Event, Vec<Annotation>)> {
    let time = |sample: f64| t0 + sample * dt;
    let mut events = Vec::new();
    // The bus counts as idle from the capture start if it starts recessive.
    let mut recessive_since: Option<usize> =
        (!dominant.first().copied().unwrap_or(true)).then_some(0);
    let mut i = 1;
    while i < dominant.len() {
        if dominant[i] && !dominant[i - 1] {
            let idle = recessive_since.is_some_and(|since| (i - since) as f64 >= IDLE_BITS * bit);
            if idle {
                let (event, notes, end) = parse_frame(dominant, i, bit, &time);
                events.push((event, notes));
                i = (end.ceil() as usize).max(i + 1);
                recessive_since = None;
                continue;
            }
            // Six dominant bits in a row break the stuffing rule: an error flag.
            let run = dominant[i..].iter().take_while(|d| **d).count();
            if run as f64 >= 5.6 * bit {
                events.push((
                    Event::CanError {
                        t_s: time(i as f64),
                        error: "error flag (6+ dominant bits)".into(),
                    },
                    vec![Annotation::new(
                        time(i as f64),
                        time((i + run) as f64),
                        "ERR",
                        Mark::Error,
                    )],
                ));
                i += run;
                continue;
            }
        }
        if !dominant[i] && (dominant[i - 1] || recessive_since.is_none()) {
            recessive_since = Some(i);
        }
        i += 1;
    }
    events
}

fn parse_frame(
    dominant: &[bool],
    sof: usize,
    bit: f64,
    time: &dyn Fn(f64) -> f64,
) -> (Event, Vec<Annotation>, f64) {
    let mut clock = BitClock {
        dominant,
        bit,
        start: sof as f64 - 0.5,
    };
    let mut stuff = Destuffer { last: None, run: 0 };
    let mut crc_bits: Vec<bool> = Vec::with_capacity(128);
    let mut notes: Vec<Annotation> = Vec::new();
    let start_t = time(sof as f64);

    let fail = |error: FrameError, notes: &mut Vec<Annotation>, at: f64| {
        let (text, position) = match error {
            FrameError::Stuff(position) => ("stuff error", position),
            FrameError::Truncated => ("frame runs past the capture", at),
        };
        notes.push(Annotation::new(
            time(position),
            time(position + bit),
            "ERR",
            Mark::Error,
        ));
        (
            Event::CanError {
                t_s: time(position),
                error: text.into(),
            },
            std::mem::take(notes),
            position + bit,
        )
    };

    macro_rules! field {
        ($bits:expr, $mark:expr, $label:expr) => {{
            let mut value: u32 = 0;
            let mut first = f64::NAN;
            let mut last = f64::NAN;
            for _ in 0..$bits {
                match stuff.next(&mut clock) {
                    Ok((b, s, e)) => {
                        crc_bits.push(b);
                        value = (value << 1) | u32::from(b);
                        if first.is_nan() {
                            first = s;
                        }
                        last = e;
                    }
                    Err(err) => {
                        let at = clock.start;
                        let (event, notes, end) = fail(err, &mut notes, at);
                        return (event, notes, end);
                    }
                }
            }
            let label: Option<String> = $label(value);
            if let Some(label) = label {
                notes.push(Annotation::new(time(first), time(last), &label, $mark));
            }
            (value, first, last)
        }};
    }

    let (_, _, id_start) = field!(1, Mark::Start, |_| Some("SOF".to_owned()));
    let (base_id, _, _) = field!(11, Mark::Address, |_| None);
    let (srr_rtr, _, _) = field!(1, Mark::Control, |_| None);
    let (ide, _, _) = field!(1, Mark::Control, |_| None);
    let extended = ide == 1;
    let (id, rtr) = if extended {
        let (ext, _, _) = field!(18, Mark::Address, |_| None);
        let (rtr, _, _) = field!(1, Mark::Control, |_| None);
        let _ = field!(2, Mark::Control, |_| None); // r1, r0
        ((base_id << 18) | ext, rtr == 1)
    } else {
        let _ = field!(1, Mark::Control, |_| None); // r0
        (base_id, srr_rtr == 1)
    };
    // One annotation for the whole identifier and control bits.
    let id_label = if extended {
        format!("ID {id:08X}{}", if rtr { " R" } else { "" })
    } else {
        format!("ID {id:03X}{}", if rtr { " R" } else { "" })
    };
    let (dlc, dlc_start, dlc_end) = field!(4, Mark::Control, |_| None);
    notes.push(Annotation::new(
        time(id_start),
        time(dlc_start),
        &id_label,
        Mark::Address,
    ));
    notes.push(Annotation::new(
        time(dlc_start),
        time(dlc_end),
        &format!("DLC {dlc}"),
        Mark::Control,
    ));
    let length = if rtr { 0 } else { dlc.min(8) as usize };
    let mut data = Vec::with_capacity(length);
    for _ in 0..length {
        let (byte, _, _) = field!(8, Mark::Data, |v: u32| Some(format!("{v:02X}")));
        data.push(byte as u8);
    }
    let computed = crc15(&crc_bits);
    let mut received: u32 = 0;
    let mut crc_first = f64::NAN;
    let mut crc_last = f64::NAN;
    for _ in 0..15 {
        match stuff.next(&mut clock) {
            Ok((b, s, e)) => {
                received = (received << 1) | u32::from(b);
                if crc_first.is_nan() {
                    crc_first = s;
                }
                crc_last = e;
            }
            Err(err) => {
                let at = clock.start;
                return fail(err, &mut notes, at);
            }
        }
    }
    let crc_ok = received == u32::from(computed);
    notes.push(Annotation::new(
        time(crc_first),
        time(crc_last),
        if crc_ok { "CRC" } else { "CRC!" },
        if crc_ok { Mark::Crc } else { Mark::Error },
    ));
    // CRC delimiter, ACK slot, ACK delimiter: not stuffed.
    let raw = |clock: &mut BitClock<'_>| clock.next();
    let Some(_) = raw(&mut clock) else {
        let at = clock.start;
        return fail(FrameError::Truncated, &mut notes, at);
    };
    let Some((ack_dominant, ack_start, ack_end)) = raw(&mut clock) else {
        let at = clock.start;
        return fail(FrameError::Truncated, &mut notes, at);
    };
    notes.push(Annotation::new(
        time(ack_start),
        time(ack_end),
        if ack_dominant { "ACK" } else { "NAK" },
        if ack_dominant { Mark::Ack } else { Mark::Nack },
    ));
    let _ = raw(&mut clock);
    // End of frame: seven recessive bits (best effort; the capture may end first).
    let eof_start = clock.start;
    let mut end = clock.start;
    for _ in 0..7 {
        match raw(&mut clock) {
            Some((_, _, e)) => end = e,
            None => break,
        }
    }
    if end > eof_start {
        notes.push(Annotation::new(
            time(eof_start),
            time(end),
            "EOF",
            Mark::Stop,
        ));
    }
    let event = Event::CanFrame {
        t_s: start_t,
        end_s: time(end),
        id,
        extended,
        rtr,
        dlc: dlc as u8,
        data,
        crc_ok,
        ack: ack_dominant,
    };
    (event, notes, end)
}

pub mod encode {
    //! A CAN frame encoder (for tests and the demo bus): field bits, CRC, stuffing, ACK, EOF.
    use super::crc15;

    fn push(bits: &mut Vec<bool>, value: u32, count: u32) {
        for i in (0..count).rev() {
            bits.push((value >> i) & 1 == 1);
        }
    }

    /// CAN bit values (true = recessive) of one frame plus 3 bits of intermission.
    pub fn frame(id: u32, extended: bool, rtr: bool, data: &[u8], acked: bool) -> Vec<bool> {
        let mut bits = vec![false]; // SOF
        if extended {
            push(&mut bits, id >> 18, 11);
            bits.push(true); // SRR
            bits.push(true); // IDE
            push(&mut bits, id & 0x3FFFF, 18);
            bits.push(rtr);
            push(&mut bits, 0, 2);
        } else {
            push(&mut bits, id, 11);
            bits.push(rtr);
            bits.push(false); // IDE
            bits.push(false); // r0
        }
        push(&mut bits, data.len() as u32, 4);
        if !rtr {
            for byte in data {
                push(&mut bits, u32::from(*byte), 8);
            }
        }
        let crc = crc15(&bits);
        push(&mut bits, u32::from(crc), 15);
        let mut stuffed = Vec::with_capacity(bits.len() + 20);
        let (mut last, mut run) = (None, 0);
        for bit in bits {
            if run == 5 {
                let stuff = !last.unwrap_or(true);
                stuffed.push(stuff);
                last = Some(stuff);
                run = 1;
            }
            if Some(bit) == last {
                run += 1;
            } else {
                last = Some(bit);
                run = 1;
            }
            stuffed.push(bit);
        }
        stuffed.push(true); // CRC delimiter
        stuffed.push(!acked); // ACK slot: dominant (false) when acknowledged
        stuffed.push(true); // ACK delimiter
        stuffed.extend([true; 7]); // EOF
        stuffed.extend([true; 3]); // intermission
        stuffed
    }
}

#[cfg(test)]
mod tests {
    use super::super::digital::{digitize, synth};
    use super::*;

    /// CAN_H-like levels (dominant high) or CAN_L-like (dominant low).
    fn bus(
        frames: &[Vec<bool>],
        idle_bits: usize,
        samples_per_bit: f64,
        dominant_high: bool,
    ) -> Vec<f32> {
        let mut bits = vec![true; idle_bits];
        for frame in frames {
            bits.extend(frame);
            bits.extend(vec![true; idle_bits]);
        }
        // `bits` are CAN values (true = recessive); map to the line level.
        let levels: Vec<bool> = bits
            .iter()
            .map(|recessive| *recessive != dominant_high)
            .collect();
        if dominant_high {
            synth::render(&levels, samples_per_bit, 0.1, 3.6, samples_per_bit * 0.08)
        } else {
            synth::render(&levels, samples_per_bit, 1.4, 5.0, samples_per_bit * 0.08)
        }
    }

    fn frames_of(result: &CanResult) -> Vec<(u32, bool, Vec<u8>, bool, bool)> {
        result
            .events
            .iter()
            .filter_map(|(e, _)| match e {
                Event::CanFrame {
                    id,
                    extended,
                    data,
                    crc_ok,
                    ack,
                    ..
                } => Some((*id, *extended, data.clone(), *crc_ok, *ack)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn decodes_standard_and_extended_frames_with_auto_bitrate_and_polarity() {
        let frames = vec![
            encode::frame(0x7B3, false, false, &[0x52], true),
            encode::frame(0x0200_0000, true, false, &[0x00, 0x64, 0x9C], false),
            encode::frame(0x00E, false, true, &[], true),
            encode::frame(
                0x123,
                false,
                false,
                &[0xFF, 0x00, 0xAA, 0x55, 0x01, 0x80, 0x7F, 0xFE],
                true,
            ),
        ];
        // 125 kbit/s at 2.5 MS/s (200 µs/div): 20 samples per bit, bit clock 0.7 % off.
        for dominant_high in [true, false] {
            let samples = bus(&frames, 14, 20.14, dominant_high);
            let logic = digitize(&samples);
            let result = decode(&logic, 1.0 / 2.5e6, 0.0, None, None);
            assert_eq!(result.bitrate, 125_000.0);
            assert_eq!(result.dominant_high, dominant_high);
            let decoded = frames_of(&result);
            assert_eq!(decoded.len(), 4, "{decoded:?}");
            assert_eq!(decoded[0], (0x7B3, false, vec![0x52], true, true));
            assert_eq!(
                decoded[1],
                (0x0200_0000, true, vec![0x00, 0x64, 0x9C], true, false)
            );
            assert_eq!(decoded[2].0, 0x00E);
            assert_eq!(
                decoded[3].2,
                vec![0xFF, 0x00, 0xAA, 0x55, 0x01, 0x80, 0x7F, 0xFE]
            );
            assert!(decoded.iter().all(|f| f.3), "CRC");
        }
    }

    #[test]
    fn decodes_at_four_samples_per_bit() {
        // 125 kbit/s at 500 kS/s (1 ms/div): the coarsest setting that still works.
        let frames = vec![encode::frame(0x7B3, false, false, &[0x52, 0x10], true)];
        let samples = bus(&frames, 12, 4.0, true);
        let result = decode(&digitize(&samples), 1.0 / 500e3, 0.0, Some(125_000.0), None);
        assert_eq!(
            frames_of(&result),
            vec![(0x7B3, false, vec![0x52, 0x10], true, true)]
        );
    }

    #[test]
    fn a_corrupted_bit_fails_the_crc_or_stuffing() {
        let mut frame = encode::frame(0x321, false, false, &[0x11, 0x22], true);
        frame[20] = !frame[20];
        let samples = bus(&[frame], 12, 16.0, true);
        let result = decode(
            &digitize(&samples),
            1.0 / 2e6,
            0.0,
            Some(125_000.0),
            Some(true),
        );
        assert_eq!(result.valid, 0);
        assert!(!result.events.is_empty());
    }
}
