//! One vocabulary for changing the scope, shared by `scopewerk set ch1.scale=500mV` and the MCP
//! `configure_scope` tool (`{"ch1": {"volts_per_div": "500mV"}}` is flattened to the same keys).

use anyhow::{Context, Result, bail};
use scopewerk_core::units::parse_quantity;
use scopewerk_core::{Change, Coupling, Edge, Sweep};
use serde_json::Value;

/// Every key `set` understands, for help texts.
pub(crate) const KEYS: &str = "timebase=1ms  hoffset=0.5 (div)  acquisition=sample|peak|average\n\
     ch1=on|off  ch1.scale=500mV  ch1.offset=-2 (div)  ch1.coupling=dc|ac|gnd  ch1.probe=10x  ch1.invert=on|off\n\
     trigger.source=ch1  trigger.level=1.2V  trigger.edge=rise|fall  trigger.sweep=auto|normal|single  trigger.coupling=dc|ac";

/// Parse `key=value` assignments.
pub(crate) fn from_assignments(assignments: &[String]) -> Result<Vec<Change>> {
    assignments
        .iter()
        .map(|assignment| {
            let (key, value) = assignment
                .split_once('=')
                .with_context(|| format!("{assignment:?} is not key=value. Keys:\n{KEYS}"))?;
            change(key.trim(), value.trim())
        })
        .collect()
}

/// Flatten a JSON object (`{"ch1": {"volts_per_div": 0.5}, "timebase": "1ms"}`) into changes.
pub(crate) fn from_json(object: &Value) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    flatten("", object, &mut changes)?;
    Ok(changes)
}

fn flatten(prefix: &str, value: &Value, changes: &mut Vec<Change>) -> Result<()> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let key = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&key, value, changes)?;
            }
        }
        Value::Null => {}
        Value::String(text) => changes.push(change(prefix, text)?),
        other => changes.push(change(prefix, &other.to_string())?),
    }
    Ok(())
}

fn change(key: &str, value: &str) -> Result<Change> {
    let key = key.to_ascii_lowercase().replace(['-', ' '], "_");
    let (scope, field) = key.split_once('.').unwrap_or((key.as_str(), ""));
    let channel = match scope {
        "ch1" | "channel1" | "1" => Some(1),
        "ch2" | "channel2" | "2" => Some(2),
        _ => None,
    };
    if let Some(channel) = channel {
        return Ok(match field {
            "" | "enabled" | "display" | "on" => Change::ChannelEnabled {
                channel,
                enabled: switch(value)?,
            },
            "scale" | "vdiv" | "volts" | "volts_per_div" | "v_div" => Change::VoltsPerDiv {
                channel,
                volts: quantity(value, "V")?,
            },
            "offset" | "offset_div" | "position" => Change::ChannelOffset {
                channel,
                div: number(value)?,
            },
            "coupling" => Change::Coupling {
                channel,
                coupling: value.parse::<Coupling>()?,
            },
            "probe" => Change::Probe {
                channel,
                factor: value
                    .trim()
                    .trim_end_matches(['x', 'X'])
                    .parse()
                    .with_context(|| format!("probe {value:?} (use 1x, 10x, 100x or 1000x)"))?,
            },
            "invert" | "inverted" | "inverse" => Change::Invert {
                channel,
                inverted: switch(value)?,
            },
            other => bail!("unknown channel setting {other:?}. Keys:\n{KEYS}"),
        });
    }
    Ok(match (scope, field) {
        ("timebase" | "tb" | "time" | "timebase_s_per_div" | "horizontal_scale", "") => {
            Change::Timebase {
                s_per_div: quantity(value, "s")?,
            }
        }
        ("hoffset" | "horizontal_offset" | "horizontal_offset_div", "")
        | ("horizontal", "offset") => Change::HorizontalOffset {
            div: number(value)?,
        },
        ("acquisition" | "acquire" | "acq", "" | "mode") => Change::Acquisition {
            mode: value.parse()?,
        },
        ("trigger" | "trig", field) => match field {
            "source" | "channel" => Change::TriggerSource {
                channel: match value.trim().to_ascii_lowercase().trim_start_matches("ch") {
                    "1" => 1,
                    "2" => 2,
                    _ => bail!("trigger source {value:?} (use ch1 or ch2)"),
                },
            },
            "level" | "level_v" => Change::TriggerLevel {
                volts: quantity(value, "V")?,
            },
            "edge" | "slope" => Change::TriggerEdge {
                edge: value.parse::<Edge>()?,
            },
            "sweep" | "mode" => Change::TriggerSweep {
                sweep: value.parse::<Sweep>()?,
            },
            "coupling" => Change::TriggerCoupling {
                coupling: value.parse::<Coupling>()?,
            },
            other => bail!("unknown trigger setting {other:?}. Keys:\n{KEYS}"),
        },
        _ => bail!("unknown setting {key:?}. Keys:\n{KEYS}"),
    })
}

/// A number with an optional unit: `"500mV"`, `"0.5"`, `"1 ms"`, `"2e-3"`.
pub(crate) fn quantity(text: &str, unit: &str) -> Result<f64> {
    let text = text.trim().trim_matches('"');
    let text = text.strip_suffix("/div").unwrap_or(text);
    if let Ok(value) = text.parse::<f64>() {
        return Ok(value);
    }
    parse_quantity(text)
        .with_context(|| format!("{text:?} is not a quantity in {unit} (e.g. 500m{unit})"))
}

fn number(text: &str) -> Result<f64> {
    let text = text.trim().trim_matches('"');
    let text = text.strip_suffix("div").unwrap_or(text).trim();
    text.parse()
        .with_context(|| format!("{text:?} is not a number of divisions"))
}

fn switch(text: &str) -> Result<bool> {
    match text.trim().trim_matches('"').to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        other => bail!("{other:?} is not on/off"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn assignments_and_json_mean_the_same() {
        let cli = from_assignments(&[
            "timebase=1ms".into(),
            "ch1.scale=500mV".into(),
            "trigger.edge=fall".into(),
            "ch2=off".into(),
        ])
        .expect("parses");
        let mcp = from_json(&json!({
            "timebase": "1 ms",
            "ch1": {"volts_per_div": 0.5},
            "trigger": {"edge": "falling"},
            "ch2": {"enabled": false},
        }))
        .expect("parses");
        assert_eq!(cli.len(), 4);
        for change in &cli {
            assert!(mcp.contains(change), "{change:?} missing in {mcp:?}");
        }
    }

    #[test]
    fn rejects_unknown_keys_with_help() {
        let err = from_assignments(&["ch1.colour=red".into()]).expect_err("unknown key");
        assert!(err.to_string().contains("trigger.level"));
    }
}
