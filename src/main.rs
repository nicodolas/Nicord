//! Nicord — desktop Discord scheduled sender (webhook or your own account).
//! Entry point: native eframe/egui window, no React / Electron / WebView.
//!
//! ⚠️  The account (self-bot) mode is forbidden by Discord's Terms of
//! Service and can result in permanent account termination. Use a spare
//! account. Nicord makes no guarantee you will never be banned. Webhook
//! mode is the official, recommended alternative.

mod antiban;
mod app;
mod autostart;
mod backup;
mod crypto;
mod destination;
mod discord;
mod embed;
mod gateway;
mod health;
mod i18n;
mod notify;
mod poll;
mod preview;
mod scenario;
mod scheduler;
mod secret_store;
mod spintax;
mod store;
mod theme;
mod timeline;
#[cfg(target_os = "windows")]
mod tray;
mod updater;
mod webhook;

use app::NicordApp;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use std::sync::Arc;

/// Window / taskbar icon, decoded from the PNG embedded at build time.
/// The PNG is stored as base64 text (assets/icon-256.b64, generated from
/// assets/icon.svg) so the build stays self-contained in one text file.
/// Returns `None` (plain default icon) instead of crashing if decoding fails.
fn load_icon() -> Option<egui::IconData> {
    let bytes = B64.decode(include_str!("../assets/icon-256.b64")).ok()?;
    let image = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (width, height) = image.dimensions();
    Some(egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    })
}

/// Default window size when no geometry was remembered.
const DEFAULT_WINDOW_SIZE: [f32; 2] = [1100.0, 700.0];

/// Clamp a remembered window geometry to sane bounds. Returns None
/// when nothing was remembered (all zeros) or the size is unusable,
/// in which case the caller uses the default size and lets the OS
/// pick the position. Sizes clamp to [640..3840] x [480..2160];
/// positions are kept as-is (multi-monitor setups legitimately use
/// negative coordinates) except non-finite values, which drop the
/// position only.
fn clamp_window_geometry(
    width: f32,
    height: f32,
    x: f32,
    y: f32,
) -> Option<([f32; 2], Option<[f32; 2]>)> {
    if width <= 0.0 || height <= 0.0 || !width.is_finite() || !height.is_finite() {
        return None;
    }
    let size = [width.clamp(640.0, 3840.0), height.clamp(480.0, 2160.0)];
    let pos = if x.is_finite() && y.is_finite() {
        Some([x, y])
    } else {
        None
    };
    Some((size, pos))
}

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("nicord=info")),
        )
        .init();

    let store = Arc::new(store::Store::open().unwrap_or_else(|e| {
        eprintln!("cannot open local store: {e}");
        std::process::exit(1);
    }));

    // Remembered window geometry (saved on exit / hide-to-tray).
    // NicordApp::new loads the config again; reading the small
    // JSON a few times at startup is harmless.
    let saved = store::load_config();
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(DEFAULT_WINDOW_SIZE)
        .with_title("Nicord");
    if let Some((size, pos)) = clamp_window_geometry(
        saved.window_width,
        saved.window_height,
        saved.window_x,
        saved.window_y,
    ) {
        viewport = viewport.with_inner_size(size);
        if let Some(pos) = pos {
            viewport = viewport.with_position(pos);
        }
    }
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Nicord",
        options,
        Box::new(|cc| {
            // Saved theme (dark default) + UI scale + bundled Inter font.
            let saved = store::load_config();
            theme::apply_theme(
                &cc.egui_ctx,
                theme::ThemeChoice::from_code(&saved.theme),
                theme::UiScale::from_code(&saved.ui_scale),
            );
            Ok(Box::new(NicordApp::new(store)))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_icon_decodes() {
        let icon = load_icon().expect("assets/icon-256.b64 must decode");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }

    #[test]
    fn clamp_window_geometry_rules() {
        // Unset (all zeros) and unusable sizes: no restore.
        assert_eq!(clamp_window_geometry(0.0, 0.0, 0.0, 0.0), None);
        assert_eq!(clamp_window_geometry(800.0, 0.0, 0.0, 0.0), None);
        assert_eq!(clamp_window_geometry(f32::NAN, 600.0, 0.0, 0.0), None);
        assert_eq!(clamp_window_geometry(800.0, f32::INFINITY, 0.0, 0.0), None);
        // Normal values pass through untouched.
        assert_eq!(
            clamp_window_geometry(1200.0, 800.0, 100.0, 50.0),
            Some(([1200.0, 800.0], Some([100.0, 50.0])))
        );
        // Sizes clamp; negative positions (multi-monitor) are kept.
        assert_eq!(
            clamp_window_geometry(100.0, 5000.0, -1920.0, 0.0),
            Some(([640.0, 2160.0], Some([-1920.0, 0.0])))
        );
        // Non-finite position drops the position only.
        assert_eq!(
            clamp_window_geometry(900.0, 600.0, f32::NAN, 0.0),
            Some(([900.0, 600.0], None))
        );
    }
}
