//! The main window: toolbar, live screen with hover readout and A/B cursors, the control panel
//! (horizontal, channels, trigger, acquisition), the measurement strip and the agent activity log.

use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AsyncApp, ClickEvent, ClipboardItem, Context, Div, FocusHandle, FontWeight,
    Hsla, InteractiveElement as _, IntoElement, KeyBinding, MouseButton, MouseDownEvent,
    MouseMoveEvent, ParentElement as _, Render, ScrollWheelEvent, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled, WeakEntity, Window, div, px,
};
use scopewerk_core::units::{
    format_si, nearest_step, step_from, timebase_steps, volts_per_div_steps,
};
use scopewerk_core::{
    Acquisition, Action, Capture, Change, Coupling, Edge, Identity, Request, Settings, Sweep,
};

use scopewerk_core::decode::{
    self, DecodeConfig, Decoded, Input, STANDARD_BAUDS, STANDARD_BITRATES, demo,
    recommended_timebase,
};

use crate::plot::{self, Geometry, Scene, time_of, value_at, x_div_of};
use crate::service::{Activity, Event, Job, LiveMode, ServiceHandle};
use crate::theme;

gpui_kit::actions!(
    scopewerk,
    [
        ToggleRun,
        SingleShot,
        Autoset,
        TimebaseFaster,
        TimebaseSlower,
        VoltsFiner,
        VoltsCoarser,
        OffsetUp,
        OffsetDown,
        SelectCh1,
        SelectCh2,
        ToggleCh1,
        ToggleCh2,
        ToggleMemory,
        ToggleAfterglow,
        ExportCsv,
        SaveScreenshot,
        CopyMeasurements,
        ClearCursors,
        TriggerToMiddle,
        TogglePause
    ]
);

const CONTEXT: &str = "ScopeView";
// Modifier names in hints, as each platform prints them.
const MOD: &str = if cfg!(target_os = "macos") {
    "⌘"
} else {
    "Ctrl+"
};
const SHIFT: &str = if cfg!(target_os = "macos") {
    "⇧"
} else {
    "Shift+"
};
const ALT: &str = if cfg!(target_os = "macos") {
    "⌥"
} else {
    "Alt"
};
const AFTERGLOW_FRAMES: usize = 6;
const NOTICE_FOR: Duration = Duration::from_secs(5);
const ACTIVITY_KEEP: usize = 40;

pub(crate) fn bind_keys(cx: &mut App) {
    let context = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("space", ToggleRun, context),
        KeyBinding::new("s", SingleShot, context),
        KeyBinding::new("secondary-shift-a", Autoset, context),
        KeyBinding::new("left", TimebaseFaster, context),
        KeyBinding::new("right", TimebaseSlower, context),
        KeyBinding::new("up", VoltsFiner, context),
        KeyBinding::new("down", VoltsCoarser, context),
        KeyBinding::new("shift-up", OffsetUp, context),
        KeyBinding::new("shift-down", OffsetDown, context),
        KeyBinding::new("1", SelectCh1, context),
        KeyBinding::new("2", SelectCh2, context),
        KeyBinding::new("secondary-1", ToggleCh1, context),
        KeyBinding::new("secondary-2", ToggleCh2, context),
        KeyBinding::new("m", ToggleMemory, context),
        KeyBinding::new("g", ToggleAfterglow, context),
        KeyBinding::new("p", TogglePause, context),
        KeyBinding::new("t", TriggerToMiddle, context),
        KeyBinding::new("escape", ClearCursors, context),
        KeyBinding::new("secondary-e", ExportCsv, context),
        KeyBinding::new("secondary-shift-s", SaveScreenshot, context),
        KeyBinding::new("secondary-shift-c", CopyMeasurements, context),
    ]);
}

enum Connection {
    Searching(String),
    Connected(Identity),
}

pub(crate) struct ScopeView {
    service: ServiceHandle,
    focus: FocusHandle,
    connection: Connection,
    broker: Result<String, String>,
    frames: VecDeque<Arc<Capture>>,
    /// What the controls show; runs ahead of the frames right after a click.
    settings: Option<Settings>,
    pending_seq: u64,
    frame_times: VecDeque<Instant>,
    selected: u8,
    live: LiveMode,
    afterglow: bool,
    geometry: Rc<Cell<Option<Geometry>>>,
    hover_x_div: Option<f64>,
    /// Cursor times in seconds from the trigger, so they stay on the signal when zooming.
    cursors: [Option<f64>; 2],
    activity: VecDeque<Activity>,
    notice: Option<(String, bool, Instant)>,
    scroll: f32,
    /// The running protocol decoder, if any.
    decoder: Option<DecodeConfig>,
    /// Its result on the newest frame.
    decoded: Option<Arc<Decoded>>,
    /// Whether memory mode was on before decoding switched it on.
    memory_before_decode: Option<bool>,
}

impl ScopeView {
    pub(crate) fn new(
        service: ServiceHandle,
        events: async_channel::Receiver<Event>,
        broker: Result<String, String>,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            while let Ok(event) = events.recv().await {
                if this
                    .update(cx, |view, cx| view.on_event(event, cx))
                    .is_err()
                {
                    return;
                }
            }
        })
        .detach();
        // Expire the notice toast without waiting for the next frame.
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let alive = this.update(cx, |view, cx| {
                    if view
                        .notice
                        .as_ref()
                        .is_some_and(|(_, _, at)| at.elapsed() > NOTICE_FOR)
                    {
                        view.notice = None;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        })
        .detach();
        Self {
            service,
            focus: cx.focus_handle(),
            connection: Connection::Searching("Looking for the DOS1102…".into()),
            broker,
            frames: VecDeque::new(),
            settings: None,
            pending_seq: 0,
            frame_times: VecDeque::new(),
            selected: 1,
            live: LiveMode {
                paused: false,
                memory: std::env::var_os("SCOPEWERK_DEMO").is_some_and(|v| !v.is_empty()),
            },
            afterglow: true,
            geometry: Rc::new(Cell::new(None)),
            hover_x_div: None,
            cursors: [None, None],
            activity: VecDeque::new(),
            notice: None,
            scroll: 0.0,
            // The demo buses show off their decoder right away.
            decoder: demo::Kind::from_env().map(|kind| match kind {
                demo::Kind::Can => DecodeConfig::Can {
                    input: Input::Ch1MinusCh2,
                    bitrate: None,
                    dominant_high: None,
                },
                demo::Kind::I2c => DecodeConfig::I2c { scl: 1, sda: 2 },
                demo::Kind::Uart => DecodeConfig::Uart {
                    input: Input::Ch1,
                    baud: None,
                    data_bits: 8,
                    parity: decode::Parity::None,
                    inverted: false,
                },
            }),
            decoded: None,
            memory_before_decode: None,
        }
    }

    pub(crate) fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn on_event(&mut self, event: Event, cx: &mut Context<'_, Self>) {
        match event {
            Event::Connected(identity) => {
                self.connection = Connection::Connected(identity);
            }
            Event::Disconnected(reason) => {
                self.connection = Connection::Searching(reason);
                self.frames.clear();
                self.settings = None;
                self.frame_times.clear();
            }
            Event::Frame { capture, job_seq } => {
                let capture = Arc::new(*capture);
                // A new scale makes old frames meaningless as afterglow.
                if self
                    .frames
                    .back()
                    .is_some_and(|last| !same_scale(&last.settings, &capture.settings))
                {
                    self.frames.clear();
                }
                if job_seq >= self.pending_seq {
                    self.settings = Some(capture.settings.clone());
                }
                if self.selected_disabled()
                    && let Some(first) = capture.settings.channels.iter().find(|c| c.enabled)
                {
                    self.selected = first.channel;
                }
                self.decoded = self
                    .decoder
                    .as_ref()
                    .and_then(|config| decode::decode(&capture, config).ok())
                    .map(Arc::new);
                self.frames.push_back(capture);
                while self.frames.len() > AFTERGLOW_FRAMES {
                    self.frames.pop_front();
                }
                self.frame_times.push_back(Instant::now());
                while self.frame_times.len() > 8 {
                    self.frame_times.pop_front();
                }
            }
            Event::Activity(activity) => {
                if activity.client != "app" {
                    self.notice = Some((
                        format!("{} · {}", client_label(&activity.client), activity.text),
                        !activity.ok,
                        Instant::now(),
                    ));
                }
                self.activity.push_front(activity);
                self.activity.truncate(ACTIVITY_KEEP);
            }
            Event::Notice { text, error } => {
                self.notice = Some((text, error, Instant::now()));
            }
        }
        cx.notify();
    }

    fn selected_disabled(&self) -> bool {
        self.settings
            .as_ref()
            .and_then(|s| s.channel(self.selected))
            .is_some_and(|c| !c.enabled)
    }

    fn fps(&self) -> Option<f64> {
        let (first, last) = (self.frame_times.front()?, self.frame_times.back()?);
        let span = last.duration_since(*first).as_secs_f64();
        let frames = u32::try_from(self.frame_times.len() - 1).ok()?;
        (frames >= 2 && span > 0.0).then(|| f64::from(frames) / span)
    }

    // ---- changing the scope -------------------------------------------------------------

    fn change(&mut self, changes: Vec<Change>, cx: &mut Context<'_, Self>) {
        let Some(settings) = self.settings.as_mut() else {
            return;
        };
        let before = settings.clone();
        for change in &changes {
            optimistic(settings, change);
        }
        self.pending_seq = self.service.changes(&before, changes);
        cx.notify();
    }

    fn act(&mut self, action: Action, cx: &mut Context<'_, Self>) {
        if let Some(settings) = self.settings.as_mut() {
            match action {
                Action::Run => settings.run_status = "AUTO".into(),
                Action::Stop => settings.run_status = "STOP".into(),
                Action::Single => {
                    settings.trigger.sweep = "SINGle".into();
                    settings.run_status = "READY".into();
                }
                Action::Autoset => {}
            }
        }
        self.pending_seq = self.service.request(Request::Act { action });
        if action == Action::Autoset {
            self.notice = Some((
                "Autoset: the scope picks timebase, V/div and trigger…".into(),
                false,
                Instant::now(),
            ));
        }
        cx.notify();
    }

    fn step_timebase(&mut self, direction: i32, cx: &mut Context<'_, Self>) {
        let Some(current) = self.settings.as_ref().map(|s| s.timebase_s_per_div) else {
            return;
        };
        let next = step_from(&timebase_steps(), current, direction);
        if (next - current).abs() > current * 1e-6 {
            self.change(vec![Change::Timebase { s_per_div: next }], cx);
        }
    }

    fn step_volts(&mut self, channel: u8, direction: i32, cx: &mut Context<'_, Self>) {
        let Some(c) = self.settings.as_ref().and_then(|s| s.channel(channel)) else {
            return;
        };
        let probe = probe_factor(&c.probe);
        let next = step_from(&volts_per_div_steps(), c.volts_per_div / probe, direction) * probe;
        if (next - c.volts_per_div).abs() > next * 1e-6 {
            self.change(
                vec![Change::VoltsPerDiv {
                    channel,
                    volts: next,
                }],
                cx,
            );
        }
    }

    fn nudge_offset(&mut self, channel: u8, delta: f64, cx: &mut Context<'_, Self>) {
        let Some(c) = self.settings.as_ref().and_then(|s| s.channel(channel)) else {
            return;
        };
        let div = ((c.offset_div + delta) * 50.0).round().clamp(-200.0, 200.0) / 50.0;
        self.change(vec![Change::ChannelOffset { channel, div }], cx);
    }

    fn nudge_position(&mut self, delta: f64, cx: &mut Context<'_, Self>) {
        let Some(settings) = self.settings.as_ref() else {
            return;
        };
        let div = ((settings.horizontal_offset_div + delta) * 50.0).round() / 50.0;
        self.change(vec![Change::HorizontalOffset { div }], cx);
    }

    fn nudge_trigger(&mut self, divs: f64, cx: &mut Context<'_, Self>) {
        let Some(settings) = self.settings.as_ref() else {
            return;
        };
        let Some(channel) = settings.channel(trigger_channel(settings)) else {
            return;
        };
        let level = settings.trigger.level_v.unwrap_or(0.0);
        let step = channel.volts_per_div * 0.2;
        let volts = ((level + divs * step) / (step / 4.0)).round() * (step / 4.0);
        self.change(vec![Change::TriggerLevel { volts }], cx);
    }

    /// Trigger level to the middle of the source signal, as the scope's 50 % key.
    fn trigger_to_middle(&mut self, cx: &mut Context<'_, Self>) {
        let (Some(settings), Some(frame)) = (self.settings.as_ref(), self.frames.back()) else {
            return;
        };
        let source = trigger_channel(settings);
        let Some(stats) = frame.stats(source) else {
            self.notice = Some((
                format!("CH{source} is off; switch it on to trigger on it"),
                true,
                Instant::now(),
            ));
            cx.notify();
            return;
        };
        let volts = f64::midpoint(stats.base_v, stats.top_v);
        self.change(vec![Change::TriggerLevel { volts }], cx);
    }

    fn set_live(&mut self, live: LiveMode, cx: &mut Context<'_, Self>) {
        self.live = live;
        self.frames.clear();
        self.service.send(Job::Live(live));
        cx.notify();
    }

    fn toggle_channel(&mut self, n: u8, cx: &mut Context<'_, Self>) {
        let Some(enabled) = self
            .settings
            .as_ref()
            .and_then(|s| s.channel(n))
            .map(|c| c.enabled)
        else {
            return;
        };
        self.change(
            vec![Change::ChannelEnabled {
                channel: n,
                enabled: !enabled,
            }],
            cx,
        );
    }

    fn screenshot(&mut self, cx: &mut Context<'_, Self>) {
        self.service.request(Request::Screenshot);
        self.notice = Some(("Reading the scope's display…".into(), false, Instant::now()));
        cx.notify();
    }

    // ---- files and clipboard ------------------------------------------------------------

    fn export_csv(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(frame) = self.frames.back().cloned() else {
            return;
        };
        let directory = crate::export_dir(crate::Folder::Documents);
        let _ = std::fs::create_dir_all(&directory);
        let name = format!("scope-{}.csv", crate::timestamp());
        let target = cx.prompt_for_new_path(&directory, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = target.await else {
                return;
            };
            let (text, error) = match std::fs::write(&path, frame.to_csv()) {
                Ok(()) => (
                    format!("Saved {} points to {}", frame.points(), path.display()),
                    false,
                ),
                Err(err) => (format!("Could not save {}: {err}", path.display()), true),
            };
            let _ = this.update(cx, |view, cx| {
                view.notice = Some((text, error, Instant::now()));
                cx.notify();
            });
        })
        .detach();
    }

    fn copy_measurements(&mut self, cx: &mut Context<'_, Self>) {
        let Some(frame) = self.frames.back() else {
            return;
        };
        let mut lines = vec![format!(
            "{}/div, {}",
            format_si(frame.settings.timebase_s_per_div, "s"),
            frame
                .settings
                .sample_rate_sa_s
                .map_or_else(|| "?".into(), |r| format_si(r, "Sa/s"))
        )];
        for waveform in &frame.waveforms {
            let Some(s) = frame.stats(waveform.channel) else {
                continue;
            };
            lines.push(format!(
                "CH{}: freq {}  period {}  pk-pk {}  mean {}  rms {}  min {}  max {}  duty {}",
                waveform.channel,
                opt_si(s.frequency_hz, "Hz"),
                opt_si(s.period_s, "s"),
                format_si(s.pk_pk_v, "V"),
                format_si(s.mean_v, "V"),
                format_si(s.rms_v, "V"),
                format_si(s.min_v, "V"),
                format_si(s.max_v, "V"),
                s.duty_cycle_pct
                    .map_or_else(|| "—".into(), |d| format!("{d:.1} %")),
            ));
        }
        cx.write_to_clipboard(ClipboardItem::new_string(lines.join("\n")));
        self.notice = Some(("Measurements copied".into(), false, Instant::now()));
        cx.notify();
    }

    // ---- mouse on the screen ------------------------------------------------------------

    fn on_plot_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<'_, Self>) {
        let position = event.position;
        let hover = self
            .geometry
            .get()
            .filter(|g| {
                g.contains_x(position.x)
                    && position.y >= g.screen.top()
                    && position.y <= g.screen.bottom()
            })
            .map(|g| g.x_div(position.x));
        if hover != self.hover_x_div {
            self.hover_x_div = hover;
            cx.notify();
        }
    }

    fn on_plot_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.focus.focus(window, cx);
        let (Some(x_div), Some(settings)) = (self.hover_x_div, self.settings.as_ref()) else {
            return;
        };
        let slot = usize::from(event.modifiers.alt || event.modifiers.shift);
        self.cursors[slot] = Some(time_of(x_div, settings));
        cx.notify();
    }

    fn on_plot_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        const STEP: f32 = 36.0;
        let delta = event.delta.pixel_delta(px(24.0));
        self.scroll += f32::from(delta.y);
        while self.scroll.abs() >= STEP {
            let up = self.scroll > 0.0;
            self.scroll -= if up { STEP } else { -STEP };
            let direction = if up { 1 } else { -1 };
            if event.modifiers.shift {
                self.nudge_offset(self.selected, f64::from(direction) * 0.2, cx);
            } else if event.modifiers.alt {
                self.step_volts(self.selected, -direction, cx);
            } else {
                self.step_timebase(-direction, cx);
            }
        }
    }
}

fn same_scale(a: &Settings, b: &Settings) -> bool {
    let close = |x: f64, y: f64| (x - y).abs() <= x.abs().max(y.abs()) * 1e-9;
    close(a.timebase_s_per_div, b.timebase_s_per_div)
        && close(a.horizontal_offset_div, b.horizontal_offset_div)
        && a.channels.iter().zip(&b.channels).all(|(x, y)| {
            close(x.volts_per_div, y.volts_per_div)
                && close(x.offset_div, y.offset_div)
                && x.enabled == y.enabled
        })
}

/// Mirror a change locally so the controls react before the next frame arrives.
fn optimistic(settings: &mut Settings, change: &Change) {
    let mut channel = |n: u8, f: &mut dyn FnMut(&mut scopewerk_core::ChannelSettings)| {
        if let Some(c) = settings.channels.iter_mut().find(|c| c.channel == n) {
            f(c);
        }
    };
    match *change {
        Change::Timebase { s_per_div } => {
            settings.timebase_s_per_div = nearest_step(&timebase_steps(), s_per_div);
        }
        Change::HorizontalOffset { div } => settings.horizontal_offset_div = div,
        Change::ChannelEnabled {
            channel: n,
            enabled,
        } => channel(n, &mut |c| c.enabled = enabled),
        Change::VoltsPerDiv { channel: n, volts } => channel(n, &mut |c| c.volts_per_div = volts),
        Change::ChannelOffset { channel: n, div } => channel(n, &mut |c| c.offset_div = div),
        Change::Coupling {
            channel: n,
            coupling,
        } => channel(n, &mut |c| c.coupling = coupling.scpi().into()),
        Change::Probe { channel: n, factor } => channel(n, &mut |c| {
            c.volts_per_div *= f64::from(factor) / probe_factor(&c.probe);
            c.probe = format!("{factor}X");
        }),
        Change::Invert {
            channel: n,
            inverted,
        } => channel(n, &mut |c| c.inverted = inverted),
        Change::TriggerSource { channel: n } => settings.trigger.source = format!("CH{n}"),
        Change::TriggerLevel { volts } => settings.trigger.level_v = Some(volts),
        Change::TriggerEdge { edge } => settings.trigger.edge = edge.scpi().into(),
        Change::TriggerSweep { sweep } => settings.trigger.sweep = sweep.scpi().into(),
        Change::TriggerCoupling { coupling } => settings.trigger.coupling = coupling.scpi().into(),
        Change::Acquisition { mode } => settings.acquisition = mode.scpi().into(),
    }
}

fn probe_factor(probe: &str) -> f64 {
    probe.trim_end_matches(['X', 'x']).parse().unwrap_or(1.0)
}

fn trigger_channel(settings: &Settings) -> u8 {
    if settings.trigger.source.ends_with('2') {
        2
    } else {
        1
    }
}

fn opt_si(value: Option<f64>, unit: &str) -> String {
    value.map_or_else(|| "—".to_owned(), |v| format_si(v, unit))
}

fn signed_si(value: f64, unit: &str) -> String {
    let text = format_si(value, unit);
    if value > 0.0 {
        format!("+{text}")
    } else {
        text
    }
}

fn client_label(client: &str) -> &'static str {
    match client {
        "mcp" => "Agent",
        "cli" => "CLI",
        "app" => "App",
        _ => "Client",
    }
}

/// The firmware's SCPI casing ("READy", "SINGlE", "SAMPle") as plain words.
pub(crate) fn pretty(token: &str) -> String {
    let lower = token.trim().to_lowercase();
    let mut chars = lower.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

fn same_token(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

// ---- building blocks ----------------------------------------------------------------------

fn caption(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(10.5))
        .text_color(theme::muted())
        .child(text.into())
}

fn mono(text: impl Into<SharedString>) -> Div {
    div()
        .font_family(theme::MONO)
        .text_size(px(12.5))
        .text_color(theme::text())
        .child(text.into())
}

fn button(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id.into())
        .px(px(9.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .bg(theme::panel_raised())
        .border_1()
        .border_color(theme::border())
        .text_size(px(12.0))
        .text_color(theme::text())
        .cursor_pointer()
        .hover(|s| s.bg(theme::border()))
        .active(|s| s.opacity(0.7))
        .child(label.into())
}

fn segment(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    on: bool,
    color: Hsla,
) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex_1()
        .h(px(22.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.0))
        .text_size(px(11.5))
        .font_weight(if on {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        })
        .text_color(if on { theme::app_bg() } else { theme::muted() })
        .when(on, |s| s.bg(color))
        .when(!on, |s| {
            s.hover(|s| s.bg(theme::border()).text_color(theme::text()))
        })
        .cursor_pointer()
        .child(label.into())
}

fn segments(children: Vec<Stateful<Div>>) -> Div {
    div()
        .flex()
        .gap(px(2.0))
        .p(px(2.0))
        .rounded(px(7.0))
        .bg(theme::app_bg())
        .border_1()
        .border_color(theme::border())
        .children(children)
}

fn card(
    title: impl Into<SharedString>,
    accent: Hsla,
    header_right: Option<AnyElement>,
    body: Vec<AnyElement>,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(10.0))
        .rounded(px(9.0))
        .bg(theme::panel())
        .border_1()
        .border_color(theme::border())
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .child(div().w(px(3.0)).h(px(12.0)).rounded(px(2.0)).bg(accent))
                        .child(
                            div()
                                .text_size(px(11.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::text())
                                .child(title.into()),
                        ),
                )
                .children(header_right),
        )
        .children(body)
}

type Step = Box<dyn Fn(&mut ScopeView, &mut Context<'_, ScopeView>)>;

impl ScopeView {
    fn stepper(
        &self,
        id: &str,
        label: &str,
        value: String,
        minus: Step,
        plus: Step,
        cx: &mut Context<'_, Self>,
    ) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(caption(label.to_owned()).w(px(54.0)))
            .child(mono(value).flex_1().text_right().pr(px(4.0)))
            .child(
                button(format!("{id}-minus"), "−")
                    .w(px(26.0))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| minus(this, cx))),
            )
            .child(
                button(format!("{id}-plus"), "+")
                    .w(px(26.0))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| plus(this, cx))),
            )
            .into_any_element()
    }

    fn toolbar(&self, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let (dot, device) = match &self.connection {
            Connection::Connected(id) => (
                theme::run(),
                format!(
                    "{} {} · {} · SN {}",
                    id.manufacturer, id.model, id.firmware, id.serial
                ),
            ),
            Connection::Searching(_) => (theme::trigger(), "No scope connected".to_owned()),
        };
        let stopped = self.settings.as_ref().is_some_and(Settings::is_stopped);
        let status = self
            .settings
            .as_ref()
            .map(|s| pretty(&s.run_status))
            .unwrap_or_default();
        let fps = self
            .fps()
            .map(|f| format!("{f:.1} fps"))
            .unwrap_or_default();
        let source = if self.live.memory {
            "Memory 10k"
        } else {
            "Screen"
        };
        let state = if stopped { theme::stop() } else { theme::run() };
        div()
            .h(px(44.0))
            // macOS: the toolbar runs under the traffic lights.
            .pl(px(if cfg!(target_os = "macos") {
                84.0
            } else {
                14.0
            }))
            .pr(px(12.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .border_b_1()
            .border_color(theme::border())
            .bg(theme::panel())
            .child(
                div()
                    .text_size(px(14.0))
                    .font_weight(FontWeight::BOLD)
                    .child("Scopewerk"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(8.0))
                    .h(px(24.0))
                    .rounded(px(12.0))
                    .bg(theme::app_bg())
                    .child(div().size(px(7.0)).rounded(px(4.0)).bg(dot))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(theme::muted())
                            .child(device),
                    ),
            )
            .child(div().flex_1())
            .when(self.live.paused, |el| {
                el.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme::trigger())
                        .child("Live view paused"),
                )
            })
            .child(
                div()
                    .font_family(theme::MONO)
                    .text_size(px(11.0))
                    .text_color(theme::faint())
                    .child(format!("{source}  {fps}")),
            )
            .when(!status.is_empty(), |el| {
                el.child(
                    div()
                        .px(px(8.0))
                        .h(px(22.0))
                        .flex()
                        .items_center()
                        .rounded(px(5.0))
                        .bg(theme::with_alpha(state, 0.18))
                        .text_size(px(11.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(state)
                        .child(status),
                )
            })
            .child(button("export", "Export CSV").on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.export_csv(window, cx)),
            ))
            .child(
                button("shot", "Screenshot")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.screenshot(cx))),
            )
    }

    fn screen(&self, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let scene = Scene {
            frames: self.frames.iter().cloned().collect(),
            settings: self.settings.clone(),
            selected: self.selected,
            hover_x_div: self.hover_x_div,
            cursors: self
                .cursors
                .map(|c| c.zip(self.settings.as_ref()).map(|(t, s)| x_div_of(t, s))),
            afterglow: self.afterglow,
            annotations: self.decoder.as_ref().map(|_| {
                Arc::new(
                    self.decoded
                        .as_ref()
                        .map(|d| d.annotations.clone())
                        .unwrap_or_default(),
                )
            }),
        };
        let empty = match &self.connection {
            Connection::Searching(reason) => Some(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(16.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Looking for the DOS1102…"),
                    )
                    .child(
                        div()
                            .max_w(px(460.0))
                            .text_center()
                            .text_size(px(12.5))
                            .text_color(theme::muted())
                            .child(reason.clone()),
                    )
                    .child(caption(
                        "Scopewerk connects on its own as soon as the scope appears.",
                    )),
            ),
            Connection::Connected(_) if self.frames.is_empty() && !self.live.paused => Some(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(caption("Reading the first frame…")),
            ),
            Connection::Connected(_) => None,
        };
        div()
            .id("screen")
            .relative()
            .flex_1()
            .min_h_0()
            .bg(theme::app_bg())
            .cursor_crosshair()
            .on_mouse_move(cx.listener(Self::on_plot_move))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_plot_down))
            .on_scroll_wheel(cx.listener(Self::on_plot_scroll))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered && this.hover_x_div.is_some() {
                    this.hover_x_div = None;
                    cx.notify();
                }
            }))
            .child(plot::plot(scene, Rc::clone(&self.geometry)))
            .child(self.readout())
            .children(empty)
    }

    /// Hover values and cursor deltas, top left over the screen.
    fn readout(&self) -> Div {
        let mut rows: Vec<Div> = Vec::new();
        if let (Some(frame), Some(x_div)) = (self.frames.back(), self.hover_x_div) {
            let t = time_of(x_div, &frame.settings);
            let mut row = div()
                .flex()
                .gap(px(12.0))
                .child(mono(format!("t {}", signed_si(t, "s"))).text_color(theme::muted()));
            for waveform in &frame.waveforms {
                if let Some(v) = value_at(frame, waveform.channel, t) {
                    row = row.child(
                        mono(format!("CH{} {}", waveform.channel, format_si(v, "V")))
                            .text_color(theme::channel(waveform.channel)),
                    );
                }
            }
            rows.push(row);
        }
        if let ([Some(a), Some(b)], Some(frame)) = (self.cursors, self.frames.back()) {
            let dt = b - a;
            let mut row = div()
                .flex()
                .gap(px(12.0))
                .child(mono(format!("Δt {}", signed_si(dt, "s"))).text_color(theme::accent()))
                .child(
                    mono(format!("1/Δt {}", format_si(1.0 / dt.abs(), "Hz")))
                        .text_color(theme::accent()),
                );
            for waveform in &frame.waveforms {
                if let (Some(va), Some(vb)) = (
                    value_at(frame, waveform.channel, a),
                    value_at(frame, waveform.channel, b),
                ) {
                    row = row.child(
                        mono(format!(
                            "ΔCH{} {}",
                            waveform.channel,
                            signed_si(vb - va, "V")
                        ))
                        .text_color(theme::channel(waveform.channel)),
                    );
                }
            }
            rows.push(row);
        } else if self.cursors.iter().any(Option::is_some) {
            rows.push(div().child(caption(format!("{ALT}-click sets cursor B · esc clears"))));
        }
        let left = self
            .geometry
            .get()
            .map_or(px(30.0), |g| g.screen.left() + px(8.0));
        let top = self
            .geometry
            .get()
            .map_or(px(26.0), |g| g.screen.top() + px(6.0));
        div()
            .absolute()
            .left(left)
            .top(top)
            .flex()
            .flex_col()
            .gap(px(3.0))
            .when(!rows.is_empty(), |d| {
                d.px(px(8.0))
                    .py(px(5.0))
                    .rounded(px(6.0))
                    .bg(theme::with_alpha(theme::app_bg(), 0.82))
            })
            .children(rows)
    }

    fn measurements(&self) -> Div {
        let strip = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .px(px(14.0))
            .py(px(8.0))
            .border_t_1()
            .border_color(theme::border())
            .bg(theme::panel());
        let Some(frame) = self.frames.back() else {
            return strip.child(caption("Measurements appear with the first frame."));
        };
        let settings = &frame.settings;
        let mut strip = strip.child(caption(format!(
            "{}/div · {} · {} · {} points · trigger {} {} {} {}",
            format_si(settings.timebase_s_per_div, "s"),
            settings
                .sample_rate_sa_s
                .map_or_else(|| "?".into(), |r| format_si(r, "Sa/s")),
            pretty(&settings.acquisition),
            frame.points(),
            settings.trigger.source,
            if same_token(&settings.trigger.edge, "FALL") {
                "↘"
            } else {
                "↗"
            },
            settings
                .trigger
                .level_v
                .map_or_else(|| "?".into(), |l| format_si(l, "V")),
            pretty(&settings.trigger.sweep),
        )));
        for waveform in &frame.waveforms {
            let Some(s) = frame.stats(waveform.channel) else {
                continue;
            };
            let cell = |name: &str, value: String| {
                div()
                    .flex()
                    .flex_col()
                    .min_w(px(74.0))
                    .child(caption(name.to_owned()))
                    .child(mono(value))
            };
            strip = strip.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .w(px(38.0))
                            .h(px(20.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.0))
                            .bg(theme::channel(waveform.channel))
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::app_bg())
                            .child(format!("CH{}", waveform.channel)),
                    )
                    .child(cell("Frequency", opt_si(s.frequency_hz, "Hz")))
                    .child(cell("Period", opt_si(s.period_s, "s")))
                    .child(cell("Pk-pk", format_si(s.pk_pk_v, "V")))
                    .child(cell("Mean", format_si(s.mean_v, "V")))
                    .child(cell("RMS", format_si(s.rms_v, "V")))
                    .child(cell("Min", format_si(s.min_v, "V")))
                    .child(cell("Max", format_si(s.max_v, "V")))
                    .child(cell(
                        "Duty",
                        s.duty_cycle_pct
                            .map_or_else(|| "—".into(), |d| format!("{d:.1} %")),
                    ))
                    .child(cell("Rise", opt_si(s.rise_time_s, "s"))),
            );
        }
        if let Some(decoded) = &self.decoded {
            strip = strip.child(decoded_table(decoded));
        }
        strip
    }

    fn controls(&self, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let column = div()
            .id("controls")
            .w(px(300.0))
            .h_full()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .p(px(10.0))
            .border_l_1()
            .border_color(theme::border())
            .bg(theme::app_bg())
            .overflow_y_scroll();
        let Some(settings) = self.settings.clone() else {
            return column
                .child(card(
                    "Controls",
                    theme::faint(),
                    None,
                    vec![
                        caption("Controls appear once the scope is connected.").into_any_element(),
                    ],
                ))
                .child(self.activity_card());
        };
        column
            .child(self.run_row(&settings, cx))
            .child(self.horizontal_card(&settings, cx))
            .child(self.channel_card(&settings, 1, cx))
            .child(self.channel_card(&settings, 2, cx))
            .child(self.trigger_card(&settings, cx))
            .child(self.acquire_card(&settings, cx))
            .child(self.decode_card(&settings, cx))
            .child(self.activity_card())
    }

    fn run_row(&self, settings: &Settings, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let stopped = settings.is_stopped();
        let (label, color) = if stopped {
            ("RUN", theme::run())
        } else {
            ("STOP", theme::stop())
        };
        div()
            .flex()
            .gap(px(6.0))
            .child(
                div()
                    .id("runstop")
                    .flex_1()
                    .h(px(34.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.0))
                    .bg(theme::with_alpha(color, 0.16))
                    .border_1()
                    .border_color(theme::with_alpha(color, 0.6))
                    .text_color(color)
                    .text_size(px(13.0))
                    .font_weight(FontWeight::BOLD)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::with_alpha(color, 0.26)))
                    .child(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.act(if stopped { Action::Run } else { Action::Stop }, cx);
                    })),
            )
            .child(
                button("single", "Single").h(px(34.0)).on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| this.act(Action::Single, cx)),
                ),
            )
            .child(
                button("autoset", "Autoset").h(px(34.0)).on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| this.act(Action::Autoset, cx)),
                ),
            )
    }

    fn horizontal_card(&self, settings: &Settings, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let span = settings.timebase_s_per_div * scopewerk_core::header::HORIZONTAL_DIVS;
        card(
            "Horizontal",
            theme::accent(),
            Some(caption(format!("screen {}", format_si(span, "s"))).into_any_element()),
            vec![
                self.stepper(
                    "tb",
                    "Timebase",
                    format!("{}/div", format_si(settings.timebase_s_per_div, "s")),
                    Box::new(|this, cx| this.step_timebase(-1, cx)),
                    Box::new(|this, cx| this.step_timebase(1, cx)),
                    cx,
                ),
                self.stepper(
                    "pos",
                    "Position",
                    format!("{:+.2} div", settings.horizontal_offset_div),
                    Box::new(|this, cx| this.nudge_position(-0.5, cx)),
                    Box::new(|this, cx| this.nudge_position(0.5, cx)),
                    cx,
                ),
            ],
        )
    }

    fn channel_card(
        &self,
        settings: &Settings,
        n: u8,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        let Some(c) = settings.channel(n).cloned() else {
            return div().id(SharedString::from(format!("ch{n}-card")));
        };
        let color = theme::channel(n);
        let selected = self.selected == n;
        let enabled = c.enabled;
        let toggle = div()
            .id(SharedString::from(format!("ch{n}-toggle")))
            .w(px(34.0))
            .h(px(18.0))
            .rounded(px(9.0))
            .p(px(2.0))
            .flex()
            .when(enabled, |d| d.justify_end().bg(color))
            .when(!enabled, |d| d.justify_start().bg(theme::border()))
            .cursor_pointer()
            .child(div().size(px(14.0)).rounded(px(7.0)).bg(theme::text()))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.change(
                    vec![Change::ChannelEnabled {
                        channel: n,
                        enabled: !enabled,
                    }],
                    cx,
                );
            }));
        let mut body: Vec<AnyElement> = Vec::new();
        if enabled {
            let low = (-4.0 - c.offset_div) * c.volts_per_div;
            let high = (4.0 - c.offset_div) * c.volts_per_div;
            body.push(self.stepper(
                &format!("ch{n}-v"),
                "Scale",
                format!("{}/div", format_si(c.volts_per_div, "V")),
                Box::new(move |this, cx| this.step_volts(n, -1, cx)),
                Box::new(move |this, cx| this.step_volts(n, 1, cx)),
                cx,
            ));
            body.push(self.stepper(
                &format!("ch{n}-o"),
                "Offset",
                format!("{:+.2} div", c.offset_div),
                Box::new(move |this, cx| this.nudge_offset(n, -0.5, cx)),
                Box::new(move |this, cx| this.nudge_offset(n, 0.5, cx)),
                cx,
            ));
            body.push(
                caption(format!(
                    "shows {} … {}",
                    format_si(low, "V"),
                    format_si(high, "V")
                ))
                .into_any_element(),
            );
            let coupling: Vec<_> = Coupling::ALL
                .iter()
                .map(|&choice| {
                    segment(
                        format!("ch{n}-c-{choice}"),
                        choice.scpi(),
                        same_token(&c.coupling, choice.scpi()),
                        color,
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            this.change(
                                vec![Change::Coupling {
                                    channel: n,
                                    coupling: choice,
                                }],
                                cx,
                            );
                        },
                    ))
                })
                .collect();
            body.push(segments(coupling).into_any_element());
            let probes: Vec<_> = [1u32, 10, 100]
                .iter()
                .map(|&factor| {
                    segment(
                        format!("ch{n}-p-{factor}"),
                        format!("{factor}X"),
                        same_token(&c.probe, &format!("{factor}X")),
                        color,
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            this.change(vec![Change::Probe { channel: n, factor }], cx);
                        },
                    ))
                })
                .collect();
            let inverted = c.inverted;
            body.push(
                div()
                    .flex()
                    .gap(px(6.0))
                    .items_center()
                    .child(caption("Probe").w(px(40.0)))
                    .child(segments(probes).flex_1())
                    .child(
                        segment(format!("ch{n}-inv"), "Invert", inverted, color)
                            .flex_none()
                            .px(px(8.0))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.change(
                                    vec![Change::Invert {
                                        channel: n,
                                        inverted: !inverted,
                                    }],
                                    cx,
                                );
                            })),
                    )
                    .into_any_element(),
            );
        }
        div()
            .id(SharedString::from(format!("ch{n}-card")))
            .rounded(px(10.0))
            .border_1()
            .border_color(if selected && enabled {
                theme::with_alpha(color, 0.7)
            } else {
                theme::app_bg()
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.selected = n;
                cx.notify();
            }))
            .child(card(
                format!("CH{n}"),
                color,
                Some(toggle.into_any_element()),
                body,
            ))
    }

    fn trigger_card(&self, settings: &Settings, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let source = trigger_channel(settings);
        let sources: Vec<_> = [1u8, 2]
            .iter()
            .map(|&n| {
                segment(
                    format!("trig-src-{n}"),
                    format!("CH{n}"),
                    source == n,
                    theme::channel(n),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.change(vec![Change::TriggerSource { channel: n }], cx)
                }))
            })
            .collect();
        let edges: Vec<_> = [(Edge::Rise, "↗ Rise"), (Edge::Fall, "↘ Fall")]
            .iter()
            .map(|&(edge, label)| {
                segment(
                    format!("trig-edge-{edge}"),
                    label,
                    same_token(&settings.trigger.edge, edge.scpi()),
                    theme::trigger(),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.change(vec![Change::TriggerEdge { edge }], cx)
                }))
            })
            .collect();
        let sweeps: Vec<_> = [
            (Sweep::Auto, "Auto"),
            (Sweep::Normal, "Normal"),
            (Sweep::Single, "Single"),
        ]
        .iter()
        .map(|&(sweep, label)| {
            segment(
                format!("trig-sweep-{sweep}"),
                label,
                same_token(&settings.trigger.sweep, sweep.scpi()),
                theme::trigger(),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.change(vec![Change::TriggerSweep { sweep }], cx)
            }))
        })
        .collect();
        let level = settings
            .trigger
            .level_v
            .map_or_else(|| "?".into(), |l| format_si(l, "V"));
        card(
            "Trigger",
            theme::trigger(),
            Some(
                button("trig-50", "50 %")
                    .h(px(20.0))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.trigger_to_middle(cx)))
                    .into_any_element(),
            ),
            vec![
                segments(sources).into_any_element(),
                self.stepper(
                    "trig-level",
                    "Level",
                    level,
                    Box::new(|this, cx| this.nudge_trigger(-1.0, cx)),
                    Box::new(|this, cx| this.nudge_trigger(1.0, cx)),
                    cx,
                ),
                segments(edges).into_any_element(),
                segments(sweeps).into_any_element(),
            ],
        )
    }

    fn acquire_card(&self, settings: &Settings, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let modes: Vec<_> = [
            (Acquisition::Sample, "Sample"),
            (Acquisition::Peak, "Peak"),
            (Acquisition::Average, "Average"),
        ]
        .iter()
        .map(|&(mode, label)| {
            segment(
                format!("acq-{mode}"),
                label,
                same_token(&settings.acquisition, mode.scpi()),
                theme::accent(),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.change(vec![Change::Acquisition { mode }], cx)
            }))
        })
        .collect();
        let live = self.live;
        let sources = vec![
            segment("src-screen", "Screen 1.5k", !live.memory, theme::accent()).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.set_live(
                        LiveMode {
                            memory: false,
                            ..this.live
                        },
                        cx,
                    )
                }),
            ),
            segment("src-memory", "Memory 10k", live.memory, theme::accent()).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.set_live(
                        LiveMode {
                            memory: true,
                            ..this.live
                        },
                        cx,
                    )
                }),
            ),
        ];
        let view = vec![
            segment("afterglow", "Afterglow", self.afterglow, theme::faint()).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.afterglow = !this.afterglow;
                    cx.notify();
                }),
            ),
            segment(
                "pause",
                if live.paused {
                    "Resume view"
                } else {
                    "Pause view"
                },
                live.paused,
                theme::trigger(),
            )
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.set_live(
                    LiveMode {
                        paused: !this.live.paused,
                        ..this.live
                    },
                    cx,
                );
            })),
        ];
        let rate = settings
            .sample_rate_sa_s
            .map_or_else(|| "?".into(), |r| format_si(r, "Sa/s"));
        card(
            "Acquisition",
            theme::accent(),
            Some(caption(format!("{} · {rate}", settings.memory_depth)).into_any_element()),
            vec![
                segments(modes).into_any_element(),
                segments(sources).into_any_element(),
                segments(view).into_any_element(),
            ],
        )
    }

    fn activity_card(&self) -> impl IntoElement {
        let hint = match &self.broker {
            Ok(_) => "Agents and the CLI work through this window: scopewerk mcp".to_owned(),
            Err(err) => format!("Sharing is off: {err}"),
        };
        let rows: Vec<AnyElement> = self
            .activity
            .iter()
            .take(8)
            .map(|entry| {
                let time: chrono::DateTime<chrono::Local> = entry.at.into();
                let color = match (entry.ok, entry.client.as_str()) {
                    (false, _) => theme::stop(),
                    (true, "mcp") => theme::agent(),
                    _ => theme::muted(),
                };
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(
                        div()
                            .font_family(theme::MONO)
                            .text_size(px(10.5))
                            .text_color(theme::faint())
                            .child(time.format("%H:%M:%S").to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(color)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(client_label(&entry.client)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(10.5))
                            .text_color(theme::muted())
                            .child(entry.text.clone()),
                    )
                    .into_any_element()
            })
            .collect();
        let mut body = vec![caption(hint).into_any_element()];
        body.extend(rows);
        card("Activity", theme::agent(), None, body)
    }

    fn notice(&self) -> Option<Div> {
        let (text, error, _) = self.notice.as_ref()?;
        Some(
            div()
                .absolute()
                .bottom(px(16.0))
                .left(px(16.0))
                .max_w(px(620.0))
                .px(px(12.0))
                .py(px(8.0))
                .rounded(px(8.0))
                .bg(theme::panel_raised())
                .border_1()
                .border_color(if *error {
                    theme::stop()
                } else {
                    theme::border()
                })
                .text_size(px(12.0))
                .text_color(if *error { theme::stop() } else { theme::text() })
                .child(text.clone()),
        )
    }
}

impl Render for ScopeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::app_bg())
            .text_color(theme::text())
            .on_action(cx.listener(|this, _: &ToggleRun, _, cx| {
                let stopped = this.settings.as_ref().is_some_and(Settings::is_stopped);
                this.act(if stopped { Action::Run } else { Action::Stop }, cx);
            }))
            .on_action(cx.listener(|this, _: &SingleShot, _, cx| this.act(Action::Single, cx)))
            .on_action(cx.listener(|this, _: &Autoset, _, cx| this.act(Action::Autoset, cx)))
            .on_action(cx.listener(|this, _: &TimebaseFaster, _, cx| this.step_timebase(-1, cx)))
            .on_action(cx.listener(|this, _: &TimebaseSlower, _, cx| this.step_timebase(1, cx)))
            .on_action(cx.listener(|this, _: &VoltsFiner, _, cx| this.step_volts(this.selected, -1, cx)))
            .on_action(cx.listener(|this, _: &VoltsCoarser, _, cx| this.step_volts(this.selected, 1, cx)))
            .on_action(cx.listener(|this, _: &OffsetUp, _, cx| this.nudge_offset(this.selected, 0.2, cx)))
            .on_action(cx.listener(|this, _: &OffsetDown, _, cx| this.nudge_offset(this.selected, -0.2, cx)))
            .on_action(cx.listener(|this, _: &SelectCh1, _, cx| {
                this.selected = 1;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SelectCh2, _, cx| {
                this.selected = 2;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleCh1, _, cx| this.toggle_channel(1, cx)))
            .on_action(cx.listener(|this, _: &ToggleCh2, _, cx| this.toggle_channel(2, cx)))
            .on_action(cx.listener(|this, _: &ToggleMemory, _, cx| {
                this.set_live(LiveMode { memory: !this.live.memory, ..this.live }, cx);
            }))
            .on_action(cx.listener(|this, _: &TogglePause, _, cx| {
                this.set_live(LiveMode { paused: !this.live.paused, ..this.live }, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleAfterglow, _, cx| {
                this.afterglow = !this.afterglow;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &TriggerToMiddle, _, cx| this.trigger_to_middle(cx)))
            .on_action(cx.listener(|this, _: &ClearCursors, _, cx| {
                this.cursors = [None, None];
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ExportCsv, window, cx| this.export_csv(window, cx)))
            .on_action(cx.listener(|this, _: &SaveScreenshot, _, cx| this.screenshot(cx)))
            .on_action(cx.listener(|this, _: &CopyMeasurements, _, cx| this.copy_measurements(cx)))
            .child(self.toolbar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(self.screen(cx))
                            .child(self.measurements())
                            .child(
                                div()
                                    .px(px(14.0))
                                    .py(px(5.0))
                                    .border_t_1()
                                    .border_color(theme::border())
                                    .text_size(px(10.5))
                                    .text_color(theme::faint())
                                    .child(format!(
                                        "space run/stop   s single   {MOD}{SHIFT}A autoset   ←→ timebase   ↑↓ V/div   \
                                         {SHIFT}↑↓ offset   1/2 channel   t 50 %   m memory   g afterglow   \
                                         scroll timebase ({ALT} V/div, {SHIFT} offset)   click cursor A, {ALT}-click B"
                                    )),
                            )
                            .children(self.notice()),
                    )
                    .child(self.controls(cx)),
            )
    }
}

// ---- protocol decoding --------------------------------------------------------------------

/// The decoders offered in the panel, with their defaults.
fn default_config(protocol: &str) -> Option<DecodeConfig> {
    Some(match protocol {
        "CAN" => DecodeConfig::Can {
            input: Input::Ch1,
            bitrate: None,
            dominant_high: None,
        },
        "I²C" => DecodeConfig::I2c { scl: 1, sda: 2 },
        "SPI" => DecodeConfig::Spi {
            clk: 1,
            data: 2,
            mode: None,
            bits: 8,
            lsb_first: false,
        },
        "UART" => DecodeConfig::Uart {
            input: Input::Ch1,
            baud: None,
            data_bits: 8,
            parity: decode::Parity::None,
            inverted: false,
        },
        _ => return None,
    })
}

/// Step through Auto and a list of standard rates (ascending).
fn step_rate(current: Option<f64>, rates: &[f64], direction: i32) -> Option<f64> {
    let mut ascending = rates.to_vec();
    ascending.sort_by(f64::total_cmp);
    let index = current.and_then(|c| ascending.iter().position(|r| (r - c).abs() < c * 1e-3));
    match (index, direction > 0) {
        (None, true) => ascending.first().copied(),
        (None, false) => None,
        (Some(0), false) => None,
        (Some(i), false) => Some(ascending[i - 1]),
        (Some(i), true) => Some(ascending[(i + 1).min(ascending.len() - 1)]),
    }
}

fn rate_label(rate: Option<f64>, detected: Option<f64>, unit: &str) -> String {
    match (rate, detected) {
        (Some(rate), _) => format_si(rate, unit),
        (None, Some(found)) => format!("Auto · {}", format_si(found, unit)),
        (None, None) => "Auto".to_owned(),
    }
}

impl ScopeView {
    fn set_decoder(&mut self, config: Option<DecodeConfig>, cx: &mut Context<'_, Self>) {
        match (&self.decoder, &config) {
            (None, Some(_)) => {
                // Decoding needs the 10 000-sample memory.
                self.memory_before_decode = Some(self.live.memory);
                if !self.live.memory {
                    self.set_live(
                        LiveMode {
                            memory: true,
                            ..self.live
                        },
                        cx,
                    );
                }
            }
            (Some(_), None) if self.memory_before_decode.take() == Some(false) => {
                self.set_live(
                    LiveMode {
                        memory: false,
                        ..self.live
                    },
                    cx,
                );
            }
            _ => {}
        }
        self.decoder = config;
        self.decoded = None;
        if let (Some(config), Some(frame)) = (&self.decoder, self.frames.back()) {
            self.decoded = decode::decode(frame, config).ok().map(Arc::new);
        }
        cx.notify();
    }

    fn update_decoder(
        &mut self,
        change: impl FnOnce(&mut DecodeConfig),
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(mut config) = self.decoder.clone() {
            change(&mut config);
            self.set_decoder(Some(config), cx);
        }
    }

    fn decode_card(&self, settings: &Settings, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let current = self.decoder.as_ref().map(DecodeConfig::protocol);
        let protocols: Vec<_> = ["Off", "CAN", "I²C", "SPI", "UART"]
            .iter()
            .map(|&name| {
                let on = current.map_or(name == "Off", |p| p == name);
                segment(format!("dec-{name}"), name, on, theme::agent()).on_click(cx.listener(
                    move |this, _: &ClickEvent, _, cx| {
                        if this.decoder.as_ref().map(DecodeConfig::protocol) != Some(name) {
                            this.set_decoder(default_config(name), cx);
                        }
                    },
                ))
            })
            .collect();
        let mut body = vec![segments(protocols).into_any_element()];
        let detected = self.decoded.as_ref().and_then(|d| d.rate_hz);
        let input_row = |id: &str, input: Input, cx: &mut Context<'_, Self>, choices: &[Input]| {
            let buttons: Vec<_> = choices
                .iter()
                .map(|&choice| {
                    segment(
                        format!("{id}-{}", choice.label()),
                        choice.label(),
                        input == choice,
                        theme::agent(),
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            this.update_decoder(
                                |c| match c {
                                    DecodeConfig::Can { input, .. }
                                    | DecodeConfig::Uart { input, .. } => *input = choice,
                                    _ => {}
                                },
                                cx,
                            );
                        },
                    ))
                })
                .collect();
            segments(buttons).into_any_element()
        };
        match self.decoder.clone() {
            Some(DecodeConfig::Can {
                input,
                bitrate,
                dominant_high,
            }) => {
                body.push(input_row(
                    "can-in",
                    input,
                    cx,
                    &[Input::Ch1, Input::Ch2, Input::Ch1MinusCh2],
                ));
                body.push(self.stepper(
                    "can-rate",
                    "Bitrate",
                    rate_label(bitrate, detected, "bit/s"),
                    Box::new(move |this, cx| {
                        this.update_decoder(
                            |c| {
                                if let DecodeConfig::Can { bitrate, .. } = c {
                                    *bitrate = step_rate(*bitrate, &STANDARD_BITRATES, -1)
                                }
                            },
                            cx,
                        )
                    }),
                    Box::new(move |this, cx| {
                        this.update_decoder(
                            |c| {
                                if let DecodeConfig::Can { bitrate, .. } = c {
                                    *bitrate = step_rate(*bitrate, &STANDARD_BITRATES, 1)
                                }
                            },
                            cx,
                        )
                    }),
                    cx,
                ));
                let polarities: Vec<_> =
                    [(None, "Auto"), (Some(true), "High"), (Some(false), "Low")]
                        .iter()
                        .map(|&(value, label)| {
                            segment(
                                format!("can-pol-{label}"),
                                label,
                                dominant_high == value,
                                theme::agent(),
                            )
                            .on_click(cx.listener(
                                move |this, _: &ClickEvent, _, cx| {
                                    this.update_decoder(
                                        |c| {
                                            if let DecodeConfig::Can { dominant_high, .. } = c {
                                                *dominant_high = value
                                            }
                                        },
                                        cx,
                                    );
                                },
                            ))
                        })
                        .collect();
                body.push(segments(polarities).into_any_element());
            }
            Some(DecodeConfig::I2c { scl, .. }) => {
                let maps: Vec<_> = [(1u8, "SCL CH1 · SDA CH2"), (2u8, "SCL CH2 · SDA CH1")]
                    .iter()
                    .map(|&(clock, label)| {
                        segment(format!("i2c-{clock}"), label, scl == clock, theme::agent())
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.update_decoder(
                                    |c| {
                                        if let DecodeConfig::I2c { scl, sda } = c {
                                            (*scl, *sda) = (clock, 3 - clock)
                                        }
                                    },
                                    cx,
                                );
                            }))
                    })
                    .collect();
                body.push(segments(maps).into_any_element());
            }
            Some(DecodeConfig::Spi { clk, mode, .. }) => {
                let maps: Vec<_> = [(1u8, "CLK CH1 · data CH2"), (2u8, "CLK CH2 · data CH1")]
                    .iter()
                    .map(|&(clock, label)| {
                        segment(format!("spi-{clock}"), label, clk == clock, theme::agent())
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.update_decoder(
                                    |c| {
                                        if let DecodeConfig::Spi { clk, data, .. } = c {
                                            (*clk, *data) = (clock, 3 - clock)
                                        }
                                    },
                                    cx,
                                );
                            }))
                    })
                    .collect();
                body.push(segments(maps).into_any_element());
                let modes: Vec<_> = [None, Some(0u8), Some(1), Some(2), Some(3)]
                    .iter()
                    .map(|&value| {
                        let label =
                            value.map_or_else(|| "Auto".to_owned(), |m| format!("Mode {m}"));
                        segment(
                            format!("spi-mode-{label}"),
                            label,
                            mode == value,
                            theme::agent(),
                        )
                        .on_click(cx.listener(
                            move |this, _: &ClickEvent, _, cx| {
                                this.update_decoder(
                                    |c| {
                                        if let DecodeConfig::Spi { mode, .. } = c {
                                            *mode = value
                                        }
                                    },
                                    cx,
                                );
                            },
                        ))
                    })
                    .collect();
                body.push(segments(modes).into_any_element());
            }
            Some(DecodeConfig::Uart { input, baud, .. }) => {
                body.push(input_row("uart-in", input, cx, &[Input::Ch1, Input::Ch2]));
                body.push(self.stepper(
                    "uart-baud",
                    "Baud",
                    rate_label(baud, detected, "Bd"),
                    Box::new(move |this, cx| {
                        this.update_decoder(
                            |c| {
                                if let DecodeConfig::Uart { baud, .. } = c {
                                    *baud = step_rate(*baud, &STANDARD_BAUDS, -1)
                                }
                            },
                            cx,
                        )
                    }),
                    Box::new(move |this, cx| {
                        this.update_decoder(
                            |c| {
                                if let DecodeConfig::Uart { baud, .. } = c {
                                    *baud = step_rate(*baud, &STANDARD_BAUDS, 1)
                                }
                            },
                            cx,
                        )
                    }),
                    cx,
                ));
            }
            None => {
                body.push(caption("Turns the scope into a 2-channel logic analyzer. Decodes the 10k-sample memory of every frame.").into_any_element());
            }
        }
        if let Some(decoded) = &self.decoded {
            body.push(
                caption(decoded.summary.clone())
                    .text_color(theme::text())
                    .into_any_element(),
            );
            if let Some(warning) = decoded.warnings.first() {
                body.push(
                    caption(warning.clone())
                        .text_color(theme::trigger())
                        .into_any_element(),
                );
            }
            // Offer the timebase that gives ~10 samples per bit when the current one is too slow.
            let rate = decoded.rate_hz.or(match &self.decoder {
                Some(DecodeConfig::Can { bitrate, .. }) => *bitrate,
                Some(DecodeConfig::Uart { baud, .. }) => *baud,
                _ => None,
            });
            if let Some(rate) = rate
                && decoded.samples_per_bit.is_none_or(|s| s < 6.0)
            {
                let target = recommended_timebase(rate, 10.0);
                if (target - settings.timebase_s_per_div).abs() > target * 1e-6 {
                    body.push(
                        button(
                            "dec-timebase",
                            format!("Use {}/div", format_si(target, "s")),
                        )
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.change(vec![Change::Timebase { s_per_div: target }], cx)
                        }))
                        .into_any_element(),
                    );
                }
            }
        }
        card("Decode", theme::agent(), None, body)
    }
}

/// The decoded events of the newest frame, below the measurements.
fn decoded_table(decoded: &Decoded) -> AnyElement {
    // UART reads better as lines of text than as one row per byte.
    let entries: Vec<(f64, String, bool)> = if decoded.protocol == "UART" {
        decode::uart_lines(&decoded.events, decoded.rate_hz)
            .into_iter()
            .map(|(t, bytes)| {
                let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
                (
                    t,
                    format!("{:<24} {}", decode::escape_text(&bytes), hex.join(" ")),
                    true,
                )
            })
            .collect()
    } else {
        decoded
            .events
            .iter()
            .map(|e| (e.t_s(), e.describe(), e.is_ok()))
            .collect()
    };
    let rows: Vec<AnyElement> = entries
        .into_iter()
        .take(400)
        .map(|(t, text, ok)| {
            div()
                .flex()
                .gap(px(14.0))
                .child(
                    mono(format!(
                        "{}{}",
                        if t >= 0.0 { "+" } else { "" },
                        format_si(t, "s")
                    ))
                    .w(px(96.0))
                    .text_color(theme::faint()),
                )
                .child(mono(text).text_color(if ok { theme::text() } else { theme::stop() }))
                .into_any_element()
        })
        .collect();
    let empty = rows
        .is_empty()
        .then(|| caption("Nothing decoded in this frame.").into_any_element());
    div()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .pt(px(4.0))
        .border_t_1()
        .border_color(theme::border())
        .child(
            div()
                .flex()
                .gap(px(10.0))
                .items_baseline()
                .child(
                    div()
                        .text_size(px(11.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::agent())
                        .child(decoded.protocol.clone()),
                )
                .child(caption(format!("{} · {}", decoded.summary, decoded.detail))),
        )
        .child(
            div()
                .id("decoded-rows")
                .max_h(px(132.0))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(1.0))
                .children(empty)
                .children(rows),
        )
        .into_any_element()
}
