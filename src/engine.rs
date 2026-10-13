use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};

use enigo::{Button, Direction, Enigo, Mouse as _, Settings};

use crate::commands::{EngineCommand, EngineStatus};
use crate::config::{ClickerConfig, CountMode, MouseChoice};

/// Abstract click source so the engine can be tested without touching the OS.
pub trait Clicker: Send {
    /// Perform one full click with the given button.
    fn click(&mut self, button: MouseChoice) -> Result<(), String>;
}

/// Real implementation backed by `enigo` (SendInput on Windows, CGEvent on macOS, X11 on Linux).
pub struct EnigoClicker {
    enigo: Enigo,
}

impl EnigoClicker {
    pub fn new() -> Result<Self, String> {
        let enigo = Enigo::new(&Settings::default()).map_err(|e| format!("input backend init failed: {e}"))?;
        Ok(Self { enigo })
    }
}

impl Clicker for EnigoClicker {
    fn click(&mut self, button: MouseChoice) -> Result<(), String> {
        let button = match button {
            MouseChoice::Left => Button::Left,
            MouseChoice::Middle => Button::Middle,
            MouseChoice::Right => Button::Right,
        };
        self.enigo.button(button, Direction::Click).map_err(|e| e.to_string())
    }
}

/// Creates clickers on demand; injectable for tests.
pub type ClickerFactory = Box<dyn FnMut() -> Result<Box<dyn Clicker>, String> + Send>;

/// Engine thread body. Blocks until `EngineCommand::Shutdown` or channel disconnect.
///
/// While running, clicks are paced by waiting on the command channel with the
/// configured interval as timeout: a timeout means "time to click", any incoming
/// command (e.g. Stop) is handled instantly without waiting out the interval.
pub fn run(
    mut clicker_factory: ClickerFactory,
    cmd_rx: Receiver<EngineCommand>,
    status_tx: Sender<EngineStatus>,
    initial: ClickerConfig,
) {
    let mut config = initial;
    let mut clicker: Option<Box<dyn Clicker>> = None;
    let mut running = false;
    let mut clicks_done: u64 = 0;

    let mut set_clicker = |clicker: &mut Option<Box<dyn Clicker>>| -> Result<(), String> {
        if clicker.is_none() {
            *clicker = Some(clicker_factory()?);
        }
        Ok(())
    };

    loop {
        if !running {
            match cmd_rx.recv() {
                Ok(EngineCommand::Start) => {
                    if let Err(e) = set_clicker(&mut clicker) {
                        let _ = status_tx.send(EngineStatus::Error(e));
                        continue;
                    }
                    clicks_done = 0;
                    running = true;
                    let _ = status_tx.send(EngineStatus::Running { clicks_done });
                }
                Ok(EngineCommand::SetConfig(new_config)) => config = new_config,
                Ok(EngineCommand::Shutdown) | Err(_) => break,
                Ok(EngineCommand::Stop) => {}
            }
        } else {
            let timeout = config.interval();
            match cmd_rx.recv_timeout(timeout) {
                Ok(EngineCommand::Stop) | Err(RecvTimeoutError::Disconnected) => {
                    running = false;
                    let _ = status_tx.send(EngineStatus::Idle);
                }
                Ok(EngineCommand::Shutdown) => break,
                Ok(EngineCommand::SetConfig(new_config)) => config = new_config,
                Ok(EngineCommand::Start) => {
                    clicks_done = 0;
                    let _ = status_tx.send(EngineStatus::Running { clicks_done });
                }
                Err(RecvTimeoutError::Timeout) => {
                    let backend = clicker.as_mut().expect("clicker exists while running");
                    match backend.click(config.button) {
                        Ok(()) => {
                            clicks_done += 1;
                            let _ = status_tx.send(EngineStatus::Running { clicks_done });
                            if let CountMode::Fixed(limit) = config.count_mode {
                                if clicks_done >= u64::from(limit) {
                                    running = false;
                                    let _ = status_tx.send(EngineStatus::Idle);
                                }
                            }
                        }
                        Err(e) => {
                            running = false;
                            // Drop the backend so the next Start re-initializes it.
                            clicker = None;
                            let _ = status_tx.send(EngineStatus::Error(e));
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    struct FakeClicker {
        counter: Arc<AtomicU64>,
        fail: bool,
    }

    impl Clicker for FakeClicker {
        fn click(&mut self, _button: MouseChoice) -> Result<(), String> {
            if self.fail {
                Err("fake failure".into())
            } else {
                self.counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
    }

    fn fake_factory(counter: Arc<AtomicU64>, fail: bool) -> ClickerFactory {
        Box::new(move || {
            Ok(Box::new(FakeClicker {
                counter: counter.clone(),
                fail,
            }) as Box<dyn Clicker>)
        })
    }

    fn fast_config() -> ClickerConfig {
        ClickerConfig {
            interval_ms: 5.0,
            ..ClickerConfig::default()
        }
    }

    fn spawn_engine(
        factory: ClickerFactory,
        config: ClickerConfig,
    ) -> (Sender<EngineCommand>, Receiver<EngineStatus>, thread::JoinHandle<()>) {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || run(factory, cmd_rx, status_tx, config));
        (cmd_tx, status_rx, handle)
    }

    /// Waits until a matching status arrives or the deadline passes.
    fn wait_for_status(status_rx: &Receiver<EngineStatus>, deadline: Instant, pred: impl Fn(&EngineStatus) -> bool) -> EngineStatus {
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for status");
            match status_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(status) if pred(&status) => return status,
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(e) => panic!("status channel error: {e}"),
            }
        }
    }

    #[test]
    fn engine_performs_fixed_count_then_goes_idle() {
        let counter = Arc::new(AtomicU64::new(0));
        let mut config = fast_config();
        config.count_mode = CountMode::Fixed(5);
        let (cmd_tx, status_rx, handle) = spawn_engine(fake_factory(counter.clone(), false), config);

        cmd_tx.send(EngineCommand::Start).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_running_5 = false;
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for Idle");
            match status_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(EngineStatus::Idle) => break,
                Ok(EngineStatus::Running { clicks_done: 5 }) => saw_running_5 = true,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => continue,
                Err(e) => panic!("status channel error: {e}"),
            }
        }
        assert!(saw_running_5, "expected a Running {{ clicks_done: 5 }} status");
        assert_eq!(counter.load(Ordering::SeqCst), 5);

        cmd_tx.send(EngineCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn stop_is_instant_even_with_huge_interval() {
        let counter = Arc::new(AtomicU64::new(0));
        let mut config = fast_config();
        config.interval_ms = 10_000.0;
        let (cmd_tx, status_rx, handle) = spawn_engine(fake_factory(counter.clone(), false), config);

        cmd_tx.send(EngineCommand::Start).unwrap();
        thread::sleep(Duration::from_millis(50));
        let stop_sent = Instant::now();
        cmd_tx.send(EngineCommand::Stop).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        wait_for_status(&status_rx, deadline, |s| *s == EngineStatus::Idle);
        assert!(
            stop_sent.elapsed() < Duration::from_millis(500),
            "Stop took {:?}, engine should not wait out the interval",
            stop_sent.elapsed()
        );
        assert_eq!(counter.load(Ordering::SeqCst), 0);

        cmd_tx.send(EngineCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn click_error_is_reported_and_engine_recovers() {
        let counter = Arc::new(AtomicU64::new(0));
        let (cmd_tx, status_rx, handle) = spawn_engine(fake_factory(counter.clone(), true), fast_config());

        cmd_tx.send(EngineCommand::Start).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        wait_for_status(&status_rx, deadline, |s| matches!(s, EngineStatus::Error(_)));
        // The engine survived the failure and is idle again: a Stop must be accepted silently.
        cmd_tx.send(EngineCommand::Stop).unwrap();
        cmd_tx.send(EngineCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn set_config_while_idle_applies_to_next_run() {
        let counter = Arc::new(AtomicU64::new(0));
        let (cmd_tx, status_rx, handle) = spawn_engine(fake_factory(counter.clone(), false), fast_config());

        let mut config = fast_config();
        config.count_mode = CountMode::Fixed(2);
        cmd_tx.send(EngineCommand::SetConfig(config.clone())).unwrap();
        cmd_tx.send(EngineCommand::Start).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        wait_for_status(&status_rx, deadline, |s| *s == EngineStatus::Idle);
        assert_eq!(counter.load(Ordering::SeqCst), 2);

        cmd_tx.send(EngineCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }
}
