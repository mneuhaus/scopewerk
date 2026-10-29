//! `scopewerk`: the Hanmatek DOS1102 from the terminal, and as an MCP server for agents.
//! Goes through the running Scopewerk app when there is one, otherwise straight over USB.

mod changes;
mod decode_cmd;
mod mcp;
mod report;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use scopewerk_core::image::plot_png;
use scopewerk_core::units::format_si;
use scopewerk_core::{Action, Capture, Reply, Request, Session, usb};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "scopewerk",
    version,
    about = "Hanmatek DOS1102 oscilloscope over USB: capture, configure, export, and an MCP server for agents",
    after_help = "Examples:\n  scopewerk status\n  scopewerk set timebase=1ms ch1.scale=500mV ch1.offset=-2 trigger.level=1.2V\n  scopewerk capture --memory --csv bus.csv --png bus.png\n  scopewerk single --timeout 30 --csv event.csv\n  scopewerk watch 60 --log levels.jsonl\n  scopewerk decode can --input ch1-ch2 --single 10\n  scopewerk mcp            (for Claude Code: claude mcp add scopewerk -- scopewerk mcp)"
)]
struct Cli {
    /// Machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Identity, connection and all settings.
    Status,
    /// List attached DOS1102 scopes.
    List,
    /// Read the screen (or the acquisition memory) and print measurements.
    Capture(CaptureArgs),
    /// Arm one acquisition, wait for the trigger, then capture it.
    Single {
        /// Seconds to wait for a trigger.
        #[arg(long, default_value_t = 10.0)]
        timeout: f64,
        #[command(flatten)]
        capture: CaptureArgs,
    },
    /// Change settings, verified by reading them back. Keys: see --help.
    #[command(after_help = changes::KEYS)]
    Set {
        /// key=value pairs, e.g. timebase=1ms ch1.scale=500mV
        #[arg(required = true)]
        assignments: Vec<String>,
    },
    /// Acquire continuously.
    Run,
    /// Freeze the screen.
    Stop,
    /// Let the scope pick timebase, V/div and trigger.
    Autoset,
    /// Save the scope's own display as PNG.
    Screenshot {
        /// Output file.
        #[arg(default_value = "scope.png")]
        path: PathBuf,
    },
    /// The scope's own automatic measurements of a channel.
    Measure {
        #[arg(default_value_t = 1)]
        channel: u8,
    },
    /// Print per-channel levels repeatedly (about two reads per second).
    Watch {
        /// How long to watch, in seconds.
        #[arg(default_value_t = 10.0)]
        seconds: f64,
        /// Append every read as JSON lines (with full traces).
        #[arg(long)]
        log: Option<PathBuf>,
    },
    /// Decode a bus like a logic analyzer: CAN, I²C, SPI or UART.
    #[command(subcommand)]
    Decode(decode_cmd::Protocol),
    /// Send raw SCPI commands and print the replies.
    Scpi {
        #[arg(required = true)]
        commands: Vec<String>,
    },
    /// Run the MCP server on stdio.
    Mcp,
}

#[derive(clap::Args)]
struct CaptureArgs {
    /// Channels to read (default: the ones switched on).
    #[arg(short, long, value_delimiter = ',')]
    channels: Vec<u8>,
    /// Acquisition memory (10 000 samples at the real sample rate) instead of screen points.
    #[arg(long)]
    memory: bool,
    /// Write time_s,ch1_v,ch2_v CSV.
    #[arg(long)]
    csv: Option<PathBuf>,
    /// Write the full capture (settings + samples) as JSON.
    #[arg(long = "save-json")]
    save_json: Option<PathBuf>,
    /// Render a scope-style plot.
    #[arg(long)]
    png: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();
    if let Err(err) = run(cli) {
        eprintln!("scopewerk: {err:#}");
        std::process::exit(1);
    }
}

fn session() -> Result<Session> {
    Session::connect("cli").map_err(|err| anyhow!("{err}"))
}

fn ask(session: &mut Session, request: Request) -> Result<Reply> {
    session.call(request).map_err(|err| anyhow!("{err}"))
}

fn run(cli: Cli) -> Result<()> {
    let json = cli.json;
    match cli.command {
        Command::Mcp => mcp::serve(),
        Command::List => {
            let scopes = usb::list().map_err(|err| anyhow!("{err}"))?;
            if json {
                let list: Vec<_> = scopes
                    .iter()
                    .map(|s| json!({"serial": s.serial, "protocol": s.protocol(), "bus": s.bus}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else if scopes.is_empty() {
                println!("no DOS1102 attached");
            } else {
                for scope in scopes {
                    println!(
                        "DOS1102 serial {} ({}, bus {})",
                        scope.serial.as_deref().unwrap_or("?"),
                        scope.protocol(),
                        scope.bus
                    );
                }
            }
            Ok(())
        }
        Command::Status => {
            let mut session = session()?;
            let Reply::Status(status) = ask(&mut session, Request::Status)? else {
                bail!("unexpected reply")
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "identity": status.identity,
                        "via": status.via,
                        "running": status.running,
                        "settings": report::settings_json(&status.settings),
                    }))?
                );
                return Ok(());
            }
            let id = &status.identity;
            println!(
                "{} {} serial {} firmware {} · {} · via {}",
                id.manufacturer,
                id.model,
                id.serial,
                id.firmware,
                if status.running { "RUN" } else { "STOP" },
                if status.via == "app" {
                    "Scopewerk app"
                } else {
                    "USB"
                }
            );
            println!("{}", report::settings_line(&status.settings));
            for channel in &status.settings.channels {
                let (low, high) = report::visible_range(channel);
                println!(
                    "CH{} {:>3}  {:>9}/div  offset {:+.2} div  {} {}{}  shows {} … {}",
                    channel.channel,
                    if channel.enabled { "on" } else { "off" },
                    format_si(channel.volts_per_div, "V"),
                    channel.offset_div,
                    channel.coupling,
                    channel.probe,
                    if channel.inverted { " inverted" } else { "" },
                    format_si(low, "V"),
                    format_si(high, "V"),
                );
            }
            Ok(())
        }
        Command::Capture(args) => {
            let mut session = session()?;
            let channels = channels_or_enabled(&mut session, &args.channels)?;
            let Reply::Capture(capture) = ask(
                &mut session,
                Request::Capture {
                    channels,
                    memory: args.memory,
                },
            )?
            else {
                bail!("unexpected reply")
            };
            output_capture(&capture, &args, json)
        }
        Command::Single {
            timeout,
            capture: args,
        } => {
            let mut session = session()?;
            let channels = channels_or_enabled(&mut session, &args.channels)?;
            let timeout_ms = u64::try_from(Duration::try_from_secs_f64(timeout)?.as_millis())?;
            if !json {
                eprintln!("armed, waiting up to {timeout} s for a trigger…");
            }
            let request = Request::SingleShot {
                channels,
                memory: args.memory,
                timeout_ms,
            };
            let Reply::SingleShot(capture) = ask(&mut session, request)? else {
                bail!("unexpected reply")
            };
            let capture = capture.with_context(|| {
                format!("no trigger within {timeout} s (the scope stays armed); check `scopewerk status`")
            })?;
            output_capture(&capture, &args, json)
        }
        Command::Set { assignments } => {
            let changes = changes::from_assignments(&assignments)?;
            let mut session = session()?;
            let Reply::Applied(report) = ask(&mut session, Request::Apply { changes })? else {
                bail!("unexpected reply")
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                for outcome in &report.outcomes {
                    println!(
                        "{} {}",
                        if outcome.applied { "✓" } else { "✗" },
                        outcome.message
                    );
                }
            }
            if !report.all_applied() {
                bail!("some settings were not applied");
            }
            Ok(())
        }
        Command::Run => act(Action::Run, json),
        Command::Stop => act(Action::Stop, json),
        Command::Autoset => act(Action::Autoset, json),
        Command::Screenshot { path } => {
            let mut session = session()?;
            let Reply::Png(bytes) = ask(&mut session, Request::Screenshot)? else {
                bail!("unexpected reply")
            };
            std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
            println!("{}", path.display());
            Ok(())
        }
        Command::Measure { channel } => {
            let mut session = session()?;
            let Reply::Json(value) = ask(&mut session, Request::ScopeMeasurements { channel })?
            else {
                bail!("unexpected reply")
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else if let Some(items) = value
                .as_object()
                .and_then(|o| o.values().next())
                .and_then(|v| v.as_object())
            {
                for (name, reading) in items {
                    let reading = reading.as_str().unwrap_or_default().trim();
                    if reading != "?" {
                        println!("{name:>16}  {reading}");
                    }
                }
            }
            Ok(())
        }
        Command::Watch { seconds, log } => watch(seconds, log, json),
        Command::Decode(protocol) => decode_cmd::run(protocol, json),
        Command::Scpi { commands } => {
            let mut session = session()?;
            for command in commands {
                let Reply::Text(reply) = ask(
                    &mut session,
                    Request::Scpi {
                        command: command.clone(),
                    },
                )?
                else {
                    bail!("unexpected reply")
                };
                if json {
                    println!("{}", json!({"command": command, "reply": reply}));
                } else {
                    println!("{command} → {reply}");
                }
            }
            Ok(())
        }
    }
}

fn act(action: Action, json: bool) -> Result<()> {
    let mut session = session()?;
    let Reply::Settings(settings) = ask(&mut session, Request::Act { action })? else {
        bail!("unexpected reply")
    };
    if json {
        println!("{}", report::settings_json(&settings));
    } else {
        println!("{}", report::settings_line(&settings));
    }
    Ok(())
}

fn channels_or_enabled(session: &mut Session, wanted: &[u8]) -> Result<Vec<u8>> {
    if !wanted.is_empty() {
        return Ok(wanted.to_vec());
    }
    let Reply::Status(status) = ask(session, Request::Status)? else {
        bail!("unexpected reply")
    };
    let enabled: Vec<u8> = status
        .settings
        .channels
        .iter()
        .filter(|c| c.enabled)
        .map(|c| c.channel)
        .collect();
    Ok(if enabled.is_empty() {
        vec![1, 2]
    } else {
        enabled
    })
}

fn output_capture(capture: &Capture, args: &CaptureArgs, json: bool) -> Result<()> {
    if let Some(path) = &args.csv {
        std::fs::write(path, capture.to_csv())
            .with_context(|| format!("writing {}", path.display()))?;
    }
    if let Some(path) = &args.save_json {
        std::fs::write(path, serde_json::to_vec(capture)?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    if let Some(path) = &args.png {
        std::fs::write(path, plot_png(capture, 1520, 800)?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report::capture_json(capture, 0))?
        );
    } else {
        for line in report::capture_lines(capture) {
            println!("{line}");
        }
    }
    Ok(())
}

fn watch(seconds: f64, log: Option<PathBuf>, json: bool) -> Result<()> {
    use std::io::Write as _;
    let mut session = session()?;
    let channels = channels_or_enabled(&mut session, &[])?;
    let mut log = log
        .map(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("opening {}", path.display()))
        })
        .transpose()?;
    let start = Instant::now();
    while start.elapsed().as_secs_f64() < seconds {
        let Reply::Capture(capture) = ask(
            &mut session,
            Request::Capture {
                channels: channels.clone(),
                memory: false,
            },
        )?
        else {
            bail!("unexpected reply")
        };
        let t = start.elapsed().as_secs_f64();
        if let Some(log) = log.as_mut() {
            let mut line = serde_json::to_string(&json!({"t": t, "capture": capture}))?;
            line.push('\n');
            log.write_all(line.as_bytes())?;
        }
        let mut parts = Vec::new();
        let mut entry = json!({"t_s": report::sig(t, 4)});
        for &channel in &channels {
            let Some(stats) = capture.stats(channel) else {
                continue;
            };
            parts.push(format!(
                "CH{channel} min {:>9} mean {:>9} max {:>9}",
                format_si(stats.min_v, "V"),
                format_si(stats.mean_v, "V"),
                format_si(stats.max_v, "V")
            ));
            entry[format!("ch{channel}")] = report::stats_json(&stats);
        }
        if json {
            println!("{entry}");
        } else {
            println!("t={t:6.1}s  {}", parts.join("  |  "));
        }
    }
    Ok(())
}
