//! The scope thread: owns the USB connection, reconnects when the scope comes and goes, streams
//! frames to the window, and runs jobs from the window and from broker clients (CLI, MCP) one at
//! a time between frames.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use scopewerk_core::broker::{self, Handler};
use scopewerk_core::{Capture, Change, Error, Identity, Reply, Request, Scope, Settings};

/// How often to look for a scope while none is connected.
const RECONNECT_EVERY: Duration = Duration::from_secs(1);
/// Frames while the scope is stopped only need to notice knob changes on the device.
const STOPPED_FRAME_EVERY: Duration = Duration::from_millis(900);

/// What the window hears from the scope thread.
#[derive(Debug)]
pub(crate) enum Event {
    Connected(Identity),
    Disconnected(String),
    Frame {
        capture: Box<Capture>,
        job_seq: u64,
    },
    Activity(Activity),
    /// Something the person should see briefly (saved file, snapped value, error).
    Notice {
        text: String,
        error: bool,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct Activity {
    pub(crate) at: std::time::SystemTime,
    pub(crate) client: String,
    pub(crate) text: String,
    pub(crate) ok: bool,
}

/// Work for the scope thread.
pub(crate) enum Job {
    /// From the window: quick, unverified changes against the window's view of the settings.
    Changes {
        seq: u64,
        settings: Box<Settings>,
        changes: Vec<Change>,
    },
    /// A full request; `reply` is set for broker clients waiting on the answer.
    Request {
        seq: u64,
        client: String,
        request: Request,
        reply: Option<Sender<Result<Reply, String>>>,
    },
    Live(LiveMode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LiveMode {
    pub(crate) paused: bool,
    /// Read the 10 000-sample memory instead of the screen.
    pub(crate) memory: bool,
}

#[derive(Clone)]
pub(crate) struct ServiceHandle {
    jobs: Sender<Job>,
    seq: Arc<Mutex<u64>>,
}

impl ServiceHandle {
    pub(crate) fn next_seq(&self) -> u64 {
        let mut seq = self
            .seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *seq += 1;
        *seq
    }

    pub(crate) fn send(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    pub(crate) fn changes(&self, settings: &Settings, changes: Vec<Change>) -> u64 {
        let seq = self.next_seq();
        self.send(Job::Changes {
            seq,
            settings: Box::new(settings.clone()),
            changes,
        });
        seq
    }

    pub(crate) fn request(&self, request: Request) -> u64 {
        let seq = self.next_seq();
        self.send(Job::Request {
            seq,
            client: "app".into(),
            request,
            reply: None,
        });
        seq
    }
}

/// Start the scope thread and the broker socket. Events arrive on the returned channel.
pub(crate) fn start() -> (
    ServiceHandle,
    async_channel::Receiver<Event>,
    Result<String, String>,
) {
    let (jobs_tx, jobs_rx) = mpsc::channel();
    let (events_tx, events_rx) = async_channel::unbounded();
    let handle = ServiceHandle {
        jobs: jobs_tx,
        seq: Arc::new(Mutex::new(0)),
    };

    let broker_handle = handle.clone();
    let handler: Handler = Arc::new(move |client: &str, request: Request| {
        let (reply_tx, reply_rx) = mpsc::channel();
        let seq = broker_handle.next_seq();
        broker_handle.send(Job::Request {
            seq,
            client: client.to_owned(),
            request,
            reply: Some(reply_tx),
        });
        reply_rx
            .recv()
            .unwrap_or_else(|_| Err("the Scopewerk app is shutting down".into()))
    });
    let broker = broker::serve(handler).map_err(|err| err.to_string());

    thread::Builder::new()
        .name("scopewerk-scope".into())
        .spawn(move || {
            if let Some(kind) = scopewerk_core::decode::demo::Kind::from_env() {
                run_demo(kind, &jobs_rx, &events_tx);
            } else {
                Worker::new(jobs_rx, events_tx).run();
            }
        })
        .expect("spawning the scope thread");
    (handle, events_rx, broker)
}

struct Worker {
    jobs: Receiver<Job>,
    events: async_channel::Sender<Event>,
    scope: Option<Scope>,
    live: LiveMode,
    /// Last job whose effect the next frame includes.
    done_seq: u64,
    last_frame: Option<Instant>,
    stopped: bool,
}

impl Worker {
    fn new(jobs: Receiver<Job>, events: async_channel::Sender<Event>) -> Self {
        Self {
            jobs,
            events,
            scope: None,
            live: LiveMode {
                paused: false,
                memory: false,
            },
            done_seq: 0,
            last_frame: None,
            stopped: false,
        }
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send_blocking(event);
    }

    fn run(mut self) {
        let mut last_attempt: Option<Instant> = None;
        let mut last_error = String::new();
        loop {
            if self.scope.is_none() && last_attempt.is_none_or(|t| t.elapsed() >= RECONNECT_EVERY) {
                last_attempt = Some(Instant::now());
                match Scope::open(None).and_then(|mut scope| scope.identity().map(|id| (scope, id)))
                {
                    Ok((scope, identity)) => {
                        self.scope = Some(scope);
                        last_error.clear();
                        self.emit(Event::Connected(identity));
                    }
                    Err(err) => {
                        let text = err.to_string();
                        if text != last_error {
                            self.emit(Event::Disconnected(text.clone()));
                            last_error = text;
                        }
                    }
                }
            }

            // Jobs first, so clicks and agents never wait behind more than one frame.
            let wait = if self.scope.is_some() && self.frame_due() {
                Duration::ZERO
            } else {
                Duration::from_millis(50)
            };
            match self.jobs.recv_timeout(wait) {
                Ok(job) => {
                    self.run_job(job);
                    while let Ok(job) = self.jobs.try_recv() {
                        self.run_job(job);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }

            if self.scope.is_some() && self.frame_due() {
                self.frame();
            }
        }
    }

    fn frame_due(&self) -> bool {
        if self.live.paused {
            return false;
        }
        let interval = if self.stopped {
            STOPPED_FRAME_EVERY
        } else {
            Duration::ZERO
        };
        self.last_frame.is_none_or(|t| t.elapsed() >= interval)
    }

    fn frame(&mut self) {
        self.last_frame = Some(Instant::now());
        let memory = self.live.memory;
        let result = self.with_scope(|scope| {
            let header = scope.header()?;
            let channels: Vec<u8> = header
                .settings()
                .channels
                .iter()
                .filter(|c| c.enabled)
                .map(|c| c.channel)
                .collect();
            if memory {
                scope.capture_memory(&channels)
            } else {
                scope.capture_with(&header, &channels)
            }
        });
        if let Some(capture) = result {
            self.stopped = capture.settings.is_stopped();
            self.emit(Event::Frame {
                capture: Box::new(capture),
                job_seq: self.done_seq,
            });
        }
    }

    fn run_job(&mut self, job: Job) {
        match job {
            Job::Live(mode) => {
                self.live = mode;
                self.last_frame = None;
            }
            Job::Changes {
                seq,
                settings,
                changes,
            } => {
                let result = self.with_scope(|scope| scope.send_changes(&settings, &changes));
                if let Some(notes) = result {
                    for note in notes {
                        self.emit(Event::Notice {
                            text: note,
                            error: false,
                        });
                    }
                }
                self.done_seq = seq;
                self.last_frame = None;
            }
            Job::Request {
                seq,
                client,
                request,
                reply,
            } => {
                let description = request.describe();
                let changes_scope = request.changes_scope();
                let result = match self.scope.as_mut() {
                    Some(scope) => broker::handle(scope, &request),
                    None => Err(Error::NotFound),
                };
                if let Err(err) = &result
                    && is_fatal(err)
                {
                    self.drop_scope(err);
                }
                let ok = result.is_ok();
                let detail = match &result {
                    Ok(Reply::Applied(report)) if !report.all_applied() => {
                        " (some values not applied)".to_owned()
                    }
                    Ok(_) => String::new(),
                    Err(err) => format!(": {err}"),
                };
                // The window's own requests are visible already; log agents and the CLI.
                if client != "app" || !ok {
                    self.emit(Event::Activity(Activity {
                        at: std::time::SystemTime::now(),
                        client: client.clone(),
                        text: format!("{description}{detail}"),
                        ok,
                    }));
                }
                if let Some(reply) = reply {
                    let _ = reply.send(result.map_err(|err| err.to_string()));
                } else if let Ok(Reply::Png(bytes)) = result {
                    save_screenshot(&bytes, &self.events);
                }
                self.done_seq = seq;
                if changes_scope {
                    self.last_frame = None;
                }
            }
        }
    }

    /// Run `work` on the connected scope; drop the connection on USB-level failures.
    fn with_scope<T>(&mut self, work: impl FnOnce(&mut Scope) -> Result<T, Error>) -> Option<T> {
        let scope = self.scope.as_mut()?;
        match work(scope) {
            Ok(value) => Some(value),
            Err(err) => {
                if is_fatal(&err) {
                    self.drop_scope(&err);
                } else {
                    self.emit(Event::Notice {
                        text: err.to_string(),
                        error: true,
                    });
                }
                None
            }
        }
    }

    fn drop_scope(&mut self, err: &Error) {
        self.scope = None;
        self.emit(Event::Disconnected(err.to_string()));
    }
}

fn is_fatal(err: &Error) -> bool {
    matches!(
        err,
        Error::Disconnected
            | Error::Timeout
            | Error::Usb(_)
            | Error::Transfer(_)
            | Error::OutOfSync
    )
}

/// Screenshots requested from the window land in ~/Pictures/Scopewerk.
fn save_screenshot(bytes: &[u8], events: &async_channel::Sender<Event>) {
    let dir = crate::export_dir(crate::Folder::Pictures);
    let path = dir.join(format!("scope-{}.png", crate::timestamp()));
    let result = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, bytes));
    let event = match result {
        Ok(()) => Event::Notice {
            text: format!("Screenshot saved to {}", path.display()),
            error: false,
        },
        Err(err) => Event::Notice {
            text: format!("Could not save the screenshot: {err}"),
            error: true,
        },
    };
    let _ = events.send_blocking(event);
}

/// `SCOPEWERK_DEMO=can|i2c|uart`: no USB, a simulated bus instead. Controls and agents get a
/// friendly refusal.
fn run_demo(
    kind: scopewerk_core::decode::demo::Kind,
    jobs: &Receiver<Job>,
    events: &async_channel::Sender<Event>,
) {
    let send = |event| {
        let _ = events.send_blocking(event);
    };
    send(Event::Connected(Identity {
        manufacturer: "DEMO".into(),
        model: "DOS1102".into(),
        serial: format!("simulated {} bus", kind.label()),
        firmware: "—".into(),
    }));
    let mut frame = 0u32;
    loop {
        match jobs.recv_timeout(Duration::from_millis(650)) {
            Ok(Job::Request {
                reply: Some(reply), ..
            }) => {
                let _ = reply.send(Err(
                    "Scopewerk runs in demo mode (SCOPEWERK_DEMO); there is no scope".into(),
                ));
            }
            Ok(Job::Changes { .. } | Job::Request { .. }) => send(Event::Notice {
                text: "Demo mode: the scope is simulated, settings do not change".into(),
                error: false,
            }),
            Ok(Job::Live(_)) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        frame += 1;
        let capture = scopewerk_core::decode::demo::capture_of(kind, frame);
        send(Event::Frame {
            capture: Box::new(capture),
            job_seq: u64::MAX,
        });
    }
}
