//! Colours: a dark instrument panel, channel colours as on the DOS1102 (CH1 yellow, CH2 cyan).

use gpui_kit::{Hsla, Rgba, rgb, rgba};

pub(crate) fn hsla(color: Rgba) -> Hsla {
    color.into()
}

pub(crate) fn app_bg() -> Hsla {
    hsla(rgb(0x0d1014))
}
pub(crate) fn panel() -> Hsla {
    hsla(rgb(0x151a21))
}
pub(crate) fn panel_raised() -> Hsla {
    hsla(rgb(0x1c222b))
}
pub(crate) fn border() -> Hsla {
    hsla(rgb(0x262d37))
}
pub(crate) fn text() -> Hsla {
    hsla(rgb(0xe6eaf0))
}
pub(crate) fn muted() -> Hsla {
    hsla(rgb(0x8a94a3))
}
pub(crate) fn faint() -> Hsla {
    hsla(rgb(0x5a6372))
}
pub(crate) fn screen_bg() -> Hsla {
    hsla(rgb(0x06080b))
}
pub(crate) fn grid() -> Hsla {
    hsla(rgba(0xffffff26))
}
pub(crate) fn grid_axis() -> Hsla {
    hsla(rgba(0xffffff52))
}
pub(crate) fn run() -> Hsla {
    hsla(rgb(0x2bd576))
}
pub(crate) fn stop() -> Hsla {
    hsla(rgb(0xff5a5f))
}
pub(crate) fn trigger() -> Hsla {
    hsla(rgb(0xff9f1c))
}
pub(crate) fn accent() -> Hsla {
    hsla(rgb(0x5b8cff))
}
pub(crate) fn agent() -> Hsla {
    hsla(rgb(0xb18cff))
}
pub(crate) fn cursor() -> Hsla {
    hsla(rgba(0xffffffb0))
}

pub(crate) fn channel(channel: u8) -> Hsla {
    if channel == 2 {
        hsla(rgb(0x2ec8f0))
    } else {
        hsla(rgb(0xf5d90a))
    }
}

pub(crate) fn with_alpha(color: Hsla, alpha: f32) -> Hsla {
    Hsla {
        a: color.a * alpha,
        ..color
    }
}

/// A monospace face every installation of the platform has.
pub(crate) const MONO: &str = if cfg!(target_os = "macos") {
    "Menlo"
} else if cfg!(target_os = "windows") {
    "Consolas"
} else {
    "DejaVu Sans Mono"
};
