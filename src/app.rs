use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use eframe::egui;
use global_hotkey::hotkey::{Code, HotKey};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use crate::commands::{EngineCommand, EngineStatus};
use crate::config::{ClickerConfig, CountMode, MouseChoice};

/// How long the "press any key" bind window stays open.
const BIND_TIMEOUT: Duration = Duration::from_secs(5);

/// Keys offered for binding: everything people plausibly use as a clicker hotkey.
/// Keys the OS refuses to register are silently skipped; Power/Sleep and
/// keyboard-internal keys are excluded on purpose.
const BINDABLE_CODES: &[Code] = &[
    Code::F1, Code::F2, Code::F3, Code::F4, Code::F5, Code::F6, Code::F7, Code::F8, Code::F9,
    Code::F10, Code::F11, Code::F12, Code::F13, Code::F14, Code::F15, Code::F16, Code::F17,
    Code::F18, Code::F19, Code::F20, Code::F21, Code::F22, Code::F23, Code::F24,
    Code::KeyA, Code::KeyB, Code::KeyC, Code::KeyD, Code::KeyE, Code::KeyF, Code::KeyG,
    Code::KeyH, Code::KeyI, Code::KeyJ, Code::KeyK, Code::KeyL, Code::KeyM, Code::KeyN,
    Code::KeyO, Code::KeyP, Code::KeyQ, Code::KeyR, Code::KeyS, Code::KeyT, Code::KeyU,
    Code::KeyV, Code::KeyW, Code::KeyX, Code::KeyY, Code::KeyZ,
    Code::Digit0, Code::Digit1, Code::Digit2, Code::Digit3, Code::Digit4, Code::Digit5,
    Code::Digit6, Code::Digit7, Code::Digit8, Code::Digit9,
    Code::Space, Code::Enter, Code::Backspace, Code::Tab, Code::Escape, Code::CapsLock,
    Code::ContextMenu, Code::ScrollLock, Code::Pause, Code::PrintScreen,
    Code::Insert, Code::Delete, Code::Home, Code::End, Code::PageUp, Code::PageDown,
    Code::ArrowUp, Code::ArrowDown, Code::ArrowLeft, Code::ArrowRight,
    Code::ShiftLeft, Code::ShiftRight, Code::ControlLeft, Code::ControlRight,
    Code::AltLeft, Code::AltRight, Code::MetaLeft, Code::MetaRight,
    Code::Backquote, Code::Minus, Code::Equal, Code::BracketLeft, Code::BracketRight,
    Code::Backslash, Code::Semicolon, Code::Quote, Code::Comma, Code::Period, Code::Slash,
    Code::IntlBackslash, Code::IntlRo, Code::IntlYen,
    Code::Numpad0, Code::Numpad1, Code::Numpad2, Code::Numpad3, Code::Numpad4, Code::Numpad5,
    Code::Numpad6, Code::Numpad7, Code::Numpad8, Code::Numpad9, Code::NumpadAdd,
    Code::NumpadSubtract, Code::NumpadMultiply, Code::NumpadDivide, Code::NumpadDecimal,
    Code::NumpadEnter, Code::NumpadEqual,
    Code::MediaPlayPause, Code::MediaStop, Code::MediaTrackNext, Code::MediaTrackPrevious,
    Code::AudioVolumeUp, Code::AudioVolumeDown, Code::AudioVolumeMute,
    Code::BrowserBack, Code::BrowserForward, Code::BrowserRefresh, Code::BrowserHome,
    Code::BrowserFavorites, Code::BrowserSearch,
];

/// Active "press any key" binding session.
struct HotkeyBinder {
    /// When the bind window closes and the previous hotkey is restored.
    deadline: Instant,
    /// Temporarily registered keys; the OS eats their presses while bound.
    temp: Vec<(HotKey, Code)>,
}

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
    hotkey_error: Option<String>,
    binder: Option<HotkeyBinder>,
}

impl QAutoClickApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: ClickerConfig,
        cmd_tx: Sender<EngineCommand>,
        status_rx: Receiver<EngineStatus>,
        engine: Option<JoinHandle<()>>,
        running: Arc<AtomicBool>,
        _hotkey_id: Arc<AtomicU32>,
    ) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        let hotkey_manager = GlobalHotKeyManager::new().ok();
        let mut app = Self {
            config,
            cmd_tx,
            status_rx,
            engine,
            running,
            clicks_done: 0,
            last_error: None,
            hotkey_manager,
            hotkey: HotKey::new(None, Code::F6),
            hotkey_error: None,
            binder: None,
        };
        app.register_active_hotkey();
        app
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

    /// Drain global hotkey events: toggles the clicker normally, captures
    /// the pressed key while a bind session is open.
    fn poll_events(&mut self) {
        let receiver = GlobalHotKeyEvent::receiver();
        while let Ok(event) = receiver.try_recv() {
            if event.state() != HotKeyState::Pressed {
                continue;
            }
            let captured = self
                .binder
                .as_ref()
                .and_then(|binder| binder.temp.iter().find(|(hotkey, _)| hotkey.id() == event.id))
                .map(|(_, code)| *code);
            if let Some(code) = captured {
                self.finish_binding(code);
                break; // binder state changed; the rest of the queue is safe to handle next frame
            }
            if self.binder.is_none() && event.id == self.hotkey.id() {
                self.toggle();
            }
        }
        let expired = self
            .binder
            .as_ref()
            .is_some_and(|binder| Instant::now() >= binder.deadline);
        if expired {
            self.cancel_binding();
        }
    }

    /// Flip running state and tell the engine.
    fn toggle(&mut self) {
        let was_running = self.running.fetch_xor(true, Ordering::SeqCst);
        let cmd = if was_running { EngineCommand::Stop } else { EngineCommand::Start };
        let _ = self.cmd_tx.send(cmd);
    }

    fn on_config_changed(&mut self) {
        self.config.save();
        let _ = self.cmd_tx.send(EngineCommand::SetConfig(self.config.clone()));
    }

    /// Open a bind window: grab every bindable key system-wide for 5 seconds.
    fn start_binding(&mut self) {
        let Some(manager) = self.hotkey_manager.as_ref() else {
            self.hotkey_error = Some("global hotkeys unavailable — cannot bind".to_owned());
            return;
        };
        // The currently active key joins the grab so it can be re-bound or replaced.
        let _ = manager.unregister(self.hotkey);
        let mut temp = Vec::new();
        for code in BINDABLE_CODES {
            let hotkey = HotKey::new(None, *code);
            if manager.register(hotkey).is_ok() {
                temp.push((hotkey, *code));
            }
        }
        self.hotkey_error = None;
        self.binder = Some(HotkeyBinder {
            deadline: Instant::now() + BIND_TIMEOUT,
            temp,
        });
    }

    /// Close the bind window without changing the hotkey.
    fn cancel_binding(&mut self) {
        let Some(binder) = self.binder.take() else {
            return;
        };
        if let Some(manager) = self.hotkey_manager.as_ref() {
            let hotkeys: Vec<HotKey> = binder.temp.into_iter().map(|(hotkey, _)| hotkey).collect();
            let _ = manager.unregister_all(&hotkeys);
        }
        self.register_active_hotkey();
    }

    /// Close the bind window, making `code` the new toggle hotkey.
    fn finish_binding(&mut self, code: Code) {
        if let Some(binder) = self.binder.take() {
            if let Some(manager) = self.hotkey_manager.as_ref() {
                let hotkeys: Vec<HotKey> = binder.temp.into_iter().map(|(hotkey, _)| hotkey).collect();
                let _ = manager.unregister_all(&hotkeys);
            }
        }
        self.config.hotkey_code = code.to_string();
        self.register_active_hotkey();
        self.on_config_changed();
    }

    /// (Re-)register the hotkey stored in the config.
    fn register_active_hotkey(&mut self) {
        self.hotkey_error = None;
        let Some(manager) = self.hotkey_manager.as_ref() else {
            return;
        };
        let code = match Code::from_str(&self.config.hotkey_code) {
            Ok(code) => code,
            Err(_) => {
                self.hotkey_error = Some(format!("unknown key: {}", self.config.hotkey_code));
                return;
            }
        };
        let hotkey = HotKey::new(None, code);
        match manager.register(hotkey) {
            Ok(()) => self.hotkey = hotkey,
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
        // Called even while the window is hidden: keep engine and hotkey state fresh.
        self.poll_status();
        self.poll_events();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_status();
        self.poll_events();

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
                                .range(0.001..=3_600_000.0)
                                .speed(10)
                                .suffix(" ms"),
                        )
                        .changed()
                    {
                        self.on_config_changed();
                    }
                    ui.weak(format!("≈ {:.0}/s", 1000.0 / self.config.interval_ms.max(0.001)));
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

                ui.label("Floating delay");
                ui.horizontal(|ui| {
                    let mut enabled = self.config.jitter.enabled;
                    if ui.checkbox(&mut enabled, "").changed() {
                        self.config.jitter.enabled = enabled;
                        self.on_config_changed();
                    }
                    if self.config.jitter.enabled {
                        ui.weak("-");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.config.jitter.offset_minus_ms)
                                    .range(0.0..=3_600_000.0)
                                    .speed(1)
                                    .suffix(" ms"),
                            )
                            .changed()
                        {
                            self.on_config_changed();
                        }
                        ui.weak("+");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.config.jitter.offset_plus_ms)
                                    .range(0.0..=3_600_000.0)
                                    .speed(1)
                                    .suffix(" ms"),
                            )
                            .changed()
                        {
                            self.on_config_changed();
                        }
                        ui.weak("speed");
                        if ui
                            .add(
                                egui::DragValue::new(&mut self.config.jitter.change_speed_ms_per_s)
                                    .range(0.0..=3_600_000.0)
                                    .speed(1)
                                    .suffix(" ms/s"),
                            )
                            .changed()
                        {
                            self.on_config_changed();
                        }
                    }
                });
                ui.end_row();

                ui.label("Toggle hotkey");
                ui.horizontal(|ui| {
                    let binding = self.binder.is_some();
                    if binding {
                        let remaining = self
                            .binder
                            .as_ref()
                            .map_or(0.0, |b| (b.deadline - Instant::now()).as_secs_f32().max(0.0));
                        if ui.button(format!("Cancel ({remaining:.1}s)")).clicked() {
                            self.cancel_binding();
                        }
                        ui.colored_label(egui::Color32::LIGHT_BLUE, "Press any key…");
                    } else {
                        ui.monospace(pretty_key(&self.config.hotkey_code));
                        if ui.button("Bind").clicked() {
                            self.start_binding();
                        }
                        ui.weak("(system-wide)");
                    }
                });
                ui.end_row();
            });

            if self.config.interval_ms < 1.0 {
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "⚠ Interval below 1 ms — high CPU load possible",
                );
            }
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

        // Keep polling engine status and global hotkeys even when idle/unfocused,
        // and keep the bind countdown ticking.
        let repaint = if self.binder.is_some() || self.running.load(Ordering::SeqCst) {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(100)
        };
        ui.ctx().request_repaint_after(repaint);
    }
}

/// Human-friendly name for a `keyboard_types::Code` name ("KeyA" → "A").
fn pretty_key(code_name: &str) -> String {
    if let Some(letter) = code_name.strip_prefix("Key") {
        if letter.len() == 1 {
            return letter.to_ascii_uppercase();
        }
    }
    if let Some(digit) = code_name.strip_prefix("Digit") {
        return digit.to_owned();
    }
    match code_name {
        "Minus" => "-".into(),
        "Equal" => "=".into(),
        "Comma" => ",".into(),
        "Period" => ".".into(),
        "Slash" => "/".into(),
        "Semicolon" => ";".into(),
        "Quote" => "'".into(),
        "Backquote" => "`".into(),
        "Backslash" => "\\".into(),
        "BracketLeft" => "[".into(),
        "BracketRight" => "]".into(),
        "ShiftLeft" => "L Shift".into(),
        "ShiftRight" => "R Shift".into(),
        "ControlLeft" => "L Ctrl".into(),
        "ControlRight" => "R Ctrl".into(),
        "AltLeft" => "L Alt".into(),
        "AltRight" => "R Alt".into(),
        "MetaLeft" => "L Win".into(),
        "MetaRight" => "R Win".into(),
        "ArrowUp" => "Up".into(),
        "ArrowDown" => "Down".into(),
        "ArrowLeft" => "Left".into(),
        "ArrowRight" => "Right".into(),
        other => other.to_owned(),
    }
}
