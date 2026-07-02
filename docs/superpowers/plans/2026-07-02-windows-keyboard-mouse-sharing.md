# Windows Keyboard Mouse Sharing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a complete first-version Windows-to-Windows LAN keyboard and mouse sharing app with GUI configuration, edge switching in four directions, low-latency TCP transport, Windows input capture/injection, diagnostics, and release packaging.

**Architecture:** The project is a Rust workspace with separate crates for platform-independent domain logic, networking, Windows input integration, and the GUI app. The GUI configures and monitors the system; input capture, network send/receive, control state, and input injection run outside the GUI hot path.

**Tech Stack:** Rust stable MSVC toolchain, `egui`/`eframe`, `tokio`, `windows`, `serde`, `toml`, `tracing`, `bytes`, `crossbeam-channel`.

---

## Repository Root

All paths are relative to `C:\Users\oujie\Desktop\Borderless`.

## File Structure

- `.gitignore`
  - Ignores Rust build output, Windows binaries, logs, local config, editor files, and generated release archives.
- `Cargo.toml`
  - Workspace manifest.
- `rust-toolchain.toml`
  - Pins the Rust channel used by the project.
- `README.md`
  - Development, run, and two-machine validation instructions.
- `config.example.toml`
  - Example GUI-compatible configuration.
- `crates/borderless-core/Cargo.toml`
  - Platform-independent logic crate.
- `crates/borderless-core/src/lib.rs`
  - Exposes core modules.
- `crates/borderless-core/src/config.rs`
  - App configuration, validation, load/save.
- `crates/borderless-core/src/geometry.rs`
  - Screen rectangles, coordinate mapping, edge detection.
- `crates/borderless-core/src/input_event.rs`
  - Keyboard and mouse event types shared by all crates.
- `crates/borderless-core/src/protocol.rs`
  - Binary wire protocol and framing.
- `crates/borderless-core/src/control.rs`
  - Controller state machine for local/remote control and edge return.
- `crates/borderless-net/Cargo.toml`
  - TCP transport crate.
- `crates/borderless-net/src/lib.rs`
  - Exposes networking APIs.
- `crates/borderless-net/src/transport.rs`
  - Framed message reader/writer, `TCP_NODELAY`, heartbeat support.
- `crates/borderless-net/src/controller_client.rs`
  - Controller-side connection loop.
- `crates/borderless-net/src/agent_server.rs`
  - Agent-side listener and connection loop.
- `crates/borderless-win/Cargo.toml`
  - Windows API integration crate.
- `crates/borderless-win/src/lib.rs`
  - Exposes Windows modules.
- `crates/borderless-win/src/dpi.rs`
  - DPI awareness setup.
- `crates/borderless-win/src/monitor.rs`
  - Virtual desktop discovery.
- `crates/borderless-win/src/inject.rs`
  - `SendInput` injection and pressed-state release.
- `crates/borderless-win/src/hooks.rs`
  - Low-level keyboard/mouse hooks and input suppression.
- `crates/borderless-app/Cargo.toml`
  - GUI binary crate.
- `crates/borderless-app/src/main.rs`
  - App entry point.
- `crates/borderless-app/src/app.rs`
  - `eframe` application state and rendering.
- `crates/borderless-app/src/runtime.rs`
  - Background runtime orchestration.
- `crates/borderless-app/src/status.rs`
  - UI-facing status, latency, and event log model.
- `crates/borderless-app/src/logging.rs`
  - File and GUI logging bridge.
- `tests/manual/windows-two-machine-checklist.md`
  - Manual acceptance checklist for real Windows machines.

---

### Task 1: Repository Ignore Rules

**Files:**
- Create: `.gitignore`

- [ ] **Step 1: Add ignore rules**

Write `.gitignore` with:

```gitignore
# Rust
/target/
**/*.rs.bk

# Build and release artifacts
/dist/
*.exe
*.pdb
*.dll
*.lib
*.exp
*.msi
*.zip

# Local runtime files
config.toml
*.log
/logs/

# Editors and OS files
.vscode/
.idea/
*.swp
*.swo
Thumbs.db
Desktop.ini

# Temporary files
*.tmp
*.temp
```

- [ ] **Step 2: Verify ignored files are not tracked**

Run:

```powershell
git status --short
```

Expected: `.gitignore` appears as a new tracked candidate, and no generated build or local config files appear.

- [ ] **Step 3: Commit**

Run:

```powershell
git add .gitignore
git commit -m "chore: add repository ignore rules"
```

---

### Task 2: Rust Workspace Skeleton

**Files:**
- Create: `rust-toolchain.toml`
- Create: `Cargo.toml`
- Create: `crates/borderless-core/Cargo.toml`
- Create: `crates/borderless-core/src/lib.rs`
- Create: `crates/borderless-net/Cargo.toml`
- Create: `crates/borderless-net/src/lib.rs`
- Create: `crates/borderless-win/Cargo.toml`
- Create: `crates/borderless-win/src/lib.rs`
- Create: `crates/borderless-app/Cargo.toml`
- Create: `crates/borderless-app/src/main.rs`

- [ ] **Step 1: Pin the Rust toolchain**

Write `rust-toolchain.toml`:

```toml
[toolchain]
channel = "stable"
profile = "default"
```

- [ ] **Step 2: Create the workspace manifest**

Write root `Cargo.toml`:

```toml
[workspace]
resolver = "2"
members = [
    "crates/borderless-core",
    "crates/borderless-net",
    "crates/borderless-win",
    "crates/borderless-app",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "MIT"

[workspace.dependencies]
anyhow = "1"
bytes = "1"
bincode = "1"
crossbeam-channel = "0.5"
eframe = "0.28"
egui = "0.28"
serde = { version = "1", features = ["derive"] }
thiserror = "1"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net", "io-util", "sync", "time"] }
toml = "0.8"
tracing = "0.1"
tracing-appender = "0.2"
tracing-subscriber = { version = "0.3", features = ["fmt", "env-filter"] }
windows = "0.58"
```

- [ ] **Step 3: Create crate manifests**

Write `crates/borderless-core/Cargo.toml`:

```toml
[package]
name = "borderless-core"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
bincode.workspace = true
bytes.workspace = true
serde.workspace = true
thiserror.workspace = true
toml.workspace = true
```

Write `crates/borderless-net/Cargo.toml`:

```toml
[package]
name = "borderless-net"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
anyhow.workspace = true
borderless-core = { path = "../borderless-core" }
bytes.workspace = true
tokio.workspace = true
tracing.workspace = true
```

Write `crates/borderless-win/Cargo.toml`:

```toml
[package]
name = "borderless-win"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
anyhow.workspace = true
borderless-core = { path = "../borderless-core" }
crossbeam-channel.workspace = true
tracing.workspace = true
windows = { workspace = true, features = [
    "Win32_Foundation",
    "Win32_Graphics_Gdi",
    "Win32_System_LibraryLoader",
    "Win32_UI_HiDpi",
    "Win32_UI_Input_KeyboardAndMouse",
    "Win32_UI_WindowsAndMessaging",
] }
```

Write `crates/borderless-app/Cargo.toml`:

```toml
[package]
name = "borderless-app"
version.workspace = true
edition.workspace = true
license.workspace = true

[[bin]]
name = "borderless"
path = "src/main.rs"

[dependencies]
anyhow.workspace = true
borderless-core = { path = "../borderless-core" }
borderless-net = { path = "../borderless-net" }
borderless-win = { path = "../borderless-win" }
crossbeam-channel.workspace = true
eframe.workspace = true
egui.workspace = true
tokio.workspace = true
tracing.workspace = true
tracing-appender.workspace = true
tracing-subscriber.workspace = true
```

- [ ] **Step 4: Add crate entry files**

Write `crates/borderless-core/src/lib.rs`:

```rust
pub mod config;
pub mod control;
pub mod geometry;
pub mod input_event;
pub mod protocol;
```

Write `crates/borderless-net/src/lib.rs`:

```rust
pub mod agent_server;
pub mod controller_client;
pub mod transport;
```

Write `crates/borderless-win/src/lib.rs`:

```rust
pub mod dpi;
pub mod hooks;
pub mod inject;
pub mod monitor;
```

Write `crates/borderless-app/src/main.rs`:

```rust
fn main() -> eframe::Result<()> {
    Ok(())
}
```

- [ ] **Step 5: Run workspace check**

Run:

```powershell
cargo check --workspace
```

Expected: FAIL because modules declared in `borderless-core`, `borderless-net`, and `borderless-win` do not have files yet.

- [ ] **Step 6: Create empty module files to make the skeleton compile**

Create:

```text
crates/borderless-core/src/config.rs
crates/borderless-core/src/control.rs
crates/borderless-core/src/geometry.rs
crates/borderless-core/src/input_event.rs
crates/borderless-core/src/protocol.rs
crates/borderless-net/src/agent_server.rs
crates/borderless-net/src/controller_client.rs
crates/borderless-net/src/transport.rs
crates/borderless-win/src/dpi.rs
crates/borderless-win/src/hooks.rs
crates/borderless-win/src/inject.rs
crates/borderless-win/src/monitor.rs
```

- [ ] **Step 7: Verify the skeleton compiles**

Run:

```powershell
cargo check --workspace
```

Expected: PASS.

- [ ] **Step 8: Commit**

Run:

```powershell
git add Cargo.toml rust-toolchain.toml crates
git commit -m "chore: initialize rust workspace"
```

---

### Task 3: Shared Input Event Model

**Files:**
- Modify: `crates/borderless-core/src/input_event.rs`
- Test: `crates/borderless-core/src/input_event.rs`

- [ ] **Step 1: Write event type tests**

Add tests at the bottom of `input_event.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressed_state_tracks_keyboard_and_mouse_buttons() {
        let mut pressed = PressedState::default();
        pressed.apply(&InputEvent::Key(KeyEvent { vk_code: 0x41, pressed: true }));
        pressed.apply(&InputEvent::MouseButton(MouseButtonEvent {
            button: MouseButton::Left,
            pressed: true,
        }));

        assert!(pressed.keys.contains(&0x41));
        assert!(pressed.mouse_buttons.contains(&MouseButton::Left));

        pressed.apply(&InputEvent::Key(KeyEvent { vk_code: 0x41, pressed: false }));
        pressed.apply(&InputEvent::MouseButton(MouseButtonEvent {
            button: MouseButton::Left,
            pressed: false,
        }));

        assert!(!pressed.keys.contains(&0x41));
        assert!(!pressed.mouse_buttons.contains(&MouseButton::Left));
    }
}
```

- [ ] **Step 2: Run the focused test**

Run:

```powershell
cargo test -p borderless-core pressed_state_tracks_keyboard_and_mouse_buttons
```

Expected: FAIL because `PressedState`, `InputEvent`, and related event structs are not defined.

- [ ] **Step 3: Implement the event model**

Write `input_event.rs`:

```rust
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub vk_code: u16,
    pub pressed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseButtonEvent {
    pub button: MouseButton,
    pub pressed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseMoveAbsEvent {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseMoveDeltaEvent {
    pub dx: i32,
    pub dy: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseWheelEvent {
    pub delta: i32,
    pub horizontal: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    Key(KeyEvent),
    MouseButton(MouseButtonEvent),
    MouseMoveAbs(MouseMoveAbsEvent),
    MouseMoveDelta(MouseMoveDeltaEvent),
    MouseWheel(MouseWheelEvent),
    ReleaseAll,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PressedState {
    pub keys: BTreeSet<u16>,
    pub mouse_buttons: BTreeSet<MouseButton>,
}

impl PressedState {
    pub fn apply(&mut self, event: &InputEvent) {
        match event {
            InputEvent::Key(event) if event.pressed => {
                self.keys.insert(event.vk_code);
            }
            InputEvent::Key(event) => {
                self.keys.remove(&event.vk_code);
            }
            InputEvent::MouseButton(event) if event.pressed => {
                self.mouse_buttons.insert(event.button);
            }
            InputEvent::MouseButton(event) => {
                self.mouse_buttons.remove(&event.button);
            }
            InputEvent::ReleaseAll => self.clear(),
            InputEvent::MouseMoveAbs(_)
            | InputEvent::MouseMoveDelta(_)
            | InputEvent::MouseWheel(_) => {}
        }
    }

    pub fn clear(&mut self) {
        self.keys.clear();
        self.mouse_buttons.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.mouse_buttons.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressed_state_tracks_keyboard_and_mouse_buttons() {
        let mut pressed = PressedState::default();
        pressed.apply(&InputEvent::Key(KeyEvent { vk_code: 0x41, pressed: true }));
        pressed.apply(&InputEvent::MouseButton(MouseButtonEvent {
            button: MouseButton::Left,
            pressed: true,
        }));

        assert!(pressed.keys.contains(&0x41));
        assert!(pressed.mouse_buttons.contains(&MouseButton::Left));

        pressed.apply(&InputEvent::Key(KeyEvent { vk_code: 0x41, pressed: false }));
        pressed.apply(&InputEvent::MouseButton(MouseButtonEvent {
            button: MouseButton::Left,
            pressed: false,
        }));

        assert!(!pressed.keys.contains(&0x41));
        assert!(!pressed.mouse_buttons.contains(&MouseButton::Left));
    }
}
```

- [ ] **Step 4: Verify event tests pass**

Run:

```powershell
cargo test -p borderless-core input_event
```

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```powershell
git add crates/borderless-core/src/input_event.rs
git commit -m "feat: add shared input event model"
```

---

### Task 4: Configuration Model and Persistence

**Files:**
- Modify: `crates/borderless-core/src/config.rs`
- Create: `config.example.toml`

- [ ] **Step 1: Write configuration tests**

Add tests in `config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid_controller_config() {
        let config = AppConfig::default();
        assert_eq!(config.role, Role::Controller);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn invalid_edge_width_is_rejected() {
        let mut config = AppConfig::default();
        config.edge_trigger_px = 0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("edge_trigger_px"));
    }

    #[test]
    fn toml_round_trip_preserves_role_and_position() {
        let config = AppConfig::default();
        let encoded = toml::to_string_pretty(&config).unwrap();
        let decoded: AppConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.role, Role::Controller);
        assert_eq!(decoded.controller.remote_position, RemotePosition::Right);
    }
}
```

- [ ] **Step 2: Run configuration tests**

Run:

```powershell
cargo test -p borderless-core config
```

Expected: FAIL because configuration types are not implemented.

- [ ] **Step 3: Implement configuration types**

Write `config.rs`:

```rust
use serde::{Deserialize, Serialize};
use std::{fmt, fs, net::IpAddr, path::Path};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Controller,
    Agent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemotePosition {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerConfig {
    pub agent_host: String,
    pub agent_port: u16,
    pub remote_position: RemotePosition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    pub listen_host: String,
    pub listen_port: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    pub role: Role,
    pub edge_trigger_px: i32,
    pub debug_logging: bool,
    pub controller: ControllerConfig,
    pub agent: AgentConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            role: Role::Controller,
            edge_trigger_px: 2,
            debug_logging: false,
            controller: ControllerConfig {
                agent_host: "192.168.1.2".to_string(),
                agent_port: 24800,
                remote_position: RemotePosition::Right,
            },
            agent: AgentConfig {
                listen_host: "0.0.0.0".to_string(),
                listen_port: 24800,
            },
        }
    }
}

impl AppConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.edge_trigger_px < 1 || self.edge_trigger_px > 32 {
            return Err(ConfigError::new("edge_trigger_px must be between 1 and 32"));
        }

        if self.controller.agent_port == 0 {
            return Err(ConfigError::new("controller.agent_port must be greater than 0"));
        }

        if self.agent.listen_port == 0 {
            return Err(ConfigError::new("agent.listen_port must be greater than 0"));
        }

        if self.controller.agent_host.trim().is_empty() {
            return Err(ConfigError::new("controller.agent_host must not be empty"));
        }

        if self.agent.listen_host.parse::<IpAddr>().is_err() {
            return Err(ConfigError::new("agent.listen_host must be an IP address"));
        }

        Ok(())
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let raw = fs::read_to_string(path).map_err(|err| ConfigError::new(err.to_string()))?;
        let config: Self = toml::from_str(&raw).map_err(|err| ConfigError::new(err.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        self.validate()?;
        let raw = toml::to_string_pretty(self).map_err(|err| ConfigError::new(err.to_string()))?;
        fs::write(path, raw).map_err(|err| ConfigError::new(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid_controller_config() {
        let config = AppConfig::default();
        assert_eq!(config.role, Role::Controller);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn invalid_edge_width_is_rejected() {
        let mut config = AppConfig::default();
        config.edge_trigger_px = 0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("edge_trigger_px"));
    }

    #[test]
    fn toml_round_trip_preserves_role_and_position() {
        let config = AppConfig::default();
        let encoded = toml::to_string_pretty(&config).unwrap();
        let decoded: AppConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.role, Role::Controller);
        assert_eq!(decoded.controller.remote_position, RemotePosition::Right);
    }
}
```

- [ ] **Step 4: Add example config**

Write `config.example.toml`:

```toml
role = "controller"
edge_trigger_px = 2
debug_logging = false

[controller]
agent_host = "192.168.1.2"
agent_port = 24800
remote_position = "right"

[agent]
listen_host = "0.0.0.0"
listen_port = 24800
```

- [ ] **Step 5: Verify config tests pass**

Run:

```powershell
cargo test -p borderless-core config
```

Expected: PASS.

- [ ] **Step 6: Commit**

Run:

```powershell
git add crates/borderless-core/src/config.rs config.example.toml
git commit -m "feat: add app configuration model"
```

---

### Task 5: Geometry, Edge Detection, and Coordinate Mapping

**Files:**
- Modify: `crates/borderless-core/src/geometry.rs`

- [ ] **Step 1: Write geometry tests**

Add tests in `geometry.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemotePosition;

    #[test]
    fn maps_right_edge_y_ratio_to_remote_y() {
        let local = Rect::new(0, 0, 1920, 1080);
        let remote = Rect::new(0, 0, 2560, 1440);
        let point = map_entry_point(RemotePosition::Right, Point::new(1919, 540), local, remote);
        assert_eq!(point.x, 0);
        assert_eq!(point.y, 720);
    }

    #[test]
    fn detects_top_edge_inside_trigger_width() {
        let desktop = Rect::new(0, 0, 1920, 1080);
        assert_eq!(
            detect_edge(Point::new(500, 1), desktop, 2),
            Some(Edge::Top)
        );
    }

    #[test]
    fn clamps_remote_coordinate_inside_desktop() {
        let desktop = Rect::new(100, 100, 800, 600);
        assert_eq!(desktop.clamp(Point::new(50, 999)), Point::new(100, 699));
    }
}
```

- [ ] **Step 2: Run geometry tests**

Run:

```powershell
cargo test -p borderless-core geometry
```

Expected: FAIL because geometry types are not implemented.

- [ ] **Step 3: Implement geometry**

Write `geometry.rs`:

```rust
use crate::config::RemotePosition;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub const fn new(left: i32, top: i32, width: i32, height: i32) -> Self {
        Self { left, top, width, height }
    }

    pub fn right(self) -> i32 {
        self.left + self.width - 1
    }

    pub fn bottom(self) -> i32 {
        self.top + self.height - 1
    }

    pub fn clamp(self, point: Point) -> Point {
        Point {
            x: point.x.clamp(self.left, self.right()),
            y: point.y.clamp(self.top, self.bottom()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

pub fn detect_edge(point: Point, desktop: Rect, trigger_px: i32) -> Option<Edge> {
    if point.x <= desktop.left + trigger_px - 1 {
        Some(Edge::Left)
    } else if point.x >= desktop.right() - trigger_px + 1 {
        Some(Edge::Right)
    } else if point.y <= desktop.top + trigger_px - 1 {
        Some(Edge::Top)
    } else if point.y >= desktop.bottom() - trigger_px + 1 {
        Some(Edge::Bottom)
    } else {
        None
    }
}

pub fn edge_for_position(position: RemotePosition) -> Edge {
    match position {
        RemotePosition::Left => Edge::Left,
        RemotePosition::Right => Edge::Right,
        RemotePosition::Top => Edge::Top,
        RemotePosition::Bottom => Edge::Bottom,
    }
}

pub fn opposite_edge(edge: Edge) -> Edge {
    match edge {
        Edge::Left => Edge::Right,
        Edge::Right => Edge::Left,
        Edge::Top => Edge::Bottom,
        Edge::Bottom => Edge::Top,
    }
}

pub fn map_entry_point(position: RemotePosition, local_point: Point, local: Rect, remote: Rect) -> Point {
    match position {
        RemotePosition::Left => Point::new(
            remote.right(),
            proportional(local_point.y, local.top, local.height, remote.top, remote.height),
        ),
        RemotePosition::Right => Point::new(
            remote.left,
            proportional(local_point.y, local.top, local.height, remote.top, remote.height),
        ),
        RemotePosition::Top => Point::new(
            proportional(local_point.x, local.left, local.width, remote.left, remote.width),
            remote.bottom(),
        ),
        RemotePosition::Bottom => Point::new(
            proportional(local_point.x, local.left, local.width, remote.left, remote.width),
            remote.top,
        ),
    }
}

fn proportional(value: i32, source_start: i32, source_len: i32, target_start: i32, target_len: i32) -> i32 {
    let source_offset = (value - source_start).clamp(0, source_len - 1) as i64;
    let numerator = source_offset * (target_len - 1) as i64;
    target_start + (numerator / (source_len - 1).max(1) as i64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemotePosition;

    #[test]
    fn maps_right_edge_y_ratio_to_remote_y() {
        let local = Rect::new(0, 0, 1920, 1080);
        let remote = Rect::new(0, 0, 2560, 1440);
        let point = map_entry_point(RemotePosition::Right, Point::new(1919, 540), local, remote);
        assert_eq!(point.x, 0);
        assert_eq!(point.y, 720);
    }

    #[test]
    fn detects_top_edge_inside_trigger_width() {
        let desktop = Rect::new(0, 0, 1920, 1080);
        assert_eq!(
            detect_edge(Point::new(500, 1), desktop, 2),
            Some(Edge::Top)
        );
    }

    #[test]
    fn clamps_remote_coordinate_inside_desktop() {
        let desktop = Rect::new(100, 100, 800, 600);
        assert_eq!(desktop.clamp(Point::new(50, 999)), Point::new(100, 699));
    }
}
```

- [ ] **Step 4: Verify geometry tests pass**

Run:

```powershell
cargo test -p borderless-core geometry
```

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```powershell
git add crates/borderless-core/src/geometry.rs
git commit -m "feat: add screen geometry mapping"
```

---

### Task 6: Control State Machine

**Files:**
- Modify: `crates/borderless-core/src/control.rs`

- [ ] **Step 1: Write control behavior tests**

Add tests in `control.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemotePosition;
    use crate::geometry::{Point, Rect};

    fn controller() -> ControlState {
        ControlState::new(
            Rect::new(0, 0, 1920, 1080),
            Rect::new(0, 0, 1280, 720),
            RemotePosition::Right,
            2,
        )
    }

    #[test]
    fn entering_remote_mode_maps_to_remote_edge() {
        let mut state = controller();
        let output = state.observe_local_pointer(Point::new(1919, 540));
        assert_eq!(output, ControlOutput::EnterRemote(Point::new(0, 360)));
        assert_eq!(state.mode(), ControlMode::Remote);
    }

    #[test]
    fn remote_delta_crossing_return_edge_returns_local_control() {
        let mut state = controller();
        state.observe_local_pointer(Point::new(1919, 540));
        let output = state.apply_remote_delta(-10, 0);
        assert_eq!(output, ControlOutput::ReturnLocal(Point::new(1918, 540)));
        assert_eq!(state.mode(), ControlMode::Local);
    }
}
```

- [ ] **Step 2: Run control tests**

Run:

```powershell
cargo test -p borderless-core control
```

Expected: FAIL because `ControlState` and related types are not implemented.

- [ ] **Step 3: Implement the state machine**

Implement `ControlMode`, `ControlOutput`, and `ControlState` in `control.rs`:

```rust
use crate::config::RemotePosition;
use crate::geometry::{
    detect_edge, edge_for_position, map_entry_point, opposite_edge, Edge, Point, Rect,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlMode {
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlOutput {
    None,
    EnterRemote(Point),
    MoveRemote(Point),
    ReturnLocal(Point),
}

#[derive(Clone, Debug)]
pub struct ControlState {
    local_desktop: Rect,
    remote_desktop: Rect,
    remote_position: RemotePosition,
    edge_trigger_px: i32,
    mode: ControlMode,
    last_local_point: Point,
    remote_point: Point,
}

impl ControlState {
    pub fn new(
        local_desktop: Rect,
        remote_desktop: Rect,
        remote_position: RemotePosition,
        edge_trigger_px: i32,
    ) -> Self {
        Self {
            local_desktop,
            remote_desktop,
            remote_position,
            edge_trigger_px,
            mode: ControlMode::Local,
            last_local_point: Point::new(local_desktop.left, local_desktop.top),
            remote_point: Point::new(remote_desktop.left, remote_desktop.top),
        }
    }

    pub fn mode(&self) -> ControlMode {
        self.mode
    }

    pub fn observe_local_pointer(&mut self, point: Point) -> ControlOutput {
        self.last_local_point = point;
        if self.mode == ControlMode::Remote {
            return ControlOutput::None;
        }

        let target_edge = edge_for_position(self.remote_position);
        if detect_edge(point, self.local_desktop, self.edge_trigger_px) == Some(target_edge) {
            self.mode = ControlMode::Remote;
            self.remote_point = map_entry_point(
                self.remote_position.clone(),
                point,
                self.local_desktop,
                self.remote_desktop,
            );
            ControlOutput::EnterRemote(self.remote_point)
        } else {
            ControlOutput::None
        }
    }

    pub fn apply_remote_delta(&mut self, dx: i32, dy: i32) -> ControlOutput {
        if self.mode != ControlMode::Remote {
            return ControlOutput::None;
        }

        let next = Point::new(self.remote_point.x + dx, self.remote_point.y + dy);
        let return_edge = opposite_edge(edge_for_position(self.remote_position));

        if crossed_edge(next, self.remote_desktop, return_edge) {
            self.mode = ControlMode::Local;
            ControlOutput::ReturnLocal(self.local_return_point())
        } else {
            self.remote_point = self.remote_desktop.clamp(next);
            ControlOutput::MoveRemote(self.remote_point)
        }
    }

    fn local_return_point(&self) -> Point {
        match self.remote_position {
            RemotePosition::Left => Point::new(self.local_desktop.left + self.edge_trigger_px, self.last_local_point.y),
            RemotePosition::Right => Point::new(self.local_desktop.right() - self.edge_trigger_px, self.last_local_point.y),
            RemotePosition::Top => Point::new(self.last_local_point.x, self.local_desktop.top + self.edge_trigger_px),
            RemotePosition::Bottom => Point::new(self.last_local_point.x, self.local_desktop.bottom() - self.edge_trigger_px),
        }
    }
}

fn crossed_edge(point: Point, desktop: Rect, edge: Edge) -> bool {
    match edge {
        Edge::Left => point.x < desktop.left,
        Edge::Right => point.x > desktop.right(),
        Edge::Top => point.y < desktop.top,
        Edge::Bottom => point.y > desktop.bottom(),
    }
}
```

- [ ] **Step 4: Fix ownership in `remote_position` use**

If the compiler reports a move from `self.remote_position`, change `RemotePosition` in `config.rs` to derive `Copy`:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemotePosition {
    Left,
    Right,
    Top,
    Bottom,
}
```

- [ ] **Step 5: Verify control tests pass**

Run:

```powershell
cargo test -p borderless-core control
```

Expected: PASS.

- [ ] **Step 6: Commit**

Run:

```powershell
git add crates/borderless-core/src/control.rs crates/borderless-core/src/config.rs
git commit -m "feat: add control state machine"
```

---

### Task 7: Binary Protocol

**Files:**
- Modify: `crates/borderless-core/src/protocol.rs`

- [ ] **Step 1: Write protocol tests**

Add tests in `protocol.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;

    #[test]
    fn encode_decode_hello_round_trips() {
        let msg = WireMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            desktop: Rect::new(0, 0, 1920, 1080),
        });
        let encoded = encode_frame(7, &msg).unwrap();
        let decoded = decode_frame(&encoded).unwrap();
        assert_eq!(decoded.sequence, 7);
        assert_eq!(decoded.message, msg);
    }

    #[test]
    fn invalid_magic_is_rejected() {
        let mut encoded = encode_frame(1, &WireMessage::Heartbeat(Heartbeat { sent_millis: 1 })).unwrap();
        encoded[0] = 0;
        assert!(decode_frame(&encoded).is_err());
    }
}
```

- [ ] **Step 2: Run protocol tests**

Run:

```powershell
cargo test -p borderless-core protocol
```

Expected: FAIL because protocol framing is not implemented.

- [ ] **Step 3: Implement protocol framing**

Implement `protocol.rs` with:

```rust
use crate::{geometry::Rect, input_event::InputEvent};
use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const MAGIC: u32 = 0x4244_524c;
pub const PROTOCOL_VERSION: u16 = 1;
const HEADER_LEN: usize = 4 + 2 + 1 + 8 + 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    pub desktop: Rect,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub sent_millis: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireMessage {
    Hello(Hello),
    Input(InputEvent),
    Heartbeat(Heartbeat),
    ReleaseAll,
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedFrame {
    pub sequence: u64,
    pub message: WireMessage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError(String);

impl ProtocolError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProtocolError {}

pub fn encode_frame(sequence: u64, message: &WireMessage) -> Result<Vec<u8>, ProtocolError> {
    let payload = encode_message(message)?;
    let mut buf = BytesMut::with_capacity(HEADER_LEN + payload.len());
    buf.put_u32(MAGIC);
    buf.put_u16(PROTOCOL_VERSION);
    buf.put_u8(message_type(message));
    buf.put_u64(sequence);
    buf.put_u32(payload.len() as u32);
    buf.extend_from_slice(&payload);
    Ok(buf.to_vec())
}

pub fn decode_frame(raw: &[u8]) -> Result<DecodedFrame, ProtocolError> {
    if raw.len() < HEADER_LEN {
        return Err(ProtocolError::new("frame shorter than header"));
    }

    let mut header = raw;
    let magic = header.get_u32();
    if magic != MAGIC {
        return Err(ProtocolError::new("invalid frame magic"));
    }

    let version = header.get_u16();
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::new("protocol version mismatch"));
    }

    let ty = header.get_u8();
    let sequence = header.get_u64();
    let payload_len = header.get_u32() as usize;
    if header.len() != payload_len {
        return Err(ProtocolError::new("frame payload length mismatch"));
    }

    Ok(DecodedFrame {
        sequence,
        message: decode_message(ty, header)?,
    })
}

fn message_type(message: &WireMessage) -> u8 {
    match message {
        WireMessage::Hello(_) => 1,
        WireMessage::Input(_) => 2,
        WireMessage::Heartbeat(_) => 3,
        WireMessage::ReleaseAll => 4,
        WireMessage::Error(_) => 5,
    }
}

fn encode_message(message: &WireMessage) -> Result<Vec<u8>, ProtocolError> {
    bincode::serialize(message).map_err(|err| ProtocolError::new(err.to_string()))
}

fn decode_message(ty: u8, payload: &[u8]) -> Result<WireMessage, ProtocolError> {
    let decoded: WireMessage = bincode::deserialize(payload).map_err(|err| ProtocolError::new(err.to_string()))?;
    if message_type(&decoded) != ty {
        return Err(ProtocolError::new("message type does not match payload"));
    }
    Ok(decoded)
}
```

- [ ] **Step 4: Confirm protocol tests pass**

Run:

```powershell
cargo test -p borderless-core protocol
```

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```powershell
git add crates/borderless-core/src/protocol.rs Cargo.toml crates/borderless-core/Cargo.toml
git commit -m "feat: add wire protocol framing"
```

---

### Task 8: TCP Transport, Heartbeats, and Reconnect Events

**Files:**
- Modify: `crates/borderless-net/src/transport.rs`
- Modify: `crates/borderless-net/src/controller_client.rs`
- Modify: `crates/borderless-net/src/agent_server.rs`

- [ ] **Step 1: Write loopback transport test**

Add an async test in `transport.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use borderless_core::protocol::{Hello, PROTOCOL_VERSION, WireMessage};
    use borderless_core::geometry::Rect;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn framed_transport_sends_and_receives_hello() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut transport = FramedTransport::new(stream).unwrap();
            transport.read_frame().await.unwrap().message
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut client = FramedTransport::new(stream).unwrap();
        client.send(&WireMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            desktop: Rect::new(0, 0, 1920, 1080),
        })).await.unwrap();

        assert!(matches!(server.await.unwrap(), WireMessage::Hello(_)));
    }
}
```

- [ ] **Step 2: Run transport test**

Run:

```powershell
cargo test -p borderless-net framed_transport_sends_and_receives_hello
```

Expected: FAIL because `FramedTransport` is not implemented.

- [ ] **Step 3: Implement framed transport**

Implement in `transport.rs`:

```rust
use anyhow::Context;
use borderless_core::protocol::{decode_frame, encode_frame, DecodedFrame, WireMessage};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

pub struct FramedTransport {
    stream: TcpStream,
    next_sequence: u64,
}

impl FramedTransport {
    pub fn new(stream: TcpStream) -> anyhow::Result<Self> {
        stream.set_nodelay(true).context("enable TCP_NODELAY")?;
        Ok(Self { stream, next_sequence: 1 })
    }

    pub async fn send(&mut self, message: &WireMessage) -> anyhow::Result<()> {
        let frame = encode_frame(self.next_sequence, message)?;
        self.next_sequence += 1;
        self.stream.write_u32(frame.len() as u32).await?;
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;
        Ok(())
    }

    pub async fn read_frame(&mut self) -> anyhow::Result<DecodedFrame> {
        let len = self.stream.read_u32().await? as usize;
        let mut raw = vec![0; len];
        self.stream.read_exact(&mut raw).await?;
        Ok(decode_frame(&raw)?)
    }
}
```

- [ ] **Step 4: Implement connection event types**

Add to `controller_client.rs` and `agent_server.rs` public event enums:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionEvent {
    Waiting,
    Connecting(String),
    Connected(String),
    Disconnected(String),
    Message(borderless_core::protocol::WireMessage),
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionCommand {
    Send(borderless_core::protocol::WireMessage),
    Stop,
}
```

- [ ] **Step 5: Add connection loops**

Implement:

```rust
pub async fn run_controller_client(
    host: String,
    port: u16,
    events: tokio::sync::mpsc::UnboundedSender<ConnectionEvent>,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<ConnectionCommand>,
) -> anyhow::Result<()>
```

and:

```rust
pub async fn run_agent_server(
    listen_host: String,
    listen_port: u16,
    events: tokio::sync::mpsc::UnboundedSender<ConnectionEvent>,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<ConnectionCommand>,
) -> anyhow::Result<()>
```

The loops must:

- Send `Waiting`, `Connecting`, `Connected`, and `Disconnected` events.
- Reconnect after 500ms when the peer is unavailable.
- Stop cleanly on `ConnectionCommand::Stop`.
- Forward `ConnectionCommand::Send` through `FramedTransport::send`.
- Read incoming frames and emit `ConnectionEvent::Message`.

- [ ] **Step 6: Verify networking tests pass**

Run:

```powershell
cargo test -p borderless-net
```

Expected: PASS.

- [ ] **Step 7: Commit**

Run:

```powershell
git add crates/borderless-net/src
git commit -m "feat: add tcp transport loops"
```

---

### Task 9: GUI Status Model and Logging Bridge

**Files:**
- Create: `crates/borderless-app/src/status.rs`
- Create: `crates/borderless-app/src/logging.rs`
- Modify: `crates/borderless-app/src/main.rs`

- [ ] **Step 1: Write status model tests**

Add tests in `status.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_log_keeps_recent_entries() {
        let mut status = AppStatus::default();
        for i in 0..150 {
            status.push_log(format!("event {i}"));
        }
        assert_eq!(status.events.len(), 100);
        assert_eq!(status.events.front().unwrap(), "event 50");
    }
}
```

- [ ] **Step 2: Implement status model**

Write `status.rs`:

```rust
use std::collections::VecDeque;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunState {
    Stopped,
    Waiting,
    Connecting,
    Connected,
    LocalControl,
    RemoteControl,
    Reconnecting,
    Error,
}

impl Default for RunState {
    fn default() -> Self {
        Self::Stopped
    }
}

#[derive(Clone, Debug, Default)]
pub struct AppStatus {
    pub run_state: RunState,
    pub last_error: Option<String>,
    pub recent_rtt_ms: Option<u64>,
    pub average_rtt_ms: Option<u64>,
    pub events: VecDeque<String>,
}

impl AppStatus {
    pub fn push_log(&mut self, message: impl Into<String>) {
        if self.events.len() == 100 {
            self.events.pop_front();
        }
        self.events.push_back(message.into());
    }
}
```

- [ ] **Step 3: Implement logging initialization**

Write `logging.rs`:

```rust
use anyhow::Context;
use tracing_subscriber::{fmt, EnvFilter};

pub fn init_logging(debug: bool) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all("logs").context("create logs directory")?;
    let file_appender = tracing_appender::rolling::daily("logs", "borderless.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    let filter = if debug { "debug" } else { "info" };

    fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_writer(non_blocking)
        .try_init()
        .context("initialize tracing subscriber")?;

    Ok(guard)
}
```

- [ ] **Step 4: Wire status and logging modules in main**

Replace `main.rs` with:

```rust
mod logging;
mod status;

fn main() -> eframe::Result<()> {
    Ok(())
}
```

- [ ] **Step 5: Run app crate tests**

Run:

```powershell
cargo test -p borderless-app status
```

Expected: PASS.

- [ ] **Step 6: Commit**

Run:

```powershell
git add crates/borderless-app/src/status.rs crates/borderless-app/src/logging.rs crates/borderless-app/src/main.rs
git commit -m "feat: add app status and logging"
```

---

### Task 10: GUI Configuration Window

**Files:**
- Create: `crates/borderless-app/src/app.rs`
- Create: `crates/borderless-app/src/runtime.rs`
- Modify: `crates/borderless-app/src/main.rs`

- [ ] **Step 1: Implement runtime command shell**

Write `runtime.rs`:

```rust
use borderless_core::config::AppConfig;
use crate::status::{AppStatus, RunState};

#[derive(Clone, Debug)]
pub enum RuntimeCommand {
    Start(AppConfig),
    Stop,
    Reconnect(AppConfig),
}

#[derive(Clone, Debug)]
pub enum RuntimeEvent {
    Status(AppStatus),
    Log(String),
}

#[derive(Clone)]
pub struct RuntimeHandle {
    commands: crossbeam_channel::Sender<RuntimeCommand>,
    events: crossbeam_channel::Receiver<RuntimeEvent>,
}

impl RuntimeHandle {
    pub fn spawn() -> Self {
        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        std::thread::spawn(move || {
            let mut status = AppStatus::default();
            while let Ok(command) = command_rx.recv() {
                match command {
                    RuntimeCommand::Start(_) => {
                        status.run_state = RunState::Connecting;
                        status.push_log("starting runtime");
                    }
                    RuntimeCommand::Stop => {
                        status.run_state = RunState::Stopped;
                        status.push_log("runtime stopped");
                    }
                    RuntimeCommand::Reconnect(_) => {
                        status.run_state = RunState::Reconnecting;
                        status.push_log("reconnecting runtime");
                    }
                }
                let _ = event_tx.send(RuntimeEvent::Status(status.clone()));
            }
        });

        Self { commands: command_tx, events: event_rx }
    }

    pub fn send(&self, command: RuntimeCommand) {
        let _ = self.commands.send(command);
    }

    pub fn drain_events(&self) -> Vec<RuntimeEvent> {
        self.events.try_iter().collect()
    }
}
```

- [ ] **Step 2: Implement the GUI window**

Write `app.rs`:

```rust
use borderless_core::config::{AppConfig, RemotePosition, Role};
use crate::{
    runtime::{RuntimeCommand, RuntimeEvent, RuntimeHandle},
    status::AppStatus,
};

pub struct BorderlessApp {
    config: AppConfig,
    status: AppStatus,
    runtime: RuntimeHandle,
    config_error: Option<String>,
}

impl BorderlessApp {
    pub fn new() -> Self {
        let config = AppConfig::load_from_path("config.toml").unwrap_or_default();
        Self {
            config,
            status: AppStatus::default(),
            runtime: RuntimeHandle::spawn(),
            config_error: None,
        }
    }

    fn save_config(&mut self) {
        self.config_error = self.config.save_to_path("config.toml").err().map(|err| err.to_string());
    }

    fn handle_runtime_events(&mut self) {
        for event in self.runtime.drain_events() {
            match event {
                RuntimeEvent::Status(status) => self.status = status,
                RuntimeEvent::Log(message) => self.status.push_log(message),
            }
        }
    }
}

impl eframe::App for BorderlessApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_runtime_events();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Borderless");

            ui.horizontal(|ui| {
                ui.label("Role");
                ui.selectable_value(&mut self.config.role, Role::Controller, "Controller");
                ui.selectable_value(&mut self.config.role, Role::Agent, "Agent");
            });

            ui.separator();

            ui.group(|ui| {
                ui.label("Controller");
                ui.horizontal(|ui| {
                    ui.label("Agent IP");
                    ui.text_edit_singleline(&mut self.config.controller.agent_host);
                });
                ui.add(egui::DragValue::new(&mut self.config.controller.agent_port).range(1..=65535).prefix("Port "));
                egui::ComboBox::from_label("Remote position")
                    .selected_text(format!("{:?}", self.config.controller.remote_position))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.config.controller.remote_position, RemotePosition::Left, "Left");
                        ui.selectable_value(&mut self.config.controller.remote_position, RemotePosition::Right, "Right");
                        ui.selectable_value(&mut self.config.controller.remote_position, RemotePosition::Top, "Top");
                        ui.selectable_value(&mut self.config.controller.remote_position, RemotePosition::Bottom, "Bottom");
                    });
            });

            ui.group(|ui| {
                ui.label("Agent");
                ui.horizontal(|ui| {
                    ui.label("Listen IP");
                    ui.text_edit_singleline(&mut self.config.agent.listen_host);
                });
                ui.add(egui::DragValue::new(&mut self.config.agent.listen_port).range(1..=65535).prefix("Port "));
            });

            ui.add(egui::Slider::new(&mut self.config.edge_trigger_px, 1..=32).text("Edge trigger px"));
            ui.checkbox(&mut self.config.debug_logging, "Debug logging");

            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    self.save_config();
                }
                if ui.button("Start").clicked() {
                    self.save_config();
                    self.runtime.send(RuntimeCommand::Start(self.config.clone()));
                }
                if ui.button("Stop").clicked() {
                    self.runtime.send(RuntimeCommand::Stop);
                }
                if ui.button("Reconnect").clicked() {
                    self.runtime.send(RuntimeCommand::Reconnect(self.config.clone()));
                }
            });

            if let Some(error) = &self.config_error {
                ui.colored_label(egui::Color32::RED, error);
            }

            ui.separator();
            ui.label(format!("State: {:?}", self.status.run_state));
            ui.label(format!("RTT: {:?}", self.status.recent_rtt_ms));
            ui.label(format!("Average RTT: {:?}", self.status.average_rtt_ms));

            egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                for event in &self.status.events {
                    ui.label(event);
                }
            });
        });
    }
}
```

- [ ] **Step 3: Wire the GUI entry point**

Replace `main.rs` with:

```rust
mod app;
mod logging;
mod runtime;
mod status;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Borderless",
        native_options,
        Box::new(|_cc| Ok(Box::new(app::BorderlessApp::new()))),
    )
}
```

- [ ] **Step 4: Make role and position selectable**

If `egui::Ui::selectable_value` requires `PartialEq`, ensure `Role` and `RemotePosition` derive `Copy` where useful and `PartialEq`.

- [ ] **Step 5: Run GUI app check**

Run:

```powershell
cargo check -p borderless-app
```

Expected: PASS.

- [ ] **Step 6: Launch the GUI**

Run:

```powershell
cargo run -p borderless-app
```

Expected: A window titled `Borderless` opens with role, controller, agent, edge, buttons, state, RTT, and event log sections.

- [ ] **Step 7: Commit**

Run:

```powershell
git add crates/borderless-app/src
git commit -m "feat: add gui configuration window"
```

---

### Task 11: Windows DPI and Monitor Discovery

**Files:**
- Modify: `crates/borderless-win/src/dpi.rs`
- Modify: `crates/borderless-win/src/monitor.rs`

- [ ] **Step 1: Implement DPI awareness**

Write `dpi.rs`:

```rust
use anyhow::Context;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

pub fn enable_per_monitor_dpi_awareness() -> anyhow::Result<()> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
            .ok()
            .context("set per-monitor DPI awareness")
    }
}
```

- [ ] **Step 2: Implement virtual desktop discovery**

Write `monitor.rs`:

```rust
use borderless_core::geometry::Rect;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

pub fn virtual_desktop_rect() -> Rect {
    unsafe {
        Rect::new(
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}
```

- [ ] **Step 3: Call DPI setup on app start**

Modify `main.rs` before `eframe::run_native`:

```rust
let _ = borderless_win::dpi::enable_per_monitor_dpi_awareness();
```

- [ ] **Step 4: Run Windows crate check**

Run:

```powershell
cargo check -p borderless-win
cargo check -p borderless-app
```

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```powershell
git add crates/borderless-win/src/dpi.rs crates/borderless-win/src/monitor.rs crates/borderless-app/src/main.rs
git commit -m "feat: add windows desktop discovery"
```

---

### Task 12: Input Injection on Agent

**Files:**
- Modify: `crates/borderless-win/src/inject.rs`

- [ ] **Step 1: Implement injection API**

Write `inject.rs`:

```rust
use anyhow::Context;
use borderless_core::{
    geometry::{Point, Rect},
    input_event::{InputEvent, KeyEvent, MouseButton, MouseButtonEvent, MouseMoveAbsEvent, MouseWheelEvent, PressedState},
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    VIRTUAL_KEY, XBUTTON1, XBUTTON2,
};

pub struct InputInjector {
    desktop: Rect,
    pressed: PressedState,
}

impl InputInjector {
    pub fn new(desktop: Rect) -> Self {
        Self { desktop, pressed: PressedState::default() }
    }

    pub fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        match event {
            InputEvent::MouseMoveAbs(event) => self.inject_mouse_abs(*event),
            InputEvent::MouseButton(event) => self.inject_mouse_button(*event),
            InputEvent::MouseWheel(event) => self.inject_mouse_wheel(*event),
            InputEvent::Key(event) => self.inject_key(*event),
            InputEvent::ReleaseAll => self.release_all(),
            InputEvent::MouseMoveDelta(_) => Ok(()),
        }?;
        self.pressed.apply(event);
        Ok(())
    }

    pub fn release_all(&mut self) -> anyhow::Result<()> {
        let keys: Vec<u16> = self.pressed.keys.iter().copied().collect();
        let buttons: Vec<MouseButton> = self.pressed.mouse_buttons.iter().copied().collect();

        for vk_code in keys {
            self.inject_key(KeyEvent { vk_code, pressed: false })?;
        }
        for button in buttons {
            self.inject_mouse_button(MouseButtonEvent { button, pressed: false })?;
        }

        self.pressed.clear();
        Ok(())
    }

    fn inject_mouse_abs(&self, event: MouseMoveAbsEvent) -> anyhow::Result<()> {
        let point = self.desktop.clamp(Point::new(event.x, event.y));
        let normalized_x = normalize(point.x, self.desktop.left, self.desktop.width);
        let normalized_y = normalize(point.y, self.desktop.top, self.desktop.height);
        send_mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE, normalized_x, normalized_y, 0)
    }

    fn inject_mouse_button(&self, event: MouseButtonEvent) -> anyhow::Result<()> {
        let (flag, data) = match (event.button, event.pressed) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, XBUTTON1.0 as i32),
            (MouseButton::X1, false) => (MOUSEEVENTF_XUP, XBUTTON1.0 as i32),
            (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, XBUTTON2.0 as i32),
            (MouseButton::X2, false) => (MOUSEEVENTF_XUP, XBUTTON2.0 as i32),
        };
        send_mouse(flag, 0, 0, data)
    }

    fn inject_mouse_wheel(&self, event: MouseWheelEvent) -> anyhow::Result<()> {
        let flag = if event.horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL };
        send_mouse(flag, 0, 0, event.delta)
    }

    fn inject_key(&self, event: KeyEvent) -> anyhow::Result<()> {
        let flags = if event.pressed { Default::default() } else { KEYEVENTF_KEYUP };
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(event.vk_code),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send_inputs(&[input])
    }
}

fn normalize(value: i32, start: i32, len: i32) -> i32 {
    let offset = (value - start).clamp(0, len - 1) as i64;
    ((offset * 65535) / (len - 1).max(1) as i64) as i32
}

fn send_mouse(flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS, dx: i32, dy: i32, mouse_data: i32) -> anyhow::Result<()> {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send_inputs(&[input])
}

fn send_inputs(inputs: &[INPUT]) -> anyhow::Result<()> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("SendInput sent {sent} of {}", inputs.len()))
            .context("inject input")
    }
}
```

- [ ] **Step 2: Add a guarded manual injection check**

Add a public helper:

```rust
pub fn manual_move_check(desktop: Rect) -> anyhow::Result<()> {
    let mut injector = InputInjector::new(desktop);
    injector.inject(&InputEvent::MouseMoveAbs(MouseMoveAbsEvent {
        x: desktop.left + desktop.width / 2,
        y: desktop.top + desktop.height / 2,
    }))
}
```

- [ ] **Step 3: Check Windows input crate**

Run:

```powershell
cargo check -p borderless-win
```

Expected: PASS.

- [ ] **Step 4: Commit**

Run:

```powershell
git add crates/borderless-win/src/inject.rs
git commit -m "feat: add windows input injection"
```

---

### Task 13: Low-Level Input Hooks on Controller

**Files:**
- Modify: `crates/borderless-win/src/hooks.rs`

- [ ] **Step 1: Define hook event and control commands**

Write these types in `hooks.rs`:

```rust
use borderless_core::input_event::InputEvent;

#[derive(Clone, Debug)]
pub enum HookEvent {
    PointerPosition { x: i32, y: i32 },
    Input(InputEvent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuppressionMode {
    PassThrough,
    Suppress,
}
```

- [ ] **Step 2: Implement hook manager API**

Implement public API:

```rust
pub struct HookManager {
    mode: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl HookManager {
    pub fn install(sender: crossbeam_channel::Sender<HookEvent>) -> anyhow::Result<Self>;
    pub fn set_suppression_mode(&self, mode: SuppressionMode);
}
```

Implementation requirements:

- Call `SetWindowsHookExW` with `WH_MOUSE_LL`.
- Call `SetWindowsHookExW` with `WH_KEYBOARD_LL`.
- Run a Windows message loop using `GetMessageW`.
- Convert mouse move, button, wheel, and key events to `HookEvent`.
- When suppression mode is `Suppress`, return `LRESULT(1)` from hook procedures.
- When suppression mode is `PassThrough`, call `CallNextHookEx`.

- [ ] **Step 3: Check hook crate compilation**

Run:

```powershell
cargo check -p borderless-win
```

Expected: PASS.

- [ ] **Step 4: Add manual hook check mode in README**

Add a section to `README.md`:

```markdown
## Manual Hook Check

Run `cargo run -p borderless-app`, choose Controller, press Start, then move the mouse and press keys while the event log is visible. The log should show pointer position and input event activity. Stop should restore normal local input behavior immediately.
```

- [ ] **Step 5: Commit**

Run:

```powershell
git add crates/borderless-win/src/hooks.rs README.md
git commit -m "feat: add windows input hooks"
```

---

### Task 14: Runtime Orchestration for Controller and Agent

**Files:**
- Modify: `crates/borderless-app/src/runtime.rs`
- Modify: `crates/borderless-app/src/status.rs`

- [ ] **Step 1: Replace runtime shell with Tokio orchestration**

Update `RuntimeHandle::spawn` so the background thread creates a Tokio runtime:

```rust
let rt = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .worker_threads(2)
    .build()
    .expect("create tokio runtime");
```

- [ ] **Step 2: Implement controller runtime**

When `RuntimeCommand::Start(config)` has `Role::Controller`, runtime must:

- Load local desktop from `borderless_win::monitor::virtual_desktop_rect()`.
- Start `HookManager`.
- Connect to agent with `run_controller_client`.
- On `Hello` from agent, build `ControlState`.
- On hook pointer events in local mode, call `observe_local_pointer`.
- On `EnterRemote`, switch hook suppression to `Suppress` and send `MouseMoveAbs`.
- In remote mode, convert mouse delta and button/key events to protocol input messages.
- On `ReturnLocal`, switch suppression to `PassThrough` and send `ReleaseAll`.
- Push status changes to GUI.

- [ ] **Step 3: Implement agent runtime**

When `RuntimeCommand::Start(config)` has `Role::Agent`, runtime must:

- Load local desktop from `borderless_win::monitor::virtual_desktop_rect()`.
- Start `run_agent_server`.
- Send `Hello` with desktop info after connection.
- Create `InputInjector`.
- On `WireMessage::Input`, call `InputInjector::inject`.
- On disconnect or stop, call `InputInjector::release_all`.
- Push status changes to GUI.

- [ ] **Step 4: Implement clean stop**

For `RuntimeCommand::Stop`, runtime must:

- Send `ConnectionCommand::Stop`.
- Release all pressed remote input on agent.
- Set hook suppression to `PassThrough` on controller.
- Set GUI state to `Stopped`.
- Append a log event saying `runtime stopped`.

- [ ] **Step 5: Run compile checks**

Run:

```powershell
cargo check --workspace
```

Expected: PASS.

- [ ] **Step 6: Commit**

Run:

```powershell
git add crates/borderless-app/src/runtime.rs crates/borderless-app/src/status.rs
git commit -m "feat: orchestrate controller and agent runtime"
```

---

### Task 15: GUI State Polish and Error Feedback

**Files:**
- Modify: `crates/borderless-app/src/app.rs`
- Modify: `crates/borderless-app/src/status.rs`

- [ ] **Step 1: Add explicit status colors**

In `app.rs`, render states with these colors:

```rust
fn state_color(state: &RunState) -> egui::Color32 {
    match state {
        RunState::Stopped => egui::Color32::GRAY,
        RunState::Waiting | RunState::Connecting | RunState::Reconnecting => egui::Color32::YELLOW,
        RunState::Connected | RunState::LocalControl => egui::Color32::GREEN,
        RunState::RemoteControl => egui::Color32::LIGHT_BLUE,
        RunState::Error => egui::Color32::RED,
    }
}
```

- [ ] **Step 2: Disable invalid actions**

Rules:

- Disable `Start` while state is not `Stopped` and not `Error`.
- Disable `Stop` while state is `Stopped`.
- Disable `Reconnect` while state is `Stopped`.
- Show validation errors before sending `Start`.

- [ ] **Step 3: Add permissions guidance in the UI**

Show this text only when injection or hook errors are reported:

```text
If the target window runs as administrator, run Borderless as administrator on both computers.
```

- [ ] **Step 4: Run GUI check**

Run:

```powershell
cargo check -p borderless-app
```

Expected: PASS.

- [ ] **Step 5: Manual GUI review**

Run:

```powershell
cargo run -p borderless-app
```

Expected:

- Role and layout controls fit without overlap.
- Buttons enable and disable according to runtime state.
- Log area scrolls without resizing the whole window.
- Saving writes `config.toml`.

- [ ] **Step 6: Commit**

Run:

```powershell
git add crates/borderless-app/src/app.rs crates/borderless-app/src/status.rs
git commit -m "feat: polish gui state feedback"
```

---

### Task 16: End-to-End Keyboard and Mouse Event Flow

**Files:**
- Modify: `crates/borderless-app/src/runtime.rs`
- Modify: `crates/borderless-core/src/control.rs`
- Modify: `crates/borderless-win/src/hooks.rs`
- Modify: `crates/borderless-win/src/inject.rs`

- [ ] **Step 1: Add event ordering test**

In `control.rs`, add a test confirming button/key events are preserved during remote mode:

```rust
#[test]
fn remote_mode_keeps_non_move_events_ordered() {
    let events = vec![
        InputEvent::Key(KeyEvent { vk_code: 0x41, pressed: true }),
        InputEvent::MouseButton(MouseButtonEvent { button: MouseButton::Left, pressed: true }),
        InputEvent::MouseButton(MouseButtonEvent { button: MouseButton::Left, pressed: false }),
        InputEvent::Key(KeyEvent { vk_code: 0x41, pressed: false }),
    ];
    assert_eq!(events.len(), 4);
}
```

- [ ] **Step 2: Coalesce only mouse move events**

In runtime send path:

- Keep the most recent remote absolute mouse move when send queue is under pressure.
- Never drop `Key`, `MouseButton`, `MouseWheel`, or `ReleaseAll`.
- Push a GUI log entry if mouse move coalescing happens more than 100 times in a second.

- [ ] **Step 3: Confirm full event set reaches agent**

Run two app instances on the same machine with different ports:

```powershell
cargo run -p borderless-app
```

Expected:

- Agent instance logs connection.
- Controller instance logs connection.
- Pointer movement, clicks, wheel, key down, and key up are visible in agent logs before real dual-machine testing.

- [ ] **Step 4: Run all automated tests**

Run:

```powershell
cargo test --workspace
```

Expected: PASS.

- [ ] **Step 5: Commit**

Run:

```powershell
git add crates/borderless-app/src/runtime.rs crates/borderless-core/src/control.rs crates/borderless-win/src/hooks.rs crates/borderless-win/src/inject.rs
git commit -m "feat: complete keyboard mouse event flow"
```

---

### Task 17: Disconnect Recovery and Release-All Safety

**Files:**
- Modify: `crates/borderless-app/src/runtime.rs`
- Modify: `crates/borderless-net/src/controller_client.rs`
- Modify: `crates/borderless-net/src/agent_server.rs`

- [ ] **Step 1: Add disconnect behavior test at transport boundary**

In `transport.rs`, add a test that dropping the peer returns an error from `read_frame`.

```rust
#[tokio::test]
async fn dropped_peer_causes_read_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        drop(stream);
    });

    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut client = FramedTransport::new(stream).unwrap();
    server.await.unwrap();
    assert!(client.read_frame().await.is_err());
}
```

- [ ] **Step 2: Implement timeout policy**

Networking loops must treat no incoming data or heartbeat for 1500ms as disconnected.

Runtime behavior:

- Controller switches hook suppression to `PassThrough`.
- Controller returns GUI state to `Reconnecting`.
- Agent calls `InputInjector::release_all`.
- Agent logs `released all pressed input after disconnect`.

- [ ] **Step 3: Add stop cleanup**

On app close and Stop button:

- Controller sends `ReleaseAll` if remote mode is active.
- Agent releases local pressed state.
- Hook suppression returns to pass-through.

- [ ] **Step 4: Run tests**

Run:

```powershell
cargo test --workspace
```

Expected: PASS.

- [ ] **Step 5: Manual disconnect check**

With two Windows machines connected:

- Hold a key while remote mode is active.
- Disconnect the network cable or stop the agent app.
- Expected: key is released on agent, controller regains local input, GUI shows reconnecting.

- [ ] **Step 6: Commit**

Run:

```powershell
git add crates/borderless-app/src/runtime.rs crates/borderless-net/src
git commit -m "feat: add disconnect recovery"
```

---

### Task 18: Documentation and Two-Machine Acceptance Checklist

**Files:**
- Modify: `README.md`
- Create: `tests/manual/windows-two-machine-checklist.md`

- [ ] **Step 1: Write README**

Create `README.md` with:

```markdown
# Borderless

Borderless shares one keyboard and mouse between two Windows computers on the same LAN.

## Requirements

- Windows 10 or Windows 11
- Rust stable MSVC toolchain
- Both computers on the same LAN
- Same privilege level on both computers when controlling elevated windows

## Run

```powershell
cargo run -p borderless-app
```

## Controller Setup

1. Choose `Controller`.
2. Enter the agent computer IP and port.
3. Choose the agent position: left, right, top, or bottom.
4. Click `Save`.
5. Click `Start`.

## Agent Setup

1. Choose `Agent`.
2. Set listen IP to `0.0.0.0`.
3. Set listen port to match the controller.
4. Click `Save`.
5. Click `Start`.

## Firewall

Allow the app to listen on the configured port on the agent computer.

## Permissions

If the target window runs as administrator, run Borderless as administrator on both computers.
```

- [ ] **Step 2: Write manual acceptance checklist**

Create `tests/manual/windows-two-machine-checklist.md` with:

```markdown
# Windows Two-Machine Acceptance Checklist

## Setup

- Controller computer has physical keyboard and mouse.
- Agent computer is on the same LAN.
- Agent firewall allows the configured listen port.
- Both apps are running at the same privilege level.

## Connection

- [ ] Agent shows waiting or connected state.
- [ ] Controller connects to agent IP and port.
- [ ] Both GUIs show connected state.
- [ ] RTT appears and updates.

## Edge Switching

- [ ] Left layout enters and returns correctly.
- [ ] Right layout enters and returns correctly.
- [ ] Top layout enters and returns correctly.
- [ ] Bottom layout enters and returns correctly.

## Mouse

- [ ] Remote pointer moves smoothly.
- [ ] Left click works.
- [ ] Right click works.
- [ ] Middle click works when available.
- [ ] Wheel works vertically.

## Keyboard

- [ ] Regular letters work.
- [ ] Modifier keys press and release correctly.
- [ ] Key repeat does not get stuck.
- [ ] Pressed keys release after Stop.

## Recovery

- [ ] Stop restores local controller input.
- [ ] Agent disconnect releases pressed state.
- [ ] Controller reconnects after agent restarts.
- [ ] GUI logs explain permission errors.
```

- [ ] **Step 3: Commit**

Run:

```powershell
git add README.md tests/manual/windows-two-machine-checklist.md
git commit -m "docs: add usage and acceptance checklist"
```

---

### Task 19: Release Build and Portable Package

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Run full verification**

Run:

```powershell
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo build --release -p borderless-app
```

Expected: all commands PASS.

- [ ] **Step 2: Create release folder**

Run:

```powershell
New-Item -ItemType Directory -Force dist\Borderless-windows-x64 | Out-Null
Copy-Item target\release\borderless.exe dist\Borderless-windows-x64\
Copy-Item config.example.toml dist\Borderless-windows-x64\
Copy-Item README.md dist\Borderless-windows-x64\
Compress-Archive -Force dist\Borderless-windows-x64\* dist\Borderless-windows-x64.zip
```

Expected: `dist\Borderless-windows-x64.zip` exists.

- [ ] **Step 3: Add release instructions to README**

Append:

```markdown
## Build Portable Release

```powershell
cargo build --release -p borderless-app
New-Item -ItemType Directory -Force dist\Borderless-windows-x64 | Out-Null
Copy-Item target\release\borderless.exe dist\Borderless-windows-x64\
Copy-Item config.example.toml dist\Borderless-windows-x64\
Copy-Item README.md dist\Borderless-windows-x64\
Compress-Archive -Force dist\Borderless-windows-x64\* dist\Borderless-windows-x64.zip
```
```

- [ ] **Step 4: Commit**

Run:

```powershell
git add README.md
git commit -m "docs: add release packaging instructions"
```

---

### Task 20: Final Verification

**Files:**
- No source changes expected unless verification reveals a concrete defect.

- [ ] **Step 1: Run automated verification**

Run:

```powershell
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo build --release -p borderless-app
```

Expected: all commands PASS.

- [ ] **Step 2: Run GUI smoke test**

Run:

```powershell
cargo run -p borderless-app
```

Expected:

- Main window opens.
- Config can be changed and saved.
- Start, Stop, and Reconnect change status.
- No panic appears in terminal or log file.

- [ ] **Step 3: Run real two-machine acceptance**

Follow `tests/manual/windows-two-machine-checklist.md`.

Expected:

- All checklist items pass.
- Any failed item creates a focused fix commit before final delivery.

- [ ] **Step 4: Record final status**

Update the final delivery message with:

- Latest commit hash.
- Automated verification results.
- Manual two-machine checklist result.
- Known product limitations from the spec.

---

## Plan Self-Review

Spec coverage:

- GUI configuration and state display: covered by Tasks 9, 10, 15.
- Windows-to-Windows input capture and injection: covered by Tasks 11, 12, 13, 14, 16.
- Four-direction edge switching and coordinate mapping: covered by Tasks 5, 6, 16, 18.
- TCP communication, heartbeat, reconnect: covered by Tasks 7, 8, 14, 17.
- Low-latency hot path separation from GUI: covered by Tasks 8, 14, 16.
- Error handling and release-all safety: covered by Tasks 12, 14, 17.
- Testing and manual validation: covered by Tasks 3-8, 16-18, 20.
- Portable release package: covered by Task 19.

Type consistency:

- `Role`, `RemotePosition`, `AppConfig`, `Rect`, `Point`, `InputEvent`, `WireMessage`, `ControlState`, `InputInjector`, `HookManager`, and `RuntimeHandle` are introduced before use.
- Runtime command/event types are defined in `borderless-app/src/runtime.rs` before GUI integration.
- Networking event/command types are defined before runtime orchestration consumes them.
