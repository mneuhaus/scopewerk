//! Scopewerk core: a pure-Rust driver for the Hanmatek DOS1102 oscilloscope (OWON SDS SCPI
//! dialect over USB, no libusb, no Python) and the broker that lets the desktop app, the CLI and
//! the MCP server share one scope.

pub mod broker;
pub mod control;
pub mod decode;
pub mod endpoint;
pub mod header;
pub mod image;
pub mod scope;
pub mod units;
pub mod usb;
pub mod waveform;

pub use broker::{Reply, Request, Session, Status};
pub use control::{Acquisition, Action, ApplyReport, Change, Coupling, Edge, Outcome, Sweep};
pub use decode::{DecodeConfig, Decoded, Input};
pub use header::{ChannelSettings, ScreenHeader, Settings, TriggerSettings};
pub use scope::{Identity, Scope};
pub use waveform::{Capture, Stats, Waveform};

/// Everything that can go wrong talking to the scope.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "no DOS1102 found on USB (5345:1235). Connect the square USB-B port on the back of the \
         scope, not the USB-A stick port on the front"
    )]
    NotFound,
    #[error("the scope is in use by another program (only one process can hold its USB interface)")]
    Busy,
    #[error("the scope has no bulk endpoints on interface 0")]
    NoEndpoints,
    #[error("timed out waiting for the scope")]
    Timeout,
    #[error("the scope was disconnected")]
    Disconnected,
    #[error("USBTMC reply out of sync; unplug and replug the scope's USB cable")]
    OutOfSync,
    #[error("USB: {0}")]
    Usb(#[source] nusb::Error),
    #[error("USB transfer failed: {0:?}")]
    Transfer(nusb::transfer::TransferError),
    #[error("unexpected reply: {0}")]
    Protocol(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("the scope did not apply {what}: asked for {wanted}, it reports {actual}")]
    NotApplied {
        what: String,
        wanted: String,
        actual: String,
    },
    #[error("broker: {0}")]
    Broker(String),
    #[error(
        "Windows has the scope on the \"{0}\" driver, Scopewerk needs WinUSB. Install it once with \
         Zadig (https://zadig.akeo.ie): Options > List All Devices, pick \"Oscilloscope\" (USB ID 5345 \
         1235), choose WinUSB, Replace Driver"
    )]
    NeedsDriver(String),
}
