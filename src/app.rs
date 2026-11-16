//! Native egui UI (eframe, immediate-mode — no React / WebView).
//! Screens: Onboarding (ToS warning → Key → Token) → Dashboard →
//! Builder (list-based, add/remove/move up-down) → Logs → Settings.
//! Light by design: simple widgets, no canvas, no animations.

use crate::{
    antiban::AntiBanConfig,
    crypto::{decrypt_token, encrypt_token, mask_token},
    discord::{Channel, DiscordClient, Guild},
    license::{verify_license_key, Plan},
    scenario::{Node, NodeKind, Scenario},
    scheduler::{free_channels_violation, Engine},
    store::{config_dir, load_config, save_config, AppConfig, Store},
};
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
    pub plan: Plan,
    pub status: Arc<Mutex<String>>,
    pub tos_checked: bool,
    pub key_input: String,
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
}

impl NicordApp {
    pub fn new(store: Arc<Store>) -> Self {
        let config = load_config();
        let scenarios = store.load_scenarios().unwrap_or_default();
        let plan = if config.license_key.trim().is_empty() {
            Plan::Free
        } else {
            verify_license_key(&config.license_key)
                .map(|info| info.plan)
                .unwrap_or(Plan::Free)
        };
        let screen = if !config.tos_accepted || config.license_key.trim().is_empty() {
            Screen::Onboarding
        } else {
            Screen::Dashboard
        };
        let engine = Arc::new(Engine::new(store.clone(), AntiBanConfig::default()));
        let has_enabled = scenarios.iter().any(|s| s.enabled);
        let app = Self {
            store,
            engine,
            scenarios: Arc::new(Mutex::new(scenarios)),
            config,
            screen,
            plan,
            status: Arc::new(Mutex::new(String::new())),
            tos_checked: false,
            key_input: String::new(),
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

    fn start_engine(&self) {
        self.engine.resume();
        let engine = self.engine.clone();
        let scenarios = self.scenarios.clone();
        let config = self.config.clone();
        let plan = self.plan;
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(_) => return,
            };
            let engine2 = engine.clone();
            let scenarios2 = scenarios.clone();
            rt.block_on(async move {
                engine2.set_plan(plan);
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
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Nicord");
                ui.label(format!("Plan: {:?}", self.plan));
                if self.screen != Screen::Onboarding {
                    for (label, screen) in [
                        ("Dashboard", Screen::Dashboard),
                        ("Builder", Screen::Builder),
                        ("Logs", Screen::Logs),
                        ("Settings", Screen::Settings),
                    ] {
                        if ui.button(label).clicked() {
                            self.screen = screen;
                        }
                    }
                }
                if ui.button("⛔ Emergency Stop").clicked() {
                    self.engine.stop_all();
                    self.set_status("EMERGENCY STOP — all queues halted.");
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let status = self.status_text();
            if !status.is_empty() {
                ui.colored_label(egui::Color32::YELLOW, &status);
                ui.separator();
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
    fn show_onboarding(&mut self, ui: &mut egui::Ui) {
        ui.heading("Welcome to Nicord");
        ui.colored_label(
            egui::Color32::RED,
            "⚠ WARNING — READ FIRST: Automating a normal Discord user account \
             (self-bot) is forbidden by Discord's Terms of Service and can result \
             in PERMANENT account termination. Nicord makes no guarantee you will \
             never be banned. Use a SPARE account, never your main account.",
        );
        ui.checkbox(
            &mut self.tos_checked,
            "I understand the risk and accept responsibility for my own account.",
        );
        ui.separator();
        ui.label("Step 1 — License key (enter once, unlocks features)");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.key_input).hint_text("NICORD-XXXX-..."));
            if ui.button("Activate key").clicked() {
                if !self.tos_checked {
                    self.set_status("Please accept the ToS warning first.");
                } else {
                    match verify_license_key(&self.key_input) {
                        Ok(info) => {
                            self.plan = info.plan;
                            self.engine.set_plan(info.plan);
                            self.config.license_key = self.key_input.clone();
                            let _ = save_config(&self.config);
                            self.set_status(&format!("Key accepted — plan {:?}", info.plan));
                        }
                        Err(e) => self.set_status(&format!("Key rejected: {e}")),
                    }
                }
            }
            if ui.button("Continue Free (1 channel)").clicked() {
                if !self.tos_checked {
                    self.set_status("Please accept the ToS warning first.");
                } else {
                    self.plan = Plan::Free;
                    self.set_status("Free plan: 1 channel, 1 scenario.");
                }
            }
        });
        ui.separator();
        ui.label("Step 2 — Your Discord token (paste once, stored encrypted locally)");
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
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.token_input)
                    .password(true)
                    .hint_text("paste your Discord token here"),
            );
            if ui.button("Save token").clicked() {
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
        if ui.button("Skip for now → Dashboard").clicked() {
            self.config.tos_accepted = self.tos_checked;
            let _ = save_config(&self.config);
            self.screen = Screen::Dashboard;
        }
    }

    fn show_dashboard(&mut self, ui: &mut egui::Ui) {
        ui.heading("Dashboard");
        let count = self.scenarios.lock().map(|l| l.len()).unwrap_or(0);
        ui.label(format!(
            "Scenarios: {}  •  Running: {}  •  Sent today: {}  •  Free limit: 1 channel, 1 scenario",
            count,
            self.engine.running.load(std::sync::atomic::Ordering::SeqCst),
            self.sent_today
        ));
        ui.horizontal(|ui| {
            if ui.button("▶ Start engine").clicked() {
                self.start_engine();
            }
            if ui.button("⏸ Stop engine").clicked() {
                self.engine.stop_all();
                self.set_status("Engine stopped.");
            }
            if ui.button("✉ Test send (sends a REAL message)").clicked() {
                self.test_send();
            }
            if ui.button("▁ Minimise (engine keeps running)").clicked() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
        });
        ui.label("Note: there is no system-tray icon yet — keep the window open (minimised is fine). Closing the window stops the engine.");
        ui.separator();
        ui.label("Scenarios (click Builder to edit):");
        let names: Vec<String> = self
            .scenarios
            .lock()
            .map(|l| {
                l.iter()
                    .map(|s| {
                        let next = s
                            .schedule
                            .as_ref()
                            .map(|sc| {
                                format!(
                                    "next {}",
                                    sc.next_after(chrono::Local::now()).format("%Y-%m-%d %H:%M")
                                )
                            })
                            .unwrap_or_default();
                        format!(
                            "{} — {} — {} — {}",
                            s.name,
                            if s.enabled { "enabled" } else { "disabled" },
                            s.schedule.as_ref().map(|sc| sc.label()).unwrap_or_default(),
                            next
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        if names.is_empty() {
            ui.label("No scenarios yet. Go to Builder → New scenario.");
        }
        for name in &names {
            ui.label(format!("• {name}"));
        }
    }

    fn show_builder(&mut self, ui: &mut egui::Ui) {
        ui.heading("Scenario Builder (list-based — light, no heavy canvas)");
        ui.label("Add nodes, move them up/down, edit the selected scenario. Dry-run validates without sending.");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_scenario_name)
                    .hint_text("New scenario name"),
            );
            if ui.button("＋ New scenario").clicked() {
                if self.plan == Plan::Free {
                    let running = self
                        .scenarios
                        .lock()
                        .map(|l| l.iter().filter(|s| s.enabled).count())
                        .unwrap_or(0);
                    let total = self.scenarios.lock().map(|l| l.len()).unwrap_or(0);
                    if total >= 1 && running >= 1 {
                        self.set_status("Free plan: only 1 scenario. Upgrade with a Pro key.");
                    } else {
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
                } else {
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
            }
        });
        ui.separator();
        // List scenarios; select one to edit its messages / channel / nodes.
        let mut selected = self.selected;
        let mut to_delete: Option<usize> = None;
        let mut to_toggle: Option<usize> = None;
        {
            let list = self.scenarios.lock().unwrap();
            for (i, s) in list.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.selectable_label(selected == Some(i), &s.name).clicked() {
                        selected = Some(i);
                    }
                    ui.label(format!("channel: {}", s.channel_id));
                    if ui
                        .button(if s.enabled { "Disable" } else { "Enable" })
                        .clicked()
                    {
                        to_toggle = Some(i);
                    }
                    if ui.button("🗑 Delete").clicked() {
                        to_delete = Some(i);
                    }
                });
            }
        }
        self.selected = selected;
        if let Some(i) = to_toggle {
            let mut msg: Option<String> = None;
            if let Ok(mut list) = self.scenarios.lock() {
                let was_enabled = list.get(i).map(|s| s.enabled).unwrap_or(false);
                if !was_enabled {
                    // Hypothetical list with this scenario enabled: Free
                    // allows 1 running scenario AND 1 distinct channel.
                    let mut hypo = list.clone();
                    if let Some(s) = hypo.get_mut(i) {
                        s.enabled = true;
                    }
                    let running = hypo.iter().filter(|x| x.enabled).count();
                    if self.plan == Plan::Free && running > 1 {
                        msg = Some("Free plan: only 1 running scenario.".to_string());
                    } else if let Some(v) = free_channels_violation(&hypo, self.plan) {
                        msg = Some(format!("{v}. Upgrade with a Pro key."));
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
        ui.separator();
        if let Some(i) = self.selected {
            // Edit the selected scenario in place.
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
            ui.label("Channel picker (loads your servers/channels from Discord):");
            ui.horizontal(|ui| {
                if ui.button("↻ Load servers").clicked() {
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
            egui::ScrollArea::vertical()
                .max_height(120.0)
                .show(ui, |ui| {
                    for m in &messages {
                        ui.label(format!("• {m}"));
                    }
                });
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_message).hint_text("Add a message…"),
                );
                if ui.button("＋ Add message").clicked() && !self.new_message.trim().is_empty() {
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
            ui.horizontal(|ui| {
                if ui.button("＋ Add: Send Message node").clicked() {
                    if let Ok(mut list) = self.scenarios.lock() {
                        if let Some(s) = list.get_mut(i) {
                            s.nodes.push(Node::new(NodeKind::ActionSendMessage));
                        }
                    }
                    self.save_scenarios();
                }
                if ui.button("＋ Add: Wait node").clicked() {
                    if let Ok(mut list) = self.scenarios.lock() {
                        if let Some(s) = list.get_mut(i) {
                            s.nodes.push(Node::new(NodeKind::UtilityWait));
                        }
                    }
                    self.save_scenarios();
                }
                if ui.button("✓ Validate (dry-run)").clicked() {
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
            });
            // Persist channel_id edits (Free: 1 distinct channel).
            if let Ok(mut list) = self.scenarios.lock() {
                let cur = list.get(i).map(|s| s.channel_id.clone());
                if cur.as_deref() != Some(channel_id.as_str()) {
                    let mut hypo = list.clone();
                    if let Some(h) = hypo.get_mut(i) {
                        h.channel_id = channel_id.clone();
                    }
                    if let Some(v) = free_channels_violation(&hypo, self.plan) {
                        self.set_status(&format!("{v}. Upgrade with a Pro key."));
                    } else if let Some(s) = list.get_mut(i) {
                        s.channel_id = channel_id;
                        let _ = self.store.save_scenario(s);
                    }
                }
            }
            // Scenario export / import (JSON).
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("⬇ Export scenario JSON").clicked() {
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
                ui.add(
                    egui::TextEdit::singleline(&mut self.import_path)
                        .hint_text("Path to a scenario .json to import"),
                );
                if ui.button("⬆ Import scenario").clicked() {
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
                                if let Some(v) = free_channels_violation(&hypo, self.plan) {
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
        }
    }

    fn show_logs(&mut self, ui: &mut egui::Ui) {
        ui.heading("Logs");
        ui.label("Send history (token is never logged).");
        ui.horizontal(|ui| {
            ui.label("Filter:");
            ui.add(
                egui::TextEdit::singleline(&mut self.log_filter)
                    .hint_text("scenario name or status, e.g. sent"),
            );
            if ui.button("⬇ Export logs (CSV)").clicked() {
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
                    Ok(Ok(())) => self.set_status(&format!("Logs exported to {}", path.display())),
                    Ok(Err(e)) => self.set_status(&format!("Export failed: {e}")),
                    Err(e) => self.set_status(&format!("Cannot load logs: {e}")),
                }
            }
            if ui.button("Refresh").clicked() {
                ctx_request_repaint(ui);
            }
        });
        match self
            .store
            .recent_logs_filtered(100, &self.log_filter.clone())
        {
            Ok(rows) => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (ts, name, status, detail) in rows {
                        ui.label(format!("[{ts}] {name} — {status} — {detail}"));
                    }
                });
            }
            Err(e) => {
                ui.label(format!("Cannot load logs: {e}"));
            }
        }
    }

    fn show_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        ui.label(format!(
            "Plan: {:?} (Free = 1 channel, 1 scenario, 100 msgs/day)",
            self.plan
        ));
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.key_input).hint_text("New license key"));
            if ui.button("Apply key").clicked() {
                match verify_license_key(&self.key_input) {
                    Ok(info) => {
                        self.plan = info.plan;
                        self.engine.set_plan(info.plan);
                        self.config.license_key = self.key_input.clone();
                        let _ = save_config(&self.config);
                        self.set_status(&format!("Key accepted — plan {:?}", info.plan));
                    }
                    Err(e) => self.set_status(&format!("Key rejected: {e}")),
                }
            }
        });
        ui.checkbox(&mut self.config.autostart, "Start with the computer");
        ui.label("Note: autostart is saved here, but the OS hook is not wired yet — enable Nicord in your OS startup apps after the first run (see README). There is no system-tray icon yet: keep the window open (minimised is fine); closing the window stops the engine.");
        if ui.button("Save settings").clicked() {
            let _ = save_config(&self.config);
            self.set_status("Settings saved.");
        }
        ui.separator();
        ui.colored_label(egui::Color32::RED, "Danger zone");
        if ui
            .button("🗑 Delete all local data (token, key, scenarios)")
            .clicked()
        {
            self.config = AppConfig::default();
            let _ = save_config(&self.config);
            self.screen = Screen::Onboarding;
            self.set_status("Local data cleared.");
        }
    }
}

fn ctx_request_repaint(ui: &mut egui::Ui) {
    ui.ctx().request_repaint();
}
