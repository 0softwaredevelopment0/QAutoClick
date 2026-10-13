mod app;
mod commands;
mod config;
mod engine;

use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::mpsc::channel;
use std::sync::Arc;

use eframe::egui;

use app::QAutoClickApp;
use config::ClickerConfig;
use engine::{ClickerFactory, EnigoClicker};

fn main() -> eframe::Result<()> {
    let config = ClickerConfig::load();
    let (cmd_tx, cmd_rx) = channel();
    let (status_tx, status_rx) = channel();
    let running = Arc::new(AtomicBool::new(false));
    let hotkey_id = Arc::new(AtomicU32::new(0));

    let factory: ClickerFactory = Box::new(|| EnigoClicker::new().map(|c| Box::new(c) as Box<dyn engine::Clicker>));
    let initial_config = config.clone();
    let engine_handle = std::thread::spawn(move || engine::run(factory, cmd_rx, status_tx, initial_config));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 480.0])
            .with_min_inner_size([360.0, 400.0]),
        ..Default::default()
    };

    eframe::run_native(
        "QAutoClick",
        options,
        Box::new(move |cc| {
            Ok(Box::new(QAutoClickApp::new(
                cc,
                config,
                cmd_tx,
                status_rx,
                Some(engine_handle),
                running,
                hotkey_id,
            )) as Box<dyn eframe::App>)
        }),
    )
}
