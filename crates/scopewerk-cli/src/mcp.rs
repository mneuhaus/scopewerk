//! `scopewerk mcp`: a Model Context Protocol server on stdio. Each tool call opens a session
//! (through the running Scopewerk app if there is one, so the person sees what the agent does)
//! and closes it again, so the app and other tools can use the scope in between.

use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use scopewerk_core::broker::base64;
use scopewerk_core::image::plot_png;
use scopewerk_core::{Action, Capture, Reply, Request, Session};
use serde_json::{Value, json};

use crate::changes;
use crate::report::{capture_json, settings_json, sig, stats_json};

const INSTRUCTIONS: &str = "Scopewerk drives a Hanmatek DOS1102 two-channel 110 MHz oscilloscope over USB. \
Start with scope_status. The screen is 15.2 divisions wide (time 0 = trigger point at the centre) and 8 \
divisions high; a channel shows offset_div ± 4 divisions times volts_per_div. Voltages are at the probe tip. \
Typical loop: configure_scope (timebase, V/div, offset, trigger) → capture_waveform (numbers + plot) → adjust. \
Every change is read back from the scope and reported; values snap to the scope's 1-2-5 steps. \
capture_waveform warns about clipping or a too-coarse V/div. For one-off events use single_shot. \
scope_screenshot shows the scope's real display.";

pub(crate) fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(err) => {
                write(
                    &mut stdout,
                    &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": err.to_string()}}),
                )?;
                continue;
            }
        };
        // Notifications carry no id and get no answer.
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let response = match method {
            "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": initialize(&params)}),
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools()}}),
            "tools/call" => json!({"jsonrpc": "2.0", "id": id, "result": call(&params)}),
            other => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown method {other}")}})
            }
        };
        write(&mut stdout, &response)?;
    }
    Ok(())
}

fn write(out: &mut impl Write, message: &Value) -> Result<()> {
    let mut text = serde_json::to_string(message)?;
    text.push('\n');
    out.write_all(text.as_bytes())?;
    out.flush()?;
    Ok(())
}

fn initialize(params: &Value) -> Value {
    const SUPPORTED: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let version = SUPPORTED
        .iter()
        .find(|v| **v == requested)
        .unwrap_or(&SUPPORTED[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "scopewerk", "title": "Scopewerk (Hanmatek DOS1102)", "version": env!("CARGO_PKG_VERSION")},
        "instructions": INSTRUCTIONS,
    })
}

fn channels_schema() -> Value {
    json!({"type": "array", "items": {"type": "integer", "enum": [1, 2]}, "description": "Channels to read; default: every channel that is switched on."})
}

fn quantity_schema(description: &str) -> Value {
    json!({"type": ["string", "number"], "description": description})
}

fn channel_schema(n: u8) -> Value {
    json!({
        "type": "object",
        "description": format!("CH{n} settings; give only what should change."),
        "properties": {
            "enabled": {"type": "boolean"},
            "volts_per_div": quantity_schema("V/div at the probe tip, e.g. \"500mV\" or 0.5 (snaps to 1-2-5 steps)"),
            "offset_div": {"type": "number", "description": "Where 0 V sits, divisions from the centre, -4..4 (positive = up)"},
            "coupling": {"type": "string", "enum": ["DC", "AC", "GND"]},
            "probe": {"type": "string", "enum": ["1X", "10X", "100X", "1000X"], "description": "Must match the probe switch, or voltages are off by that factor"},
            "inverted": {"type": "boolean"},
        },
        "additionalProperties": false,
    })
}

fn tools() -> Value {
    let read_only = json!({"readOnlyHint": true, "openWorldHint": false});
    let changes = json!({"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false});
    json!([
        {
            "name": "scope_status",
            "title": "Scope status",
            "description": "Identity, connection path and every setting: timebase, sample rate, per-channel V/div, offset, coupling, probe, visible voltage range, trigger, run state. Call this first.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": read_only,
        },
        {
            "name": "capture_waveform",
            "title": "Capture waveform",
            "description": "Read what the scope currently shows. Returns settings, per-channel measurements (min/max/pk-pk/mean/RMS, high/low level, frequency, period, duty cycle, rise/fall time, edge counts), warnings (clipping, poor V/div use) and a plot image. Use source=memory for 10 000 samples at the real sample rate instead of 1 520 screen points. Ask for samples (e.g. 200) to get a peak-preserving time series; csv_path saves the full data.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channels": channels_schema(),
                    "source": {"type": "string", "enum": ["screen", "memory"], "default": "screen"},
                    "samples": {"type": "integer", "minimum": 0, "maximum": 4000, "default": 0, "description": "Number of (time, volts) points to include"},
                    "plot": {"type": "boolean", "default": true, "description": "Attach a PNG plot of the traces"},
                    "csv_path": {"type": "string", "description": "Absolute path to write time_s,ch1_v,ch2_v CSV at full resolution"},
                },
                "additionalProperties": false,
            },
            "annotations": read_only,
        },
        {
            "name": "configure_scope",
            "title": "Configure scope",
            "description": "Change any settings in one call; only the given fields change. Each change is sent, read back from the scope and reported as applied or not (values snap to the scope's steps). Probe changes are applied first. Returns the resulting settings.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "timebase": quantity_schema("Time per division, e.g. \"1ms\", \"20us\" or 0.001 (2 ns … 100 s, 1-2-5 steps)"),
                    "horizontal_offset_div": {"type": "number", "description": "Move the trigger point; divisions, positive = right"},
                    "acquisition": {"type": "string", "enum": ["sample", "peak", "average"]},
                    "ch1": channel_schema(1),
                    "ch2": channel_schema(2),
                    "trigger": {
                        "type": "object",
                        "properties": {
                            "source": {"type": "string", "enum": ["CH1", "CH2"]},
                            "level": quantity_schema("Trigger level at the probe tip, e.g. \"1.2V\""),
                            "edge": {"type": "string", "enum": ["rise", "fall"]},
                            "sweep": {"type": "string", "enum": ["auto", "normal", "single"], "description": "auto = free-running, normal = only on trigger, single = one shot"},
                            "coupling": {"type": "string", "enum": ["DC", "AC"]},
                        },
                        "additionalProperties": false,
                    },
                },
                "additionalProperties": false,
            },
            "annotations": changes,
        },
        {
            "name": "run_control",
            "title": "Run / stop / autoset",
            "description": "run = acquire continuously, stop = freeze the screen, single = arm one acquisition, autoset = let the scope choose timebase, V/div and trigger for the connected signals (overwrites settings).",
            "inputSchema": {
                "type": "object",
                "properties": {"action": {"type": "string", "enum": ["run", "stop", "single", "autoset"]}},
                "required": ["action"],
                "additionalProperties": false,
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "openWorldHint": false},
        },
        {
            "name": "single_shot",
            "title": "Single-shot capture",
            "description": "Arm the trigger for one acquisition, wait until it fires, and return the capture like capture_waveform. Configure trigger source/level/edge first. If nothing triggers within timeout_s the scope stays armed and the result says so.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "channels": channels_schema(),
                    "timeout_s": {"type": "number", "default": 10, "maximum": 300},
                    "source": {"type": "string", "enum": ["screen", "memory"], "default": "memory"},
                    "samples": {"type": "integer", "minimum": 0, "maximum": 4000, "default": 0},
                    "plot": {"type": "boolean", "default": true},
                    "csv_path": {"type": "string"},
                },
                "additionalProperties": false,
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "openWorldHint": false},
        },
        {
            "name": "monitor_levels",
            "title": "Monitor levels over time",
            "description": "Read the screen repeatedly for duration_s and return per-channel min/mean/max/frequency for every read, e.g. to watch a bus while a device powers up. About 2 reads per second; keep the timebase fast (not scan mode) so each read is fresh.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "duration_s": {"type": "number", "default": 10, "maximum": 300},
                    "channels": channels_schema(),
                },
                "additionalProperties": false,
            },
            "annotations": read_only,
        },
        {
            "name": "scope_screenshot",
            "title": "Scope screenshot",
            "description": "The scope's real 800×480 display as PNG (takes about 2 s), including its own labels and menus.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": read_only,
        },
        {
            "name": "scope_measurements",
            "title": "Scope's own measurements",
            "description": "The scope's built-in automatic measurements for one channel (as the scope computes them; '?' means not measurable).",
            "inputSchema": {
                "type": "object",
                "properties": {"channel": {"type": "integer", "enum": [1, 2]}},
                "required": ["channel"],
                "additionalProperties": false,
            },
            "annotations": read_only,
        },
        {
            "name": "send_scpi",
            "title": "Raw SCPI",
            "description": "Send one raw SCPI command (OWON SDS dialect, e.g. ':CH1:SCALe?') and return the reply. Set commands reply with an empty string and are NOT verified; prefer configure_scope.",
            "inputSchema": {
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false,
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "openWorldHint": false},
        },
    ])
}

fn call(params: &Value) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match run_tool(name, &args) {
        Ok(content) => json!({"content": content, "isError": false}),
        Err(err) => {
            json!({"content": [{"type": "text", "text": format!("{err:#}")}], "isError": true})
        }
    }
}

fn text(value: &Value) -> Value {
    json!({"type": "text", "text": serde_json::to_string_pretty(value).unwrap_or_default()})
}

fn png(bytes: &[u8]) -> Value {
    json!({"type": "image", "data": base64(bytes), "mimeType": "image/png"})
}

fn session() -> Result<Session> {
    Session::connect("mcp").map_err(|err| anyhow!("{err}"))
}

fn ask(session: &mut Session, request: Request) -> Result<Reply> {
    session.call(request).map_err(|err| anyhow!("{err}"))
}

fn run_tool(name: &str, args: &Value) -> Result<Vec<Value>> {
    let mut session = session()?;
    match name {
        "scope_status" => {
            let Reply::Status(status) = ask(&mut session, Request::Status)? else {
                bail!("unexpected reply")
            };
            Ok(vec![text(&json!({
                "identity": status.identity,
                "connected_via": if status.via == "app" { "Scopewerk app (the person sees changes live)" } else { "USB" },
                "running": status.running,
                "settings": settings_json(&status.settings),
            }))])
        }
        "capture_waveform" => {
            let channels = requested_channels(&mut session, args)?;
            let memory = args.get("source").and_then(Value::as_str) == Some("memory");
            let Reply::Capture(capture) = ask(&mut session, Request::Capture { channels, memory })?
            else {
                bail!("unexpected reply")
            };
            capture_content(&capture, args)
        }
        "configure_scope" => {
            let changes = changes::from_json(args)?;
            if changes.is_empty() {
                bail!(
                    "nothing to change: pass e.g. {{\"timebase\": \"1ms\", \"ch1\": {{\"volts_per_div\": \"500mV\"}}}}"
                );
            }
            let Reply::Applied(report) = ask(&mut session, Request::Apply { changes })? else {
                bail!("unexpected reply")
            };
            let outcomes: Vec<Value> = report
                .outcomes
                .iter()
                .map(|o| json!({"applied": o.applied, "result": o.message}))
                .collect();
            Ok(vec![text(&json!({
                "all_applied": report.all_applied(),
                "changes": outcomes,
                "settings": settings_json(&report.settings),
            }))])
        }
        "run_control" => {
            let action = match args.get("action").and_then(Value::as_str) {
                Some("run") => Action::Run,
                Some("stop") => Action::Stop,
                Some("single") => Action::Single,
                Some("autoset") => Action::Autoset,
                other => bail!("action {other:?} (use run, stop, single or autoset)"),
            };
            let Reply::Settings(settings) = ask(&mut session, Request::Act { action })? else {
                bail!("unexpected reply")
            };
            Ok(vec![text(
                &json!({"done": format!("{action:?}").to_lowercase(), "settings": settings_json(&settings)}),
            )])
        }
        "single_shot" => {
            let channels = requested_channels(&mut session, args)?;
            let memory = args.get("source").and_then(Value::as_str) != Some("screen");
            let timeout_s = args
                .get("timeout_s")
                .and_then(Value::as_f64)
                .unwrap_or(10.0)
                .clamp(0.5, 300.0);
            let timeout_ms = duration_ms(timeout_s);
            let Reply::SingleShot(capture) = ask(
                &mut session,
                Request::SingleShot {
                    channels,
                    memory,
                    timeout_ms,
                },
            )?
            else {
                bail!("unexpected reply")
            };
            match capture {
                Some(capture) => capture_content(&capture, args),
                None => Ok(vec![text(&json!({
                    "triggered": false,
                    "note": format!("no trigger within {timeout_s} s; the scope is still armed. Check trigger source, level and edge with scope_status."),
                }))]),
            }
        }
        "monitor_levels" => monitor(&mut session, args),
        "scope_screenshot" => {
            let Reply::Png(bytes) = ask(&mut session, Request::Screenshot)? else {
                bail!("unexpected reply")
            };
            Ok(vec![png(&bytes)])
        }
        "scope_measurements" => {
            let channel = channel_arg(args)?;
            let Reply::Json(value) = ask(&mut session, Request::ScopeMeasurements { channel })?
            else {
                bail!("unexpected reply")
            };
            Ok(vec![text(&value)])
        }
        "send_scpi" => {
            let command = args
                .get("command")
                .and_then(Value::as_str)
                .context("command is required")?;
            let Reply::Text(reply) = ask(
                &mut session,
                Request::Scpi {
                    command: command.to_owned(),
                },
            )?
            else {
                bail!("unexpected reply")
            };
            Ok(vec![text(&json!({"command": command, "reply": reply}))])
        }
        other => bail!("unknown tool {other}"),
    }
}

fn duration_ms(seconds: f64) -> u64 {
    Duration::try_from_secs_f64(seconds)
        .map_or(10_000, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn channel_arg(args: &Value) -> Result<u8> {
    match args.get("channel").and_then(Value::as_u64) {
        Some(1) => Ok(1),
        Some(2) => Ok(2),
        other => bail!("channel {other:?} (use 1 or 2)"),
    }
}

/// The channels asked for, or every enabled one.
fn requested_channels(session: &mut Session, args: &Value) -> Result<Vec<u8>> {
    if let Some(list) = args.get("channels").and_then(Value::as_array) {
        let channels: Vec<u8> = list
            .iter()
            .filter_map(Value::as_u64)
            .filter_map(|c| u8::try_from(c).ok())
            .collect();
        if !channels.is_empty() {
            return Ok(channels);
        }
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

fn capture_content(capture: &Capture, args: &Value) -> Result<Vec<Value>> {
    let samples = args
        .get("samples")
        .and_then(Value::as_u64)
        .map_or(0, |s| usize::try_from(s.min(4000)).unwrap_or(0));
    let mut report = capture_json(capture, samples);
    if let Some(path) = args.get("csv_path").and_then(Value::as_str) {
        std::fs::write(path, capture.to_csv()).with_context(|| format!("writing {path}"))?;
        report["csv_path"] = json!(path);
    }
    let mut content = vec![text(&report)];
    if args.get("plot").and_then(Value::as_bool).unwrap_or(true) {
        content.push(png(&plot_png(capture, 960, 505)?));
    }
    Ok(content)
}

fn monitor(session: &mut Session, args: &Value) -> Result<Vec<Value>> {
    let duration = args
        .get("duration_s")
        .and_then(Value::as_f64)
        .unwrap_or(10.0)
        .clamp(0.5, 300.0);
    let channels = requested_channels(session, args)?;
    let start = Instant::now();
    let mut reads = Vec::new();
    while start.elapsed().as_secs_f64() < duration {
        let Reply::Capture(capture) = ask(
            session,
            Request::Capture {
                channels: channels.clone(),
                memory: false,
            },
        )?
        else {
            bail!("unexpected reply")
        };
        let mut entry = json!({"t_s": sig(start.elapsed().as_secs_f64(), 4)});
        for &channel in &channels {
            if let Some(stats) = capture.stats(channel) {
                let full = stats_json(&stats);
                entry[format!("ch{channel}")] = json!({
                    "min_v": full["min_v"], "mean_v": full["mean_v"], "max_v": full["max_v"],
                    "frequency_hz": full["frequency_hz"],
                });
            }
        }
        reads.push(entry);
    }
    Ok(vec![text(&json!({"reads": reads.len(), "series": reads}))])
}
