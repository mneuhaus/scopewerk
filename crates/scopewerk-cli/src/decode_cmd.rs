//! `scopewerk decode can|i2c|spi|uart`: capture and decode a bus.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use scopewerk_core::decode::Parity;
use scopewerk_core::units::format_si;
use scopewerk_core::{DecodeConfig, Input, Reply, Request, Session};

use crate::changes::quantity;
use crate::report;

#[derive(Subcommand)]
pub(crate) enum Protocol {
    /// CAN 2.0A/B: CRC check, ACK, auto bitrate and polarity.
    Can {
        /// ch1, ch2, ch1-ch2 (CAN_H on CH1, CAN_L on CH2) or ch2-ch1.
        #[arg(long, default_value = "ch1")]
        input: String,
        /// e.g. 125k, 500k; default: detect.
        #[arg(long)]
        bitrate: Option<String>,
        /// high (CAN_H, differential) or low (CAN_L, RX pin); default: detect.
        #[arg(long)]
        polarity: Option<String>,
        #[command(flatten)]
        common: Common,
    },
    /// I²C: START/STOP, address, R/W, data, ACK/NACK.
    I2c {
        #[arg(long, default_value_t = 1)]
        scl: u8,
        #[arg(long, default_value_t = 2)]
        sda: u8,
        #[command(flatten)]
        common: Common,
    },
    /// SPI from clock + one data line; words split at clock pauses.
    Spi {
        #[arg(long, default_value_t = 1)]
        clk: u8,
        #[arg(long, default_value_t = 2)]
        data: u8,
        /// SPI mode 0-3; default: CPOL from the idle clock, CPHA 0.
        #[arg(long)]
        mode: Option<u8>,
        #[arg(long, default_value_t = 8)]
        bits: u8,
        #[arg(long)]
        lsb_first: bool,
        #[command(flatten)]
        common: Common,
    },
    /// UART: auto baud, 8N1 by default.
    Uart {
        #[arg(long, default_value = "ch1")]
        input: String,
        /// e.g. 115200; default: detect.
        #[arg(long)]
        baud: Option<String>,
        /// none, even or odd.
        #[arg(long, default_value = "none")]
        parity: String,
        #[arg(long, default_value_t = 8)]
        bits: u8,
        /// Idle-low line.
        #[arg(long)]
        inverted: bool,
        #[command(flatten)]
        common: Common,
    },
}

#[derive(Args)]
pub(crate) struct Common {
    /// Arm a single acquisition and wait for the trigger (seconds).
    #[arg(long, value_name = "SECONDS")]
    single: Option<f64>,
    /// Decode the 1 520 screen points instead of the 10 000-sample memory.
    #[arg(long)]
    screen: bool,
    /// Also write the capture as CSV.
    #[arg(long)]
    csv: Option<PathBuf>,
    /// Print at most this many events.
    #[arg(long, default_value_t = 200)]
    limit: usize,
}

fn rate(text: Option<&String>) -> Result<Option<f64>> {
    text.map(|t| {
        quantity(
            t.trim_end_matches("bps")
                .trim_end_matches("bit/s")
                .trim_end_matches("Bd"),
            "bit/s",
        )
    })
    .transpose()
}

pub(crate) fn run(protocol: Protocol, json: bool) -> Result<()> {
    let (config, common) = match protocol {
        Protocol::Can {
            input,
            bitrate,
            polarity,
            common,
        } => {
            let dominant_high = match polarity.as_deref().map(str::to_ascii_lowercase).as_deref() {
                None | Some("auto") => None,
                Some("high" | "h" | "can_h") => Some(true),
                Some("low" | "l" | "can_l" | "rx") => Some(false),
                Some(other) => bail!("polarity {other:?} (use high, low or auto)"),
            };
            (
                DecodeConfig::Can {
                    input: input.parse::<Input>()?,
                    bitrate: rate(bitrate.as_ref())?,
                    dominant_high,
                },
                common,
            )
        }
        Protocol::I2c { scl, sda, common } => (DecodeConfig::I2c { scl, sda }, common),
        Protocol::Spi {
            clk,
            data,
            mode,
            bits,
            lsb_first,
            common,
        } => {
            if mode.is_some_and(|m| m > 3) {
                bail!("SPI mode must be 0, 1, 2 or 3");
            }
            (
                DecodeConfig::Spi {
                    clk,
                    data,
                    mode,
                    bits,
                    lsb_first,
                },
                common,
            )
        }
        Protocol::Uart {
            input,
            baud,
            parity,
            bits,
            inverted,
            common,
        } => {
            let parity = match parity.to_ascii_lowercase().as_str() {
                "none" | "n" => Parity::None,
                "even" | "e" => Parity::Even,
                "odd" | "o" => Parity::Odd,
                other => bail!("parity {other:?} (use none, even or odd)"),
            };
            (
                DecodeConfig::Uart {
                    input: input.parse::<Input>()?,
                    baud: rate(baud.as_ref())?,
                    data_bits: bits,
                    parity,
                    inverted,
                },
                common,
            )
        }
    };
    let single_timeout_ms = common
        .single
        .map(|s| {
            std::time::Duration::try_from_secs_f64(s)
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        })
        .transpose()?;
    let mut session = Session::connect("cli").map_err(|err| anyhow!("{err}"))?;
    if single_timeout_ms.is_some() && !json {
        eprintln!("armed, waiting for a trigger…");
    }
    let request = Request::Decode {
        config,
        memory: !common.screen,
        single_timeout_ms,
    };
    let Reply::Decoded(reply) = session.call(request).map_err(|err| anyhow!("{err}"))? else {
        bail!("unexpected reply")
    };
    let reply = reply.context(
        "no trigger in time (the scope stays armed); check the trigger with `scopewerk status`",
    )?;
    if let Some(path) = &common.csv {
        std::fs::write(path, reply.capture.to_csv())
            .with_context(|| format!("writing {}", path.display()))?;
    }
    let decoded = &reply.decoded;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report::decoded_json(decoded, common.limit))?
        );
        return Ok(());
    }
    println!("{}  ({})", decoded.summary, decoded.detail);
    for event in decoded.events.iter().take(common.limit) {
        let t = event.t_s();
        let sign = if t >= 0.0 { "+" } else { "" };
        println!(
            "{:>12}  {}{}",
            format!("{sign}{}", format_si(t, "s")),
            if event.is_ok() { "" } else { "✗ " },
            event.describe()
        );
    }
    if decoded.events.len() > common.limit {
        println!("… {} more", decoded.events.len() - common.limit);
    }
    for warning in &decoded.warnings {
        println!("! {warning}");
    }
    Ok(())
}
