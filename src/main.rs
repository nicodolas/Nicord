//! Nicord — desktop Discord auto-sender (your own account).
//! Entry point: native eframe/egui window, no React / Electron / WebView.
//!
//! ⚠️  Self-bots are forbidden by Discord's Terms of Service and can result
//! in permanent account termination. Use a spare account. Nicord makes no
//! guarantee you will never be banned.

mod antiban;
mod app;
mod autostart;
mod crypto;
mod discord;
mod gateway;
mod scenario;
mod scheduler;
mod spintax;
mod store;
mod theme;
#[cfg(target_os = "windows")]
mod tray;

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

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1100.0, 700.0])
        .with_title("Nicord");
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
            // Discord-like dark theme + bundled Inter font.
            theme::apply_theme(&cc.egui_ctx);
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
}
