//! The scope screen: square divisions like the DOS1102 (15.2 × 8), dotted graticule, traces with
//! afterglow, ground and trigger markers, measurement cursors and the hover crosshair.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::{
    App, Bounds, ContentMask, Hsla, IntoElement, PathBuilder, Pixels, SharedString, Styled as _,
    TextAlign, TextRun, Window, canvas, fill, font, point, px, size,
};
use scopewerk_core::decode::{Annotation, Mark};
use scopewerk_core::header::{HORIZONTAL_DIVS, VERTICAL_DIVS};
use scopewerk_core::{Capture, ChannelSettings, Settings};

use crate::theme;

/// Room around the screen for the channel, trigger and cursor markers.
const MARGIN_SIDE: f32 = 22.0;
const MARGIN_TOP: f32 = 18.0;
const MARGIN_BOTTOM: f32 = 6.0;
/// The protocol lane under the screen while a decoder runs.
const LANE_HEIGHT: f32 = 24.0;
const LANE_GAP: f32 = 8.0;

/// Where the screen sits inside the plot area, in square divisions.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Geometry {
    pub(crate) screen: Bounds<Pixels>,
    pub(crate) div: f32,
    /// The decoder lane, when shown.
    pub(crate) lane: Option<Bounds<Pixels>>,
}

impl Geometry {
    pub(crate) fn new(bounds: Bounds<Pixels>, lane: bool) -> Self {
        let lane_room = if lane { LANE_HEIGHT + LANE_GAP } else { 0.0 };
        let width = f32::from(bounds.size.width) - 2.0 * MARGIN_SIDE;
        let height = f32::from(bounds.size.height) - MARGIN_TOP - MARGIN_BOTTOM - lane_room;
        let div = (width / to_f32(HORIZONTAL_DIVS))
            .min(height / to_f32(VERTICAL_DIVS))
            .max(4.0)
            .floor();
        let screen_w = div * to_f32(HORIZONTAL_DIVS);
        let screen_h = div * to_f32(VERTICAL_DIVS);
        let left = f32::from(bounds.origin.x) + (f32::from(bounds.size.width) - screen_w) / 2.0;
        let top = f32::from(bounds.origin.y) + MARGIN_TOP + (height - screen_h).max(0.0) / 2.0;
        let lane = lane.then(|| {
            Bounds::new(
                point(px(left), px(top + screen_h + LANE_GAP)),
                size(px(screen_w), px(LANE_HEIGHT)),
            )
        });
        Self {
            screen: Bounds::new(point(px(left), px(top)), size(px(screen_w), px(screen_h))),
            div,
            lane,
        }
    }

    fn centre(&self) -> (f32, f32) {
        let c = self.screen.center();
        (f32::from(c.x), f32::from(c.y))
    }

    /// Pixel x of a horizontal position in divisions from the centre.
    pub(crate) fn x(&self, x_div: f64) -> f32 {
        self.centre().0 + to_f32(x_div) * self.div
    }

    /// Pixel y of a vertical position in divisions from the centre (up is positive).
    pub(crate) fn y(&self, y_div: f64) -> f32 {
        self.centre().1 - to_f32(y_div) * self.div
    }

    pub(crate) fn x_div(&self, x: Pixels) -> f64 {
        f64::from((f32::from(x) - self.centre().0) / self.div)
    }

    pub(crate) fn contains_x(&self, x: Pixels) -> bool {
        x >= self.screen.left() && x <= self.screen.right()
    }
}

#[expect(clippy::cast_possible_truncation, reason = "screen coordinates")]
fn to_f32(value: f64) -> f32 {
    value as f32
}

/// Horizontal screen position of time `t` (seconds from the trigger).
pub(crate) fn x_div_of(t: f64, settings: &Settings) -> f64 {
    t / settings.timebase_s_per_div - settings.horizontal_offset_div
}

pub(crate) fn time_of(x_div: f64, settings: &Settings) -> f64 {
    (x_div + settings.horizontal_offset_div) * settings.timebase_s_per_div
}

/// Voltage of a channel at time `t`, linearly interpolated.
pub(crate) fn value_at(capture: &Capture, channel: u8, t: f64) -> Option<f64> {
    let waveform = capture.waveform(channel)?;
    let position = (t - capture.t0) / capture.dt;
    if !(0.0..=position_max(waveform.volts.len())).contains(&position) {
        return None;
    }
    let index = position.floor();
    let fraction = position - index;
    let i = usize::try_from(index as i64).ok()?;
    let a = f64::from(*waveform.volts.get(i)?);
    let b = waveform.volts.get(i + 1).map_or(a, |v| f64::from(*v));
    Some(a + (b - a) * fraction)
}

fn position_max(points: usize) -> f64 {
    count_f64(points.saturating_sub(1))
}

#[expect(clippy::cast_precision_loss, reason = "sample counts")]
fn count_f64(value: usize) -> f64 {
    value as f64
}

/// Everything the screen shows for one paint.
#[derive(Clone)]
pub(crate) struct Scene {
    /// Oldest first; the last one is the current frame.
    pub(crate) frames: Vec<Arc<Capture>>,
    /// The settings to draw markers with (may be ahead of the frames after a click).
    pub(crate) settings: Option<Settings>,
    pub(crate) selected: u8,
    pub(crate) hover_x_div: Option<f64>,
    pub(crate) cursors: [Option<f64>; 2],
    pub(crate) afterglow: bool,
    /// Decoder annotations of the newest frame; `Some` shows the lane.
    pub(crate) annotations: Option<Arc<Vec<Annotation>>>,
}

/// The plot element. `geometry` receives the layout so mouse handlers can map positions.
pub(crate) fn plot(scene: Scene, geometry: Rc<Cell<Option<Geometry>>>) -> impl IntoElement {
    let scene_has_lane = scene.annotations.is_some();
    canvas(
        move |bounds, _, _| {
            let g = Geometry::new(bounds, scene_has_lane);
            geometry.set(Some(g));
            g
        },
        move |_, g, window, cx| paint(&scene, g, window, cx),
    )
    .size_full()
}

fn paint(scene: &Scene, g: Geometry, window: &mut Window, cx: &mut App) {
    let started = std::time::Instant::now();
    paint_screen(scene, g, window, cx);
    if std::env::var_os("SCOPEWERK_TRACE").is_some() {
        eprintln!("scopewerk: paint {:?}", started.elapsed());
    }
}

fn paint_screen(scene: &Scene, g: Geometry, window: &mut Window, cx: &mut App) {
    window.paint_quad(fill(g.screen, theme::screen_bg()));
    paint_grid(g, window);

    let mask = ContentMask { bounds: g.screen };
    window.with_content_mask(Some(mask), |window| {
        let count = scene.frames.len();
        for (age, frame) in scene.frames.iter().enumerate() {
            let newest = age + 1 == count;
            if !newest && !scene.afterglow {
                continue;
            }
            // Older frames fade out: 0.1, 0.18, … under the bright current trace.
            let alpha = if newest {
                1.0
            } else {
                0.08 + 0.1 * age_fraction(age, count)
            };
            for waveform in &frame.waveforms {
                paint_trace(
                    frame,
                    waveform.channel,
                    view_of(scene, frame),
                    g,
                    theme::with_alpha(theme::channel(waveform.channel), alpha),
                    window,
                );
            }
        }
        if let Some(x_div) = scene.hover_x_div {
            paint_vline(
                g,
                g.x(x_div),
                theme::with_alpha(theme::cursor(), 0.45),
                1.0,
                window,
            );
            if let Some(frame) = scene.frames.last() {
                let view = view_of(scene, frame);
                let t = time_of(x_div, view);
                for waveform in &frame.waveforms {
                    if let (Some(v), Some(channel), Some(captured)) = (
                        value_at(frame, waveform.channel, t),
                        view.channel(waveform.channel),
                        frame.settings.channel(waveform.channel),
                    ) {
                        let v = v * probe_ratio(channel, captured);
                        let y = g.y(v / channel.volts_per_div + channel.offset_div);
                        let dot = Bounds::new(
                            point(px(g.x(x_div) - 3.0), px(y - 3.0)),
                            size(px(6.0), px(6.0)),
                        );
                        window.paint_quad(gpui_kit::quad(
                            dot,
                            px(3.0),
                            theme::channel(waveform.channel),
                            px(1.0),
                            theme::screen_bg(),
                            gpui_kit::BorderStyle::Solid,
                        ));
                    }
                }
            }
        }
    });

    for (index, cursor) in scene.cursors.iter().enumerate() {
        if let Some(x_div) = cursor {
            let x = g.x(*x_div);
            paint_vline(g, x, theme::with_alpha(theme::accent(), 0.9), 1.0, window);
            label(
                window,
                cx,
                if index == 0 { "A" } else { "B" },
                x,
                f32::from(g.screen.top()) - 15.0,
                theme::accent(),
                TextAlign::Center,
            );
        }
    }

    if let Some(settings) = &scene.settings {
        paint_markers(scene, settings, g, window, cx);
    }
    if let (Some(lane), Some(notes), Some(frame)) =
        (g.lane, scene.annotations.as_ref(), scene.frames.last())
    {
        paint_lane(lane, notes, &frame.settings, g, window, cx);
    }
}

fn age_fraction(age: usize, count: usize) -> f32 {
    #[expect(clippy::cast_precision_loss, reason = "a handful of frames")]
    let fraction = age as f32 / count.max(1) as f32;
    fraction
}

fn paint_grid(g: Geometry, window: &mut Window) {
    let dot = |window: &mut Window, x: f32, y: f32, color: Hsla| {
        window.paint_quad(fill(
            Bounds::new(point(px(x), px(y)), size(px(1.0), px(1.0))),
            color,
        ));
    };
    let half_w = HORIZONTAL_DIVS / 2.0;
    let left = f32::from(g.screen.left());
    let right = f32::from(g.screen.right());
    let top = f32::from(g.screen.top());
    let bottom = f32::from(g.screen.bottom());
    let fifth = g.div / 5.0;

    // Vertical division lines, dotted every fifth of a division.
    let mut div = -half_w.floor();
    while div <= half_w {
        let x = g.x(div).round();
        let mut y = top;
        while y <= bottom {
            dot(window, x, y.round(), theme::grid());
            y += fifth;
        }
        div += 1.0;
    }
    for div in -4..=4 {
        let y = g.y(f64::from(div)).round();
        let mut x = g.x(-half_w.floor());
        while x >= left {
            x -= fifth;
        }
        x += fifth;
        while x <= right {
            dot(window, x.round(), y, theme::grid());
            x += fifth;
        }
    }
    // Centre axes with tick marks, as on the scope.
    let (cx, cy) = (g.x(0.0).round(), g.y(0.0).round());
    let mut x = cx;
    while x >= left {
        x -= fifth;
    }
    x += fifth;
    while x <= right {
        window.paint_quad(fill(
            Bounds::new(point(px(x.round()), px(cy - 2.0)), size(px(1.0), px(5.0))),
            theme::grid_axis(),
        ));
        x += fifth;
    }
    let mut y = top;
    while y <= bottom {
        window.paint_quad(fill(
            Bounds::new(point(px(cx - 2.0), px(y.round())), size(px(5.0), px(1.0))),
            theme::grid_axis(),
        ));
        y += fifth;
    }
    // Frame.
    window.paint_quad(gpui_kit::outline(
        g.screen,
        theme::border(),
        gpui_kit::BorderStyle::Solid,
    ));
}

fn paint_vline(g: Geometry, x: f32, color: Hsla, width: f32, window: &mut Window) {
    let bounds = Bounds::new(
        point(px(x - width / 2.0), g.screen.top()),
        size(px(width), g.screen.size.height),
    );
    window.paint_quad(fill(bounds, color));
}

/// The settings a frame is drawn with: the window's, which run ahead of the scope right after a
/// click. Stored volts and times are re-projected at once, the way a digital scope rescales its
/// acquisition before the next one arrives.
fn view_of<'a>(scene: &'a Scene, frame: &'a Capture) -> &'a Settings {
    scene.settings.as_ref().unwrap_or(&frame.settings)
}

/// A probe switched in the window but not yet in the data scales the stored volts.
fn probe_ratio(shown: &ChannelSettings, captured: &ChannelSettings) -> f64 {
    let factor = |probe: &str| {
        probe
            .trim_end_matches(['X', 'x'])
            .parse::<f64>()
            .unwrap_or(1.0)
    };
    factor(&shown.probe) / factor(&captured.probe)
}

fn paint_trace(
    frame: &Capture,
    channel: u8,
    view: &Settings,
    g: Geometry,
    color: Hsla,
    window: &mut Window,
) {
    let (Some(waveform), Some(captured), Some(settings)) = (
        frame.waveform(channel),
        frame.settings.channel(channel),
        view.channel(channel),
    ) else {
        return;
    };
    if waveform.volts.len() < 2 || !settings.enabled || !settings.volts_per_div.is_finite() {
        return;
    }
    let ratio = probe_ratio(settings, captured);
    let limit = f64::from(VERTICAL_DIVS as f32 / 2.0 + 0.3);
    let y_of = |volts: f32| {
        let y_div = f64::from(volts) * ratio / settings.volts_per_div + settings.offset_div;
        g.y(y_div.clamp(-limit, limit))
    };
    let x_of = |index: usize| g.x(x_div_of(frame.time_at(index), view));

    let mut path = PathBuilder::stroke(px(1.4));
    let columns = f32::from(g.screen.size.width).max(1.0);
    let dense = count_f64(waveform.volts.len()) > f64::from(columns) * 2.0;
    if dense {
        // One min/max pair per pixel column keeps every peak of 10 000-point memory reads.
        let mut column: Option<(i32, f32, f32)> = None;
        let mut first = true;
        let mut emit = |path: &mut PathBuilder, x: i32, low: f32, high: f32| {
            #[expect(clippy::cast_precision_loss, reason = "pixel column")]
            let xf = x as f32;
            if first {
                path.move_to(point(px(xf), px(low)));
                first = false;
            } else {
                path.line_to(point(px(xf), px(low)));
            }
            path.line_to(point(px(xf), px(high)));
        };
        for (index, volts) in waveform.volts.iter().enumerate() {
            #[expect(clippy::cast_possible_truncation, reason = "pixel column")]
            let x = x_of(index).round() as i32;
            let y = y_of(*volts);
            column = match column {
                Some((cx, low, high)) if cx == x => Some((cx, low.max(y), high.min(y))),
                Some((cx, low, high)) => {
                    emit(&mut path, cx, low, high);
                    Some((x, y, y))
                }
                None => Some((x, y, y)),
            };
        }
        if let Some((cx, low, high)) = column {
            emit(&mut path, cx, low, high);
        }
    } else {
        for (index, volts) in waveform.volts.iter().enumerate() {
            let p = point(px(x_of(index)), px(y_of(*volts)));
            if index == 0 {
                path.move_to(p);
            } else {
                path.line_to(p);
            }
        }
    }
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn paint_markers(
    scene: &Scene,
    settings: &Settings,
    g: Geometry,
    window: &mut Window,
    cx: &mut App,
) {
    let left = f32::from(g.screen.left());
    let right = f32::from(g.screen.right());
    // Channel ground markers on the left: where 0 V sits.
    for channel in settings.channels.iter().filter(|c| c.enabled) {
        let y = g.y(channel.offset_div.clamp(-4.0, 4.0));
        let color = theme::channel(channel.channel);
        let selected = channel.channel == scene.selected;
        let tab = Bounds::new(
            point(px(left - 19.0), px(y - 7.0)),
            size(px(16.0), px(14.0)),
        );
        window.paint_quad(gpui_kit::quad(
            tab,
            px(3.0),
            if selected {
                color
            } else {
                theme::with_alpha(color, 0.55)
            },
            px(0.0),
            color,
            gpui_kit::BorderStyle::Solid,
        ));
        arrow_right(
            window,
            left - 3.0,
            y,
            if selected {
                color
            } else {
                theme::with_alpha(color, 0.55)
            },
        );
        label(
            window,
            cx,
            if channel.channel == 1 { "1" } else { "2" },
            left - 11.0,
            y - 7.0,
            theme::screen_bg(),
            TextAlign::Center,
        );
    }

    // Trigger level on the right, on the source channel's scale.
    let source = settings
        .trigger
        .source
        .trim_start_matches("CH")
        .parse::<u8>()
        .unwrap_or(1);
    if let (Some(level), Some(channel)) = (settings.trigger.level_v, settings.channel(source)) {
        let y_div = level / channel.volts_per_div + channel.offset_div;
        if y_div.is_finite() {
            let y = g.y(y_div.clamp(-4.0, 4.0));
            arrow_left(window, right + 3.0, y, theme::trigger());
            let tab = Bounds::new(
                point(px(right + 3.0), px(y - 7.0)),
                size(px(16.0), px(14.0)),
            );
            window.paint_quad(gpui_kit::quad(
                tab,
                px(3.0),
                theme::trigger(),
                px(0.0),
                theme::trigger(),
                gpui_kit::BorderStyle::Solid,
            ));
            label(
                window,
                cx,
                "T",
                right + 11.0,
                y - 7.0,
                theme::screen_bg(),
                TextAlign::Center,
            );
            if (-4.0..=4.0).contains(&y_div) {
                let line = Bounds::new(
                    point(g.screen.left(), px(y)),
                    size(g.screen.size.width, px(1.0)),
                );
                window.paint_quad(fill(line, theme::with_alpha(theme::trigger(), 0.18)));
            }
        }
    }
    // Trigger point at the top: time zero.
    let x = g.x(x_div_of(0.0, settings)).clamp(left, right);
    triangle_down(window, x, f32::from(g.screen.top()) - 2.0, theme::trigger());
}

fn arrow_right(window: &mut Window, tip_x: f32, y: f32, color: Hsla) {
    let mut path = PathBuilder::fill();
    path.move_to(point(px(tip_x), px(y)));
    path.line_to(point(px(tip_x - 5.0), px(y - 5.0)));
    path.line_to(point(px(tip_x - 5.0), px(y + 5.0)));
    path.close();
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn arrow_left(window: &mut Window, tip_x: f32, y: f32, color: Hsla) {
    let mut path = PathBuilder::fill();
    path.move_to(point(px(tip_x), px(y)));
    path.line_to(point(px(tip_x + 5.0), px(y - 5.0)));
    path.line_to(point(px(tip_x + 5.0), px(y + 5.0)));
    path.close();
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn triangle_down(window: &mut Window, x: f32, bottom: f32, color: Hsla) {
    let mut path = PathBuilder::fill();
    path.move_to(point(px(x), px(bottom)));
    path.line_to(point(px(x - 6.0), px(bottom - 9.0)));
    path.line_to(point(px(x + 6.0), px(bottom - 9.0)));
    path.close();
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn label(
    window: &mut Window,
    cx: &mut App,
    text: &str,
    x: f32,
    top: f32,
    color: Hsla,
    align: TextAlign,
) {
    let text = SharedString::from(text.to_owned());
    let run = TextRun {
        len: text.len(),
        font: gpui_kit::Font {
            weight: gpui_kit::FontWeight::BOLD,
            ..font(theme::MONO)
        },
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window
        .text_system()
        .shape_line(text, px(10.5), &[run], None);
    let width = f32::from(line.width);
    let origin_x = match align {
        TextAlign::Center => x - width / 2.0,
        TextAlign::Right => x - width,
        TextAlign::Left => x,
    };
    let _ = line.paint(
        point(px(origin_x), px(top)),
        px(14.0),
        TextAlign::Left,
        None,
        window,
        cx,
    );
}

fn mark_color(mark: Mark) -> Hsla {
    match mark {
        Mark::Start | Mark::Stop => theme::run(),
        Mark::Address => theme::accent(),
        Mark::Data => theme::hsla(gpui_kit::rgb(0x8a6cff)),
        Mark::Control => theme::faint(),
        Mark::Crc => theme::hsla(gpui_kit::rgb(0x2f9d80)),
        Mark::Ack => theme::run(),
        Mark::Nack | Mark::Error => theme::stop(),
    }
}

/// Labelled boxes under the screen, aligned with the traces, like a logic analyzer.
fn paint_lane(
    lane: Bounds<Pixels>,
    notes: &[Annotation],
    settings: &Settings,
    g: Geometry,
    window: &mut Window,
    cx: &mut App,
) {
    window.paint_quad(gpui_kit::quad(
        lane,
        px(4.0),
        theme::panel(),
        px(1.0),
        theme::border(),
        gpui_kit::BorderStyle::Solid,
    ));
    let (left, right) = (f32::from(lane.left()), f32::from(lane.right()));
    let (top, height) = (
        f32::from(lane.top()) + 3.0,
        f32::from(lane.size.height) - 6.0,
    );
    window.with_content_mask(Some(ContentMask { bounds: lane }), |window| {
        for (index, note) in notes.iter().enumerate() {
            let x0 = g.x(x_div_of(note.start_s, settings));
            let x1 = g.x(x_div_of(note.end_s, settings));
            if x1 < left || x0 > right {
                continue;
            }
            let color = mark_color(note.mark);
            if x1 - x0 < 3.0 {
                // A point event (I²C START/STOP/ACK): a narrow tick with its letter.
                let tick = Bounds::new(point(px(x0 - 1.0), px(top)), size(px(2.0), px(height)));
                window.paint_quad(fill(tick, color));
                // One-letter marks (I²C S, P, A, N) get their letter when it does not run into the
                // next annotation; otherwise the tick colour alone tells ACK (green) from NACK (red).
                let room = notes.get(index + 1).map_or(f32::INFINITY, |next| {
                    g.x(x_div_of(next.start_s, settings)) - x0
                });
                if note.text.chars().count() == 1 && room > 12.0 {
                    label_fit(window, cx, &note.text, x0 + 3.0, top + 2.0, 20.0, color);
                }
                continue;
            }
            let bounds = Bounds::new(
                point(px(x0 + 0.5), px(top)),
                size(px(x1 - x0 - 1.0), px(height)),
            );
            window.paint_quad(gpui_kit::quad(
                bounds,
                px(3.0),
                theme::with_alpha(color, 0.22),
                px(1.0),
                theme::with_alpha(color, 0.8),
                gpui_kit::BorderStyle::Solid,
            ));
            label_fit(
                window,
                cx,
                &note.text,
                x0 + 4.0,
                top + 2.0,
                x1 - x0 - 8.0,
                theme::text(),
            );
        }
    });
}

/// Paint `text` if it fits into `max_width`; else without its first word ("ID 7B3" → "7B3"), else
/// a shorter prefix with an ellipsis, else nothing.
fn label_fit(
    window: &mut Window,
    cx: &mut App,
    text: &str,
    x: f32,
    top: f32,
    max_width: f32,
    color: Hsla,
) {
    if max_width < 6.0 {
        return;
    }
    let shape = |window: &mut Window, text: &str| {
        let text = SharedString::from(text.to_owned());
        let run = TextRun {
            len: text.len(),
            font: font(theme::MONO),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window
            .text_system()
            .shape_line(text, px(10.5), &[run], None)
    };
    // Shorter forms in order: without the first word ("ID 7B3 R" → "7B3 R"), or the initial of a
    // single word ("ACK" → "A"); the last one is truncated with an ellipsis if still too long.
    let mut forms = vec![text.to_owned()];
    match text.split_once(' ') {
        Some((_, rest)) => forms.push(rest.to_owned()),
        None => forms.extend(text.chars().next().map(String::from)),
    }
    for form in &forms[..forms.len() - 1] {
        let line = shape(window, form);
        if f32::from(line.width) <= max_width {
            let _ = line.paint(
                point(px(x), px(top)),
                px(14.0),
                TextAlign::Left,
                None,
                window,
                cx,
            );
            return;
        }
    }
    let mut candidate = forms[forms.len() - 1].clone();
    loop {
        let line = shape(window, &candidate);
        if f32::from(line.width) <= max_width {
            let _ = line.paint(
                point(px(x), px(top)),
                px(14.0),
                TextAlign::Left,
                None,
                window,
                cx,
            );
            return;
        }
        let keep = candidate
            .trim_end_matches('…')
            .chars()
            .count()
            .saturating_sub(1);
        if keep < 1 {
            return;
        }
        candidate = candidate.chars().take(keep).collect::<String>() + "…";
    }
}
