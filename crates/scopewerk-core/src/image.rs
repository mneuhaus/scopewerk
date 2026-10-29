//! PNG output: the scope's own screenshot (BMP → PNG) and a rendered plot of a [`Capture`],
//! drawn like a scope screen (15.2 × 8 divisions, trigger at the centre).

use crate::Error;
use crate::header::{HORIZONTAL_DIVS, VERTICAL_DIVS, to_f64};
use crate::waveform::Capture;

type Rgb = [u8; 3];

pub const CH1_COLOR: Rgb = [0xF5, 0xD9, 0x0A];
pub const CH2_COLOR: Rgb = [0x2E, 0xC8, 0xF0];
const BACKGROUND: Rgb = [0x0B, 0x0E, 0x12];
const GRID: Rgb = [0x26, 0x2C, 0x34];
const AXIS: Rgb = [0x4A, 0x53, 0x5E];

pub fn channel_color(channel: u8) -> Rgb {
    if channel == 2 { CH2_COLOR } else { CH1_COLOR }
}

/// Convert the scope's 24- or 32-bit BMP screenshot to PNG.
pub fn bmp_to_png(bmp: &[u8]) -> Result<Vec<u8>, Error> {
    let field = |at: usize, len: usize| -> Result<&[u8], Error> {
        bmp.get(at..at + len)
            .ok_or_else(|| Error::Protocol("truncated BMP".into()))
    };
    let u32_at = |at| field(at, 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let pixels_at = usize::try_from(u32_at(10)?).unwrap_or(usize::MAX);
    let width = i32::from_le_bytes(field(18, 4)?.try_into().unwrap_or_default());
    let height = i32::from_le_bytes(field(22, 4)?.try_into().unwrap_or_default());
    let bits = u16::from_le_bytes(field(28, 2)?.try_into().unwrap_or_default());
    let bytes_per_pixel = match bits {
        24 => 3,
        32 => 4,
        other => return Err(Error::Protocol(format!("{other}-bit BMP is not supported"))),
    };
    let width_px = usize::try_from(width.unsigned_abs()).unwrap_or(0);
    let height_px = usize::try_from(height.unsigned_abs()).unwrap_or(0);
    let stride = (width_px * bytes_per_pixel).next_multiple_of(4);
    let mut rgb = Vec::with_capacity(width_px * height_px * 3);
    for row in 0..height_px {
        // Positive height means the rows are stored bottom-up.
        let source_row = if height > 0 { height_px - 1 - row } else { row };
        let start = pixels_at + source_row * stride;
        let line = bmp
            .get(start..start + width_px * bytes_per_pixel)
            .ok_or_else(|| Error::Protocol("truncated BMP pixels".into()))?;
        for pixel in line.chunks_exact(bytes_per_pixel) {
            rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
    }
    encode_png(&rgb, width_px, height_px)
}

/// Render a capture as a scope-style PNG (min/max per pixel column, so 10 000-point memory
/// captures keep their peaks).
pub fn plot_png(capture: &Capture, width: usize, height: usize) -> Result<Vec<u8>, Error> {
    let mut canvas = Canvas::new(width, height);
    canvas.grid();

    let points = capture.points();
    if points >= 2 {
        let span = to_f64(points - 1) * capture.dt;
        let timebase = capture.settings.timebase_s_per_div;
        // Memory captures span more than the screen; keep the scope's scale at the centre.
        let divs_shown = (span / timebase).max(HORIZONTAL_DIVS);
        let t_centre = capture.t0 + span / 2.0;
        for waveform in &capture.waveforms {
            let Some(channel) = capture.settings.channel(waveform.channel) else {
                continue;
            };
            let color = channel_color(waveform.channel);
            let mut previous: Option<(i64, i64)> = None;
            let mut column: Option<(i64, f64, f64)> = None;
            let mut flush = |canvas: &mut Canvas, x: i64, low: f64, high: f64| {
                let (y_low, y_high) = (canvas.y_of(low), canvas.y_of(high));
                if let Some((px, py)) = previous {
                    canvas.line(px, py, x, (y_low + y_high) / 2, color);
                }
                canvas.line(x, y_low, x, y_high, color);
                previous = Some((x, (y_low + y_high) / 2));
            };
            for (index, volts) in waveform.volts.iter().enumerate() {
                let t = capture.t0 + to_f64(index) * capture.dt;
                let x_div = (t - t_centre) / timebase;
                let x = canvas.x_of(x_div / divs_shown * HORIZONTAL_DIVS);
                let y_div = f64::from(*volts) / channel.volts_per_div + channel.offset_div;
                column = match column {
                    Some((cx, low, high)) if cx == x => Some((cx, low.min(y_div), high.max(y_div))),
                    Some((cx, low, high)) => {
                        flush(&mut canvas, cx, low, high);
                        Some((x, y_div, y_div))
                    }
                    None => Some((x, y_div, y_div)),
                };
            }
            if let Some((cx, low, high)) = column {
                flush(&mut canvas, cx, low, high);
            }
        }
    }
    encode_png(&canvas.rgb, width, height)
}

struct Canvas {
    rgb: Vec<u8>,
    width: usize,
    height: usize,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        let mut rgb = Vec::with_capacity(width * height * 3);
        for _ in 0..width * height {
            rgb.extend_from_slice(&BACKGROUND);
        }
        Self { rgb, width, height }
    }

    /// Pixel column of a horizontal position in divisions from the centre.
    fn x_of(&self, div: f64) -> i64 {
        let w = to_f64(self.width - 1);
        round(w / 2.0 + div / HORIZONTAL_DIVS * w)
    }

    /// Pixel row of a vertical position in divisions from the centre (up is positive).
    fn y_of(&self, div: f64) -> i64 {
        let h = to_f64(self.height - 1);
        round(h / 2.0 - div / VERTICAL_DIVS * h)
    }

    fn grid(&mut self) {
        let half_h = HORIZONTAL_DIVS / 2.0;
        let mut div = -half_h.floor();
        while div <= half_h {
            let x = self.x_of(div);
            let color = if div == 0.0 { AXIS } else { GRID };
            self.dotted_line(x, 0, x, to_i64(self.height) - 1, color, div == 0.0);
            div += 1.0;
        }
        for div in -4..=4 {
            let y = self.y_of(f64::from(div));
            let color = if div == 0 { AXIS } else { GRID };
            self.dotted_line(0, y, to_i64(self.width) - 1, y, color, div == 0);
        }
    }

    fn dotted_line(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, color: Rgb, solid: bool) {
        let mut step = 0;
        self.walk(x0, y0, x1, y1, |canvas, x, y| {
            if solid || step % 4 == 0 {
                canvas.set(x, y, color);
            }
            step += 1;
        });
    }

    fn line(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, color: Rgb) {
        self.walk(x0, y0, x1, y1, |canvas, x, y| {
            canvas.set(x, y, color);
            canvas.set(x, y + 1, color);
        });
    }

    /// Bresenham.
    fn walk(
        &mut self,
        x0: i64,
        y0: i64,
        x1: i64,
        y1: i64,
        mut plot: impl FnMut(&mut Self, i64, i64),
    ) {
        let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let (mut x, mut y, mut err) = (x0, y0, dx + dy);
        loop {
            plot(self, x, y);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    fn set(&mut self, x: i64, y: i64, color: Rgb) {
        let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
            return;
        };
        if x < self.width && y < self.height {
            let at = (y * self.width + x) * 3;
            self.rgb[at..at + 3].copy_from_slice(&color);
        }
    }
}

fn round(value: f64) -> i64 {
    if value.is_finite() {
        // Clamp first so the conversion cannot overflow.
        #[expect(clippy::cast_possible_truncation, reason = "clamped to a pixel range")]
        let pixel = value.clamp(-1e6, 1e6).round() as i64;
        pixel
    } else {
        -1
    }
}

fn to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn encode_png(rgb: &[u8], width: usize, height: usize) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let to_u32 =
        |v: usize| u32::try_from(v).map_err(|_| Error::InvalidArgument("image too large".into()));
    let mut encoder = png::Encoder::new(&mut out, to_u32(width)?, to_u32(height)?);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|err| Error::Protocol(format!("png: {err}")))?;
    writer
        .write_image_data(rgb)
        .map_err(|err| Error::Protocol(format!("png: {err}")))?;
    writer
        .finish()
        .map_err(|err| Error::Protocol(format!("png: {err}")))?;
    Ok(out)
}
