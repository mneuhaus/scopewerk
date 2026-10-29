//! Changing the scope: typed [`Change`]s that snap to valid settings, go out as SCPI and are
//! verified by reading the header back. The firmware silently ignores values it does not like,
//! so nothing here reports success without seeing the new value on the scope.

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::header::Settings;
use crate::scope::{Scope, check_channel};
use crate::units::{
    format_si, nearest_step, timebase_command_value, timebase_steps, trim_number,
    volts_command_value, volts_per_div_steps,
};
use crate::{Capture, Error};

macro_rules! choice {
    ($(#[$doc:meta])* $name:ident { $($variant:ident => $scpi:literal $(| $alias:literal)*),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &[$name] = &[$($name::$variant),+];

            /// The token the scope uses in commands and replies.
            pub fn scpi(self) -> &'static str {
                match self { $($name::$variant => $scpi),+ }
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(text: &str) -> Result<Self, Error> {
                let wanted = text.trim();
                $(
                    if wanted.eq_ignore_ascii_case($scpi)
                        || wanted.eq_ignore_ascii_case(stringify!($variant))
                        $(|| wanted.eq_ignore_ascii_case($alias))*
                    {
                        return Ok($name::$variant);
                    }
                )+
                let options: Vec<&str> = $name::ALL.iter().map(|v| v.scpi()).collect();
                Err(Error::InvalidArgument(format!(
                    "{} {wanted:?} (use one of {})", stringify!($name), options.join(", ")
                )))
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;
            fn try_from(text: String) -> Result<Self, Error> { text.parse() }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String { value.scpi().to_owned() }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.scpi()) }
        }
    };
}

choice!(
    /// Input coupling.
    Coupling { Dc => "DC", Ac => "AC", Gnd => "GND" | "ground" }
);
choice!(
    /// Trigger edge.
    Edge { Rise => "RISE" | "rising", Fall => "FALL" | "falling" }
);
choice!(
    /// Trigger sweep: free-running, wait for trigger, or one shot.
    Sweep { Auto => "AUTO", Normal => "NORMal" | "norm", Single => "SINGle" }
);
choice!(
    /// Acquisition mode.
    Acquisition { Sample => "SAMPle" | "samp", Peak => "PEAK" | "peak_detect", Average => "AVERage" | "avg" }
);

/// One setting to change. Voltages are at the probe tip; offsets in divisions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "set", rename_all = "snake_case")]
pub enum Change {
    /// Seconds per division (snapped to the 1-2-5 ladder, 2 ns … 100 s).
    Timebase {
        s_per_div: f64,
    },
    /// Moves the trigger point; divisions, positive = right.
    HorizontalOffset {
        div: f64,
    },
    ChannelEnabled {
        channel: u8,
        enabled: bool,
    },
    /// Volts per division at the probe tip (snapped to the 1-2-5 ladder).
    VoltsPerDiv {
        channel: u8,
        volts: f64,
    },
    /// Where 0 V sits, divisions from the centre (−4 … +4, positive = up).
    ChannelOffset {
        channel: u8,
        div: f64,
    },
    Coupling {
        channel: u8,
        coupling: Coupling,
    },
    /// Probe attenuation: 1, 10, 100 or 1000.
    Probe {
        channel: u8,
        factor: u32,
    },
    Invert {
        channel: u8,
        inverted: bool,
    },
    TriggerSource {
        channel: u8,
    },
    /// Trigger level in volts at the probe tip.
    TriggerLevel {
        volts: f64,
    },
    TriggerEdge {
        edge: Edge,
    },
    TriggerSweep {
        sweep: Sweep,
    },
    TriggerCoupling {
        coupling: Coupling,
    },
    Acquisition {
        mode: Acquisition,
    },
}

/// Things to do rather than settings to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Run,
    Stop,
    /// Arm one acquisition (sweep SINGLE + run); the scope stops after the next trigger.
    Single,
    /// Let the scope pick timebase, V/div and trigger for the connected signals.
    Autoset,
}

/// What happened to one requested change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub change: Change,
    pub applied: bool,
    /// Human summary, e.g. "timebase 1.3 ms/div snapped to 1.00 ms/div".
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyReport {
    pub outcomes: Vec<Outcome>,
    pub settings: Settings,
}

impl ApplyReport {
    pub fn all_applied(&self) -> bool {
        self.outcomes.iter().all(|o| o.applied)
    }
}

impl Change {
    fn channel(&self) -> Option<u8> {
        match self {
            Self::ChannelEnabled { channel, .. }
            | Self::VoltsPerDiv { channel, .. }
            | Self::ChannelOffset { channel, .. }
            | Self::Coupling { channel, .. }
            | Self::Probe { channel, .. }
            | Self::Invert { channel, .. }
            | Self::TriggerSource { channel } => Some(*channel),
            _ => None,
        }
    }

    /// Snap to a value the scope accepts. Returns the snapped change and a note when it moved.
    fn snapped(&self, current: &Settings) -> Result<(Change, Option<String>), Error> {
        if let Some(channel) = self.channel() {
            check_channel(channel)?;
        }
        Ok(match *self {
            Self::Timebase { s_per_div } => {
                positive(s_per_div, "timebase")?;
                let snapped = nearest_step(&timebase_steps(), s_per_div);
                let note = moved(s_per_div, snapped, "s/div");
                (Self::Timebase { s_per_div: snapped }, note)
            }
            Self::VoltsPerDiv { channel, volts } => {
                positive(volts, "volts per division")?;
                let probe = probe_of(current, channel);
                let snapped = nearest_step(&volts_per_div_steps(), volts / probe) * probe;
                let note = moved(volts, snapped, "V/div");
                (
                    Self::VoltsPerDiv {
                        channel,
                        volts: snapped,
                    },
                    note,
                )
            }
            Self::ChannelOffset { channel, div } => {
                let clamped = (div * 50.0).round().clamp(-200.0, 200.0) / 50.0;
                let note = moved(div, clamped, "div");
                (
                    Self::ChannelOffset {
                        channel,
                        div: clamped,
                    },
                    note,
                )
            }
            Self::HorizontalOffset { div } => {
                let rounded = (div * 50.0).round() / 50.0;
                (
                    Self::HorizontalOffset { div: rounded },
                    moved(div, rounded, "div"),
                )
            }
            Self::Probe { factor, .. } if ![1, 10, 100, 1000].contains(&factor) => {
                return Err(Error::InvalidArgument(format!(
                    "probe factor {factor} (use 1, 10, 100 or 1000)"
                )));
            }
            ref other => (other.clone(), None),
        })
    }

    fn command(&self, current: &Settings) -> String {
        match *self {
            Self::Timebase { s_per_div } => {
                format!(":HORIzontal:SCALe {}", timebase_command_value(s_per_div))
            }
            Self::HorizontalOffset { div } => format!(":HORIzontal:OFFSet {}", trim_number(div)),
            Self::ChannelEnabled { channel, enabled } => {
                format!(":CH{channel}:DISPlay {}", on_off(enabled))
            }
            Self::VoltsPerDiv { channel, volts } => format!(
                ":CH{channel}:SCALe {}",
                volts_command_value(volts / probe_of(current, channel))
            ),
            Self::ChannelOffset { channel, div } => {
                format!(":CH{channel}:OFFSet {}", trim_number(div))
            }
            Self::Coupling { channel, coupling } => format!(":CH{channel}:COUPling {coupling}"),
            Self::Probe { channel, factor } => format!(":CH{channel}:PROBe {factor}X"),
            Self::Invert { channel, inverted } => {
                format!(":CH{channel}:INVerse {}", on_off(inverted))
            }
            Self::TriggerSource { channel } => format!(":TRIGger:SINGle:EDGE:SOURce CH{channel}"),
            Self::TriggerLevel { volts } => {
                format!(":TRIGger:SINGle:EDGE:LEVel {}", trim_number(volts))
            }
            Self::TriggerEdge { edge } => format!(":TRIGger:SINGle:EDGE:SLOPe {edge}"),
            Self::TriggerSweep { sweep } => format!(":TRIGger:SINGle:SWEep {sweep}"),
            Self::TriggerCoupling { coupling } => {
                format!(":TRIGger:SINGle:EDGE:COUPling {coupling}")
            }
            Self::Acquisition { mode } => format!(":ACQuire:MODE {mode}"),
        }
    }

    /// `Ok` with a description when the settings show the change, `Err(actual)` otherwise.
    fn verify(&self, settings: &Settings) -> Result<String, String> {
        let channel = |n: u8| settings.channel(n);
        let close = |a: f64, b: f64, tolerance: f64| (a - b).abs() <= tolerance;
        match *self {
            Self::Timebase { s_per_div } => {
                let actual = settings.timebase_s_per_div;
                check(close(actual, s_per_div, s_per_div * 1e-3), || {
                    format_si(actual, "s/div")
                })
                .map(|v| format!("timebase {v}"))
            }
            Self::HorizontalOffset { div } => {
                let actual = settings.horizontal_offset_div;
                check(close(actual, div, 0.011), || format!("{actual} div"))
                    .map(|v| format!("horizontal offset {v}"))
            }
            Self::ChannelEnabled {
                channel: n,
                enabled,
            } => {
                let actual = channel(n).map(|c| c.enabled);
                check(actual == Some(enabled), || format!("{actual:?}"))
                    .map(|_| format!("CH{n} {}", if enabled { "on" } else { "off" }))
            }
            Self::VoltsPerDiv { channel: n, volts } => {
                let actual = channel(n).map_or(f64::NAN, |c| c.volts_per_div);
                check(close(actual, volts, volts * 1e-3), || {
                    format_si(actual, "V/div")
                })
                .map(|v| format!("CH{n} {v}"))
            }
            Self::ChannelOffset { channel: n, div } => {
                let actual = channel(n).map_or(f64::NAN, |c| c.offset_div);
                check(close(actual, div, 0.011), || format!("{actual} div"))
                    .map(|v| format!("CH{n} offset {v}"))
            }
            Self::Coupling {
                channel: n,
                coupling,
            } => {
                let actual = channel(n).map(|c| c.coupling.clone()).unwrap_or_default();
                check(same(&actual, coupling.scpi()), || actual.clone())
                    .map(|v| format!("CH{n} coupling {v}"))
            }
            Self::Probe { channel: n, factor } => {
                let actual = channel(n).map(|c| c.probe.clone()).unwrap_or_default();
                check(same(&actual, &format!("{factor}X")), || actual.clone())
                    .map(|v| format!("CH{n} probe {v}"))
            }
            Self::Invert {
                channel: n,
                inverted,
            } => {
                let actual = channel(n).map(|c| c.inverted);
                check(actual == Some(inverted), || format!("{actual:?}"))
                    .map(|_| format!("CH{n} inverted {inverted}"))
            }
            Self::TriggerSource { channel: n } => {
                let actual = &settings.trigger.source;
                check(same(actual, &format!("CH{n}")), || actual.clone())
                    .map(|v| format!("trigger source {v}"))
            }
            Self::TriggerLevel { volts } => {
                let actual = settings.trigger.level_v.unwrap_or(f64::NAN);
                // The scope rounds the level to its trigger DAC step.
                let tolerance = (volts.abs() * 0.02).max(0.05);
                check(close(actual, volts, tolerance), || format_si(actual, "V"))
                    .map(|v| format!("trigger level {v}"))
            }
            Self::TriggerEdge { edge } => {
                let actual = &settings.trigger.edge;
                check(same(actual, edge.scpi()), || actual.clone())
                    .map(|v| format!("trigger edge {v}"))
            }
            Self::TriggerSweep { sweep } => {
                let actual = &settings.trigger.sweep;
                check(same(actual, sweep.scpi()), || actual.clone())
                    .map(|v| format!("trigger sweep {v}"))
            }
            Self::TriggerCoupling { coupling } => {
                let actual = &settings.trigger.coupling;
                check(same(actual, coupling.scpi()), || actual.clone())
                    .map(|v| format!("trigger coupling {v}"))
            }
            Self::Acquisition { mode } => {
                let actual = &settings.acquisition;
                check(same(actual, mode.scpi()), || actual.clone())
                    .map(|v| format!("acquisition {v}"))
            }
        }
    }
}

fn check(ok: bool, actual: impl FnOnce() -> String) -> Result<String, String> {
    let actual = actual();
    if ok { Ok(actual) } else { Err(actual) }
}

fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

fn on_off(on: bool) -> &'static str {
    if on { "ON" } else { "OFF" }
}

fn positive(value: f64, what: &str) -> Result<(), Error> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(Error::InvalidArgument(format!(
            "{what} must be a positive number, got {value}"
        )))
    }
}

fn moved(wanted: f64, snapped: f64, unit: &str) -> Option<String> {
    let differs = (wanted - snapped).abs() > snapped.abs().max(1e-12) * 1e-6;
    differs.then(|| {
        format!(
            "{} {unit} snapped to {}",
            trim_number(wanted),
            format_si(snapped, unit)
        )
    })
}

fn probe_of(settings: &Settings, channel: u8) -> f64 {
    settings.channel(channel).map_or(1.0, |c| {
        c.probe.trim_end_matches(['X', 'x']).parse().unwrap_or(1.0)
    })
}

impl Scope {
    /// The current settings in SI units.
    pub fn settings(&mut self) -> Result<Settings, Error> {
        Ok(self.header()?.settings())
    }

    /// Apply changes in order, then read the header once and check every one of them.
    /// Probe changes go first so that V/div and trigger values are meant at the new probe.
    pub fn apply(&mut self, changes: &[Change]) -> Result<ApplyReport, Error> {
        let mut ordered: Vec<&Change> = changes
            .iter()
            .filter(|c| matches!(c, Change::Probe { .. }))
            .collect();
        ordered.extend(
            changes
                .iter()
                .filter(|c| !matches!(c, Change::Probe { .. })),
        );

        let mut current = self.settings()?;
        let mut sent = Vec::with_capacity(ordered.len());
        for change in ordered {
            let (snapped, note) = change.snapped(&current)?;
            self.scpi(&snapped.command(&current))?;
            if matches!(snapped, Change::Probe { .. }) {
                // V/div commands depend on the probe; read it back before continuing.
                current = self.settings()?;
            }
            sent.push((snapped, note));
        }
        let settings = self.settings()?;
        let outcomes = sent
            .into_iter()
            .map(|(change, note)| {
                let (applied, message) = match change.verify(&settings) {
                    Ok(done) => (true, note.map_or(done.clone(), |n| format!("{done} ({n})"))),
                    Err(actual) => (
                        false,
                        format!(
                            "not applied: the scope still reports {actual} after `{}`",
                            change.command(&settings)
                        ),
                    ),
                };
                Outcome {
                    change,
                    applied,
                    message,
                }
            })
            .collect();
        Ok(ApplyReport { outcomes, settings })
    }

    /// Run, stop, single or autoset; returns the settings afterwards.
    pub fn act(&mut self, action: Action) -> Result<Settings, Error> {
        match action {
            Action::Run => self.scpi(":RUNning RUN")?,
            Action::Stop => self.scpi(":RUNning STOP")?,
            Action::Single => {
                self.scpi(":TRIGger:SINGle:SWEep SINGle")?;
                self.scpi(":RUNning RUN")?
            }
            Action::Autoset => {
                let reply = self.scpi(":AUTOset ON")?;
                // Autoset takes a few seconds; the scope answers queries meanwhile.
                std::thread::sleep(Duration::from_millis(2500));
                reply
            }
        };
        self.settings()
    }

    /// Is the scope acquiring (`RUN`) or frozen (`STOP`)?
    pub fn running(&mut self) -> Result<bool, Error> {
        Ok(!same(&self.scpi(":RUNning?")?, "STOP"))
    }

    /// Arm a single acquisition and wait until the scope triggered and stopped, then read it.
    /// Returns `None` when nothing triggered within `timeout`; the scope stays armed.
    pub fn single_shot(
        &mut self,
        channels: &[u8],
        memory: bool,
        timeout: Duration,
    ) -> Result<Option<Capture>, Error> {
        self.act(Action::Single)?;
        let deadline = Instant::now() + timeout;
        // Give the scope a moment to leave the stopped state before polling it.
        std::thread::sleep(Duration::from_millis(300));
        while Instant::now() < deadline {
            if !self.running()? {
                let capture = if memory {
                    self.capture_memory(channels)?
                } else {
                    self.capture(channels)?
                };
                return Ok(Some(capture));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_parse_loosely_and_print_as_scpi() {
        assert_eq!("dc".parse::<Coupling>().ok(), Some(Coupling::Dc));
        assert_eq!("falling".parse::<Edge>().ok(), Some(Edge::Fall));
        assert_eq!("normal".parse::<Sweep>().ok(), Some(Sweep::Normal));
        assert_eq!(Sweep::Normal.to_string(), "NORMal");
        assert!("sideways".parse::<Edge>().is_err());
        let change: Change =
            serde_json::from_str(r#"{"set":"trigger_edge","edge":"fall"}"#).expect("parses");
        assert_eq!(change, Change::TriggerEdge { edge: Edge::Fall });
    }
}
