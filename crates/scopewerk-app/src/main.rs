//! Scopewerk: the Hanmatek DOS1102 as a native app. Live screen, controls, measurements,
//! export, and a broker so agents (`scopewerk mcp`) and the CLI work through this window.
//!
//! `SCOPEWERK_BACKGROUND=1` opens a floating window without taking focus (for scripted
//! screenshots: typing elsewhere never reaches the shortcuts, and the window keeps rendering).
//! `SCOPEWERK_DEMO=1` shows a simulated CAN bus instead of a USB scope.

mod plot;
mod service;
mod theme;
mod view;

use std::path::PathBuf;

use gpui_kit::{
    App, AppContext as _, KeyBinding, Menu, MenuItem, SharedString, WindowBounds, WindowOptions,
    point, px, size,
};

use crate::view::ScopeView;

gpui_kit::actions!(scopewerk_app, [Quit, Hide]);

/// `~/<folder>/Scopewerk`, where exports and screenshots go.
pub(crate) fn export_dir(folder: &str) -> PathBuf {
    std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join(folder)
        .join("Scopewerk")
}

/// Local time for file names: `2026-10-29-174210`.
pub(crate) fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d-%H%M%S").to_string()
}

/// `SCOPEWERK_WINDOW=1600x1200` overrides the initial window size (presentations, screenshots).
fn window_size() -> (f32, f32) {
    std::env::var("SCOPEWERK_WINDOW")
        .ok()
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
        .unwrap_or((1400.0, 900.0))
}

fn main() {
    let background = std::env::var_os("SCOPEWERK_BACKGROUND").is_some_and(|v| !v.is_empty());
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                KeyBinding::new("cmd-h", Hide, None),
            ]);
            view::bind_keys(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.on_action(|_: &Hide, cx| cx.hide());
            cx.set_menus(vec![Menu {
                name: SharedString::from("Scopewerk"),
                items: vec![
                    MenuItem::action("Hide Scopewerk", Hide),
                    MenuItem::separator(),
                    MenuItem::action("Quit Scopewerk", Quit),
                ],
                disabled: false,
            }]);

            let (service, events, broker) = service::start();
            if let Err(err) = &broker {
                eprintln!("scopewerk: sharing with the CLI/MCP is off: {err}");
            }
            let (width, height) = window_size();
            let bounds = gpui_kit::Bounds::centered(None, size(px(width), px(height)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui_kit::TitlebarOptions {
                    title: Some("Scopewerk".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(14.0), px(15.0))),
                }),
                window_min_size: Some(size(px(980.0), px(620.0))),
                focus: !background,
                // Occluded windows stop rendering; a non-activating pop-up stays visible without focus.
                kind: if background {
                    gpui_kit::WindowKind::PopUp
                } else {
                    gpui_kit::WindowKind::Normal
                },
                app_id: Some("com.neuhaus.scopewerk".into()),
                ..Default::default()
            };
            let opened = gpui_kit::open_window(options, cx, move |window, cx| {
                let view = cx.new(|cx| ScopeView::new(service, events, broker, cx));
                if !background {
                    view.read(cx).focus_handle().focus(window, cx);
                }
                window.on_window_should_close(cx, |_, cx| {
                    cx.defer(|cx| cx.quit());
                    true
                });
                view
            });
            if let Err(err) = opened {
                eprintln!("scopewerk: could not open the window: {err:#}");
                cx.quit();
            }
            if !background {
                cx.activate(true);
            }
        });
}
