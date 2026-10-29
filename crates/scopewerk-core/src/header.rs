//! The screen header the scope returns for `:DATA:WAVE:SCREen:HEAD?`, and the normalized
//! [`Settings`] view (SI numbers, snake_case) that Scopewerk shows to people and agents.

use serde::{Deserialize, Serialize};

use crate::units::parse_quantity;

/// Raw header as sent by firmware V1.5.0. Field names follow the scope's JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenHeader {
    #[serde(rename = "TIMEBASE")]
    pub timebase: TimebaseHeader,
    #[serde(rename = "SAMPLE")]
    pub sample: SampleHeader,
    #[serde(rename = "CHANNEL")]
    pub channels: Vec<ChannelHeader>,
    #[serde(rename = "DATATYPE")]
    pub data_type: String,
    #[serde(rename = "RUNSTATUS")]
    pub run_status: String,
    #[serde(rename = "IDN")]
    pub idn: String,
    #[serde(rename = "MODEL")]
    pub model: String,
    #[serde(rename = "Trig")]
    pub trigger: TriggerHeader,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TimebaseHeader {
    #[serde(rename = "SCALE")]
    pub scale: String,
    /// Horizontal offset in screen pixels (50 per division).
    #[serde(rename = "HOFFSET")]
    pub hoffset: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SampleHeader {
    #[serde(rename = "FULLSCREEN")]
    pub full_screen: usize,
    #[serde(rename = "SLOWMOVE")]
    pub slow_move: i64,
    #[serde(rename = "DATALEN")]
    pub data_len: usize,
    #[serde(rename = "SAMPLERATE")]
    pub sample_rate: String,
    #[serde(rename = "TYPE")]
    pub kind: String,
    #[serde(rename = "DEPMEM")]
    pub depth: String,
    /// Memory headers only: index of the first on-screen sample.
    #[serde(rename = "SCREENOFFSET")]
    pub screen_offset: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelHeader {
    #[serde(rename = "NAME")]
    pub name: String,
    #[serde(rename = "DISPLAY")]
    pub display: String,
    #[serde(rename = "COUPLING")]
    pub coupling: String,
    #[serde(rename = "PROBE")]
    pub probe: String,
    #[serde(rename = "SCALE")]
    pub scale: String,
    /// Vertical offset in screen pixels (50 per division).
    #[serde(rename = "OFFSET")]
    pub offset: i64,
    #[serde(rename = "FREQUENCE")]
    pub frequency: f64,
    #[serde(rename = "INVERSE")]
    pub inverse: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TriggerHeader {
    #[serde(rename = "Mode")]
    pub mode: String,
    #[serde(rename = "Type")]
    pub kind: String,
    #[serde(rename = "Items")]
    pub items: TriggerItems,
    #[serde(rename = "Sweep")]
    pub sweep: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TriggerItems {
    #[serde(rename = "Channel")]
    pub channel: String,
    #[serde(rename = "Level")]
    pub level: String,
    #[serde(rename = "Edge")]
    pub edge: String,
    #[serde(rename = "Coupling")]
    pub coupling: String,
    #[serde(rename = "HoldOff")]
    pub holdoff: String,
}

/// Screen pixels per division; channel and horizontal offsets are given in pixels.
pub const PIXELS_PER_DIV: f64 = 50.0;
/// Horizontal divisions across the DOS1102 screen (760 pixels).
pub const HORIZONTAL_DIVS: f64 = 15.2;
/// Vertical divisions on the screen.
pub const VERTICAL_DIVS: f64 = 8.0;

/// Scope state in SI units, the shape every Scopewerk interface reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub timebase_s_per_div: f64,
    pub horizontal_offset_div: f64,
    pub sample_rate_sa_s: Option<f64>,
    pub memory_depth: String,
    pub acquisition: String,
    pub run_status: String,
    pub channels: Vec<ChannelSettings>,
    pub trigger: TriggerSettings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelSettings {
    pub channel: u8,
    pub enabled: bool,
    /// V/div at the probe tip (the header's SCALE times the probe factor).
    pub volts_per_div: f64,
    /// Where 0 V sits, in divisions from the screen centre (positive = up).
    pub offset_div: f64,
    pub coupling: String,
    pub probe: String,
    pub inverted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerSettings {
    pub mode: String,
    pub kind: String,
    pub source: String,
    pub level_v: Option<f64>,
    pub edge: String,
    pub coupling: String,
    pub holdoff_s: Option<f64>,
    pub sweep: String,
}

impl ScreenHeader {
    pub fn settings(&self) -> Settings {
        Settings {
            timebase_s_per_div: parse_quantity(&self.timebase.scale).unwrap_or(f64::NAN),
            horizontal_offset_div: pixels_to_div(self.timebase.hoffset),
            sample_rate_sa_s: parse_quantity(&self.sample.sample_rate),
            memory_depth: self.sample.depth.clone(),
            acquisition: self.sample.kind.clone(),
            run_status: self.run_status.clone(),
            channels: self
                .channels
                .iter()
                .enumerate()
                .map(|(index, channel)| ChannelSettings {
                    channel: u8::try_from(index + 1).unwrap_or(u8::MAX),
                    enabled: channel.display.eq_ignore_ascii_case("ON"),
                    volts_per_div: channel.tip_volts_per_div(),
                    offset_div: pixels_to_div(channel.offset),
                    coupling: channel.coupling.clone(),
                    probe: channel.probe.clone(),
                    inverted: channel.inverse.eq_ignore_ascii_case("ON"),
                })
                .collect(),
            trigger: TriggerSettings {
                mode: self.trigger.mode.clone(),
                kind: self.trigger.kind.clone(),
                source: self.trigger.items.channel.clone(),
                level_v: parse_quantity(&self.trigger.items.level),
                edge: self.trigger.items.edge.clone(),
                coupling: self.trigger.items.coupling.clone(),
                holdoff_s: parse_quantity(&self.trigger.items.holdoff),
                sweep: self.trigger.sweep.clone(),
            },
        }
    }

    /// Seconds between two screen points.
    pub fn seconds_per_point(&self) -> f64 {
        let timebase = parse_quantity(&self.timebase.scale).unwrap_or(f64::NAN);
        let full_screen = if self.sample.full_screen > 0 {
            self.sample.full_screen
        } else {
            1520
        };
        timebase * HORIZONTAL_DIVS / to_f64(full_screen)
    }
}

impl ChannelHeader {
    /// The probe attenuation ("10X" → 10).
    pub fn probe_factor(&self) -> f64 {
        self.probe
            .trim()
            .trim_end_matches(['X', 'x'])
            .parse::<f64>()
            .unwrap_or(1.0)
    }

    /// SCALE is input-referred: at 10X the scope's own measurements read ten times higher while
    /// SCALE stays the same (verified on V1.5.0).
    pub fn tip_volts_per_div(&self) -> f64 {
        parse_quantity(&self.scale).unwrap_or(f64::NAN) * self.probe_factor()
    }
}

impl Settings {
    pub fn channel(&self, channel: u8) -> Option<&ChannelSettings> {
        self.channels.iter().find(|c| c.channel == channel)
    }
}

fn pixels_to_div(pixels: i64) -> f64 {
    to_f64(pixels) / PIXELS_PER_DIV
}

/// Lossless enough for screen-sized integers.
#[expect(clippy::cast_precision_loss, reason = "screen-sized values")]
pub(crate) fn to_f64<T: TryInto<i64>>(value: T) -> f64 {
    value.try_into().map_or(f64::NAN, |v: i64| v as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"TIMEBASE":{"SCALE":"1.0ms","HOFFSET":0},"SAMPLE":{"FULLSCREEN":1520,"SLOWMOVE":-1,"DATALEN":1520,"SAMPLERATE":"(500KS/s)","TYPE":"SAMPle","DEPMEM":"10K"},"CHANNEL":[{"NAME":"CH1","DISPLAY":"ON","Current_Rate":10000.0,"Current_Ratio":25.0,"Measure_Current_Switch":"OFF","COUPLING":"DC","PROBE":"1X","SCALE":"1.00V","OFFSET":-150,"FREQUENCE":0.0,"INVERSE":"OFF"},{"NAME":"CH2","DISPLAY":"OFF","COUPLING":"AC","PROBE":"10X","SCALE":"500mV","OFFSET":25,"FREQUENCE":0.0,"INVERSE":"OFF"}],"DATATYPE":"SCREEN","RUNSTATUS":"AUTO","IDN":"   HANMA,DOS1102,2308237,V1.5.0","MODEL":"110211101","Trig":{"Mode":"SINGle","Type":"EDGE","Items":{"Channel":"CH1","Level":"600mV","Edge":"RISE","Coupling":"DC","HoldOff":"100ns"},"Sweep":"AUTO"}}"#;

    #[test]
    fn normalizes_a_real_header() {
        let header: ScreenHeader = serde_json::from_str(SAMPLE).expect("header parses");
        let settings = header.settings();
        assert!((settings.timebase_s_per_div - 1e-3).abs() < 1e-12);
        assert_eq!(settings.sample_rate_sa_s, Some(500_000.0));
        let ch1 = settings.channel(1).expect("ch1");
        assert!(ch1.enabled);
        assert!((ch1.offset_div + 3.0).abs() < 1e-9);
        let ch2 = settings.channel(2).expect("ch2");
        assert!(!ch2.enabled);
        // CH2 has a 10X probe: 500 mV/div at the input is 5 V/div at the tip.
        assert!((ch2.volts_per_div - 5.0).abs() < 1e-12);
        assert_eq!(settings.trigger.level_v, Some(0.6));
        assert!((header.seconds_per_point() - 1e-5).abs() < 1e-12);
    }
}
