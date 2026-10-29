//! Protocol decoding on captured traces, like a two-channel logic analyzer: CAN (one wire or
//! differential), I²C (SCL + SDA), SPI (clock + one data line) and UART. Thresholds, CAN
//! bitrate and polarity, UART baud and SPI clock polarity are detected automatically.
//!
//! Decode the acquisition memory (10 000 samples) for resolution; [`recommended_timebase`]
//! picks a timebase with enough samples per bit.

pub mod can;
mod digital;
mod i2c;
mod spi;
mod uart;

use serde::{Deserialize, Serialize};

pub use can::STANDARD_BITRATES;
pub use uart::{Parity, STANDARD_BAUDS};

use crate::Error;
use crate::units::{format_si, nearest_step, timebase_steps};
use crate::waveform::Capture;

/// Which signal a single-line decoder reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Input {
    #[default]
    Ch1,
    Ch2,
    /// CH1 − CH2, e.g. CAN_H on CH1 and CAN_L on CH2.
    Ch1MinusCh2,
    Ch2MinusCh1,
}

impl Input {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ch1 => "CH1",
            Self::Ch2 => "CH2",
            Self::Ch1MinusCh2 => "CH1−CH2",
            Self::Ch2MinusCh1 => "CH2−CH1",
        }
    }

    fn channels(self) -> &'static [u8] {
        match self {
            Self::Ch1 => &[1],
            Self::Ch2 => &[2],
            Self::Ch1MinusCh2 | Self::Ch2MinusCh1 => &[1, 2],
        }
    }
}

impl std::str::FromStr for Input {
    type Err = Error;
    fn from_str(text: &str) -> Result<Self, Error> {
        match text
            .trim()
            .to_ascii_lowercase()
            .replace([' ', '_', '−'], "-")
            .as_str()
        {
            "1" | "ch1" => Ok(Self::Ch1),
            "2" | "ch2" => Ok(Self::Ch2),
            "diff" | "ch1-ch2" | "1-2" | "differential" => Ok(Self::Ch1MinusCh2),
            "ch2-ch1" | "2-1" => Ok(Self::Ch2MinusCh1),
            other => Err(Error::InvalidArgument(format!(
                "input {other:?} (use ch1, ch2, ch1-ch2 or ch2-ch1)"
            ))),
        }
    }
}

fn default_one() -> u8 {
    1
}
fn default_two() -> u8 {
    2
}
fn default_bits() -> u8 {
    8
}

/// What to decode and how.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
pub enum DecodeConfig {
    Can {
        #[serde(default)]
        input: Input,
        /// Bits per second; `None` tries the standard rates.
        #[serde(default)]
        bitrate: Option<f64>,
        /// `true` for CAN_H or CH1−CH2, `false` for CAN_L or a transceiver's RX pin; `None` detects.
        #[serde(default)]
        dominant_high: Option<bool>,
    },
    I2c {
        #[serde(default = "default_one")]
        scl: u8,
        #[serde(default = "default_two")]
        sda: u8,
    },
    Spi {
        #[serde(default = "default_one")]
        clk: u8,
        #[serde(default = "default_two")]
        data: u8,
        /// SPI mode 0–3; `None` takes CPOL from the idle clock and assumes CPHA 0.
        #[serde(default)]
        mode: Option<u8>,
        #[serde(default = "default_bits")]
        bits: u8,
        #[serde(default)]
        lsb_first: bool,
    },
    Uart {
        #[serde(default)]
        input: Input,
        #[serde(default)]
        baud: Option<f64>,
        #[serde(default = "default_bits")]
        data_bits: u8,
        #[serde(default)]
        parity: Parity,
        /// Idle-low line (e.g. RS-232 levels before the transceiver).
        #[serde(default)]
        inverted: bool,
    },
}

impl DecodeConfig {
    pub fn protocol(&self) -> &'static str {
        match self {
            Self::Can { .. } => "CAN",
            Self::I2c { .. } => "I²C",
            Self::Spi { .. } => "SPI",
            Self::Uart { .. } => "UART",
        }
    }

    /// The channels that must be switched on and captured.
    pub fn channels(&self) -> Vec<u8> {
        let mut channels = match self {
            Self::Can { input, .. } | Self::Uart { input, .. } => input.channels().to_vec(),
            Self::I2c { scl, sda } => vec![*scl, *sda],
            Self::Spi { clk, data, .. } => vec![*clk, *data],
        };
        channels.sort_unstable();
        channels.dedup();
        channels
    }

    /// The bit rate in use, when known before decoding.
    fn bitrate(&self) -> Option<f64> {
        match self {
            Self::Can { bitrate, .. } => *bitrate,
            Self::Uart { baud, .. } => *baud,
            Self::I2c { .. } | Self::Spi { .. } => None,
        }
    }
}

/// Kinds of annotation, for colouring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mark {
    Start,
    Stop,
    Address,
    Data,
    Control,
    Crc,
    Ack,
    Nack,
    Error,
}

/// A labelled span on the time axis (seconds from the trigger).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Annotation {
    pub start_s: f64,
    pub end_s: f64,
    pub text: String,
    pub mark: Mark,
}

impl Annotation {
    pub(crate) fn new(start_s: f64, end_s: f64, text: &str, mark: Mark) -> Self {
        Self {
            start_s,
            end_s,
            text: text.to_owned(),
            mark,
        }
    }
}

/// One decoded unit. Times are seconds from the trigger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    CanFrame {
        t_s: f64,
        end_s: f64,
        id: u32,
        extended: bool,
        rtr: bool,
        dlc: u8,
        data: Vec<u8>,
        crc_ok: bool,
        /// Some other node pulled the ACK slot dominant.
        ack: bool,
    },
    CanError {
        t_s: f64,
        error: String,
    },
    I2c {
        t_s: f64,
        end_s: f64,
        address: u8,
        read: bool,
        address_ack: bool,
        data: Vec<u8>,
        acks: Vec<bool>,
        /// Ended with STOP (false: repeated START or end of capture).
        stop: bool,
    },
    Spi {
        t_s: f64,
        end_s: f64,
        words: Vec<u32>,
    },
    Uart {
        t_s: f64,
        value: u8,
        framing_error: bool,
        parity_error: bool,
    },
}

impl Event {
    pub fn t_s(&self) -> f64 {
        match self {
            Self::CanFrame { t_s, .. }
            | Self::CanError { t_s, .. }
            | Self::I2c { t_s, .. }
            | Self::Spi { t_s, .. }
            | Self::Uart { t_s, .. } => *t_s,
        }
    }

    /// One line for logs and tables.
    pub fn describe(&self) -> String {
        let hex = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        match self {
            Self::CanFrame {
                id,
                extended,
                rtr,
                dlc,
                data,
                crc_ok,
                ack,
                ..
            } => {
                let id = if *extended {
                    format!("{id:08X}")
                } else {
                    format!("{id:03X}")
                };
                let body = if *rtr {
                    format!("RTR [{dlc}]")
                } else {
                    format!("[{dlc}] {}", hex(data))
                };
                let crc = if *crc_ok { "" } else { "  CRC error" };
                let ack = if *ack { "" } else { "  no ACK" };
                format!("{id}  {body}{crc}{ack}")
            }
            Self::CanError { error, .. } => error.clone(),
            Self::I2c {
                address,
                read,
                address_ack,
                data,
                acks,
                stop,
                ..
            } => {
                let direction = if *read { "read" } else { "write" };
                let nak = if *address_ack { "" } else { " (address NACK)" };
                let last_nack = acks.last().is_some_and(|a| !a) && *read;
                let end = if *stop { "" } else { " (no STOP)" };
                let nack_note = if !last_nack && acks.iter().any(|a| !a) {
                    " NACK"
                } else {
                    ""
                };
                format!(
                    "0x{address:02X} {direction}{nak}: {}{nack_note}{end}",
                    hex(data)
                )
            }
            Self::Spi { words, .. } => words
                .iter()
                .map(|w| format!("{w:02X}"))
                .collect::<Vec<_>>()
                .join(" "),
            Self::Uart {
                value,
                framing_error,
                parity_error,
                ..
            } => {
                let ch = if value.is_ascii_graphic() || *value == b' ' {
                    format!("'{}'", *value as char)
                } else {
                    String::new()
                };
                let errors = match (framing_error, parity_error) {
                    (true, _) => "  framing error",
                    (false, true) => "  parity error",
                    _ => "",
                };
                format!("{value:02X} {ch}{errors}")
            }
        }
    }

    pub fn is_ok(&self) -> bool {
        match self {
            Self::CanFrame { crc_ok, .. } => *crc_ok,
            Self::CanError { .. } => false,
            Self::I2c { address_ack, .. } => *address_ack,
            Self::Spi { .. } => true,
            Self::Uart {
                framing_error,
                parity_error,
                ..
            } => !framing_error && !parity_error,
        }
    }
}

/// What the decoder found, with the parameters it settled on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decoded {
    pub protocol: String,
    pub summary: String,
    /// Bit rate, baud or SPI clock in Hz.
    pub rate_hz: Option<f64>,
    pub samples_per_bit: Option<f64>,
    /// CAN polarity, SPI mode, input used, …
    pub detail: String,
    pub thresholds_v: Vec<(u8, f64)>,
    pub events: Vec<Event>,
    pub annotations: Vec<Annotation>,
    pub warnings: Vec<String>,
}

/// Largest 1-2-5 timebase that still gives `samples_per_bit` in the 10 000-sample memory
/// (the DOS1102 samples 500 points per division).
pub fn recommended_timebase(bitrate: f64, samples_per_bit: f64) -> f64 {
    let limit = 500.0 / (bitrate * samples_per_bit);
    let steps = timebase_steps();
    steps
        .iter()
        .rev()
        .copied()
        .find(|s| *s <= limit * 1.001)
        .unwrap_or_else(|| nearest_step(&steps, limit))
}

fn input_volts(capture: &Capture, input: Input) -> Result<Vec<f32>, Error> {
    let channel = |n: u8| {
        capture.waveform(n).map(|w| w.volts.clone()).ok_or_else(|| {
            Error::InvalidArgument(format!("CH{n} is not in the capture; switch it on"))
        })
    };
    Ok(match input {
        Input::Ch1 => channel(1)?,
        Input::Ch2 => channel(2)?,
        Input::Ch1MinusCh2 | Input::Ch2MinusCh1 => {
            let (a, b) = (channel(1)?, channel(2)?);
            let sign = if input == Input::Ch1MinusCh2 {
                1.0
            } else {
                -1.0
            };
            a.iter().zip(&b).map(|(x, y)| sign * (x - y)).collect()
        }
    })
}

fn volts_per_div(capture: &Capture, input: Input) -> f64 {
    input
        .channels()
        .iter()
        .filter_map(|n| capture.settings.channel(*n).map(|c| c.volts_per_div))
        .fold(0.0, f64::max)
}

/// Decode a capture.
pub fn decode(capture: &Capture, config: &DecodeConfig) -> Result<Decoded, Error> {
    let dt = capture.dt;
    let t0 = capture.t0;
    let mut warnings = Vec::new();
    let mut check = |name: &str, logic: &digital::Logic, vpd: f64| {
        if logic.transitions() == 0 || logic.swing() < vpd * 0.4 {
            warnings.push(format!(
                "{name} shows no logic signal (swing {} at {}/div); check the probe, V/div and timebase",
                format_si(logic.swing(), "V"),
                format_si(vpd, "V")
            ));
        }
    };
    let (events, rate_hz, detail, thresholds) = match config {
        DecodeConfig::Can {
            input,
            bitrate,
            dominant_high,
        } => {
            let logic = digital::digitize(&input_volts(capture, *input)?);
            check(input.label(), &logic, volts_per_div(capture, *input));
            let result = can::decode(&logic, dt, t0, *bitrate, *dominant_high);
            let polarity = if result.dominant_high {
                "dominant high (CAN_H-like)"
            } else {
                "dominant low (CAN_L / RX-like)"
            };
            let ack_missing = result
                .events
                .iter()
                .filter(|(e, _)| {
                    matches!(
                        e,
                        Event::CanFrame {
                            crc_ok: true,
                            ack: false,
                            ..
                        }
                    )
                })
                .count();
            if ack_missing > 0 {
                warnings.push(format!(
                    "{ack_missing} valid frame(s) without ACK: no other node acknowledged them"
                ));
            }
            (
                result.events,
                Some(result.bitrate),
                format!("{} {polarity}", input.label()),
                vec![(input_channel(*input), logic.threshold_v)],
            )
        }
        DecodeConfig::Uart {
            input,
            baud,
            data_bits,
            parity,
            inverted,
        } => {
            let logic = digital::digitize(&input_volts(capture, *input)?);
            check(input.label(), &logic, volts_per_div(capture, *input));
            let format = uart::Format {
                baud: *baud,
                data_bits: *data_bits,
                parity: *parity,
                idle_high: !inverted,
            };
            let result = uart::decode(&logic.levels, &logic.inner_runs(), dt, t0, &format);
            let text: String = result
                .events
                .iter()
                .filter_map(|(e, _)| match e {
                    Event::Uart { value, .. } if value.is_ascii_graphic() || *value == b' ' => {
                        Some(*value as char)
                    }
                    _ => None,
                })
                .collect();
            let detail = format!(
                "{} {data_bits}{}1{}",
                input.label(),
                match parity {
                    Parity::None => "N",
                    Parity::Even => "E",
                    Parity::Odd => "O",
                },
                if *inverted { " inverted" } else { "" }
            );
            let detail = if text.is_empty() {
                detail
            } else {
                format!("{detail}, text {text:?}")
            };
            (
                result.events,
                result.baud,
                detail,
                vec![(input_channel(*input), logic.threshold_v)],
            )
        }
        DecodeConfig::I2c { scl, sda } => {
            let (scl_logic, sda_logic) = (logic_of(capture, *scl)?, logic_of(capture, *sda)?);
            check(
                &format!("SCL (CH{scl})"),
                &scl_logic,
                channel_vpd(capture, *scl),
            );
            check(
                &format!("SDA (CH{sda})"),
                &sda_logic,
                channel_vpd(capture, *sda),
            );
            let events = i2c::decode(&scl_logic.levels, &sda_logic.levels, dt, t0);
            let clock =
                digital::unit_run(&scl_logic.inner_runs()).map(|half| 1.0 / (2.0 * half * dt));
            (
                events,
                clock,
                format!("SCL CH{scl}, SDA CH{sda}"),
                vec![(*scl, scl_logic.threshold_v), (*sda, sda_logic.threshold_v)],
            )
        }
        DecodeConfig::Spi {
            clk,
            data,
            mode,
            bits,
            lsb_first,
        } => {
            let (clk_logic, data_logic) = (logic_of(capture, *clk)?, logic_of(capture, *data)?);
            check(
                &format!("CLK (CH{clk})"),
                &clk_logic,
                channel_vpd(capture, *clk),
            );
            let result = spi::decode(
                &clk_logic.levels,
                &data_logic.levels,
                dt,
                t0,
                *mode,
                (*bits).clamp(1, 32),
                *lsb_first,
            );
            if mode.is_none() {
                warnings.push(format!("SPI mode {} assumed (CPOL from the idle clock, CPHA 0); set mode if bytes look shifted", result.mode));
            }
            (
                result.events,
                result.clock_hz,
                format!(
                    "CLK CH{clk}, data CH{data}, mode {}, {bits} bit {}",
                    result.mode,
                    if *lsb_first { "LSB first" } else { "MSB first" }
                ),
                vec![
                    (*clk, clk_logic.threshold_v),
                    (*data, data_logic.threshold_v),
                ],
            )
        }
    };

    let samples_per_bit = rate_hz.filter(|r| r.is_finite()).map(|r| 1.0 / (r * dt));
    let timebase = capture.settings.timebase_s_per_div;
    if let Some(spb) = samples_per_bit {
        let rate = rate_hz.unwrap_or(f64::NAN);
        let factor = if matches!(config, DecodeConfig::I2c { .. } | DecodeConfig::Spi { .. }) {
            2.0
        } else {
            1.0
        };
        if spb / factor < 4.0 {
            warnings.push(format!(
                "only {:.1} samples per bit; set the timebase to {}/div or faster (and decode the memory)",
                spb / factor,
                format_si(recommended_timebase(rate * factor, 10.0), "s")
            ));
        }
    } else if events.is_empty()
        && let Some(rate) = config.bitrate()
    {
        warnings.push(format!(
            "nothing decoded; at {} a timebase of {}/div gives 10 samples per bit",
            format_si(rate, "bit/s"),
            format_si(recommended_timebase(rate, 10.0), "s")
        ));
    }
    if events.is_empty() && warnings.is_empty() {
        warnings.push(format!(
            "no {} traffic in this {} window; use single shot with a trigger on the bus to catch it",
            config.protocol(),
            format_si(capture.dt * capture.points() as f64, "s")
        ));
    }
    let _ = timebase;

    let ok = events.iter().filter(|(e, _)| e.is_ok()).count();
    let rate_text = rate_hz.filter(|r| r.is_finite()).map(|r| {
        format!(
            " at {}",
            format_si(
                r,
                if matches!(config, DecodeConfig::Uart { .. }) {
                    "Bd"
                } else if matches!(config, DecodeConfig::Can { .. }) {
                    "bit/s"
                } else {
                    "Hz"
                }
            )
        )
    });
    let summary = format!(
        "{} {} {}{}{}",
        events.len(),
        config.protocol(),
        match config {
            DecodeConfig::Can { .. } => "frame(s)",
            DecodeConfig::I2c { .. } | DecodeConfig::Spi { .. } => "transfer(s)",
            DecodeConfig::Uart { .. } => "byte(s)",
        },
        if ok == events.len() {
            String::new()
        } else {
            format!(", {} with errors", events.len() - ok)
        },
        rate_text.unwrap_or_default(),
    );
    let (events, notes): (Vec<Event>, Vec<Vec<Annotation>>) = events.into_iter().unzip();
    Ok(Decoded {
        protocol: config.protocol().to_owned(),
        summary,
        rate_hz: rate_hz.filter(|r| r.is_finite()),
        samples_per_bit,
        detail,
        thresholds_v: thresholds,
        events,
        annotations: notes.into_iter().flatten().collect(),
        warnings,
    })
}

fn input_channel(input: Input) -> u8 {
    if input == Input::Ch2 { 2 } else { 1 }
}

fn channel_vpd(capture: &Capture, channel: u8) -> f64 {
    capture
        .settings
        .channel(channel)
        .map_or(0.0, |c| c.volts_per_div)
}

fn logic_of(capture: &Capture, channel: u8) -> Result<digital::Logic, Error> {
    let input = if channel == 2 { Input::Ch2 } else { Input::Ch1 };
    Ok(digital::digitize(&input_volts(capture, input)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{ChannelSettings, Settings, TriggerSettings};
    use crate::waveform::Waveform;

    fn capture(ch1: Vec<f32>, ch2: Vec<f32>, dt: f64) -> Capture {
        let channel = |n| ChannelSettings {
            channel: n,
            enabled: true,
            volts_per_div: 1.0,
            offset_div: 0.0,
            coupling: "DC".into(),
            probe: "1X".into(),
            inverted: false,
        };
        Capture {
            captured_at_ms: 0,
            settings: Settings {
                timebase_s_per_div: 200e-6,
                horizontal_offset_div: 0.0,
                sample_rate_sa_s: Some(1.0 / dt),
                memory_depth: "10K".into(),
                acquisition: "SAMPle".into(),
                run_status: "STOP".into(),
                channels: vec![channel(1), channel(2)],
                trigger: TriggerSettings {
                    mode: "SINGle".into(),
                    kind: "EDGE".into(),
                    source: "CH1".into(),
                    level_v: Some(1.0),
                    edge: "FALL".into(),
                    coupling: "DC".into(),
                    holdoff_s: None,
                    sweep: "SINGle".into(),
                },
            },
            dt,
            t0: -0.002,
            waveforms: vec![
                Waveform {
                    channel: 1,
                    volts: ch1,
                },
                Waveform {
                    channel: 2,
                    volts: ch2,
                },
            ],
        }
    }

    #[test]
    fn decodes_differential_can_from_two_channels() {
        // Fault-tolerant CAN: CAN_H 0 → 3.6 V and CAN_L 5 → 1.4 V when dominant.
        let mut bits = vec![true; 15];
        bits.extend(can::encode::frame(0x7B3, false, false, &[0x52], true));
        bits.extend(vec![true; 30]);
        let h_levels: Vec<bool> = bits.iter().map(|recessive| !recessive).collect();
        let l_levels: Vec<bool> = bits.clone();
        let spb = 20.0;
        let ch1 = digital::synth::render(&h_levels, spb, 0.0, 3.6, 1.5);
        let ch2 = digital::synth::render(&l_levels, spb, 1.4, 5.0, 1.5);
        let capture = capture(ch1, ch2, 1.0 / 2.5e6);
        let decoded = decode(
            &capture,
            &DecodeConfig::Can {
                input: Input::Ch1MinusCh2,
                bitrate: None,
                dominant_high: None,
            },
        )
        .expect("decodes");
        assert_eq!(decoded.rate_hz, Some(125_000.0));
        assert_eq!(decoded.events.len(), 1, "{decoded:?}");
        assert_eq!(decoded.events[0].describe(), "7B3  [1] 52");
        assert!(decoded.warnings.is_empty(), "{:?}", decoded.warnings);
        assert!(decoded.annotations.iter().any(|a| a.text == "ID 7B3"));
    }

    #[test]
    fn the_i2c_and_uart_demos_decode() {
        let i2c =
            decode(&demo::i2c_capture(1), &DecodeConfig::I2c { scl: 1, sda: 2 }).expect("i2c");
        let lines: Vec<String> = i2c.events.iter().map(Event::describe).collect();
        assert_eq!(lines, vec!["0x48 write: 00", "0x48 read: 1A 80"], "{i2c:?}");
        let config = DecodeConfig::Uart {
            input: Input::Ch1,
            baud: None,
            data_bits: 8,
            parity: Parity::None,
            inverted: false,
        };
        let uart = decode(&demo::uart_capture(0), &config).expect("uart");
        assert_eq!(uart.rate_hz, Some(115_200.0));
        assert!(uart.detail.contains("MEAS:VOLT?"), "{}", uart.detail);
    }

    #[test]
    fn the_demo_bus_decodes_on_every_input() {
        let capture = demo::can_capture(4);
        for input in [Input::Ch1, Input::Ch2, Input::Ch1MinusCh2] {
            let decoded = decode(
                &capture,
                &DecodeConfig::Can {
                    input,
                    bitrate: None,
                    dominant_high: None,
                },
            )
            .expect("decodes");
            let frames: Vec<String> = decoded.events.iter().map(Event::describe).collect();
            assert_eq!(frames.len(), 3, "{input:?}: {frames:?}");
            assert!(frames[1].starts_with("7B3  RTR"), "{frames:?}");
            assert!(frames[2].starts_with("02000000"), "{frames:?}");
        }
    }

    #[test]
    fn recommends_a_timebase_with_enough_samples() {
        // 125 kbit/s with 10 samples per bit needs 1.25 MS/s: 400 µs/div → 200 µs/div step.
        assert!((recommended_timebase(125_000.0, 10.0) - 200e-6).abs() < 1e-12);
        assert!((recommended_timebase(500_000.0, 10.0) - 100e-6).abs() < 1e-12);
    }
}

pub mod demo {
    //! Synthetic two-channel captures for trying Scopewerk without a scope (`SCOPEWERK_DEMO`):
    //! a fault-tolerant CAN bus (CAN_H on CH1, CAN_L on CH2), an I²C sensor read (SCL on CH1, SDA
    //! on CH2) and a UART conversation (TX on CH1, RX on CH2). Each is a 10 000-sample memory
    //! read with realistic edges, noise and 8-bit quantization.

    use super::can::encode;
    use crate::header::{ChannelSettings, Settings, TriggerSettings};
    use crate::waveform::{Capture, Waveform};

    /// Which bus the demo shows.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Kind {
        Can,
        I2c,
        Uart,
    }

    impl Kind {
        pub fn label(self) -> &'static str {
            match self {
                Self::Can => "CAN",
                Self::I2c => "I²C",
                Self::Uart => "UART",
            }
        }

        /// From `SCOPEWERK_DEMO`: `i2c`, `uart`, anything else (e.g. `1`) is CAN.
        pub fn from_env() -> Option<Self> {
            let value = std::env::var("SCOPEWERK_DEMO")
                .ok()
                .filter(|v| !v.is_empty())?;
            Some(match value.to_ascii_lowercase().as_str() {
                "i2c" => Self::I2c,
                "uart" | "serial" => Self::Uart,
                _ => Self::Can,
            })
        }
    }

    const POINTS: usize = 10_000;

    /// One sample per level, with an RC edge, noise and quantization to 1/25 division.
    fn render(levels: &[bool], low: f64, high: f64, smoothing: f64, seed: u32) -> Vec<f32> {
        let step = (high - low).abs() / 50.0;
        let mut v = if levels.first().copied().unwrap_or(true) {
            high
        } else {
            low
        };
        let mut state = seed | 1;
        levels
            .iter()
            .map(|level| {
                let target = if *level { high } else { low };
                v += (target - v) * smoothing;
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let noise = (f64::from(state % 1000) / 1000.0 - 0.5) * step * 2.0;
                (((v + noise) / step).round() * step) as f32
            })
            .collect()
    }

    fn stretch(bits: &[bool], samples_per_bit: f64) -> Vec<bool> {
        (0..POINTS)
            .map(|i| {
                bits.get((i as f64 / samples_per_bit) as usize)
                    .copied()
                    .unwrap_or(true)
            })
            .collect()
    }

    fn channel(c: u8, volts_per_div: f64, offset_div: f64) -> ChannelSettings {
        ChannelSettings {
            channel: c,
            enabled: true,
            volts_per_div,
            offset_div,
            coupling: "DC".into(),
            probe: "1X".into(),
            inverted: false,
        }
    }

    fn capture(
        timebase: f64,
        channels: Vec<ChannelSettings>,
        trigger: (&str, f64, &str),
        waves: [Vec<f32>; 2],
    ) -> Capture {
        let rate = 500.0 / timebase;
        let [ch1, ch2] = waves;
        Capture {
            captured_at_ms: 0,
            settings: Settings {
                timebase_s_per_div: timebase,
                horizontal_offset_div: 0.0,
                sample_rate_sa_s: Some(rate),
                memory_depth: "10K".into(),
                acquisition: "SAMPle".into(),
                run_status: "TRIG".into(),
                channels,
                trigger: TriggerSettings {
                    mode: "SINGle".into(),
                    kind: "EDGE".into(),
                    source: trigger.0.into(),
                    level_v: Some(trigger.1),
                    edge: trigger.2.into(),
                    coupling: "DC".into(),
                    holdoff_s: Some(1e-7),
                    sweep: "NORMal".into(),
                },
            },
            dt: 1.0 / rate,
            t0: -(POINTS as f64 / 2.0) / rate,
            waveforms: vec![
                Waveform {
                    channel: 1,
                    volts: ch1,
                },
                Waveform {
                    channel: 2,
                    volts: ch2,
                },
            ],
        }
    }

    /// Frame `n` of the chosen demo; the data changes with `n`.
    pub fn capture_of(kind: Kind, n: u32) -> Capture {
        match kind {
            Kind::Can => can_capture(n),
            Kind::I2c => i2c_capture(n),
            Kind::Uart => uart_capture(n),
        }
    }

    /// Fault-tolerant CAN at 125 kbit/s, 200 µs/div: R-Net-like frames.
    pub fn can_capture(n: u32) -> Capture {
        let x = (n.wrapping_mul(37) % 200) as u8;
        let frames = [
            encode::frame(0x00E, false, false, &[x, 0x64u8.wrapping_sub(x)], true),
            encode::frame(0x7B3, false, true, &[], true),
            encode::frame(
                0x0200_0000,
                true,
                false,
                &[0x00, x, 0x9C],
                !n.is_multiple_of(3),
            ),
        ];
        // Traffic starts where the screen does (the memory shows 2.4 divisions more on each side).
        let mut bits = vec![true; 65];
        for frame in &frames {
            bits.extend(frame);
            bits.extend(vec![true; 18]);
        }
        let recessive = stretch(&bits, 20.0);
        let dominant: Vec<bool> = recessive.iter().map(|r| !r).collect();
        capture(
            200e-6,
            vec![channel(1, 2.0, -1.0), channel(2, 2.0, -3.0)],
            ("CH2", 3.2, "FALL"),
            [
                render(&dominant, 0.0, 3.6, 0.35, n),
                render(&recessive, 1.4, 5.0, 0.35, n ^ 0x55),
            ],
        )
    }

    /// I²C at 100 kHz, 100 µs/div: a temperature sensor at 0x48 read twice.
    pub fn i2c_capture(n: u32) -> Capture {
        let half = 25; // 5 µs at 5 MS/s
        let (mut scl, mut sda) = (Vec::with_capacity(POINTS), Vec::with_capacity(POINTS));
        let mut put = |c: bool, d: bool, count: usize| {
            for _ in 0..count {
                scl.push(c);
                sda.push(d);
            }
        };
        put(true, true, 1300);
        let temperature = 0x19 + (n % 4) as u8;
        for (address, read, payload) in [
            (0x48u8, false, vec![0x00u8]),
            (0x48, true, vec![temperature, 0x80]),
        ] {
            // START
            put(true, true, half);
            put(true, false, half);
            put(false, false, half / 2);
            let mut bytes = vec![(u16::from(address) << 1 | u16::from(read)) << 1];
            let last = payload.len().saturating_sub(1);
            for (i, byte) in payload.iter().enumerate() {
                // The master NACKs the last byte of a read.
                let nack = read && i == last;
                bytes.push((u16::from(*byte) << 1) | u16::from(nack));
            }
            for word in bytes {
                for bit in (0..9).rev() {
                    let level = (word >> bit) & 1 == 1;
                    put(false, level, half / 2);
                    put(true, level, half);
                    put(false, level, half / 2);
                }
            }
            // STOP
            put(false, false, half / 2);
            put(true, false, half);
            put(true, true, 200);
        }
        scl.resize(POINTS, true);
        sda.resize(POINTS, true);
        capture(
            100e-6,
            vec![channel(1, 2.0, 1.0), channel(2, 2.0, -3.0)],
            ("CH2", 1.65, "FALL"),
            [
                render(&scl, 0.0, 3.3, 0.25, n),
                render(&sda, 0.0, 3.3, 0.18, n ^ 0x33),
            ],
        )
    }

    /// UART 115 200 Bd 8N1, 200 µs/div: a command on TX and the reply on RX.
    pub fn uart_capture(n: u32) -> Capture {
        let samples_per_bit = 2.5e6 / 115_200.0;
        let line = |text: &[u8], idle_before: usize| {
            let mut bits = vec![true; idle_before];
            for byte in text {
                bits.push(false);
                bits.extend((0..8).map(|i| (byte >> i) & 1 == 1));
                bits.push(true);
                bits.push(true);
            }
            stretch(&bits, samples_per_bit)
        };
        let reply = format!("OK {}.{}V\r\n", 12 + n % 2, 40 + n % 9);
        let tx = line(b"MEAS:VOLT?\r\n", 60);
        let rx = line(reply.as_bytes(), 200);
        capture(
            200e-6,
            vec![channel(1, 2.0, 0.5), channel(2, 2.0, -3.5)],
            ("CH1", 1.65, "FALL"),
            [
                render(&tx, 0.0, 3.3, 0.5, n),
                render(&rx, 0.0, 3.3, 0.5, n ^ 0x77),
            ],
        )
    }
}

/// UART bytes grouped into lines: a pause longer than three character times, or a line feed,
/// ends a group. Returns (start time, bytes).
pub fn uart_lines(events: &[Event], baud: Option<f64>) -> Vec<(f64, Vec<u8>)> {
    let char_time = baud.map_or(1e-3, |b| 10.0 / b);
    let mut lines: Vec<(f64, f64, Vec<u8>)> = Vec::new();
    for event in events {
        let Event::Uart { t_s, value, .. } = event else {
            continue;
        };
        match lines.last_mut() {
            Some((_, last, bytes))
                if *t_s - *last < 3.0 * char_time && bytes.last() != Some(&b'\n') =>
            {
                bytes.push(*value);
                *last = *t_s;
            }
            _ => lines.push((*t_s, *t_s, vec![*value])),
        }
    }
    lines
        .into_iter()
        .map(|(start, _, bytes)| (start, bytes))
        .collect()
}

/// Bytes as readable text with escapes: `MEAS:VOLT?\r\n`.
pub fn escape_text(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| match b {
            b'\r' => "\\r".to_owned(),
            b'\n' => "\\n".to_owned(),
            b'\t' => "\\t".to_owned(),
            b if b.is_ascii_graphic() || *b == b' ' => (*b as char).to_string(),
            b => format!("\\x{b:02X}"),
        })
        .collect()
}
