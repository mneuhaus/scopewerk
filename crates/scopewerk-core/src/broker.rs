//! Sharing one scope. Only one process can hold the DOS1102's USB interface, so the Scopewerk
//! app owns it while it runs and serves the same [`Request`]s over a Unix socket. The CLI and
//! the MCP server use a [`Session`]: through the app when it is running (you watch the agent
//! work), straight over USB otherwise.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::control::{Action, ApplyReport, Change};
use crate::header::Settings;
use crate::scope::{Identity, Scope};
use crate::waveform::Capture;
use crate::{Error, image};

/// Everything a client can ask of the scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Status,
    Capture {
        channels: Vec<u8>,
        /// Acquisition memory (10 000 samples) instead of the 1 520 screen points.
        #[serde(default)]
        memory: bool,
    },
    Apply {
        changes: Vec<Change>,
    },
    Act {
        action: Action,
    },
    SingleShot {
        channels: Vec<u8>,
        #[serde(default)]
        memory: bool,
        timeout_ms: u64,
    },
    /// The scope's own display as PNG.
    Screenshot,
    ScopeMeasurements {
        channel: u8,
    },
    Scpi {
        command: String,
    },
}

impl Request {
    /// One line for activity logs: "set timebase, CH1 V/div".
    pub fn describe(&self) -> String {
        match self {
            Self::Status => "read status".into(),
            Self::Capture { channels, memory } => format!(
                "capture {} {}",
                if *memory { "memory" } else { "screen" },
                channel_list(channels)
            ),
            Self::Apply { changes } => {
                let names: Vec<String> = changes.iter().map(change_name).collect();
                format!("set {}", names.join(", "))
            }
            Self::Act { action } => format!("{action:?}").to_lowercase(),
            Self::SingleShot { channels, .. } => format!("single shot {}", channel_list(channels)),
            Self::Screenshot => "screenshot".into(),
            Self::ScopeMeasurements { channel } => format!("measure CH{channel}"),
            Self::Scpi { command } => format!("SCPI {command}"),
        }
    }

    /// Does this request change the scope (as opposed to only reading it)?
    pub fn changes_scope(&self) -> bool {
        matches!(
            self,
            Self::Apply { .. } | Self::Act { .. } | Self::SingleShot { .. } | Self::Scpi { .. }
        )
    }
}

fn channel_list(channels: &[u8]) -> String {
    let names: Vec<String> = channels.iter().map(|c| format!("CH{c}")).collect();
    names.join("+")
}

fn change_name(change: &Change) -> String {
    serde_json::to_value(change)
        .ok()
        .and_then(|v| v.get("set").and_then(|s| s.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "?".into())
        .replace('_', " ")
}

/// Identity, connection and settings in one answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub identity: Identity,
    pub running: bool,
    pub settings: Settings,
    /// "app" when served by the running Scopewerk app, "usb" for a direct connection.
    pub via: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Reply {
    Status(Status),
    Capture(Capture),
    Applied(ApplyReport),
    Settings(Settings),
    SingleShot(Option<Capture>),
    Png(#[serde(with = "base64_bytes")] Vec<u8>),
    Json(serde_json::Value),
    Text(String),
}

/// Run one request against a scope this process owns.
pub fn handle(scope: &mut Scope, request: &Request) -> Result<Reply, Error> {
    Ok(match request {
        Request::Status => Reply::Status(Status {
            identity: scope.identity()?,
            running: scope.running()?,
            settings: scope.settings()?,
            via: "usb".into(),
        }),
        Request::Capture { channels, memory } => Reply::Capture(if *memory {
            scope.capture_memory(channels)?
        } else {
            scope.capture(channels)?
        }),
        Request::Apply { changes } => Reply::Applied(scope.apply(changes)?),
        Request::Act { action } => Reply::Settings(scope.act(*action)?),
        Request::SingleShot {
            channels,
            memory,
            timeout_ms,
        } => Reply::SingleShot(scope.single_shot(
            channels,
            *memory,
            Duration::from_millis(*timeout_ms),
        )?),
        Request::Screenshot => Reply::Png(image::bmp_to_png(&scope.screenshot_bmp()?)?),
        Request::ScopeMeasurements { channel } => Reply::Json(scope.scope_measurements(*channel)?),
        Request::Scpi { command } => Reply::Text(scope.scpi(command)?),
    })
}

/// Where the app listens. `SCOPEWERK_SOCKET` overrides it.
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SCOPEWERK_SOCKET") {
        return PathBuf::from(path);
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Scopewerk/scopewerk.sock")
    } else {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map_or_else(|| home.join(".scopewerk"), PathBuf::from)
            .join("scopewerk.sock")
    }
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    client: String,
    request: Request,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Answer {
    Ok(Box<Reply>),
    Error(String),
}

/// A connection to the scope, through the app or straight over USB.
#[derive(Debug)]
pub enum Session {
    Local(Scope),
    App {
        stream: BufReader<UnixStream>,
        client: String,
    },
}

impl Session {
    /// Use the running Scopewerk app if there is one, else open the scope over USB. A scope
    /// that is briefly busy (another short-lived client) is retried for a few seconds.
    pub fn connect(client: &str) -> Result<Self, Error> {
        if let Ok(stream) = UnixStream::connect(socket_path()) {
            return Ok(Self::App {
                stream: BufReader::new(stream),
                client: client.to_owned(),
            });
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            match Scope::open(None) {
                Err(Error::Busy) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(150));
                }
                result => return result.map(Self::Local),
            }
        }
    }

    pub fn via_app(&self) -> bool {
        matches!(self, Self::App { .. })
    }

    pub fn call(&mut self, request: Request) -> Result<Reply, Error> {
        match self {
            Self::Local(scope) => handle(scope, &request),
            Self::App { stream, client } => {
                let envelope = Envelope {
                    client: client.clone(),
                    request,
                };
                let mut line = serde_json::to_string(&envelope).map_err(broker_error)?;
                line.push('\n');
                stream
                    .get_mut()
                    .write_all(line.as_bytes())
                    .map_err(broker_error)?;
                let mut answer = String::new();
                if stream.read_line(&mut answer).map_err(broker_error)? == 0 {
                    return Err(Error::Broker(
                        "the Scopewerk app closed the connection".into(),
                    ));
                }
                match serde_json::from_str::<Answer>(&answer).map_err(broker_error)? {
                    Answer::Ok(reply) => match *reply {
                        Reply::Status(mut status) => {
                            status.via = "app".into();
                            Ok(Reply::Status(status))
                        }
                        reply => Ok(reply),
                    },
                    Answer::Error(message) => Err(Error::Broker(message)),
                }
            }
        }
    }
}

fn broker_error(err: impl std::fmt::Display) -> Error {
    Error::Broker(err.to_string())
}

/// What the app plugs into [`serve`]: run a request from `client` and answer it.
pub type Handler = Arc<dyn Fn(&str, Request) -> Result<Reply, String> + Send + Sync>;

/// Listen on [`socket_path`] and answer every connection with `handler` on its own thread.
/// A stale socket file from a crashed app is replaced; a live one makes this fail.
pub fn serve(handler: Handler) -> Result<PathBuf, Error> {
    let path = socket_path();
    if UnixStream::connect(&path).is_ok() {
        return Err(Error::Broker(format!(
            "another Scopewerk app already serves {}",
            path.display()
        )));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(broker_error)?;
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).map_err(broker_error)?;
    std::thread::Builder::new()
        .name("scopewerk-broker".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let handler = Arc::clone(&handler);
                let _ = std::thread::Builder::new()
                    .name("scopewerk-client".into())
                    .spawn(move || serve_client(stream, &handler));
            }
        })
        .map_err(broker_error)?;
    Ok(path)
}

fn serve_client(stream: UnixStream, handler: &Handler) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else {
            return;
        };
        let answer = match serde_json::from_str::<Envelope>(&line) {
            Ok(envelope) => match handler(&envelope.client, envelope.request) {
                Ok(reply) => Answer::Ok(Box::new(reply)),
                Err(message) => Answer::Error(message),
            },
            Err(err) => Answer::Error(format!("bad request: {err}")),
        };
        let Ok(mut text) = serde_json::to_string(&answer) else {
            return;
        };
        text.push('\n');
        if writer.write_all(text.as_bytes()).is_err() {
            return;
        }
    }
}

mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(serde::de::Error::custom)
    }
}

/// Base64 for MCP image content.
pub fn base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_and_describe_themselves() {
        let request: Request = serde_json::from_str(
            r#"{"method":"apply","params":{"changes":[{"set":"timebase","s_per_div":0.001},{"set":"volts_per_div","channel":1,"volts":0.5}]}}"#,
        )
        .expect("parses");
        assert_eq!(request.describe(), "set timebase, volts per div");
        assert!(request.changes_scope());
        let png = Reply::Png(vec![1, 2, 3]);
        let text = serde_json::to_string(&png).expect("serializes");
        assert!(text.contains("AQID"));
        let back: Reply = serde_json::from_str(&text).expect("parses back");
        assert!(matches!(back, Reply::Png(bytes) if bytes == vec![1, 2, 3]));
    }
}
