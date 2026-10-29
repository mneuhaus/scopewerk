//! Shaping scope data for people (one-line summaries) and agents (compact JSON with rounded SI
//! numbers and explicit units in the field names).

use scopewerk_core::units::format_si;
use scopewerk_core::{Capture, ChannelSettings, Settings, Stats};
use serde_json::{Value, json};

/// Round to `digits` significant digits so JSON stays readable.
pub(crate) fn sig(value: f64, digits: i32) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    if value == 0.0 {
        return json!(0.0);
    }
    let magnitude = value.abs().log10().floor();
    let factor = 10f64.powf(f64::from(digits - 1) - magnitude);
    json!((value * factor).round() / factor)
}

fn opt(value: Option<f64>, digits: i32) -> Value {
    value.map_or(Value::Null, |v| sig(v, digits))
}

pub(crate) fn settings_json(settings: &Settings) -> Value {
    json!({
        "timebase_s_per_div": sig(settings.timebase_s_per_div, 4),
        "horizontal_offset_div": sig(settings.horizontal_offset_div, 4),
        "sample_rate_sa_s": opt(settings.sample_rate_sa_s, 4),
        "screen_span_s": sig(settings.timebase_s_per_div * scopewerk_core::header::HORIZONTAL_DIVS, 4),
        "memory_depth": settings.memory_depth,
        "acquisition": settings.acquisition,
        "trigger_status": settings.run_status,
        "channels": settings.channels.iter().map(channel_json).collect::<Vec<_>>(),
        "trigger": {
            "source": settings.trigger.source,
            "level_v": opt(settings.trigger.level_v, 4),
            "edge": settings.trigger.edge,
            "sweep": settings.trigger.sweep,
            "coupling": settings.trigger.coupling,
            "holdoff_s": opt(settings.trigger.holdoff_s, 3),
        },
    })
}

fn channel_json(channel: &ChannelSettings) -> Value {
    let range = visible_range(channel);
    json!({
        "channel": channel.channel,
        "enabled": channel.enabled,
        "volts_per_div": sig(channel.volts_per_div, 4),
        "offset_div": sig(channel.offset_div, 4),
        "coupling": channel.coupling,
        "probe": channel.probe,
        "inverted": channel.inverted,
        "visible_range_v": [sig(range.0, 4), sig(range.1, 4)],
    })
}

/// Lowest and highest voltage that fits on screen (8 divisions).
pub(crate) fn visible_range(channel: &ChannelSettings) -> (f64, f64) {
    let low = (-4.0 - channel.offset_div) * channel.volts_per_div;
    let high = (4.0 - channel.offset_div) * channel.volts_per_div;
    (low, high)
}

pub(crate) fn stats_json(stats: &Stats) -> Value {
    json!({
        "min_v": sig(stats.min_v, 4),
        "max_v": sig(stats.max_v, 4),
        "pk_pk_v": sig(stats.pk_pk_v, 4),
        "mean_v": sig(stats.mean_v, 4),
        "rms_v": sig(stats.rms_v, 4),
        "low_level_v": sig(stats.base_v, 4),
        "high_level_v": sig(stats.top_v, 4),
        "frequency_hz": opt(stats.frequency_hz, 5),
        "period_s": opt(stats.period_s, 5),
        "duty_cycle_pct": opt(stats.duty_cycle_pct, 3),
        "rise_time_s": opt(stats.rise_time_s, 3),
        "fall_time_s": opt(stats.fall_time_s, 3),
        "rising_edges": stats.rising_edges,
        "falling_edges": stats.falling_edges,
    })
}

/// Warnings an engineer would call out when glancing at the screen.
pub(crate) fn warnings(capture: &Capture) -> Vec<String> {
    let mut notes = Vec::new();
    for waveform in &capture.waveforms {
        let Some(channel) = capture.settings.channel(waveform.channel) else {
            continue;
        };
        let Some(stats) = capture.stats(waveform.channel) else {
            continue;
        };
        let (low, high) = visible_range(channel);
        let margin = channel.volts_per_div * 0.05;
        let n = waveform.channel;
        if !channel.enabled {
            notes.push(format!(
                "CH{n} is switched off on the scope; its data is not meaningful"
            ));
            continue;
        }
        if stats.max_v >= high - margin || stats.min_v <= low + margin {
            notes.push(format!(
                "CH{n} is clipped at the screen edge (visible {}…{}); raise V/div or move the offset",
                format_si(low, "V"),
                format_si(high, "V")
            ));
        } else if stats.pk_pk_v < channel.volts_per_div * 0.5 && stats.pk_pk_v > 0.0 {
            notes.push(format!(
                "CH{n} uses less than half a division ({} pk-pk at {}/div); lower V/div for resolution",
                format_si(stats.pk_pk_v, "V"),
                format_si(channel.volts_per_div, "V")
            ));
        }
        if let Some(period) = stats.period_s {
            let span =
                capture.settings.timebase_s_per_div * scopewerk_core::header::HORIZONTAL_DIVS;
            if span / period > 60.0 {
                notes.push(format!(
                    "CH{n} shows {:.0} periods on screen; a faster timebase shows the shape better",
                    span / period
                ));
            }
        }
    }
    notes
}

pub(crate) fn capture_json(capture: &Capture, samples: usize) -> Value {
    let channels: Vec<Value> = capture
        .waveforms
        .iter()
        .map(|waveform| {
            let mut entry = json!({
                "channel": waveform.channel,
                "points": waveform.volts.len(),
                "stats": capture.stats(waveform.channel).as_ref().map(stats_json),
            });
            if samples > 0 {
                let (t, v) = decimate(capture, &waveform.volts, samples);
                entry["samples"] = json!({ "t_s": t, "v": v, "note": "min/max-preserving decimation; full data via csv_path" });
            }
            entry
        })
        .collect();
    json!({
        "settings": settings_json(&capture.settings),
        "dt_s": sig(capture.dt, 4),
        "t0_s": sig(capture.t0, 5),
        "t0_note": "time 0 is the trigger point",
        "channels": channels,
        "warnings": warnings(capture),
    })
}

/// About `target` points that keep every peak: each bucket contributes its minimum and maximum
/// in time order.
fn decimate(capture: &Capture, volts: &[f32], target: usize) -> (Vec<Value>, Vec<Value>) {
    let buckets = (target / 2).max(1);
    let size = volts.len().div_ceil(buckets).max(1);
    let mut times = Vec::with_capacity(target);
    let mut values = Vec::with_capacity(target);
    for (bucket, chunk) in volts.chunks(size).enumerate() {
        let base = bucket * size;
        let (mut lo, mut hi) = (0, 0);
        for (i, v) in chunk.iter().enumerate() {
            if *v < chunk[lo] {
                lo = i;
            }
            if *v > chunk[hi] {
                hi = i;
            }
        }
        let mut pair = [lo, hi];
        pair.sort_unstable();
        for (n, i) in pair.iter().enumerate() {
            if n == 1 && pair[0] == pair[1] {
                break;
            }
            times.push(sig(capture.time_at(base + i), 5));
            values.push(sig(f64::from(chunk[*i]), 4));
        }
    }
    (times, values)
}

/// One line per channel for terminals.
pub(crate) fn capture_lines(capture: &Capture) -> Vec<String> {
    let mut lines = vec![settings_line(&capture.settings)];
    for waveform in &capture.waveforms {
        let Some(channel) = capture.settings.channel(waveform.channel) else {
            continue;
        };
        let stats = capture.stats(waveform.channel).unwrap_or_default();
        let frequency = stats
            .frequency_hz
            .map_or_else(|| "—".to_owned(), |f| format_si(f, "Hz"));
        lines.push(format!(
            "CH{} {:>9}/div {} {:>4}  min {:>9}  max {:>9}  pk-pk {:>9}  mean {:>9}  rms {:>9}  freq {}{}",
            waveform.channel,
            format_si(channel.volts_per_div, "V"),
            channel.coupling,
            channel.probe,
            format_si(stats.min_v, "V"),
            format_si(stats.max_v, "V"),
            format_si(stats.pk_pk_v, "V"),
            format_si(stats.mean_v, "V"),
            format_si(stats.rms_v, "V"),
            frequency,
            stats
                .duty_cycle_pct
                .map_or_else(String::new, |d| format!("  duty {d:.1} %")),
        ));
    }
    lines.extend(warnings(capture).into_iter().map(|w| format!("! {w}")));
    lines
}

pub(crate) fn settings_line(settings: &Settings) -> String {
    let trigger = &settings.trigger;
    format!(
        "{}/div  {}  {}  trigger {} {} {} {}  [{}]",
        format_si(settings.timebase_s_per_div, "s"),
        settings
            .sample_rate_sa_s
            .map_or_else(|| "?".to_owned(), |r| format_si(r, "Sa/s")),
        settings.acquisition,
        trigger.source,
        trigger.edge,
        trigger
            .level_v
            .map_or_else(|| "?".to_owned(), |l| format_si(l, "V")),
        trigger.sweep,
        settings.run_status,
    )
}

/// A decode for agents: summary, detected parameters, warnings, and events with hex payloads.
pub(crate) fn decoded_json(decoded: &scopewerk_core::Decoded, limit: usize) -> Value {
    let events: Vec<Value> = decoded
        .events
        .iter()
        .take(limit)
        .map(|event| {
            let mut value = serde_json::to_value(event).unwrap_or(Value::Null);
            if let Some(map) = value.as_object_mut() {
                for key in ["t_s", "end_s"] {
                    if let Some(t) = map.get(key).and_then(Value::as_f64) {
                        map.insert(key.into(), sig(t, 6));
                    }
                }
                if let Some(bytes) = map.get("data").and_then(Value::as_array) {
                    let hex: Vec<String> = bytes
                        .iter()
                        .filter_map(Value::as_u64)
                        .map(|b| format!("{b:02X}"))
                        .collect();
                    map.insert("data".into(), json!(hex.join(" ")));
                }
                if let Some(words) = map.get("words").and_then(Value::as_array) {
                    let hex: Vec<String> = words
                        .iter()
                        .filter_map(Value::as_u64)
                        .map(|w| format!("{w:02X}"))
                        .collect();
                    map.insert("words".into(), json!(hex.join(" ")));
                }
                if map.get("type").and_then(Value::as_str) == Some("can_frame")
                    && let Some(id) = map.get("id").and_then(Value::as_u64)
                {
                    let extended = map
                        .get("extended")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    map.insert(
                        "id".into(),
                        json!(if extended {
                            format!("0x{id:08X}")
                        } else {
                            format!("0x{id:03X}")
                        }),
                    );
                }
                if let Some(address) = map.get("address").and_then(Value::as_u64) {
                    map.insert("address".into(), json!(format!("0x{address:02X}")));
                }
                map.insert("text".into(), json!(event.describe()));
            }
            value
        })
        .collect();
    json!({
        "protocol": decoded.protocol,
        "summary": decoded.summary,
        "detected": {
            "rate_hz": decoded.rate_hz.map(|r| sig(r, 5)),
            "samples_per_bit": decoded.samples_per_bit.map(|s| sig(s, 3)),
            "detail": decoded.detail,
            "thresholds_v": decoded.thresholds_v.iter().map(|(ch, v)| json!({"channel": ch, "volts": sig(*v, 3)})).collect::<Vec<_>>(),
        },
        "events": events,
        "events_total": decoded.events.len(),
        "warnings": decoded.warnings,
        "time_note": "t_s is seconds from the trigger point",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_to_significant_digits() {
        assert_eq!(sig(0.123_456, 3), json!(0.123));
        assert_eq!(sig(1234.5, 3), json!(1230.0));
        assert_eq!(sig(f64::NAN, 3), Value::Null);
    }
}
