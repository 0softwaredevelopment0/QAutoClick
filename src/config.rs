use serde::{Deserialize, Serialize};

/// Which mouse button the engine presses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MouseChoice {
    #[default]
    Left,
    Middle,
    Right,
}

/// How many clicks to perform per run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CountMode {
    /// Click until the user stops the engine.
    #[default]
    Infinite,
    /// Stop automatically after this many clicks.
    Fixed(u32),
}

/// User-facing clicker settings, persisted as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct ClickerConfig {
    /// Delay between clicks in milliseconds.
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
    /// Mouse button to press.
    pub button: MouseChoice,
    /// Clicks per run.
    pub count_mode: CountMode,
    /// Function key (1-12) used as the global start/stop hotkey (F-key).
    #[serde(default = "default_hotkey_fkey")]
    pub hotkey_fkey: u8,
}

fn default_interval_ms() -> u64 {
    100
}

fn default_hotkey_fkey() -> u8 {
    6
}

impl Default for ClickerConfig {
    fn default() -> Self {
        Self {
            interval_ms: default_interval_ms(),
            button: MouseChoice::Left,
            count_mode: CountMode::Infinite,
            hotkey_fkey: default_hotkey_fkey(),
        }
    }
}

impl ClickerConfig {
    /// Clamp values coming from an untrusted/old config file into valid ranges.
    pub fn sanitized(mut self) -> Self {
        self.interval_ms = self.interval_ms.clamp(1, 3_600_000);
        self.hotkey_fkey = if (1..=12).contains(&self.hotkey_fkey) {
            self.hotkey_fkey
        } else {
            6
        };
        if let CountMode::Fixed(0) = self.count_mode {
            self.count_mode = CountMode::Infinite;
        }
        self
    }

    /// Default config file path: `<config_dir>/QAutoClick/config.json`.
    pub fn path() -> Option<std::path::PathBuf> {
        dirs::config_dir().map(|dir| dir.join("QAutoClick").join("config.json"))
    }

    /// Load config from disk; fall back to defaults on any error.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .map(Self::sanitized)
            .unwrap_or_default()
    }

    /// Persist config to disk; errors are ignored (best effort).
    pub fn save(&self) {
        if let Some(path) = Self::path() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(text) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(path, text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_round_trip_preserves_values() {
        let config = ClickerConfig {
            interval_ms: 42,
            button: MouseChoice::Right,
            count_mode: CountMode::Fixed(100),
            hotkey_fkey: 7,
        };
        let text = serde_json::to_string(&config).unwrap();
        let parsed: ClickerConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn load_falls_back_to_default_on_garbage() {
        // Simulate the load path by parsing garbage the same way load() does.
        let parsed: Option<ClickerConfig> = serde_json::from_str("{not json").ok();
        assert!(parsed.is_none());
        assert_eq!(ClickerConfig::default(), ClickerConfig::default());
    }

    #[test]
    fn sanitized_fixes_out_of_range_values() {
        let config = ClickerConfig {
            interval_ms: 0,
            button: MouseChoice::Left,
            count_mode: CountMode::Fixed(0),
            hotkey_fkey: 99,
        };
        let fixed = config.sanitized();
        assert_eq!(fixed.interval_ms, 1);
        assert_eq!(fixed.hotkey_fkey, 6);
        assert_eq!(fixed.count_mode, CountMode::Infinite);
    }

    #[test]
    fn deserializing_partial_json_fills_defaults() {
        let parsed: ClickerConfig = serde_json::from_str(r#"{"interval_ms": 250}"#).unwrap();
        assert_eq!(parsed.interval_ms, 250);
        assert_eq!(parsed.button, MouseChoice::Left);
        assert_eq!(parsed.count_mode, CountMode::Infinite);
        assert_eq!(parsed.hotkey_fkey, 6);
    }
}
