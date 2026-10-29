//! I²C from SCL and SDA: START/repeated START/STOP, address with R/W, data bytes with ACK/NACK.

use super::{Annotation, Event, Mark};

pub(crate) fn decode(
    scl: &[bool],
    sda: &[bool],
    dt: f64,
    t0: f64,
) -> Vec<(Event, Vec<Annotation>)> {
    let time = |i: usize| t0 + i as f64 * dt;
    let mut events = Vec::new();
    let mut current: Option<Transfer> = None;
    let n = scl.len().min(sda.len());
    for i in 1..n {
        let scl_high = scl[i - 1] && scl[i];
        if scl_high && sda[i - 1] && !sda[i] {
            // START, or a repeated START that ends the running transfer.
            if let Some(transfer) = current.take() {
                events.push(transfer.finish(time(i), false));
            }
            let mut transfer = Transfer::new(time(i));
            transfer
                .notes
                .push(Annotation::new(time(i), time(i), "S", Mark::Start));
            current = Some(transfer);
            continue;
        }
        if scl_high && !sda[i - 1] && sda[i] {
            if let Some(mut transfer) = current.take() {
                transfer
                    .notes
                    .push(Annotation::new(time(i), time(i), "P", Mark::Stop));
                events.push(transfer.finish(time(i), true));
            }
            continue;
        }
        if !scl[i - 1]
            && scl[i]
            && let Some(transfer) = current.as_mut()
        {
            transfer.bit(sda[i], time(i));
        }
    }
    if let Some(transfer) = current.take() {
        let end = time(n.saturating_sub(1));
        events.push(transfer.finish(end, false));
    }
    events
}

struct Transfer {
    start: f64,
    bits: Vec<bool>,
    byte_start: f64,
    address: Option<(u8, bool, bool)>,
    data: Vec<u8>,
    acks: Vec<bool>,
    notes: Vec<Annotation>,
}

impl Transfer {
    fn new(start: f64) -> Self {
        Self {
            start,
            bits: Vec::new(),
            byte_start: start,
            address: None,
            data: Vec::new(),
            acks: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn bit(&mut self, value: bool, at: f64) {
        if self.bits.is_empty() {
            self.byte_start = at;
        }
        self.bits.push(value);
        if self.bits.len() == 8 {
            let byte = self
                .bits
                .iter()
                .fold(0u8, |acc, b| (acc << 1) | u8::from(*b));
            let label = if self.address.is_none() {
                format!(
                    "{:02X} {}",
                    byte >> 1,
                    if byte & 1 == 1 { "R" } else { "W" }
                )
            } else {
                format!("{byte:02X}")
            };
            let mark = if self.address.is_none() {
                Mark::Address
            } else {
                Mark::Data
            };
            self.notes
                .push(Annotation::new(self.byte_start, at, &label, mark));
        }
        if self.bits.len() == 9 {
            let byte = self.bits[..8]
                .iter()
                .fold(0u8, |acc, b| (acc << 1) | u8::from(*b));
            let ack = !self.bits[8];
            self.notes.push(Annotation::new(
                at,
                at,
                if ack { "A" } else { "N" },
                if ack { Mark::Ack } else { Mark::Nack },
            ));
            match self.address {
                None => self.address = Some((byte >> 1, byte & 1 == 1, ack)),
                Some(_) => {
                    self.data.push(byte);
                    self.acks.push(ack);
                }
            }
            self.bits.clear();
        }
    }

    fn finish(self, end: f64, stop: bool) -> (Event, Vec<Annotation>) {
        let (address, read, address_ack) = self.address.unwrap_or((0, false, false));
        (
            Event::I2c {
                t_s: self.start,
                end_s: end,
                address,
                read,
                address_ack,
                data: self.data,
                acks: self.acks,
                stop,
            },
            self.notes,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bit-bang an I²C write + repeated-start read at 4 samples per half clock.
    fn transaction() -> (Vec<bool>, Vec<bool>) {
        let (mut scl, mut sda) = (Vec::new(), Vec::new());
        let put = |c: bool, d: bool, n: usize, scl: &mut Vec<bool>, sda: &mut Vec<bool>| {
            for _ in 0..n {
                scl.push(c);
                sda.push(d);
            }
        };
        put(true, true, 10, &mut scl, &mut sda);
        let start = |scl: &mut Vec<bool>, sda: &mut Vec<bool>| {
            put(true, true, 2, scl, sda);
            put(true, false, 4, scl, sda);
            put(false, false, 2, scl, sda);
        };
        let byte = |value: u16, scl: &mut Vec<bool>, sda: &mut Vec<bool>| {
            for i in (0..9).rev() {
                let bit = (value >> i) & 1 == 1;
                put(false, bit, 2, scl, sda);
                put(true, bit, 4, scl, sda);
                put(false, bit, 2, scl, sda);
            }
        };
        start(&mut scl, &mut sda);
        byte((0x50 << 1) << 1, &mut scl, &mut sda); // address 0x50 write, ACK (0)
        byte(0x12 << 1, &mut scl, &mut sda); // data 0x12, ACK
        put(false, true, 2, &mut scl, &mut sda);
        start(&mut scl, &mut sda); // repeated start
        byte(((0x50 << 1) | 1) << 1, &mut scl, &mut sda); // read, ACK (low ninth bit)
        byte((0xA5 << 1) | 1, &mut scl, &mut sda); // data 0xA5, NACK
        put(false, false, 2, &mut scl, &mut sda);
        put(true, false, 3, &mut scl, &mut sda);
        put(true, true, 6, &mut scl, &mut sda); // STOP
        (scl, sda)
    }

    #[test]
    fn decodes_write_then_repeated_start_read() {
        let (scl, sda) = transaction();
        let events = decode(&scl, &sda, 1e-6, 0.0);
        assert_eq!(events.len(), 2);
        match &events[0].0 {
            Event::I2c {
                address,
                read,
                address_ack,
                data,
                stop,
                ..
            } => {
                assert_eq!(
                    (*address, *read, *address_ack, data.as_slice(), *stop),
                    (0x50, false, true, &[0x12][..], false)
                );
            }
            other => panic!("{other:?}"),
        }
        match &events[1].0 {
            Event::I2c {
                address,
                read,
                data,
                acks,
                stop,
                ..
            } => {
                assert_eq!(
                    (*address, *read, data.as_slice(), acks.as_slice(), *stop),
                    (0x50, true, &[0xA5][..], &[false][..], true)
                );
            }
            other => panic!("{other:?}"),
        }
    }
}
