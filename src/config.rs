use std::str::FromStr;

use global_hotkey::hotkey::Code;
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

/// Floating (jitter) delay: the interval smoothly drifts toward a random
/// target within `[base - offset_minus_ms, base + offset_plus_ms]`,
/// changing at `change_speed_ms_per_s`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct JitterConfig {
    /// Whether the floating delay is active.
    pub enabled: bool,
    /// Maximum deviation below the base interval, ms.
    pub offset_minus_ms: f64,
    /// Maximum deviation above the base interval, ms.
    pub offset_plus_ms: f64,
    /// How fast the interval drifts toward its random target, ms per second.
    pub change_speed_ms_per_s: f64,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            offset_minus_ms: 5.0,
            offset_plus_ms: 5.0,
            change_speed_ms_per_s: 50.0,
        }
    }
}

impl JitterConfig {
    fn sanitized(mut self) -> Self {
        self.offset_minus_ms = self.offset_minus_ms.clamp(0.0, 3_600_000.0);
        self.offset_plus_ms = self.offset_plus_ms.clamp(0.0, 3_600_000.0);
        self.change_speed_ms_per_s = self.change_speed_ms_per_s.clamp(0.0, 3_600_000.0);
        self
    }
}

/// User-facing clicker settings, persisted as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct ClickerConfig {
    /// Delay between clicks in milliseconds; may be fractional (sub-millisecond allowed).
    #[serde(default = "default_interval_ms")]
    pub interval_ms: f64,
    /// Mouse button to press.
    pub button: MouseChoice,
    /// Clicks per run.
    pub count_mode: CountMode,
    /// Global toggle hotkey as a `keyboard_types::Code` name, e.g. "F6", "KeyA", "Space".
    #[serde(default = "default_hotkey_code")]
    pub hotkey_code: String,
    /// Floating delay (jitter) settings.
    pub jitter: JitterConfig,
}

fn default_interval_ms() -> f64 {
    100.0
}

fn default_hotkey_code() -> String {
    "F6".to_owned()
}

impl Default for ClickerConfig {
    fn default() -> Self {
        Self {
            interval_ms: default_interval_ms(),
            button: MouseChoice::Left,
            count_mode: CountMode::Infinite,
            hotkey_code: default_hotkey_code(),
            jitter: JitterConfig::default(),
        }
    }
}

impl ClickerConfig {
    /// Clamp values coming from an untrusted/old config file into valid ranges.
    pub fn sanitized(mut self) -> Self {
        self.interval_ms = if self.interval_ms.is_finite() && self.interval_ms > 0.0 {
            self.interval_ms.min(3_600_000.0)
        } else {
            default_interval_ms()
        };
        if Code::from_str(&self.hotkey_code).is_err() {
            self.hotkey_code = default_hotkey_code();
        }
        if let CountMode::Fixed(0) = self.count_mode {
            self.count_mode = CountMode::Infinite;
        }
        self.jitter = self.jitter.sanitized();
        self
    }

    /// Delay as a `Duration`, safe to hand to the engine (always > 0).
    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.interval_ms / 1000.0)
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
            interval_ms: 42.0,
            button: MouseChoice::Right,
            count_mode: CountMode::Fixed(100),
            hotkey_code: "KeyX".to_owned(),
            jitter: JitterConfig {
                enabled: true,
                offset_minus_ms: 3.0,
                offset_plus_ms: 7.0,
                change_speed_ms_per_s: 25.0,
            },
        };
        let text = serde_json::to_string(&config).unwrap();
        let parsed: ClickerConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn sanitized_keeps_sub_millisecond_but_fixes_non_positive() {
        let mut config = ClickerConfig::default();
        config.interval_ms = 0.25;
        config.hotkey_code = "NotAKey".to_owned();
        config.count_mode = CountMode::Fixed(0);
        let fixed = config.sanitized();
        assert_eq!(fixed.interval_ms, 0.25);
        assert_eq!(fixed.hotkey_code, "F6");
        assert_eq!(fixed.count_mode, CountMode::Infinite);

        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut config = ClickerConfig::default();
            config.interval_ms = bad;
            assert_eq!(config.sanitized().interval_ms, default_interval_ms());
        }
    }

    #[test]
    fn deserializing_partial_json_fills_defaults() {
        let parsed: ClickerConfig = serde_json::from_str(r#"{"interval_ms": 250}"#).unwrap();
        assert_eq!(parsed.interval_ms, 250.0);
        assert_eq!(parsed.button, MouseChoice::Left);
        assert_eq!(parsed.count_mode, CountMode::Infinite);
        assert_eq!(parsed.hotkey_code, "F6");
    }

    #[test]
    fn old_config_without_hotkey_code_still_loads() {
        // Configs from versions that stored an F-key index instead of a key code.
        let old = r#"{"interval_ms": 50, "button": "Left", "count_mode": "Infinite", "hotkey_fkey": 8}"#;
        let parsed: ClickerConfig = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.interval_ms, 50.0);
        assert_eq!(parsed.hotkey_code, "F6");
    }

    #[test]
    fn interval_converts_to_positive_duration() {
        let mut config = ClickerConfig::default();
        config.interval_ms = 0.5;
        assert_eq!(config.interval(), std::time::Duration::from_micros(500));
    }

    #[test]
    fn jitter_survives_round_trip_and_sanitizes_offsets() {
        let mut config = ClickerConfig::default();
        config.jitter = JitterConfig {
            enabled: true,
            offset_minus_ms: -3.0,
            offset_plus_ms: 8.0,
            change_speed_ms_per_s: -10.0,
        };
        let text = serde_json::to_string(&config).unwrap();
        let mut parsed: ClickerConfig = serde_json::from_str(&text).unwrap();
        assert!(parsed.jitter.enabled);
        parsed.jitter = parsed.jitter.sanitized();
        assert_eq!(parsed.jitter.offset_minus_ms, 0.0);
        assert_eq!(parsed.jitter.offset_plus_ms, 8.0);
        assert_eq!(parsed.jitter.change_speed_ms_per_s, 0.0);
    }

    #[test]
    fn old_config_without_jitter_loads_with_defaults() {
        let old = r#"{"interval_ms": 100}"#;
        let parsed: ClickerConfig = serde_json::from_str(old).unwrap();
        assert!(!parsed.jitter.enabled);
        assert_eq!(parsed.jitter.offset_minus_ms, 5.0);
        assert_eq!(parsed.jitter.offset_plus_ms, 5.0);
        assert_eq!(parsed.jitter.change_speed_ms_per_s, 50.0);
    }
}
