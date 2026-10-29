//! The DOS1102 as a typed instrument: identity, screen capture, raw SCPI.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::header::{PIXELS_PER_DIV, ScreenHeader, to_f64};
use crate::units::parse_quantity;
use crate::usb::{DeviceSummary, Transport};
use crate::waveform::{Capture, decode_channel};

/// `*IDN?` split into its fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub manufacturer: String,
    pub model: String,
    pub serial: String,
    pub firmware: String,
}

impl Identity {
    fn parse(text: &str) -> Self {
        let mut fields = text.trim().split(',').map(|f| f.trim().to_owned());
        Self {
            manufacturer: fields.next().unwrap_or_default(),
            model: fields.next().unwrap_or_default(),
            serial: fields.next().unwrap_or_default(),
            firmware: fields.next().unwrap_or_default(),
        }
    }
}

/// An open DOS1102. One process at a time can hold it; see [`crate::broker`] for sharing.
pub struct Scope {
    transport: Transport,
}

impl std::fmt::Debug for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scope")
            .field("device", &self.transport.summary)
            .finish()
    }
}

impl Scope {
    /// Open the first attached DOS1102, or the one with this serial number.
    pub fn open(serial: Option<&str>) -> Result<Self, Error> {
        Ok(Self {
            transport: Transport::open(serial)?,
        })
    }

    pub fn device(&self) -> &DeviceSummary {
        &self.transport.summary
    }

    /// Send any SCPI command and return the reply as text without the firmware's `->` prompt.
    /// Set commands and unknown commands reply with an empty string.
    pub fn scpi(&mut self, command: &str) -> Result<String, Error> {
        let reply = self.transport.query(command)?;
        Ok(clean_reply(&reply))
    }

    /// Send a command and return the reply bytes untouched.
    pub fn scpi_raw(&mut self, command: &str) -> Result<Vec<u8>, Error> {
        self.transport.query(command)
    }

    pub fn identity(&mut self) -> Result<Identity, Error> {
        Ok(Identity::parse(&self.scpi("*IDN?")?))
    }

    /// The current screen header (timebase, channels, trigger).
    pub fn header(&mut self) -> Result<ScreenHeader, Error> {
        let payload = self.framed(":DATA:WAVE:SCREen:HEAD?")?;
        serde_json::from_slice(&payload)
            .map_err(|err| Error::Protocol(format!("screen header: {err}")))
    }

    /// Read the screen of the given channels (1 and/or 2) with a fresh header.
    pub fn capture(&mut self, channels: &[u8]) -> Result<Capture, Error> {
        let header = self.header()?;
        self.capture_with(&header, channels)
    }

    /// Read the screen with an already known header (saves one round trip in live views).
    pub fn capture_with(
        &mut self,
        header: &ScreenHeader,
        channels: &[u8],
    ) -> Result<Capture, Error> {
        let mut waveforms = Vec::with_capacity(channels.len());
        for &channel in channels {
            check_channel(channel)?;
            let payload = self.framed(&format!(":DATA:WAVE:SCREen:CH{channel}?"))?;
            waveforms.push(decode_channel(channel, &payload, header));
        }
        let dt = header.seconds_per_point();
        let points = waveforms.iter().map(|w| w.volts.len()).max().unwrap_or(0);
        Ok(Capture {
            captured_at_ms: now_ms(),
            settings: header.settings(),
            dt,
            t0: -(to_f64(points) / 2.0) * dt + hoffset_seconds(header),
            waveforms,
        })
    }

    /// Read the acquisition memory (all samples at the real sample rate, typically 10 000
    /// points, 5x the screen resolution) of the given channels.
    pub fn capture_memory(&mut self, channels: &[u8]) -> Result<Capture, Error> {
        let payload = self.framed(":DATA:WAVE:DEPMem:HEAD?")?;
        let mut header: ScreenHeader = serde_json::from_slice(&payload)
            .map_err(|err| Error::Protocol(format!("memory header: {err}")))?;
        // The memory header has no trigger block; the screen header does.
        header.trigger = self.header()?.trigger;
        let mut waveforms = Vec::with_capacity(channels.len());
        for &channel in channels {
            check_channel(channel)?;
            let payload = self.framed(&format!(":DATA:WAVE:DEPMem:CH{channel}?"))?;
            waveforms.push(decode_channel(channel, &payload, &header));
        }
        let rate = parse_quantity(&header.sample.sample_rate).unwrap_or(f64::NAN);
        let dt = 1.0 / rate;
        let screen_centre = header.sample.screen_offset + header.sample.full_screen / 2;
        Ok(Capture {
            captured_at_ms: now_ms(),
            settings: header.settings(),
            dt,
            t0: -to_f64(screen_centre) * dt + hoffset_seconds(&header),
            waveforms,
        })
    }

    /// The scope's display exactly as it looks right now: a 24-bit 800×480 BMP (about 1.6 s).
    pub fn screenshot_bmp(&mut self) -> Result<Vec<u8>, Error> {
        let bmp = self.framed(":DATA:WAVE:SCREen:BMP?");
        self.transport.drain_stray_reply();
        let bmp = bmp?;
        if !bmp.starts_with(b"BM") {
            return Err(Error::Protocol("screenshot is not a BMP".into()));
        }
        Ok(bmp)
    }

    /// The scope's own measurements of one channel (`:MEAS:CHn?`), as JSON.
    pub fn scope_measurements(&mut self, channel: u8) -> Result<serde_json::Value, Error> {
        let payload = self.framed(&format!(":MEAS:CH{channel}?"))?;
        serde_json::from_slice(&payload)
            .map_err(|err| Error::Protocol(format!("measurements: {err}")))
    }

    /// Replies that carry a 4-byte little-endian length prefix.
    fn framed(&mut self, command: &str) -> Result<Vec<u8>, Error> {
        let reply = self.transport.query(command)?;
        if reply.len() < 4 {
            return Err(Error::Protocol(format!("{command}: empty reply")));
        }
        let length = usize::try_from(u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]))
            .unwrap_or(usize::MAX);
        let end = (4 + length).min(reply.len());
        Ok(reply[4..end].to_vec())
    }
}

pub(crate) fn check_channel(channel: u8) -> Result<(), Error> {
    if (1..=2).contains(&channel) {
        Ok(())
    } else {
        Err(Error::InvalidArgument(format!(
            "channel {channel} (the DOS1102 has CH1 and CH2)"
        )))
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// The horizontal offset as a time shift of the screen centre.
fn hoffset_seconds(header: &ScreenHeader) -> f64 {
    let timebase = parse_quantity(&header.timebase.scale).unwrap_or(f64::NAN);
    to_f64(header.timebase.hoffset) / PIXELS_PER_DIV * timebase
}

fn clean_reply(reply: &[u8]) -> String {
    let text = String::from_utf8_lossy(reply);
    let text = text.trim_end_matches(['\n', '\r', '\0']);
    text.strip_suffix("->").unwrap_or(text).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_identity_and_replies() {
        let id = Identity::parse("   HANMA,DOS1102,2308237,V1.5.0");
        assert_eq!(id.model, "DOS1102");
        assert_eq!(id.firmware, "V1.5.0");
        assert_eq!(clean_reply(b"1.0ms->\n"), "1.0ms");
        assert_eq!(clean_reply(b""), "");
    }
}
