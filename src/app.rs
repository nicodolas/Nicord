//! Native egui UI (eframe, immediate-mode — no React / WebView).
//! Screens: Onboarding (ToS warning → Token) → Dashboard →
//! Builder (list-based, add/remove/move up-down) → Logs → Settings.
//! Light by design: simple widgets, no canvas, no animations.
//!
//! Layout: left sidebar navigation (brand, nav, today status, Emergency
//! Stop) + central content with a header and a status banner. Discord-like
//! dark theme lives in `crate::theme`.
//!
//! One simple app for everyone: 1 channel, 300 messages/day
//! (cap resets 00:00 UTC). No tiers.

use crate::{
    crypto::{decrypt_token, encrypt_token, mask_token},
    discord::{Channel, DiscordClient, DiscordError, Guild, Me},
    scenario::{Node, NodeKind, Scenario},
    scheduler::{channels_violation, Engine},
    store::{config_dir, load_config, save_config, AppConfig, Store},
    theme,
};
use egui::RichText;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Onboarding,
    Dashboard,
    Builder,
    Logs,
    Settings,
}

pub struct NicordApp {
    pub store: Arc<Store>,
    pub engine: Arc<Engine>,
    pub scenarios: Arc<Mutex<Vec<Scenario>>>,
    pub config: AppConfig,
    pub screen: Screen,
    pub status: Arc<Mutex<String>>,
    pub tos_checked: bool,
    pub token_input: String,
    pub new_scenario_name: String,
    pub new_message: String,
    pub selected: Option<usize>,
    pub sent_today: u32,
    pub guilds: Arc<Mutex<Vec<Guild>>>,
    pub channels: Arc<Mutex<Vec<Channel>>>,
    pub picker_guild_id: String,
    pub log_filter: String,
    pub import_path: String,
    /// Result of the last "Kiểm tra token" (Token Validator).
    pub token_info: Arc<Mutex<Option<Me>>>,
    /// System tray icon (Windows only, when enabled in Settings).
    #[cfg(target_os = "windows")]
    pub tray: Option<crate::tray::Tray>,
    /// Set by the tray "Thoát" menu item: the next close is a real quit.
    #[cfg(target_os = "windows")]
    pub allow_quit: bool,
}

impl NicordApp {
    pub fn new(store: Arc<Store>) -> Self {
        let mut config = load_config();
        // The autostart toggle shows the real OS state, not just the
        // last saved flag (the user may have changed it outside Nicord).
        config.autostart = crate::autostart::is_enabled();
        // Read before the struct literal below moves `config`.
        #[cfg(target_os = "windows")]
        let tray_enabled = config.tray_enabled;
        let scenarios = store.load_scenarios().unwrap_or_default();
        let screen = if !config.tos_accepted {
            Screen::Onboarding
        } else {
            Screen::Dashboard
        };
        let engine = Arc::new(Engine::new(store.clone(), config.antiban.clone()));
        let has_enabled = scenarios.iter().any(|s| s.enabled);
        let app = Self {
            store,
            engine,
            scenarios: Arc::new(Mutex::new(scenarios)),
            config,
            screen,
            status: Arc::new(Mutex::new(String::new())),
            tos_checked: false,
            token_input: String::new(),
            new_scenario_name: String::new(),
            new_message: String::new(),
            selected: None,
            sent_today: 0,
            guilds: Arc::new(Mutex::new(Vec::new())),
            channels: Arc::new(Mutex::new(Vec::new())),
            picker_guild_id: String::new(),
            log_filter: String::new(),
            import_path: String::new(),
            token_info: Arc::new(Mutex::new(None)),
            #[cfg(target_os = "windows")]
            tray: if tray_enabled {
                crate::tray::Tray::new()
            } else {
                None
            },
            #[cfg(target_os = "windows")]
            allow_quit: false,
        };
        // Auto-resume: enabled scenarios keep running across restarts.
        // The engine is started here (not only from the Dashboard button)
        // when there is anything enabled and a token is stored.
        if has_enabled && !app.config.encrypted_token.is_empty() && app.config.tos_accepted {
            app.start_engine();
            app.set_status("Resumed enabled scenarios from last session.");
        }
        app
    }

    fn set_status(&self, msg: &str) {
        if let Ok(mut s) = self.status.lock() {
            *s = msg.to_string();
        }
    }

    fn status_text(&self) -> String {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn save_scenarios(&self) {
        if let Ok(list) = self.scenarios.lock() {
            for s in list.iter() {
                let _ = self.store.save_scenario(s);
            }
        }
    }

    /// Spawn a background thread that sends one test message for the
    /// selected (or first) scenario, using the stored encrypted token.
    fn test_send(&self) {
        let token = match decrypt_token(&self.config.encrypted_token) {
            Ok(t) => t,
            Err(e) => {
                self.set_status(&format!("No valid token stored: {e}"));
                return;
            }
        };
        let scenarios = self.scenarios.clone();
        let engine = self.engine.clone();
        let status = self.status.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    if let Ok(mut s) = status.lock() {
                        *s = format!("cannot start runtime: {e}");
                    }
                    return;
                }
            };
            let scenario = {
                let list = scenarios.lock().unwrap();
                list.first().cloned()
            };
            let Some(scenario) = scenario else {
                if let Ok(mut s) = status.lock() {
                    *s = "No scenario yet — create one first.".to_string();
                }
                return;
            };
            let client = DiscordClient::new(&token);
            // Real identity for template vars (fallback only on failure).
            let (username, server_name) = rt.block_on(async {
                let u = client
                    .get_me_typed()
                    .await
                    .map(|m| m.display_name())
                    .unwrap_or_else(|_| "you".to_string());
                let s = client
                    .list_guilds()
                    .await
                    .ok()
                    .and_then(|g| g.first().map(|x| x.name.clone()))
                    .unwrap_or_else(|| "server".to_string());
                (u, s)
            });
            let result = rt.block_on(engine.send_once(&scenario, &client, &username, &server_name));
            if let Ok(mut s) = status.lock() {
                *s = match result {
                    Ok(id) => format!("Test sent! message id {id}"),
                    Err(e) => format!("Test failed: {e}"),
                };
            }
        });
    }

    /// Token Validator (Discord-BLUE): check the stored token with
    /// `GET /users/@me` in a background thread and show the account
    /// (username, id, email, 2FA, Nitro). The token itself is never shown.
    fn check_token(&self) {
        let token = match decrypt_token(&self.config.encrypted_token) {
            Ok(t) => t,
            Err(e) => {
                self.set_status(&format!("No valid token stored: {e}"));
                return;
            }
        };
        let info = self.token_info.clone();
        let status = self.status.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    if let Ok(mut s) = status.lock() {
                        *s = format!("cannot start runtime: {e}");
                    }
                    return;
                }
            };
            let client = DiscordClient::new(&token);
            match rt.block_on(client.get_me_typed()) {
                Ok(me) => {
                    if let Ok(mut i) = info.lock() {
                        *i = Some(me);
                    }
                    if let Ok(mut s) = status.lock() {
                        *s = "Token hợp lệ.".to_string();
                    }
                }
                Err(e) => {
                    if let Ok(mut i) = info.lock() {
                        *i = None;
                    }
                    if let Ok(mut s) = status.lock() {
                        *s = match e {
                            DiscordError::Unauthorized(_) => {
                                "Token không hợp lệ hoặc đã hết hạn.".to_string()
                            }
                            _ => format!("Không kiểm tra được token: {e}"),
                        };
                    }
                }
            }
        });
    }

    /// Build or drop the tray icon to match the Settings toggle
    /// (Windows only).
    #[cfg(target_os = "windows")]
    fn sync_tray(&mut self) {
        if self.config.tray_enabled && self.tray.is_none() {
            self.tray = crate::tray::Tray::new();
            if self.tray.is_none() {
                self.set_status("Could not create the tray icon.");
            }
        } else if !self.config.tray_enabled {
            self.tray = None;
        }
    }

    fn start_engine(&self) {
        self.engine.resume();
        let engine = self.engine.clone();
        let scenarios = self.scenarios.clone();
        let config = self.config.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(_) => return,
            };
            let engine2 = engine.clone();
            let scenarios2 = scenarios.clone();
            rt.block_on(async move {
                let get_client = move || {
                    decrypt_token(&config.encrypted_token)
                        .ok()
                        .map(|t| DiscordClient::new(&t))
                };
                let scenarios_fn = move || scenarios2.lock().map(|l| l.clone()).unwrap_or_default();
                engine2.run_loop(get_client, scenarios_fn).await;
            });
        });
        self.set_status("Engine started — running in background.");
    }

    /// Load the account's servers in a background thread (channel picker).
    fn load_guilds(&self) {
        let token = match decrypt_token(&self.config.encrypted_token) {
            Ok(t) => t,
            Err(e) => {
                self.set_status(&format!("No valid token stored: {e}"));
                return;
            }
        };
        let guilds = self.guilds.clone();
        let status = self.status.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(_) => return,
            };
            let client = DiscordClient::new(&token);
            match rt.block_on(client.list_guilds()) {
                Ok(list) => {
                    let n = list.len();
                    if let Ok(mut g) = guilds.lock() {
                        *g = list;
                    }
                    if let Ok(mut s) = status.lock() {
                        *s = format!("Loaded {n} servers — pick one below.");
                    }
                }
                Err(e) => {
                    if let Ok(mut s) = status.lock() {
                        *s = format!("Cannot list servers: {e}");
                    }
                }
            }
        });
    }

    /// Load text channels of one server in a background thread.
    fn load_channels(&self, guild_id: String) {
        let token = match decrypt_token(&self.config.encrypted_token) {
            Ok(t) => t,
            Err(e) => {
                self.set_status(&format!("No valid token stored: {e}"));
                return;
            }
        };
        let channels = self.channels.clone();
        let status = self.status.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(_) => return,
            };
            let client = DiscordClient::new(&token);
            match rt.block_on(client.list_channels(&guild_id)) {
                Ok(list) => {
                    let text: Vec<Channel> = list.into_iter().filter(|c| c.is_text()).collect();
                    let n = text.len();
                    if let Ok(mut c) = channels.lock() {
                        *c = text;
                    }
                    if let Ok(mut s) = status.lock() {
                        *s = format!("Loaded {n} text channels — pick one below.");
                    }
                }
                Err(e) => {
                    if let Ok(mut s) = status.lock() {
                        *s = format!("Cannot list channels: {e}");
                    }
                }
            }
        });
    }
}

impl eframe::App for NicordApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // "Sent today" is the persisted count, not a memory counter.
        self.sent_today = self.store.count_sends_today();

        // System tray (Windows only): closing the window hides Nicord
        // to the tray instead of quitting; the tray menu can show it
        // again or quit for real.
        #[cfg(target_os = "windows")]
        {
            if ctx.input(|i| i.viewport().close_requested())
                && self.tray.is_some()
                && !self.allow_quit
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
            if let Some(tray) = &self.tray {
                match tray.poll() {
                    crate::tray::TrayAction::Show => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    crate::tray::TrayAction::Quit => {
                        self.allow_quit = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    crate::tray::TrayAction::None => {}
                }
                // Keep polling the tray while the window is hidden:
                // hidden windows get no repaint on their own.
                ctx.request_repaint_after(std::time::Duration::from_millis(500));
            }
        }

        // Left sidebar: brand, navigation, today status, Emergency Stop.
        // It is on every screen, so Emergency Stop is always one click away.
        egui::SidePanel::left("sidebar")
            .exact_width(208.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::SIDEBAR_BG)
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(ctx, |ui| {
                self.show_sidebar(ui);
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin {
                        left: 18,
                        right: 18,
                        top: 14,
                        bottom: 14,
                    }),
            )
            .show(ctx, |ui| {
                // Header: screen title + the always-visible limit.
                if self.screen != Screen::Onboarding {
                    let (title, subtitle) = match self.screen {
                        Screen::Onboarding => ("", ""),
                        Screen::Dashboard => ("Dashboard", "Tổng quan"),
                        Screen::Builder => ("Scenario Builder", "Tạo và chỉnh sửa kịch bản"),
                        Screen::Logs => ("Logs", "Lịch sử gửi tin"),
                        Screen::Settings => ("Settings", "Cài đặt"),
                    };
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.heading(title);
                            ui.label(RichText::new(subtitle).small().color(theme::TEXT_MUTED));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new("1 channel • 300 messages/day • resets 00:00 UTC")
                                    .small()
                                    .color(theme::TEXT_MUTED),
                            );
                        });
                    });
                    ui.add_space(6.0);
                }
                // Status banner (same message as before, now in a card).
                let status = self.status_text();
                if !status.is_empty() {
                    egui::Frame::new()
                        .fill(theme::CARD)
                        .stroke(egui::Stroke::new(1.0_f32, theme::WARNING))
                        .corner_radius(8)
                        .inner_margin(10)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(RichText::new(&status).color(theme::WARNING));
                        });
                    ui.add_space(10.0);
                }
                match self.screen {
                    Screen::Onboarding => self.show_onboarding(ui),
                    Screen::Dashboard => self.show_dashboard(ui),
                    Screen::Builder => self.show_builder(ui),
                    Screen::Logs => self.show_logs(ui),
                    Screen::Settings => self.show_settings(ui),
                }
            });
        // Keep the engine ticking / status fresh.
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }
}

impl NicordApp {
    /// Left sidebar: brand, nav, today status card, Emergency Stop.
    fn show_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(theme::semibold("Nicord", 22.0).color(theme::TEXT));
            ui.label(RichText::new("•").size(22.0).color(theme::ACCENT_LIGHT));
        });
        ui.label(
            RichText::new("Discord auto-sender")
                .small()
                .color(theme::TEXT_MUTED),
        );
        ui.add_space(12.0);
        if self.screen != Screen::Onboarding {
            for (label, screen) in [
                ("Dashboard", Screen::Dashboard),
                ("Builder", Screen::Builder),
                ("Logs", Screen::Logs),
                ("Settings", Screen::Settings),
            ] {
                if theme::nav_button(ui, label, self.screen == screen).clicked() {
                    self.screen = screen;
                }
                ui.add_space(4.0);
            }
        } else {
            ui.label(
                RichText::new("Hoàn tất thiết lập để tiếp tục")
                    .small()
                    .color(theme::TEXT_MUTED),
            );
        }
        // Bottom of the sidebar: today status, then Emergency Stop last.
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            if ui
                .add_sized(
                    [ui.available_width(), 40.0],
                    egui::Button::new(RichText::new("Emergency Stop").color(theme::WHITE))
                        .fill(theme::DANGER),
                )
                .clicked()
            {
                self.engine.stop_all();
                self.set_status("EMERGENCY STOP — all queues halted.");
            }
            ui.add_space(8.0);
            theme::card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                // The footer uses a bottom-up layout, so force the card's
                // own contents back to top-down (otherwise "Today" would
                // render at the bottom of the card).
                ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                    ui.label(theme::semibold("Today", 14.0));
                    ui.label(
                        RichText::new(format!("Sent today: {} / 300", self.sent_today)).small(),
                    );
                    // Hand-drawn bar: the stock ProgressBar shows a blue
                    // pill even at 0/300 (min-width fill).
                    theme::progress_bar(
                        ui,
                        self.sent_today as f32 / 300.0,
                        &format!("{} / 300", self.sent_today),
                    );
                    theme::muted(ui, "Resets 00:00 UTC");
                    ui.add_space(6.0);
                    let running = self
                        .engine
                        .running
                        .load(std::sync::atomic::Ordering::SeqCst);
                    ui.horizontal(|ui| {
                        ui.label(theme::dot(if running {
                            theme::SUCCESS
                        } else {
                            theme::TEXT_MUTED
                        }));
                        ui.label(
                            RichText::new(if running {
                                "Engine running"
                            } else {
                                "Engine stopped"
                            })
                            .small(),
                        );
                    });
                    let count = self.scenarios.lock().map(|l| l.len()).unwrap_or(0);
                    theme::muted(ui, &format!("Scenarios: {count}"));
                    // Next scheduled run across enabled scenarios — fills
                    // the card with something useful instead of dead space.
                    let next = self.scenarios.lock().map(|l| {
                        l.iter()
                            .filter(|s| s.enabled)
                            .filter_map(|s| {
                                s.schedule
                                    .as_ref()
                                    .map(|sc| sc.next_after(chrono::Local::now()))
                            })
                            .min()
                    });
                    if let Ok(Some(t)) = next {
                        theme::muted(ui, &format!("Next run: {}", t.format("%d/%m %H:%M")));
                    }
                });
            });
        });
    }

    fn show_onboarding(&mut self, ui: &mut egui::Ui) {
        // Center the card vertically: the panel is ~640 px tall and the
        // card ~470 px, so a top spacer balances the empty space below it.
        let top = ((ui.available_height() - 470.0) / 2.0).max(8.0);
        ui.add_space(top);
        ui.vertical_centered(|ui| {
            theme::card_frame().show(ui, |ui| {
                ui.set_max_width(620.0);
                ui.heading("Welcome to Nicord");
                theme::muted(ui, "Gửi tin nhắn Discord tự động, theo lịch của bạn");
                ui.add_space(10.0);
                // Red ToS warning (kept verbatim, now in a red-bordered card).
                theme::danger_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new("WARNING — READ FIRST")
                            .color(theme::DANGER)
                            .text_style(egui::TextStyle::Name("SectionTitle".into())),
                    );
                    ui.label(
                        "Automating a normal Discord user account (self-bot) is forbidden \
                         by Discord's Terms of Service and can result in PERMANENT account \
                         termination. Nicord makes no guarantee you will never be banned. \
                         Use a SPARE account, never your main account.",
                    );
                });
                ui.add_space(10.0);
                ui.checkbox(
                    &mut self.tos_checked,
                    "I understand the risk and accept responsibility for my own account.",
                );
                ui.add_space(10.0);
                theme::section_title(ui, "Step 1 — Your Discord token");
                ui.label("Paste once, stored encrypted locally.");
                ui.label("Get it yourself from Discord like a developer token. Never share it with anyone — not even Nicord support.");
                if !self.config.token_last4.is_empty() {
                    // token_last4 holds ONLY the last 4 chars (never the full token);
                    // mask_token shows at most those 4, so even an old config that
                    // still contains a full token can never leak it on screen.
                    ui.label(format!(
                        "Token stored: {}",
                        mask_token(&self.config.token_last4)
                    ));
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.token_input)
                            .password(true)
                            .hint_text("paste your Discord token here"),
                    );
                    if theme::primary_button(ui, "Save token").clicked() {
                        if self.token_input.trim().is_empty() {
                            self.set_status("Token is empty.");
                        } else {
                            match encrypt_token(self.token_input.trim()) {
                                Ok(enc) => {
                                    // SECURITY: store ONLY the last 4 chars for display.
                                    // The full token must never be written to config.json
                                    // in plaintext (Spec §9).
                                    let tok = self.token_input.trim();
                                    self.config.token_last4 = tok
                                        .chars()
                                        .rev()
                                        .take(4)
                                        .collect::<Vec<_>>()
                                        .into_iter()
                                        .rev()
                                        .collect();
                                    self.config.encrypted_token = enc;
                                    self.config.tos_accepted = self.tos_checked;
                                    if save_config(&self.config).is_ok() {
                                        self.token_input.clear();
                                        self.screen = Screen::Dashboard;
                                        self.set_status("Token saved (encrypted, local only).");
                                    } else {
                                        self.set_status("Failed to save config.");
                                    }
                                }
                                Err(e) => self.set_status(&format!("Cannot encrypt token: {e}")),
                            }
                        }
                    }
                });
                ui.add_space(10.0);
                if theme::ghost_button(ui, "Skip for now -> Dashboard").clicked() {
                    self.config.tos_accepted = self.tos_checked;
                    let _ = save_config(&self.config);
                    self.screen = Screen::Dashboard;
                }
            });
        });
    }

    fn show_dashboard(&mut self, ui: &mut egui::Ui) {
        // Stat cards: scenarios, running, sent today.
        let count = self.scenarios.lock().map(|l| l.len()).unwrap_or(0);
        let running = self
            .engine
            .running
            .load(std::sync::atomic::Ordering::SeqCst);
        let next_run = self.scenarios.lock().map(|l| {
            l.iter()
                .filter_map(|s| {
                    s.schedule
                        .as_ref()
                        .map(|sc| sc.next_after(chrono::Local::now()))
                })
                .min()
        });
        let next_text = match next_run {
            Ok(Some(t)) => t.format("%d/%m %H:%M").to_string(),
            _ => "—".to_string(),
        };
        // Group B stats: success rate and send rate need the session
        // counters from the engine (failures and deletes are session-only;
        // sent today comes from the persisted log).
        let failed = self.engine.failed_count();
        let deleted = self.engine.deleted_count();
        let total = self.sent_today + failed;
        let success_pct = if total == 0 {
            None
        } else {
            Some(self.sent_today as f64 * 100.0 / total as f64)
        };
        let elapsed_min = self.engine.started_at.elapsed().as_secs_f64() / 60.0;
        let rate_per_min = if elapsed_min > 0.0 {
            self.sent_today as f64 / elapsed_min
        } else {
            0.0
        };
        ui.columns(3, |cols| {
            theme::stat_card(&mut cols[0], "Scenarios", &count.to_string());
            theme::stat_card(
                &mut cols[1],
                "Engine",
                if running { "Running" } else { "Stopped" },
            );
            theme::stat_card(
                &mut cols[2],
                "Sent today (limit 300)",
                &format!("{} / 300", self.sent_today),
            );
        });
        ui.add_space(4.0);
        ui.columns(3, |cols| {
            theme::stat_card(
                &mut cols[0],
                "Success rate",
                &success_pct
                    .map(|p| format!("{p:.0}%"))
                    .unwrap_or_else(|| "—".to_string()),
            );
            theme::stat_card(
                &mut cols[1],
                "Send rate",
                &format!("{rate_per_min:.1}/phút"),
            );
            theme::stat_card(&mut cols[2], "Deleted (this session)", &deleted.to_string());
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(theme::dot(theme::TEXT_MUTED));
            ui.label(
                RichText::new(format!("Next run: {next_text}"))
                    .small()
                    .color(theme::TEXT_MUTED),
            );
        });
        ui.add_space(12.0);
        // Actions.
        theme::card(ui, |ui| {
            theme::section_title(ui, "Actions");
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "Start engine").clicked() {
                    self.start_engine();
                }
                if ui.button("Stop engine").clicked() {
                    self.engine.stop_all();
                    self.set_status("Engine stopped.");
                }
                if ui.button("Test send (sends a REAL message)").clicked() {
                    self.test_send();
                }
                if ui.button("Minimise (engine keeps running)").clicked() {
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
                if ui.button("Export thống kê (.json)").clicked() {
                    let stats = serde_json::json!({
                        "sent_today": self.sent_today,
                        "failed_this_session": failed,
                        "deleted_this_session": deleted,
                        "success_rate_pct": success_pct,
                        "sends_per_min": rate_per_min,
                        "uptime_secs": self.engine.started_at.elapsed().as_secs(),
                        "generated_at_utc": chrono::Utc::now().to_rfc3339(),
                    });
                    let path = config_dir().join(format!(
                        "nicord-stats-{}.json",
                        chrono::Utc::now().format("%Y%m%d-%H%M%S")
                    ));
                    let result = serde_json::to_string_pretty(&stats)
                        .map_err(|e| e.to_string())
                        .and_then(|j| std::fs::write(&path, j).map_err(|e| e.to_string()));
                    match result {
                        Ok(()) => self.set_status(&format!("Stats exported to {}", path.display())),
                        Err(e) => self.set_status(&format!("Export failed: {e}")),
                    }
                }
            });
            ui.add_space(6.0);
            #[cfg(target_os = "windows")]
            let tray_note = if self.config.tray_enabled {
                "Closing the window hides Nicord to the tray — the engine keeps running. Right-click the tray icon to quit."
            } else {
                "Note: enable “System tray” in Settings to hide Nicord to the tray on close. Closing the window stops the engine."
            };
            #[cfg(not(target_os = "windows"))]
            let tray_note = "Note: there is no system-tray icon on this platform — keep the window open (minimised is fine). Closing the window stops the engine.";
            theme::muted(ui, tray_note);
        });
        ui.add_space(12.0);
        // Scenario list.
        theme::card(ui, |ui| {
            theme::section_title(ui, "Scenarios");
            let rows: Vec<(usize, String, bool, String, String)> = self
                .scenarios
                .lock()
                .map(|l| {
                    l.iter()
                        .enumerate()
                        .map(|(i, s)| {
                            let next = s
                                .schedule
                                .as_ref()
                                .map(|sc| {
                                    format!(
                                        "next {}",
                                        sc.next_after(chrono::Local::now())
                                            .format("%Y-%m-%d %H:%M")
                                    )
                                })
                                .unwrap_or_default();
                            (
                                i,
                                s.name.clone(),
                                s.enabled,
                                s.schedule.as_ref().map(|sc| sc.label()).unwrap_or_default(),
                                next,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            if rows.is_empty() {
                ui.label("No scenarios yet. Go to Builder -> New scenario.");
                ui.add_space(8.0);
                if theme::primary_button(ui, "Create a scenario").clicked() {
                    self.screen = Screen::Builder;
                }
            } else {
                let mut edit: Option<usize> = None;
                for (i, name, enabled, schedule, next) in &rows {
                    ui.horizontal(|ui| {
                        ui.label(theme::dot(if *enabled {
                            theme::SUCCESS
                        } else {
                            theme::TEXT_MUTED
                        }));
                        ui.label(theme::semibold(name, 15.0));
                        ui.label(
                            RichText::new(if *enabled { "enabled" } else { "disabled" })
                                .small()
                                .color(if *enabled {
                                    theme::SUCCESS
                                } else {
                                    theme::TEXT_MUTED
                                }),
                        );
                        ui.label(
                            RichText::new(format!("{schedule}  {next}"))
                                .small()
                                .color(theme::TEXT_MUTED),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if theme::ghost_button(ui, "Edit").clicked() {
                                edit = Some(*i);
                            }
                        });
                    });
                    ui.separator();
                }
                if let Some(i) = edit {
                    self.selected = Some(i);
                    self.screen = Screen::Builder;
                }
            }
        });
    }

    fn show_builder(&mut self, ui: &mut egui::Ui) {
        ui.label("Add nodes, move them up/down, edit the selected scenario. Dry-run validates without sending.");
        ui.add_space(12.0);
        // Two columns: scenario list | editor for the selected scenario.
        // The editor content scrolls, but the action bar (Validate, node
        // buttons, Export/Import) stays pinned below it, so the key
        // actions are always visible in the 700 px window without deep
        // scrolling.
        ui.columns(2, |cols| {
            // Distinct id_salts: two ScrollAreas in one frame must not
            // share an auto ID (egui debug-clash warnings + shared scroll
            // state).
            egui::ScrollArea::vertical()
                .id_salt("builder_list")
                .show(&mut cols[0], |ui| {
                    self.show_builder_list(ui);
                });
            let selected = self.selected;
            match selected {
                Some(i) => {
                    let action_h = 130.0;
                    let avail = cols[1].available_height();
                    egui::ScrollArea::vertical()
                        .id_salt("builder_editor")
                        .max_height((avail - action_h).max(140.0))
                        .show(&mut cols[1], |ui| {
                            self.show_builder_editor(ui, i);
                        });
                    cols[1].add_space(8.0);
                    self.show_builder_actions(&mut cols[1], i);
                }
                None => {
                    theme::card(&mut cols[1], |ui| {
                        theme::section_title(ui, "Editor");
                        theme::muted(
                            ui,
                            "Select a scenario on the left to edit it, or create a new one in the list.",
                        );
                    });
                }
            }
        });
    }

    /// Left column of the Builder: the scenario list.
    fn show_builder_list(&mut self, ui: &mut egui::Ui) {
        theme::card(ui, |ui| {
            // New-scenario row lives in this card (not above the columns)
            // so the editor column gets the full height.
            ui.horizontal(|ui| {
                let field_w = (ui.available_width() - 132.0).max(100.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_scenario_name)
                        .hint_text("New scenario name")
                        .desired_width(field_w),
                );
                if theme::primary_button(ui, "+ New scenario").clicked() {
                    // No scenario-count limit: the only limit is 1 distinct
                    // channel, enforced when a scenario is enabled.
                    let name = if self.new_scenario_name.trim().is_empty() {
                        "Untitled scenario".to_string()
                    } else {
                        self.new_scenario_name.trim().to_string()
                    };
                    let s = Scenario::new(&name);
                    if let Ok(mut list) = self.scenarios.lock() {
                        list.push(s);
                    }
                    self.save_scenarios();
                    self.new_scenario_name.clear();
                }
            });
            ui.add_space(8.0);
            theme::section_title(ui, "Your scenarios");
            let mut selected = self.selected;
            let mut to_delete: Option<usize> = None;
            let mut to_toggle: Option<usize> = None;
            {
                let list = self.scenarios.lock().unwrap();
                if list.is_empty() {
                    theme::muted(ui, "No scenarios yet — create your first one above.");
                }
                for (i, s) in list.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.selectable_label(selected == Some(i), &s.name).clicked() {
                            selected = Some(i);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // Delete is destructive: red text so it can
                            // never be confused with Enable at a glance.
                            if ui
                                .add(
                                    egui::Button::new(RichText::new("Delete").color(theme::DANGER))
                                        .fill(theme::BTN_BG),
                                )
                                .clicked()
                            {
                                to_delete = Some(i);
                            }
                            ui.add_space(6.0);
                            if ui
                                .button(if s.enabled { "Disable" } else { "Enable" })
                                .clicked()
                            {
                                to_toggle = Some(i);
                            }
                        });
                    });
                    if !s.channel_id.is_empty() {
                        theme::muted(ui, &format!("channel: {}", s.channel_id));
                    }
                    ui.separator();
                }
            }
            self.selected = selected;
            if let Some(i) = to_toggle {
                let mut msg: Option<String> = None;
                if let Ok(mut list) = self.scenarios.lock() {
                    let was_enabled = list.get(i).map(|s| s.enabled).unwrap_or(false);
                    if !was_enabled {
                        // Hypothetical list with this scenario enabled: only
                        // 1 distinct channel is allowed across enabled scenarios.
                        let mut hypo = list.clone();
                        if let Some(s) = hypo.get_mut(i) {
                            s.enabled = true;
                        }
                        if let Some(v) = channels_violation(&hypo) {
                            msg = Some(v);
                        } else if let Some(s) = list.get_mut(i) {
                            s.enabled = true;
                        }
                    } else if let Some(s) = list.get_mut(i) {
                        s.enabled = false;
                    }
                }
                self.save_scenarios();
                if let Some(m) = msg {
                    self.set_status(&m);
                }
            }
            if let Some(i) = to_delete {
                if let Ok(mut list) = self.scenarios.lock() {
                    if i < list.len() {
                        let id = list[i].id.clone();
                        list.remove(i);
                        let _ = self.store.delete_scenario(&id);
                    }
                }
                self.selected = None;
            }
        });
    }

    /// Right column of the Builder: edit the selected scenario in place.
    fn show_builder_editor(&mut self, ui: &mut egui::Ui, i: usize) {
        let mut channel_id = String::new();
        let mut messages: Vec<String> = Vec::new();
        let mut nodes: Vec<(NodeKind, String)> = Vec::new();
        if let Ok(list) = self.scenarios.lock() {
            if let Some(s) = list.get(i) {
                channel_id = s.channel_id.clone();
                messages = s.messages.clone();
                nodes = s
                    .nodes
                    .iter()
                    .map(|n| (n.kind.clone(), n.id.clone()))
                    .collect();
            }
        }
        theme::card(ui, |ui| {
            theme::section_title(ui, "Editor");
            ui.label("Channel picker (loads your servers/channels from Discord):");
            ui.horizontal(|ui| {
                if ui.button("Load servers").clicked() {
                    self.load_guilds();
                }
                let guilds = self.guilds.lock().map(|g| g.clone()).unwrap_or_default();
                if !guilds.is_empty() {
                    let mut picked = self.picker_guild_id.clone();
                    egui::ComboBox::from_label("Server")
                        .selected_text(
                            guilds
                                .iter()
                                .find(|g| g.id == picked)
                                .map(|g| g.name.clone())
                                .unwrap_or_else(|| "pick a server".to_string()),
                        )
                        .show_ui(ui, |ui| {
                            for g in &guilds {
                                if ui
                                    .selectable_value(&mut picked, g.id.clone(), &g.name)
                                    .clicked()
                                {
                                    self.picker_guild_id = g.id.clone();
                                    self.load_channels(g.id.clone());
                                }
                            }
                        });
                }
                let channels = self.channels.lock().map(|c| c.clone()).unwrap_or_default();
                if !channels.is_empty() {
                    let mut picked = channel_id.clone();
                    egui::ComboBox::from_label("Channel")
                        .selected_text(
                            channels
                                .iter()
                                .find(|c| c.id == picked)
                                .map(|c| format!("#{}", c.name))
                                .unwrap_or_else(|| "pick a channel".to_string()),
                        )
                        .show_ui(ui, |ui| {
                            for c in &channels {
                                if ui
                                    .selectable_value(
                                        &mut picked,
                                        c.id.clone(),
                                        format!("#{}", c.name),
                                    )
                                    .clicked()
                                {
                                    channel_id = c.id.clone();
                                }
                            }
                        });
                }
            });
            ui.label("Destination channel ID (or pick above):");
            ui.add(
                egui::TextEdit::singleline(&mut channel_id).hint_text("e.g. 123456789012345678"),
            );
            ui.label("Messages (rotate, spin-text {a|b} supported):");
            if messages.is_empty() {
                theme::muted(ui, "No messages yet — add one below.");
            }
            egui::ScrollArea::vertical()
                .id_salt("builder_messages")
                .max_height(90.0)
                .show(ui, |ui| {
                    for m in &messages {
                        ui.label(format!("• {m}"));
                    }
                });
            ui.horizontal(|ui| {
                // The field must leave room for the button in this narrow
                // column, or the button is pushed past the right edge.
                let field_w = (ui.available_width() - 132.0).max(100.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_message)
                        .hint_text("Add a message...")
                        .desired_width(field_w),
                );
                if theme::primary_button(ui, "+ Add message").clicked()
                    && !self.new_message.trim().is_empty()
                {
                    if let Ok(mut list) = self.scenarios.lock() {
                        if let Some(s) = list.get_mut(i) {
                            s.messages.push(self.new_message.trim().to_string());
                        }
                    }
                    self.new_message.clear();
                    self.save_scenarios();
                }
            });
            ui.label("Nodes (in order):");
            for (kind, _id) in &nodes {
                ui.label(format!("• {} — {}", kind.group(), kind.label()));
            }
            // Node buttons + Validate + Export/Import live in the pinned
            // action bar below the editor (show_builder_actions), so they
            // are always visible without scrolling.
            // Persist channel_id edits (1 distinct channel).
            if let Ok(mut list) = self.scenarios.lock() {
                let cur = list.get(i).map(|s| s.channel_id.clone());
                if cur.as_deref() != Some(channel_id.as_str()) {
                    let mut hypo = list.clone();
                    if let Some(h) = hypo.get_mut(i) {
                        h.channel_id = channel_id.clone();
                    }
                    if let Some(v) = channels_violation(&hypo) {
                        self.set_status(&v);
                    } else if let Some(s) = list.get_mut(i) {
                        s.channel_id = channel_id;
                        let _ = self.store.save_scenario(s);
                    }
                }
            }
        });
    }

    /// Pinned action bar under the Builder editor: Validate, node
    /// buttons, Export/Import. Kept outside the editor's ScrollArea
    /// so the key actions are always visible in the 700 px window.
    fn show_builder_actions(&mut self, ui: &mut egui::Ui, i: usize) {
        theme::slim_card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                if theme::primary_button(ui, "Validate (dry-run)").clicked() {
                    let result = self
                        .scenarios
                        .lock()
                        .ok()
                        .and_then(|l| l.get(i).map(|s| s.validate()));
                    match result {
                        Some(Ok(())) => {
                            self.set_status("Dry-run OK: scenario is valid, nothing was sent.")
                        }
                        Some(Err(e)) => self.set_status(&format!("Validation failed: {e}")),
                        None => self.set_status("No scenario selected."),
                    }
                }
                // Short labels so Validate + both node buttons fit on one
                // row in the narrow column (no wrap = no extra height).
                if ui.button("+ Send Message").clicked() {
                    if let Ok(mut list) = self.scenarios.lock() {
                        if let Some(s) = list.get_mut(i) {
                            s.nodes.push(Node::new(NodeKind::ActionSendMessage));
                        }
                    }
                    self.save_scenarios();
                }
                if ui.button("+ Wait").clicked() {
                    if let Ok(mut list) = self.scenarios.lock() {
                        if let Some(s) = list.get_mut(i) {
                            s.nodes.push(Node::new(NodeKind::UtilityWait));
                        }
                    }
                    self.save_scenarios();
                }
            });
            ui.add_space(4.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.import_path)
                    .hint_text("Path to a scenario .json to import"),
            );
            ui.horizontal(|ui| {
                if ui.button("Export scenario JSON").clicked() {
                    let path = config_dir().join(format!(
                        "nicord-export-{}.json",
                        self.scenarios
                            .lock()
                            .ok()
                            .and_then(|l| l.get(i).map(|s| s.id.clone()))
                            .unwrap_or_default()
                    ));
                    let result = self.scenarios.lock().ok().and_then(|l| {
                        l.get(i).map(|s| {
                            serde_json::to_string_pretty(s)
                                .map_err(|e| e.to_string())
                                .and_then(|j| std::fs::write(&path, j).map_err(|e| e.to_string()))
                        })
                    });
                    match result {
                        Some(Ok(())) => self.set_status(&format!("Exported to {}", path.display())),
                        Some(Err(e)) => self.set_status(&format!("Export failed: {e}")),
                        None => self.set_status("No scenario selected."),
                    }
                }
                if ui.button("Import scenario").clicked() {
                    let path = self.import_path.trim().to_string();
                    match std::fs::read_to_string(&path)
                        .map_err(|e| e.to_string())
                        .and_then(|s| {
                            serde_json::from_str::<Scenario>(&s).map_err(|e| e.to_string())
                        }) {
                        Ok(mut imported) => {
                            imported.id = uuid::Uuid::new_v4().to_string();
                            imported.enabled = false;
                            let mut ok = true;
                            if let Ok(list) = self.scenarios.lock() {
                                let mut hypo = list.clone();
                                hypo.push(imported.clone());
                                if let Some(v) = channels_violation(&hypo) {
                                    self.set_status(&format!("Import blocked: {v}"));
                                    ok = false;
                                }
                            }
                            if ok {
                                if let Ok(mut list) = self.scenarios.lock() {
                                    list.push(imported);
                                }
                                self.save_scenarios();
                                self.set_status("Scenario imported (disabled — enable it to run).");
                            }
                        }
                        Err(e) => self.set_status(&format!("Import failed: {e}")),
                    }
                }
            });
        });
    }
    fn show_logs(&mut self, ui: &mut egui::Ui) {
        ui.label("Send history (token is never logged).");
        ui.add_space(4.0);
        theme::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Filter:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.log_filter)
                        .hint_text("scenario name or status, e.g. sent"),
                );
                if ui.button("Export logs (CSV)").clicked() {
                    let path = config_dir().join("nicord-logs.csv");
                    let result = self
                        .store
                        .recent_logs_filtered(1000, &self.log_filter.clone())
                        .map(|rows| {
                            let mut csv = String::from("time,scenario,status,detail\n");
                            for (ts, name, status, detail) in rows {
                                csv.push_str(&format!(
                                    "{},{},{},{}\n",
                                    ts,
                                    name.replace(',', ";"),
                                    status,
                                    detail.replace(',', ";")
                                ));
                            }
                            std::fs::write(&path, csv)
                        });
                    match result {
                        Ok(Ok(())) => {
                            self.set_status(&format!("Logs exported to {}", path.display()))
                        }
                        Ok(Err(e)) => self.set_status(&format!("Export failed: {e}")),
                        Err(e) => self.set_status(&format!("Cannot load logs: {e}")),
                    }
                }
                if ui.button("Refresh").clicked() {
                    ctx_request_repaint(ui);
                }
            });
        });
        ui.add_space(12.0);
        match self
            .store
            .recent_logs_filtered(100, &self.log_filter.clone())
        {
            Ok(rows) => {
                if rows.is_empty() {
                    theme::card(ui, |ui| {
                        theme::muted(ui, "No logs yet — send history will appear here.");
                    });
                } else {
                    theme::card(ui, |ui| {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for (ts, name, status, detail) in rows {
                                let status_color = if status == "sent" {
                                    theme::SUCCESS
                                } else if status.contains("fail") || status.contains("error") {
                                    theme::DANGER
                                } else {
                                    theme::TEXT_SECONDARY
                                };
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(&ts).monospace().color(theme::TEXT_MUTED),
                                    );
                                    ui.label(theme::semibold(&name, 15.0));
                                    ui.label(RichText::new(&status).color(status_color));
                                });
                                theme::muted(ui, &detail);
                                ui.separator();
                            }
                        });
                    });
                }
            }
            Err(e) => {
                ui.label(format!("Cannot load logs: {e}"));
            }
        }
    }

    fn show_settings(&mut self, ui: &mut egui::Ui) {
        // Token Validator (Discord-BLUE): check the stored token and
        // show the account behind it. The token itself is never shown.
        theme::card(ui, |ui| {
            theme::section_title(ui, "Tài khoản");
            ui.horizontal(|ui| {
                if theme::primary_button(ui, "Kiểm tra token").clicked() {
                    self.check_token();
                }
                theme::muted(ui, "Kiểm tra token đã lưu: tên, ID, email, 2FA, Nitro.");
            });
            if let Ok(info) = self.token_info.lock() {
                if let Some(me) = info.as_ref() {
                    ui.add_space(6.0);
                    ui.label(format!("Tên: {} (@{})", me.display_name(), me.username));
                    ui.label(format!(
                        "ID: {}",
                        if me.id.is_empty() {
                            "—"
                        } else {
                            me.id.as_str()
                        }
                    ));
                    ui.label(format!("Email: {}", me.email.as_deref().unwrap_or("—")));
                    ui.label(format!(
                        "Đã xác minh email: {}",
                        if me.verified { "Có" } else { "Không" }
                    ));
                    ui.label(format!(
                        "2FA: {}",
                        if me.mfa_enabled { "Bật" } else { "Tắt" }
                    ));
                    ui.label(format!("Nitro: {}", me.nitro_label()));
                }
            }
        });
        ui.add_space(12.0);
        // Anti-ban (Discord-BLUE): Smart Templates, Auto Typing,
        // Auto Break, Auto Delete. Saved to config.json and applied to
        // the running engine on Save.
        theme::card(ui, |ui| {
            theme::section_title(ui, "Chống ban");
            ui.checkbox(
                &mut self.config.antiban.smart_templates,
                "Smart Templates: Spintax {a|b} + biến thể tin nhắn ngẫu nhiên",
            );
            theme::muted(
                ui,
                "Áp dụng sau khi thay biến {{username}}/{{server}} — tin nhắn không lặp y hệt.",
            );
            ui.checkbox(
                &mut self.config.antiban.auto_typing,
                "Auto Typing: hiện “đang gõ...” trước khi gửi",
            );
            ui.horizontal(|ui| {
                ui.label("Thời gian gõ (giây):");
                ui.add(
                    egui::DragValue::new(&mut self.config.antiban.typing_min_sec)
                        .range(0..=30)
                        .speed(1),
                );
                ui.label("–");
                ui.add(
                    egui::DragValue::new(&mut self.config.antiban.typing_max_sec)
                        .range(0..=30)
                        .speed(1),
                );
            });
            ui.checkbox(
                &mut self.config.antiban.auto_break,
                "Auto Break: nghỉ dài định kỳ sau một số tin",
            );
            ui.horizontal(|ui| {
                ui.label("Nghỉ sau (tin):");
                ui.add(
                    egui::DragValue::new(&mut self.config.antiban.break_after_min)
                        .range(1..=500)
                        .speed(1),
                );
                ui.label("–");
                ui.add(
                    egui::DragValue::new(&mut self.config.antiban.break_after_max)
                        .range(1..=500)
                        .speed(1),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Nghỉ trong (phút):");
                ui.add(
                    egui::DragValue::new(&mut self.config.antiban.break_duration_min)
                        .range(1..=180)
                        .speed(1),
                );
                ui.label("–");
                ui.add(
                    egui::DragValue::new(&mut self.config.antiban.break_duration_max)
                        .range(1..=180)
                        .speed(1),
                );
            });
            ui.checkbox(
                &mut self.config.antiban.auto_delete,
                "Auto Delete: tự xóa tin sau khi gửi",
            );
            ui.horizontal(|ui| {
                ui.label("Xóa sau (ms, 0 = ngay lập tức):");
                ui.add(
                    egui::DragValue::new(&mut self.config.antiban.delete_delay_ms)
                        .range(0..=3_600_000)
                        .speed(100),
                );
            });
            theme::muted(
                ui,
                "Auto Delete chỉ xóa tin Nicord vừa gửi, không xóa tin cũ.",
            );
        });
        ui.add_space(12.0);
        theme::card(ui, |ui| {
            theme::section_title(ui, "Limits");
            ui.label("Limits: 1 channel • 300 messages/day (resets 00:00 UTC)");
            ui.checkbox(&mut self.config.autostart, "Start with the computer");
            #[cfg(target_os = "windows")]
            ui.checkbox(
                &mut self.config.tray_enabled,
                "Thu nhỏ xuống khay hệ thống khi đóng cửa sổ (X)",
            );
            #[cfg(not(target_os = "windows"))]
            theme::muted(ui, "System tray: chỉ có trên Windows.");
            theme::muted(ui, "Khởi động cùng máy được ghi thật vào hệ thống (Windows Run / Linux autostart). Khi bật khay hệ thống, đóng cửa sổ sẽ ẩn Nicord xuống khay thay vì thoát — bấm chuột phải vào icon khay để thoát hẳn.");
            ui.add_space(8.0);
            if theme::primary_button(ui, "Save settings").clicked() {
                let _ = save_config(&self.config);
                // Live-apply the anti-ban settings to the running engine.
                self.engine.set_antiban(self.config.antiban.clone());
                // Apply the real OS autostart hook.
                match crate::autostart::set_enabled(self.config.autostart) {
                    Ok(()) => self.set_status("Settings saved."),
                    Err(e) => {
                        self.set_status(&format!("Settings saved, but autostart failed: {e}"))
                    }
                }
                #[cfg(target_os = "windows")]
                self.sync_tray();
            }
        });
        ui.add_space(12.0);
        theme::danger_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new("Danger zone")
                    .color(theme::DANGER)
                    .text_style(egui::TextStyle::Name("SectionTitle".into())),
            );
            if theme::danger_button(ui, "Delete all local data (token, scenarios)").clicked() {
                // Actually delete the scenarios too: resetting the config
                // alone left them in the DB (and in the UI), despite the
                // button promising to delete them.
                if let Ok(list) = self.scenarios.lock() {
                    for s in list.iter() {
                        let _ = self.store.delete_scenario(&s.id);
                    }
                }
                if let Ok(mut list) = self.scenarios.lock() {
                    list.clear();
                }
                self.selected = None;
                self.config = AppConfig::default();
                let _ = save_config(&self.config);
                self.screen = Screen::Onboarding;
                self.set_status("Local data cleared.");
            }
        });
    }
}

fn ctx_request_repaint(ui: &mut egui::Ui) {
    ui.ctx().request_repaint();
}
