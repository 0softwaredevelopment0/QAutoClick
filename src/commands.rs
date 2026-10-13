use crate::config::ClickerConfig;

/// Commands sent from the GUI to the clicker engine thread.
#[derive(Debug, Clone)]
pub enum EngineCommand {
    /// Begin (or restart) clicking with the latest config.
    Start,
    /// Stop clicking immediately; the engine stays alive.
    Stop,
    /// Replace the config; applied between clicks.
    SetConfig(ClickerConfig),
    /// Stop clicking and terminate the engine thread.
    Shutdown,
}

/// Status updates sent from the engine thread back to the GUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineStatus {
    /// Engine is not clicking.
    Idle,
    /// Engine is clicking; includes the number of clicks done this run.
    Running { clicks_done: u64 },
    /// The engine could not click (input backend failure).
    Error(String),
}
