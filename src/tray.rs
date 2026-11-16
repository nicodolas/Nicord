//! System tray (Windows only).
//!
//! When enabled in Settings, closing the window hides Nicord to the
//! tray instead of quitting; the tray menu offers "Hiện Nicord" and
//! "Thoát", and a left-click (or double-click) on the icon shows the
//! window again. The icon is the same embedded PNG as the window icon
//! (assets/icon-256.b64), decoded at runtime.
//!
//! This module is only compiled on Windows (`#[cfg]` in main.rs): the
//! Linux CI build never sees it, and the release target is Windows.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuId, MenuItem},
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    None,
    Show,
    Quit,
}

pub struct Tray {
    /// Kept alive: dropping the TrayIcon removes it from the tray.
    _icon: TrayIcon,
    show_id: MenuId,
    quit_id: MenuId,
}

impl Tray {
    pub fn new() -> Option<Self> {
        let (rgba, width, height) = load_rgba()?;
        let icon = Icon::from_rgba(rgba, width, height).ok()?;
        let menu = Menu::new();
        let show = MenuItem::with_id("nicord-show", "Hiện Nicord", true, None);
        let quit = MenuItem::with_id("nicord-quit", "Thoát", true, None);
        menu.append(&show).ok()?;
        menu.append(&quit).ok()?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Nicord")
            .with_icon(icon)
            .build()
            .ok()?;
        Some(Self {
            _icon: tray,
            show_id: show.id().clone(),
            quit_id: quit.id().clone(),
        })
    }

    /// Drain pending tray/menu events (call once per UI frame).
    pub fn poll(&self) -> TrayAction {
        // Menu clicks (muda channel).
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.show_id {
                return TrayAction::Show;
            }
            if ev.id == self.quit_id {
                return TrayAction::Quit;
            }
        }
        // Icon clicks (tray-icon channel): left click or double click shows.
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            match ev {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
                | TrayIconEvent::DoubleClick { .. } => return TrayAction::Show,
                _ => {}
            }
        }
        TrayAction::None
    }
}

/// Decode the embedded window-icon PNG to raw RGBA.
fn load_rgba() -> Option<(Vec<u8>, u32, u32)> {
    let bytes = B64.decode(include_str!("../assets/icon-256.b64")).ok()?;
    let image = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (width, height) = image.dimensions();
    Some((image.into_raw(), width, height))
}
