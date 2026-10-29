//! Where the app's broker listens. macOS and Linux: a Unix socket in the user's data folder.
//! Windows (the standard library has no Unix sockets there): a loopback TCP port, announced
//! together with a random token in a file in the user's local app data; clients send the token
//! first, so other users on the machine cannot drive the scope.

use std::fmt::Debug;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use crate::Error;

/// A bidirectional connection between a client and the app.
pub trait Duplex: Read + Write + Send + Debug {}
impl<T: Read + Write + Send + Debug> Duplex for T {}

pub(crate) type Stream = Box<dyn Duplex>;

fn app_dir() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("Scopewerk")
}

#[cfg(unix)]
mod platform {
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;

    use super::{Error, Stream, io};

    /// `SCOPEWERK_SOCKET` overrides the socket path.
    pub(super) fn socket_path() -> PathBuf {
        if let Some(path) = std::env::var_os("SCOPEWERK_SOCKET") {
            return PathBuf::from(path);
        }
        // Linux prefers the per-session runtime dir; macOS has none.
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if cfg!(target_os = "linux") => PathBuf::from(dir).join("scopewerk.sock"),
            _ => super::app_dir().join("scopewerk.sock"),
        }
    }

    pub(crate) fn location() -> String {
        socket_path().display().to_string()
    }

    pub(crate) fn connect() -> io::Result<Stream> {
        Ok(Box::new(UnixStream::connect(socket_path())?))
    }

    pub(crate) struct Listener(UnixListener);

    impl Listener {
        pub(crate) fn accept(&self) -> io::Result<Stream> {
            Ok(Box::new(self.0.accept()?.0))
        }
    }

    /// A stale socket from a crashed app is replaced; a live one is an error.
    pub(crate) fn listen() -> Result<Listener, Error> {
        let path = socket_path();
        if UnixStream::connect(&path).is_ok() {
            return Err(Error::Broker(format!(
                "another Scopewerk app already serves {}",
                path.display()
            )));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| Error::Broker(err.to_string()))?;
        }
        let _ = std::fs::remove_file(&path);
        UnixListener::bind(&path)
            .map(Listener)
            .map_err(|err| Error::Broker(format!("{}: {err}", path.display())))
    }
}

#[cfg(windows)]
mod platform {
    use std::hash::{BuildHasher, Hasher};
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{Error, Stream, io};

    fn announce_path() -> PathBuf {
        super::app_dir().join("broker.txt")
    }

    /// The running app's port and token, from the announce file.
    fn announced() -> io::Result<(u16, String)> {
        let text = std::fs::read_to_string(announce_path())?;
        let (port, token) = text
            .trim()
            .split_once(' ')
            .ok_or_else(|| io::Error::other("bad announce file"))?;
        let port = port.parse().map_err(|_| io::Error::other("bad port"))?;
        Ok((port, token.to_owned()))
    }

    pub(crate) fn location() -> String {
        announce_path().display().to_string()
    }

    pub(crate) fn connect() -> io::Result<Stream> {
        let (port, token) = announced()?;
        let mut stream = TcpStream::connect_timeout(
            &(Ipv4Addr::LOCALHOST, port).into(),
            Duration::from_millis(500),
        )?;
        stream.write_all(format!("{token}\n").as_bytes())?;
        Ok(Box::new(stream))
    }

    pub(crate) struct Listener {
        listener: TcpListener,
        token: String,
    }

    impl Listener {
        /// Accept the next client that presents the token; others are dropped.
        pub(crate) fn accept(&self) -> io::Result<Stream> {
            loop {
                let (stream, _) = self.listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                // Read the token byte by byte so nothing after it is buffered away.
                let mut line = Vec::new();
                let mut byte = [0u8; 1];
                let mut reader = &stream;
                while line.len() < 128 && reader.read(&mut byte)? == 1 && byte[0] != b'\n' {
                    line.push(byte[0]);
                }
                if line == self.token.as_bytes() {
                    stream.set_read_timeout(None)?;
                    return Ok(Box::new(stream));
                }
            }
        }
    }

    fn random_token() -> String {
        // RandomState is seeded from the OS; two of them give 128 random bits.
        let mut token = String::new();
        for _ in 0..2 {
            let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
            hasher.write_u64(u64::from(std::process::id()));
            token.push_str(&format!("{:016x}", hasher.finish()));
        }
        token
    }

    pub(crate) fn listen() -> Result<Listener, Error> {
        // A stale announce file points at a closed port; a live app accepts the connection.
        if connect().is_ok() {
            return Err(Error::Broker(
                "another Scopewerk app is already running".into(),
            ));
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|err| Error::Broker(err.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|err| Error::Broker(err.to_string()))?
            .port();
        let token = random_token();
        let path = announce_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| Error::Broker(err.to_string()))?;
        }
        std::fs::write(&path, format!("{port} {token}\n"))
            .map_err(|err| Error::Broker(format!("{}: {err}", path.display())))?;
        Ok(Listener { listener, token })
    }
}

pub(crate) use platform::{connect, listen, location};
