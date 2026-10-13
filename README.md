# QAutoClick

QAutoClick is a flexible and fast desktop auto-clicker for a variety of purposes. Built in Rust with a lightweight egui GUI, it stays responsive while clicking at any pace — from one click per minute to rapid fire.

## Features

- **Configurable interval** — delay between clicks from 1 ms upward, with a live clicks-per-second estimate
- **Mouse button choice** — left, middle or right click
- **Click count modes** — click infinitely or stop automatically after N clicks
- **Global hotkey** — start/stop with an F-key (F1–F12, default F6) from any application, even when the window is minimized
- **Live config** — every change applies instantly and is saved to disk
- **Profiles on disk** — settings persist in `<config_dir>/QAutoClick/config.json` (e.g. `%APPDATA%\QAutoClick\config.json` on Windows)

## Platform support

| OS      | Click simulation | Global hotkey |
|---------|------------------|---------------|
| Windows | Full (SendInput) | Full          |
| macOS   | Full (CGEvent)   | Full          |
| Linux   | X11 (libxdo)     | X11           |

Note: on Linux, Wayland sessions are not supported (input injection and global hotkeys are restricted there); run under X11/XWayland.

## Building

Requires a stable Rust toolchain.

```sh
cargo build --release
```

The binary is placed at `target/release/QAutoClick`.

## Running tests

```sh
cargo test
```

## Usage

1. Set the interval, mouse button and click count.
2. Press **Start** (or the global hotkey) — clicks begin at the current cursor position.
3. Press **Stop** (or the hotkey again) to halt instantly.

## License

See [LICENSE](LICENSE).
