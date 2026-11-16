//! Nicord — desktop Discord auto-sender (your own account).
//! Entry point: native eframe/egui window, no React / Electron / WebView.
//!
//! ⚠️  Self-bots are forbidden by Discord's Terms of Service and can result
//! in permanent account termination. Use a spare account. Nicord makes no
//! guarantee you will never be banned.

mod antiban;
mod app;
mod crypto;
mod discord;
mod license;
mod scenario;
mod scheduler;
mod store;

use app::NicordApp;
use std::sync::Arc;

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

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 700.0])
            .with_title("Nicord"),
        ..Default::default()
    };

    eframe::run_native(
        "Nicord",
        options,
        Box::new(|_cc| Ok(Box::new(NicordApp::new(store)))),
    )
}
