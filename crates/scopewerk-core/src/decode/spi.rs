//! SPI from a clock and one data line (the DOS1102 has two channels, so no chip select):
//! words are aligned by pauses in the clock. The clock's idle level gives CPOL; CPHA is a
//! parameter (mode 0 when unknown).

use super::{Annotation, Event, Mark};

pub(crate) struct SpiResult {
    pub(crate) mode: u8,
    pub(crate) clock_hz: Option<f64>,
    pub(crate) events: Vec<(Event, Vec<Annotation>)>,
}

pub(crate) fn decode(
    clk: &[bool],
    data: &[bool],
    dt: f64,
    t0: f64,
    mode: Option<u8>,
    bits: u8,
    lsb_first: bool,
) -> SpiResult {
    let time = |i: usize| t0 + i as f64 * dt;
    let n = clk.len().min(data.len());
    // CPOL: the level the clock rests at during its longest pause.
    let cpol = mode.map_or_else(|| idle_level(clk), |m| m >= 2);
    let cpha = mode.is_some_and(|m| m % 2 == 1);
    let mode = u8::from(cpol) * 2 + u8::from(cpha);
    // Modes 0 and 3 sample on the rising edge, 1 and 2 on the falling edge.
    let sample_rising = cpol == cpha;
    let edges: Vec<usize> = (1..n)
        .filter(|&i| clk[i] != clk[i - 1] && clk[i] == sample_rising)
        .collect();
    let mut periods: Vec<usize> = edges.windows(2).map(|w| w[1] - w[0]).collect();
    periods.sort_unstable();
    let period = periods.get(periods.len() / 4).copied().unwrap_or(0);
    let clock_hz = (period > 0).then(|| 1.0 / (period as f64 * dt));
    // A pause longer than four clock periods starts a new transfer.
    let gap = period.max(1) * 4;

    let mut events = Vec::new();
    let mut words: Vec<u32> = Vec::new();
    let mut notes: Vec<Annotation> = Vec::new();
    let mut word: u32 = 0;
    let mut count = 0;
    let mut word_start = 0;
    let mut transfer_start = edges.first().copied().unwrap_or(0);
    let mut previous: Option<usize> = None;
    let flush = |words: &mut Vec<u32>,
                 notes: &mut Vec<Annotation>,
                 events: &mut Vec<(Event, Vec<Annotation>)>,
                 start: usize,
                 end: usize| {
        if !words.is_empty() {
            events.push((
                Event::Spi {
                    t_s: time(start),
                    end_s: time(end),
                    words: std::mem::take(words),
                },
                std::mem::take(notes),
            ));
        }
    };
    for &edge in &edges {
        if let Some(last) = previous
            && edge - last > gap
        {
            flush(&mut words, &mut notes, &mut events, transfer_start, last);
            transfer_start = edge;
            count = 0;
            word = 0;
        }
        if count == 0 {
            word_start = edge;
        }
        let bit = u32::from(data[edge]);
        if lsb_first {
            word |= bit << count;
        } else {
            word = (word << 1) | bit;
        }
        count += 1;
        if count == bits {
            let label = if bits <= 8 {
                format!("{word:02X}")
            } else {
                format!("{word:04X}")
            };
            notes.push(Annotation::new(
                time(word_start),
                time(edge),
                &label,
                Mark::Data,
            ));
            words.push(word);
            word = 0;
            count = 0;
        }
        previous = Some(edge);
    }
    if let Some(last) = previous {
        flush(&mut words, &mut notes, &mut events, transfer_start, last);
    }
    SpiResult {
        mode,
        clock_hz,
        events,
    }
}

fn idle_level(clk: &[bool]) -> bool {
    let (mut best_len, mut best_level, mut start) = (0, false, 0);
    for i in 1..=clk.len() {
        if i == clk.len() || clk[i] != clk[i - 1] {
            if i - start > best_len {
                best_len = i - start;
                best_level = clk[start];
            }
            start = i;
        }
    }
    best_level
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus(words: &[u8], cpol: bool, cpha: bool) -> (Vec<bool>, Vec<bool>) {
        let (mut clk, mut mosi) = (vec![cpol; 40], vec![false; 40]);
        for (index, byte) in words.iter().enumerate() {
            for i in (0..8).rev() {
                let bit = (byte >> i) & 1 == 1;
                // CPHA 0: data is set before the leading edge, which samples it. CPHA 1: data
                // changes on the leading edge and the trailing edge samples it.
                let (first, second) = if cpha { (!cpol, cpol) } else { (cpol, !cpol) };
                for _ in 0..3 {
                    clk.push(first);
                    mosi.push(bit);
                }
                for _ in 0..3 {
                    clk.push(second);
                    mosi.push(bit);
                }
            }
            // A pause between the two transfers.
            let pause = if index == 1 { 60 } else { 0 };
            for _ in 0..pause {
                clk.push(cpol);
                mosi.push(false);
            }
        }
        clk.extend(vec![cpol; 40]);
        mosi.extend(vec![false; 40]);
        (clk, mosi)
    }

    #[test]
    fn decodes_words_and_splits_transfers_on_pauses() {
        for (cpol, mode) in [(false, 0u8), (true, 3u8)] {
            let (clk, mosi) = bus(&[0x9F, 0x01, 0xC2, 0x20], cpol, cpol);
            let result = decode(
                &clk,
                &mosi,
                1e-7,
                0.0,
                if cpol { Some(3) } else { None },
                8,
                false,
            );
            assert_eq!(result.mode, mode);
            let transfers: Vec<Vec<u32>> = result
                .events
                .iter()
                .map(|(e, _)| match e {
                    Event::Spi { words, .. } => words.clone(),
                    other => panic!("{other:?}"),
                })
                .collect();
            assert_eq!(transfers, vec![vec![0x9F, 0x01], vec![0xC2, 0x20]]);
            assert!((result.clock_hz.expect("clock") - 1.0 / 6e-7).abs() < 1e3);
        }
    }
}
