use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::Instant;

use enigo::{Button, Direction, Enigo, Mouse as _, Settings};

use crate::commands::{EngineCommand, EngineStatus};
use crate::config::{ClickerConfig, CountMode, MouseChoice};

/// Waiting past this point happens on a busy spin so sub-millisecond targets
/// are not quantized to the OS timer quantum.
const SPIN_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(2);

/// Safety margin subtracted from blocking waits so control lands in the spin
/// phase before the target time (blocking waits round up to the OS quantum).
const SLEEP_MARGIN: std::time::Duration = std::time::Duration::from_millis(1);

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

/// Random target for the floating delay: somewhere in
/// `[base - offset_minus, base + offset_plus]`, never below 0.001 ms.
fn next_jitter_target(config: &ClickerConfig) -> f64 {
    let jitter = &config.jitter;
    let span = jitter.offset_minus_ms + jitter.offset_plus_ms;
    (config.interval_ms - jitter.offset_minus_ms + fastrand::f64() * span).max(0.001)
}

/// Drifting state of the floating delay for one run.
struct JitterRuntime {
    current: f64,
    target: f64,
}

impl JitterRuntime {
    fn new(config: &ClickerConfig) -> Self {
        Self {
            current: config.interval_ms,
            target: next_jitter_target(config),
        }
    }

    /// Advance the drift by the time spent since the previous click and
    /// return the interval to use for scheduling the next one.
    fn interval_for_next_click(&mut self, config: &ClickerConfig, dt: std::time::Duration) -> f64 {
        let step = config.jitter.change_speed_ms_per_s.max(0.0) * dt.as_secs_f64();
        let diff = self.target - self.current;
        if diff.abs() <= step {
            // Target reached: schedule with it and roll a new one.
            self.current = self.target;
            self.target = next_jitter_target(config);
        } else {
            self.current += diff.signum() * step;
        }
        self.current.max(0.001)
    }
}

/// Outcome of waiting for the next click time.
enum Wait {
    /// Target time reached: perform a click.
    Proceed,
    /// A command arrived before the target time.
    Command(EngineCommand),
    /// Command channel closed: GUI is gone.
    Disconnected,
}

/// Windows quantizes blocking waits (sleep, recv_timeout) to the system timer
/// quantum — 15.6 ms by default, which caps the clicker at ~64 CPS. Raise the
/// resolution to 1 ms while the engine is clicking; undo it when it stops.
#[cfg(target_os = "windows")]
fn timer_resolution(active: bool) {
    use windows_sys::Win32::Media::{timeBeginPeriod, timeEndPeriod};
    unsafe {
        if active {
            timeBeginPeriod(1);
        } else {
            timeEndPeriod(1);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn timer_resolution(_active: bool) {}

/// Wait until `target`, staying responsive to commands the whole time.
///
/// While more than `SPIN_THRESHOLD` remains, block on the command channel
/// (zero CPU cost). The final stretch busy-polls the channel: the target time
/// is hit with sub-millisecond precision and Stop/SetConfig stay instant.
fn wait_for_target(cmd_rx: &Receiver<EngineCommand>, target: Instant) -> Wait {
    loop {
        let now = Instant::now();
        if now >= target {
            return Wait::Proceed;
        }
        let remaining = target - now;
        if remaining > SPIN_THRESHOLD {
            match cmd_rx.recv_timeout(remaining - SLEEP_MARGIN) {
                Ok(cmd) => return Wait::Command(cmd),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Wait::Disconnected,
            }
        }
        match cmd_rx.try_recv() {
            Ok(cmd) => return Wait::Command(cmd),
            Err(TryRecvError::Disconnected) => return Wait::Disconnected,
            Err(TryRecvError::Empty) => std::hint::spin_loop(),
        }
    }
}

/// Engine thread body. Blocks until `EngineCommand::Shutdown` or channel disconnect.
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
    // Next scheduled click time; meaningful only while running.
    let mut next_click_at = Instant::now();
    // Floating delay state: Some while the run has jitter enabled.
    let mut jitter: Option<JitterRuntime> = None;
    // When the interval of the previous click was scheduled.
    let mut last_scheduled_at = Instant::now();

    let start_run = |config: &ClickerConfig| -> Option<JitterRuntime> {
        if config.jitter.enabled {
            Some(JitterRuntime::new(config))
        } else {
            None
        }
    };

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
                    timer_resolution(true);
                    next_click_at = Instant::now() + config.interval();
                    last_scheduled_at = Instant::now();
                    jitter = start_run(&config);
                    let _ = status_tx.send(EngineStatus::Running { clicks_done });
                }
                Ok(EngineCommand::SetConfig(new_config)) => {
                    // (Re-)initialize the drift when the feature is toggled mid-run.
                    if running && new_config.jitter.enabled != config.jitter.enabled {
                        jitter = start_run(&new_config);
                    }
                    config = new_config;
                }
                Ok(EngineCommand::Shutdown) | Err(_) => break,
                Ok(EngineCommand::Stop) => {}
            }
        } else {
            match wait_for_target(&cmd_rx, next_click_at) {
                Wait::Command(EngineCommand::Stop) | Wait::Disconnected => {
                    running = false;
                    timer_resolution(false);
                    let _ = status_tx.send(EngineStatus::Idle);
                }
                Wait::Command(EngineCommand::Shutdown) => break,
                Wait::Command(EngineCommand::SetConfig(new_config)) => {
                    if new_config.jitter.enabled != config.jitter.enabled {
                        jitter = start_run(&new_config);
                    }
                    config = new_config;
                }
                Wait::Command(EngineCommand::Start) => {
                    clicks_done = 0;
                    next_click_at = Instant::now() + config.interval();
                    last_scheduled_at = Instant::now();
                    jitter = start_run(&config);
                    let _ = status_tx.send(EngineStatus::Running { clicks_done });
                }
                Wait::Proceed => {
                    let now = Instant::now();
                    let used_ms = match &mut jitter {
                        Some(state) => state.interval_for_next_click(&config, now - last_scheduled_at),
                        None => config.interval_ms,
                    };
                    next_click_at += std::time::Duration::from_secs_f64(used_ms.max(0.001) / 1000.0);
                    last_scheduled_at = now;
                    let backend = clicker.as_mut().expect("clicker exists while running");
                    match backend.click(config.button) {
                        Ok(()) => {
                            clicks_done += 1;
                            let _ = status_tx.send(EngineStatus::Running { clicks_done });
                            if let CountMode::Fixed(limit) = config.count_mode {
                                if clicks_done >= u64::from(limit) {
                                    running = false;
                                    timer_resolution(false);
                                    let _ = status_tx.send(EngineStatus::Idle);
                                }
                            }
                        }
                        Err(e) => {
                            running = false;
                            timer_resolution(false);
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
    use crate::config::JitterConfig;
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
    fn jitter_targets_stay_within_offsets() {
        let mut config = fast_config();
        config.interval_ms = 50.0;
        config.jitter = JitterConfig {
            enabled: true,
            offset_minus_ms: 20.0,
            offset_plus_ms: 5.0,
            change_speed_ms_per_s: 100.0,
        };
        for _ in 0..10_000 {
            let target = next_jitter_target(&config);
            assert!(
                (30.0..=55.0).contains(&target),
                "target {target} outside [base-minus, base+plus]"
            );
        }
    }

    #[test]
    fn jitter_drifts_toward_target_at_configured_speed() {
        let mut config = fast_config();
        config.jitter = JitterConfig {
            enabled: true,
            offset_minus_ms: 100.0,
            offset_plus_ms: 100.0,
            change_speed_ms_per_s: 100.0,
        };
        let mut state = JitterRuntime::new(&config);
        state.current = 100.0;
        state.target = 110.0;

        // 100 ms/s means 5 ms of drift per 0.05 s tick: 100 -> 105.
        let used = state.interval_for_next_click(&config, Duration::from_millis(50));
        assert_eq!(used, 105.0);
        // Remaining 5 ms are covered by a 0.05 s tick (step = 5 ms): 105 -> 110.
        let used = state.interval_for_next_click(&config, Duration::from_millis(50));
        assert_eq!(used, 110.0);
        // Target reached: a new one has been rolled within the offsets.
        assert!((0.001..=200.0).contains(&state.target));
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

    #[test]
    fn sub_millisecond_interval_reaches_thousands_of_cps() {
        let counter = Arc::new(AtomicU64::new(0));
        let mut config = fast_config();
        config.interval_ms = 0.1;
        config.count_mode = CountMode::Fixed(2000);
        let (cmd_tx, status_rx, handle) = spawn_engine(fake_factory(counter.clone(), false), config);

        let started = Instant::now();
        cmd_tx.send(EngineCommand::Start).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for Idle");
            match status_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(EngineStatus::Idle) => break,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => continue,
                Err(e) => panic!("status channel error: {e}"),
            }
        }
        let elapsed = started.elapsed();
        assert_eq!(counter.load(Ordering::SeqCst), 2000);
        // Without high-resolution pacing this run takes ~31 s (2000 x 15.6 ms quantum).
        assert!(
            elapsed < Duration::from_secs(2),
            "2000 clicks at 0.1 ms took {elapsed:?} — waits are quantized again"
        );
        let cps = 2000.0 / elapsed.as_secs_f64();
        assert!(cps > 1000.0, "expected >1000 CPS, got {cps:.0}");

        cmd_tx.send(EngineCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn floating_delay_drifts_the_interval_between_clicks() {
        let counter = Arc::new(AtomicU64::new(0));
        let mut config = fast_config();
        config.interval_ms = 10.0;
        config.jitter = JitterConfig {
            enabled: true,
            // Targets wander in [1, 10] ms — average well below the 10 ms base.
            offset_minus_ms: 9.0,
            offset_plus_ms: 0.0,
            change_speed_ms_per_s: 10_000.0,
        };
        config.count_mode = CountMode::Fixed(200);
        let (cmd_tx, status_rx, handle) = spawn_engine(fake_factory(counter.clone(), false), config);

        let started = Instant::now();
        cmd_tx.send(EngineCommand::Start).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for Idle");
            match status_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(EngineStatus::Idle) => break,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => continue,
                Err(e) => panic!("status channel error: {e}"),
            }
        }
        let elapsed = started.elapsed();
        assert_eq!(counter.load(Ordering::SeqCst), 200);
        // With the base interval alone this run needs >= 2 s; drifting to
        // [1, 10] ms targets averages roughly half of that.
        assert!(
            elapsed < Duration::from_millis(1700),
            "200 clicks took {elapsed:?} — the floating delay had no effect"
        );

        cmd_tx.send(EngineCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }

    /// Manual benchmark of the engine loop ceiling with a no-op clicker:
    /// `cargo test --release engine_ceiling -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark, run manually"]
    fn engine_ceiling_benchmark() {        let counter = Arc::new(AtomicU64::new(0));
        let mut config = fast_config();
        config.interval_ms = 0.001;
        config.count_mode = CountMode::Fixed(100_000);
        let (cmd_tx, status_rx, handle) = spawn_engine(fake_factory(counter.clone(), false), config);

        let started = Instant::now();
        cmd_tx.send(EngineCommand::Start).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for Idle");
            match status_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(EngineStatus::Idle) => break,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => continue,
                Err(e) => panic!("status channel error: {e}"),
            }
        }
        let elapsed = started.elapsed();
        let cps = 100_000.0 / elapsed.as_secs_f64();
        println!("engine ceiling: 100000 clicks in {elapsed:.3?} -> {cps:.0} CPS");
        assert_eq!(counter.load(Ordering::SeqCst), 100_000);

        cmd_tx.send(EngineCommand::Shutdown).unwrap();
        handle.join().unwrap();
    }
}
