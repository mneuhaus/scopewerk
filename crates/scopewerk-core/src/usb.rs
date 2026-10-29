//! USB transport for the DOS1102: USBTMC framing on current firmware (`5345:1235`) and the
//! raw-bulk protocol of older firmware (`5345:1234`).

use std::time::Duration;

use nusb::transfer::{
    Buffer, Bulk, ControlIn, ControlType, Direction, EndpointType, In, Out, Recipient,
    TransferError,
};
use nusb::{Endpoint, Interface, MaybeFuture};

use crate::Error;

/// Hanmatek/OWON vendor ID.
pub const VENDOR_ID: u16 = 0x5345;
/// Firmware V1.5.0 and later: a USBTMC device.
pub const PRODUCT_ID_USBTMC: u16 = 0x1235;
/// Older firmware: plain text commands over raw bulk endpoints.
pub const PRODUCT_ID_RAW: u16 = 0x1234;

const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest reply we let the scope send per USBTMC message. It must hold a whole screenshot
/// (1.15 MB): this firmware truncates anything longer and leaks the rest into the next reply.
const READ_CHUNK: usize = 64 << 20;
/// One bulk IN transfer; a multiple of every max packet size. The scope sends 64-byte packets,
/// so a transfer usually completes long before this fills.
const READ_BUFFER: usize = 16 * 1024;

// USBTMC bulk message IDs.
const DEV_DEP_MSG_OUT: u8 = 1;
const REQUEST_DEV_DEP_MSG_IN: u8 = 2;
const USBTMC_HEADER_LEN: usize = 12;

/// A scope found on the bus, before it is opened.
#[derive(Debug, Clone)]
pub struct DeviceSummary {
    pub product_id: u16,
    pub serial: Option<String>,
    pub bus: String,
}

impl DeviceSummary {
    pub fn protocol(&self) -> &'static str {
        if self.product_id == PRODUCT_ID_RAW {
            "raw-bulk"
        } else {
            "usbtmc"
        }
    }
}

fn is_scope(info: &nusb::DeviceInfo) -> bool {
    info.vendor_id() == VENDOR_ID && matches!(info.product_id(), PRODUCT_ID_USBTMC | PRODUCT_ID_RAW)
}

/// Every DOS1102 currently attached.
pub fn list() -> Result<Vec<DeviceSummary>, Error> {
    Ok(nusb::list_devices()
        .wait()
        .map_err(Error::Usb)?
        .filter(is_scope)
        .map(|info| DeviceSummary {
            product_id: info.product_id(),
            serial: info.serial_number().map(str::to_owned),
            bus: info.bus_id().to_string(),
        })
        .collect())
}

/// An opened, claimed scope that exchanges one command for one reply.
pub(crate) struct Transport {
    device: nusb::Device,
    // Keeps the claim alive for the endpoints.
    interface: Interface,
    out: Endpoint<Bulk, Out>,
    input: Endpoint<Bulk, In>,
    usbtmc: bool,
    tag: u8,
    pub(crate) summary: DeviceSummary,
}

impl Transport {
    /// Open the first attached scope, or the one with the given serial number.
    pub(crate) fn open(serial: Option<&str>) -> Result<Self, Error> {
        let info = nusb::list_devices()
            .wait()
            .map_err(Error::Usb)?
            .filter(is_scope)
            .find(|info| serial.is_none_or(|wanted| info.serial_number() == Some(wanted)))
            .ok_or(Error::NotFound)?;
        // Windows only reaches the scope through WinUSB (installed once with Zadig).
        #[cfg(target_os = "windows")]
        if let Some(driver) = info.driver()
            && !driver.eq_ignore_ascii_case("winusb")
        {
            return Err(Error::NeedsDriver(driver.to_owned()));
        }
        let summary = DeviceSummary {
            product_id: info.product_id(),
            serial: info.serial_number().map(str::to_owned),
            bus: info.bus_id().to_string(),
        };
        let device = info.open().wait().map_err(Error::Usb)?;
        let interface = device.claim_interface(0).wait().map_err(|err| {
            if err.kind() == nusb::ErrorKind::Busy {
                Error::Busy
            } else {
                Error::Usb(err)
            }
        })?;
        let (out_addr, in_addr) = bulk_endpoints(&interface)?;
        let out = interface
            .endpoint::<Bulk, Out>(out_addr)
            .map_err(Error::Usb)?;
        let input = interface
            .endpoint::<Bulk, In>(in_addr)
            .map_err(Error::Usb)?;
        let mut transport = Self {
            device,
            interface,
            out,
            input,
            usbtmc: summary.product_id != PRODUCT_ID_RAW,
            tag: 0,
            summary,
        };
        // A previous program may have left a half-read reply or a stalled pipe behind.
        transport.clear();
        Ok(transport)
    }

    /// Bring the pipes back to a known state: USBTMC INITIATE_CLEAR (drops whatever the scope
    /// still had queued), clear both endpoint halts (resets the data toggles) and drain stale
    /// IN data. A program killed mid-transfer otherwise leaves every later transfer failing
    /// with an overrun. Best effort throughout.
    fn clear(&mut self) {
        const INITIATE_CLEAR: u8 = 5;
        const CHECK_CLEAR_STATUS: u8 = 6;
        const STATUS_PENDING: u8 = 2;
        let request = |request: u8, length: u16| ControlIn {
            control_type: ControlType::Class,
            recipient: Recipient::Interface,
            request,
            value: 0,
            index: 0,
            length,
        };
        let timeout = Duration::from_millis(500);
        if self.usbtmc
            && self
                .interface
                .control_in(request(INITIATE_CLEAR, 1), timeout)
                .wait()
                .is_ok()
        {
            for _ in 0..20 {
                match self
                    .interface
                    .control_in(request(CHECK_CLEAR_STATUS, 2), timeout)
                    .wait()
                {
                    Ok(status) if status.first() == Some(&STATUS_PENDING) => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    _ => break,
                }
            }
        }
        let _ = self.out.clear_halt().wait();
        let _ = self.input.clear_halt().wait();
        for _ in 0..8 {
            let completion = self
                .input
                .transfer_blocking(Buffer::new(READ_BUFFER), Duration::from_millis(30));
            if completion.status.is_err() {
                break;
            }
        }
    }

    /// Send one command and return the complete reply (empty for set commands). A pipe error
    /// or timeout gets one retry after [`Self::clear`]; if that fails too the scope is reset on the bus
    /// (it re-enumerates) and the caller sees [`Error::Disconnected`] and reconnects.
    pub(crate) fn query(&mut self, command: &str) -> Result<Vec<u8>, Error> {
        let started = std::time::Instant::now();
        let mut reply = self.query_once(command);
        let stuck = |reply: &Result<Vec<u8>, Error>| {
            matches!(
                reply,
                Err(Error::Transfer(_) | Error::OutOfSync | Error::Timeout)
            )
        };
        if stuck(&reply) {
            self.clear();
            reply = self.query_once(command);
            if stuck(&reply) {
                let _ = self.device.reset().wait();
                reply = Err(Error::Disconnected);
            }
        }
        if std::env::var_os("SCOPEWERK_TRACE").is_some() {
            let size = reply.as_ref().map_or(0, Vec::len);
            eprintln!(
                "scopewerk: {command} -> {size} bytes in {:?}",
                started.elapsed()
            );
        }
        reply
    }

    fn query_once(&mut self, command: &str) -> Result<Vec<u8>, Error> {
        if self.usbtmc {
            self.query_usbtmc(command)
        } else {
            self.query_raw(command)
        }
    }

    fn next_tag(&mut self) -> u8 {
        self.tag = self.tag % 255 + 1;
        self.tag
    }

    fn usbtmc_header(&mut self, msg_id: u8, size: usize, attributes: u8) -> (u8, Vec<u8>) {
        let tag = self.next_tag();
        let size = u32::try_from(size).unwrap_or(u32::MAX);
        let mut header = vec![msg_id, tag, !tag, 0];
        header.extend_from_slice(&size.to_le_bytes());
        header.extend_from_slice(&[attributes, 0, 0, 0]);
        (tag, header)
    }

    fn query_usbtmc(&mut self, command: &str) -> Result<Vec<u8>, Error> {
        let mut message = command.as_bytes().to_vec();
        message.push(b'\n');
        let (_, mut packet) = self.usbtmc_header(DEV_DEP_MSG_OUT, message.len(), 1);
        packet.extend_from_slice(&message);
        packet.resize(packet.len().next_multiple_of(4), 0);
        self.write(packet)?;
        self.receive_usbtmc()
    }

    /// Fetch a message the scope sends without being asked for it again. After a screenshot
    /// firmware V1.5.0 queues a stray `"<3 bytes>->\n"` that would otherwise answer the next
    /// command.
    pub(crate) fn drain_stray_reply(&mut self) {
        if self.usbtmc {
            let _ = self.receive_usbtmc();
        }
    }

    fn receive_usbtmc(&mut self) -> Result<Vec<u8>, Error> {
        let mut reply = Vec::new();
        loop {
            let (tag, request) = self.usbtmc_header(REQUEST_DEV_DEP_MSG_IN, READ_CHUNK, 0);
            self.write(request)?;
            let raw = self.read()?;
            if raw.len() < USBTMC_HEADER_LEN || raw[0] != REQUEST_DEV_DEP_MSG_IN || raw[1] != tag {
                return Err(Error::OutOfSync);
            }
            let size = usize::try_from(u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]))
                .unwrap_or(usize::MAX);
            let end_of_message = raw[8] & 1 == 1;
            let mut chunk = raw[USBTMC_HEADER_LEN..].to_vec();
            // This firmware splits one reply over several bulk reads.
            while chunk.len() < size {
                chunk.extend_from_slice(&self.read()?);
            }
            chunk.truncate(size);
            reply.extend_from_slice(&chunk);
            if end_of_message {
                return Ok(reply);
            }
        }
    }

    fn query_raw(&mut self, command: &str) -> Result<Vec<u8>, Error> {
        self.write(command.as_bytes().to_vec())?;
        match self.read() {
            Ok(reply) => Ok(reply),
            // Old firmware answers set commands with nothing at all.
            Err(Error::Timeout) => Ok(Vec::new()),
            Err(err) => Err(err),
        }
    }

    fn write(&mut self, bytes: Vec<u8>) -> Result<(), Error> {
        self.out
            .transfer_blocking(Buffer::from(bytes), WRITE_TIMEOUT)
            .status
            .map_err(transfer_error)
    }

    fn read(&mut self) -> Result<Vec<u8>, Error> {
        let completion = self
            .input
            .transfer_blocking(Buffer::new(READ_BUFFER), READ_TIMEOUT);
        completion.status.map_err(transfer_error)?;
        Ok(completion.buffer.into_vec())
    }
}

fn transfer_error(err: TransferError) -> Error {
    match err {
        TransferError::Cancelled => Error::Timeout,
        TransferError::Disconnected => Error::Disconnected,
        other => Error::Transfer(other),
    }
}

fn bulk_endpoints(interface: &Interface) -> Result<(u8, u8), Error> {
    let descriptor = interface.descriptor().ok_or(Error::NoEndpoints)?;
    let mut out = None;
    let mut input = None;
    for endpoint in descriptor.endpoints() {
        if endpoint.transfer_type() != Bulk::TYPE {
            continue;
        }
        match endpoint.direction() {
            Direction::Out => out = out.or(Some(endpoint.address())),
            Direction::In => input = input.or(Some(endpoint.address())),
        }
    }
    out.zip(input).ok_or(Error::NoEndpoints)
}
