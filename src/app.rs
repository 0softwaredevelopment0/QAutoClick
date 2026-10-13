use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use eframe::egui;
use global_hotkey::hotkey::{Code, HotKey};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use crate::commands::{EngineCommand, EngineStatus};
use crate::config::{ClickerConfig, CountMode, MouseChoice};

pub struct QAutoClickApp {
    config: ClickerConfig,
    cmd_tx: Sender<EngineCommand>,
    status_rx: Receiver<EngineStatus>,
    engine: Option<JoinHandle<()>>,
    running: Arc<AtomicBool>,
    clicks_done: u64,
    last_error: Option<String>,
    hotkey_manager: Option<GlobalHotKeyManager>,
    hotkey: HotKey,
    hotkey_id: Arc<AtomicU32>,
    hotkey_error: Option<String>,
}

impl QAutoClickApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: ClickerConfig,
        cmd_tx: Sender<EngineCommand>,
        status_rx: Receiver<EngineStatus>,
        engine: Option<JoinHandle<()>>,
        running: Arc<AtomicBool>,
        hotkey_id: Arc<AtomicU32>,
    ) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        spawn_hotkey_watcher(running.clone(), hotkey_id.clone(), cmd_tx.clone(), cc.egui_ctx.clone());

        let (hotkey_manager, hotkey, hotkey_error) = match GlobalHotKeyManager::new() {
            Ok(manager) => {
                let hotkey = hotkey_for(config.hotkey_fkey);
                match manager.register(hotkey) {
                    Ok(()) => (Some(manager), hotkey, None),
                    Err(e) => (Some(manager), hotkey, Some(format!("hotkey registration failed: {e}"))),
                }
            }
            Err(e) => (None, hotkey_for(config.hotkey_fkey), Some(format!("global hotkeys unavailable: {e}"))),
        };
        hotkey_id.store(hotkey.id(), Ordering::SeqCst);

        Self {
            config,
            cmd_tx,
            status_rx,
            engine,
            running,
            clicks_done: 0,
            last_error: None,
            hotkey_manager,
            hotkey,
            hotkey_id,
            hotkey_error,
        }
    }

    fn poll_status(&mut self) {
        while let Ok(status) = self.status_rx.try_recv() {
            match status {
                EngineStatus::Idle => {
                    self.running.store(false, Ordering::SeqCst);
                }
                EngineStatus::Running { clicks_done } => {
                    self.running.store(true, Ordering::SeqCst);
                    self.clicks_done = clicks_done;
                    self.last_error = None;
                }
                EngineStatus::Error(e) => {
                    self.running.store(false, Ordering::SeqCst);
                    self.last_error = Some(e);
                }
            }
        }
    }

    /// Flip running state and tell the engine. Used by both the UI button and the hotkey watcher.
    fn toggle(&mut self) {
        let was_running = self.running.fetch_xor(true, Ordering::SeqCst);
        let cmd = if was_running { EngineCommand::Stop } else { EngineCommand::Start };
        let _ = self.cmd_tx.send(cmd);
    }

    fn on_config_changed(&mut self) {
        self.config.save();
        let _ = self.cmd_tx.send(EngineCommand::SetConfig(self.config.clone()));
    }

    fn reregister_hotkey(&mut self) {
        self.hotkey_error = None;
        let Some(manager) = &self.hotkey_manager else {
            return;
        };
        let _ = manager.unregister(self.hotkey);
        let hotkey = hotkey_for(self.config.hotkey_fkey);
        match manager.register(hotkey) {
            Ok(()) => {
                self.hotkey = hotkey;
                self.hotkey_id.store(hotkey.id(), Ordering::SeqCst);
            }
            Err(e) => self.hotkey_error = Some(format!("hotkey registration failed: {e}")),
        }
    }
}

impl Drop for QAutoClickApp {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(EngineCommand::Shutdown);
        if let Some(handle) = self.engine.take() {
            let _ = handle.join();
        }
    }
}

impl eframe::App for QAutoClickApp {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Called even while the window is hidden: keep engine state fresh.
        self.poll_status();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_status();

        egui::Frame::new().inner_margin(egui::Margin::same(14)).show(ui, |ui| {
            let is_running = self.running.load(Ordering::SeqCst);
            let status_text = if is_running {
                format!("Running — {} clicks", self.clicks_done)
            } else {
                "Idle".to_owned()
            };
            let status_color = if is_running { egui::Color32::LIGHT_GREEN } else { egui::Color32::GRAY };
            ui.horizontal(|ui| {
                ui.heading("QAutoClick");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.colored_label(status_color, status_text);
                });
            });
            ui.add_space(6.0);
            ui.separator();

            egui::Grid::new("settings_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Interval");
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::DragValue::new(&mut self.config.interval_ms)
                                .range(1..=3_600_000)
                                .speed(10)
                                .suffix(" ms"),
                        )
                        .changed()
                    {
                        self.on_config_changed();
                    }
                    ui.weak(format!("≈ {:.1}/s", 1000.0 / self.config.interval_ms.max(1) as f32));
                });
                ui.end_row();

                ui.label("Mouse button");
                ui.horizontal(|ui| {
                    for (choice, label) in [
                        (MouseChoice::Left, "Left"),
                        (MouseChoice::Middle, "Middle"),
                        (MouseChoice::Right, "Right"),
                    ] {
                        if ui.radio_value(&mut self.config.button, choice, label).changed() {
                            self.on_config_changed();
                        }
                    }
                });
                ui.end_row();

                ui.label("Clicks");
                ui.horizontal(|ui| {
                    if ui
                        .radio_value(&mut self.config.count_mode, CountMode::Infinite, "Infinite")
                        .changed()
                    {
                        self.on_config_changed();
                    }
                    let is_fixed = matches!(self.config.count_mode, CountMode::Fixed(_));
                    if ui.radio(is_fixed, "Fixed").clicked() && !is_fixed {
                        self.config.count_mode = CountMode::Fixed(100);
                        self.on_config_changed();
                    }
                    if let CountMode::Fixed(count) = &mut self.config.count_mode {
                        if ui
                            .add(egui::DragValue::new(count).range(1..=u32::MAX).speed(10))
                            .changed()
                        {
                            self.on_config_changed();
                        }
                    }
                });
                ui.end_row();

                ui.label("Toggle hotkey");
                ui.horizontal(|ui| {
                    let combo = egui::ComboBox::from_id_salt("hotkey_fkey")
                        .selected_text(format!("F{}", self.config.hotkey_fkey))
                        .show_ui(ui, |ui| {
                            for fkey in 1..=12u8 {
                                ui.selectable_value(&mut self.config.hotkey_fkey, fkey, format!("F{fkey}"));
                            }
                        });
                    if combo.response.changed() {
                        self.reregister_hotkey();
                        self.on_config_changed();
                    }
                    ui.weak("(system-wide)");
                });
                ui.end_row();
            });

            if let Some(error) = &self.hotkey_error {
                ui.colored_label(egui::Color32::YELLOW, error);
            }

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);

            let button_text = if is_running { "Stop" } else { "Start" };
            let button = egui::Button::new(egui::RichText::new(button_text).size(20.0))
                .min_size(egui::vec2(ui.available_width(), 42.0))
                .fill(if is_running {
                    egui::Color32::from_rgb(140, 40, 40)
                } else {
                    egui::Color32::from_rgb(40, 110, 50)
                });
            if ui.add(button).clicked() {
                self.toggle();
            }

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(format!("Clicks done: {}", self.clicks_done));
                if let CountMode::Fixed(limit) = self.config.count_mode {
                    let progress = (self.clicks_done as f32 / limit as f32).clamp(0.0, 1.0);
                    ui.add(egui::ProgressBar::new(progress).show_percentage().desired_height(14.0));
                }
            });

            if let Some(error) = &self.last_error {
                ui.colored_label(egui::Color32::RED, error);
            }
        });

        if self.running.load(Ordering::SeqCst) {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
    }
}

/// Map F-key number to a global hotkey.
fn hotkey_for(fkey: u8) -> HotKey {
    let code = match fkey {
        1 => Code::F1,
        2 => Code::F2,
        3 => Code::F3,
        4 => Code::F4,
        5 => Code::F5,
        6 => Code::F6,
        7 => Code::F7,
        8 => Code::F8,
        9 => Code::F9,
        10 => Code::F10,
        11 => Code::F11,
        _ => Code::F12,
    };
    HotKey::new(None, code)
}

/// Background thread translating global hotkey presses into engine commands.
/// Runs even when the window is unfocused; wakes the UI on every toggle.
fn spawn_hotkey_watcher(
    running: Arc<AtomicBool>,
    hotkey_id: Arc<AtomicU32>,
    cmd_tx: Sender<EngineCommand>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let receiver = GlobalHotKeyEvent::receiver();
        while let Ok(event) = receiver.recv() {
            if event.state() != HotKeyState::Pressed || event.id() != hotkey_id.load(Ordering::SeqCst) {
                continue;
            }
            let was_running = running.fetch_xor(true, Ordering::SeqCst);
            let cmd = if was_running { EngineCommand::Stop } else { EngineCommand::Start };
            let _ = cmd_tx.send(cmd);
            ctx.request_repaint();
        }
    });
}
