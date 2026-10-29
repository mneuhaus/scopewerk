//! Engineering quantities as the scope writes them ("1.00V", "500mV", "1.0ms", "(500KS/s)")
//! and the 1-2-5 setting ladders it accepts.

/// Parse a scope quantity into SI base units. Units listed in `UNITS` are stripped, then a
/// single SI prefix is applied: `"1.0ms"` → `0.001`, `"(500KS/s)"` → `500000.0`.
pub fn parse_quantity(text: &str) -> Option<f64> {
    const UNITS: [&str; 6] = ["S/s", "Sa/s", "V", "A", "s", "Hz"];
    let mut text = text.trim().trim_matches(|c| c == '(' || c == ')').trim();
    for unit in UNITS {
        if let Some(stripped) = text.strip_suffix(unit) {
            text = stripped;
            break;
        }
    }
    let (number, factor) = match text.chars().last()? {
        'n' => (&text[..text.len() - 1], -9),
        'u' => (&text[..text.len() - 1], -6),
        'µ' => (&text[..text.len() - 'µ'.len_utf8()], -6),
        'm' => (&text[..text.len() - 1], -3),
        'k' | 'K' => (&text[..text.len() - 1], 3),
        'M' => (&text[..text.len() - 1], 6),
        'G' => (&text[..text.len() - 1], 9),
        _ => (text, 0),
    };
    // Dividing by an exact power of ten keeps "100ns" at exactly 1e-7.
    let value = number.trim().parse::<f64>().ok()?;
    Some(if factor < 0 {
        value / 10f64.powi(-factor)
    } else {
        value * 10f64.powi(factor)
    })
}

/// Human formatting with an SI prefix and up to three significant digits: `0.0012, "V"` →
/// `"1.20 mV"`.
pub fn format_si(value: f64, unit: &str) -> String {
    if !value.is_finite() {
        return format!("-- {unit}");
    }
    if value == 0.0 {
        return format!("0 {unit}");
    }
    const PREFIXES: [(f64, &str); 7] = [
        (1e9, "G"),
        (1e6, "M"),
        (1e3, "k"),
        (1.0, ""),
        (1e-3, "m"),
        (1e-6, "µ"),
        (1e-9, "n"),
    ];
    let magnitude = value.abs();
    let (factor, prefix) = PREFIXES
        .iter()
        .copied()
        .find(|(factor, _)| magnitude >= *factor * 0.9995)
        .unwrap_or((1e-12, "p"));
    let scaled = value / factor;
    let digits = if scaled.abs() >= 99.95 {
        0
    } else if scaled.abs() >= 9.995 {
        1
    } else {
        2
    };
    format!("{scaled:.digits$} {prefix}{unit}")
}

/// The 1-2-5 ladder between `min` and `max` (inclusive, base units).
fn ladder(min: f64, max: f64) -> Vec<f64> {
    let mut values = Vec::new();
    let mut decade = 1e-9;
    while decade <= max * 1.001 {
        for step in [1.0, 2.0, 5.0] {
            let value = decade * step;
            if value >= min * 0.999 && value <= max * 1.001 {
                values.push(value);
            }
        }
        decade *= 10.0;
    }
    values
}

/// Timebase settings in s/div, fastest first.
pub fn timebase_steps() -> Vec<f64> {
    ladder(2e-9, 100.0)
}

/// Vertical settings in V/div at the probe tip for a 1X probe, finest first.
pub fn volts_per_div_steps() -> Vec<f64> {
    ladder(2e-3, 10.0)
}

/// The ladder value closest to `value` on a log scale.
pub fn nearest_step(steps: &[f64], value: f64) -> f64 {
    steps
        .iter()
        .copied()
        .min_by(|a, b| {
            let da = (a.ln() - value.ln()).abs();
            let db = (b.ln() - value.ln()).abs();
            da.total_cmp(&db)
        })
        .unwrap_or(value)
}

/// The neighbouring ladder step: `direction` > 0 coarser, < 0 finer.
pub fn step_from(steps: &[f64], value: f64, direction: i32) -> f64 {
    let current = nearest_step(steps, value);
    let index = steps
        .iter()
        .position(|step| (*step - current).abs() < current * 1e-6)
        .unwrap_or(0);
    let next = if direction > 0 {
        (index + 1).min(steps.len() - 1)
    } else if direction < 0 {
        index.saturating_sub(1)
    } else {
        index
    };
    steps[next]
}

/// A timebase as the scope's `:HORIzontal:SCALe` wants it. One decimal below ten ("1.0ms",
/// "2.0us"); a bare "1ms" is silently ignored by firmware V1.5.0.
pub fn timebase_command_value(seconds: f64) -> String {
    let (scaled, unit) = split_prefix(seconds, &["s", "ms", "us", "ns"]);
    if scaled < 9.95 {
        format!("{scaled:.1}{unit}")
    } else {
        format!("{scaled:.0}{unit}")
    }
}

/// A V/div value as `:CHn:SCALe` wants it ("500mV", "1V").
pub fn volts_command_value(volts: f64) -> String {
    let (scaled, unit) = split_prefix(volts, &["V", "mV"]);
    format!("{}{unit}", trim_number(scaled))
}

fn split_prefix<'a>(value: f64, units: &[&'a str]) -> (f64, &'a str) {
    let mut scaled = value;
    for unit in units {
        if scaled >= 0.9995 || *unit == units[units.len() - 1] {
            return (scaled, unit);
        }
        scaled *= 1000.0;
    }
    (value, units[0])
}

/// `2.0` → `"2"`, `2.5` → `"2.5"`.
pub fn trim_number(value: f64) -> String {
    let text = format!("{value:.4}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() || text == "-" {
        "0".to_owned()
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scope_quantities() {
        assert_eq!(parse_quantity("1.00V"), Some(1.0));
        assert_eq!(parse_quantity("500mV"), Some(0.5));
        assert_eq!(parse_quantity("1.0ms"), Some(0.001));
        assert_eq!(parse_quantity("(500KS/s)"), Some(500_000.0));
        assert_eq!(parse_quantity("100ns"), Some(1e-7));
        assert_eq!(parse_quantity("10K"), Some(10_000.0));
        assert_eq!(parse_quantity("?"), None);
    }

    #[test]
    fn formats_commands_the_firmware_accepts() {
        assert_eq!(timebase_command_value(1e-3), "1.0ms");
        assert_eq!(timebase_command_value(2e-6), "2.0us");
        assert_eq!(timebase_command_value(50e-9), "50ns");
        assert_eq!(timebase_command_value(5.0), "5.0s");
        assert_eq!(volts_command_value(0.5), "500mV");
        assert_eq!(volts_command_value(1.0), "1V");
        assert_eq!(volts_command_value(0.002), "2mV");
    }

    #[test]
    fn steps_through_the_ladder() {
        let steps = timebase_steps();
        assert_eq!(step_from(&steps, 1e-3, 1), 2e-3);
        assert!((step_from(&steps, 1e-3, -1) - 5e-4).abs() < 1e-12);
        assert_eq!(nearest_step(&steps, 0.0013), 1e-3);
        assert_eq!(format_si(0.0012, "V"), "1.20 mV");
        assert_eq!(format_si(500_000.0, "S/s"), "500 kS/s");
    }
}
