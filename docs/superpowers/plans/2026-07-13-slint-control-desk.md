# Slint Chinese Control Desk Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the eframe/egui window with the approved native Slint Chinese split control desk without changing runtime behavior.

**Architecture:** Keep `RuntimeHandle`, `RuntimeCommand`, `RuntimeEvent`, `AppConfig`, and `AppStatus` independent of UI technology. Add a pure Rust view-model projection layer, a declarative Slint window, and a UI-thread controller that maps callbacks to runtime/config commands and polls runtime events with a Slint timer.

**Tech Stack:** Rust 2021, Slint 1.17.1, Slint build compiler, existing Tokio/crossbeam runtime, vendored Lucide SVG icons.

**Depends on:** Completion of `docs/superpowers/plans/2026-07-13-tcp-only-network.md`.

**Produces:** The approved Chinese two-column UI on the TCP-only runtime. Run the targeted drag-drop plan next.

---

## File Map

- `Cargo.toml`: add Slint and Slint build dependencies; remove eframe/egui after migration.
- `crates/borderless-app/Cargo.toml`: add Slint runtime/build dependencies.
- `crates/borderless-app/build.rs`: compile the Slint source.
- `crates/borderless-app/ui/main.slint`: approved split control desk and advanced settings overlay.
- `crates/borderless-app/assets/icons/*.svg`: official Lucide settings/save/play/refresh/stop/x icons.
- `crates/borderless-app/src/ui.rs`: generated Slint module include.
- `crates/borderless-app/src/ui_model.rs`: pure config/status-to-UI projection and tests.
- `crates/borderless-app/src/ui_bridge.rs`: UI callbacks, runtime event polling, config persistence, and shutdown.
- `crates/borderless-app/src/main.rs`: initialize logging/DPI and run Slint.
- `crates/borderless-app/src/app.rs`: delete after bridge parity is verified.
- `crates/borderless-app/src/status.rs`: remain UI-framework independent.
- `README.md`: update run descriptions only after the native window works.

---

### Task 1: Add a Minimal Slint Build Without Replacing eframe

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/borderless-app/Cargo.toml`
- Create: `crates/borderless-app/build.rs`
- Create: `crates/borderless-app/ui/main.slint`
- Create: `crates/borderless-app/src/ui.rs`
- Modify: `crates/borderless-app/src/main.rs`

- [ ] **Step 1: Add a compile-time smoke test module**

Add the module declaration while leaving the eframe `main` unchanged:

```rust
mod ui;

#[cfg(test)]
mod ui_compile_tests {
    #[test]
    fn generated_slint_window_type_is_available() {
        fn accepts_window(_: Option<crate::ui::AppWindow>) {}
        accepts_window(None);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:

```powershell
cargo test -p borderless-app generated_slint_window_type_is_available
```

Expected: FAIL because `src/ui.rs` and generated `AppWindow` do not exist.

- [ ] **Step 3: Add Slint dependencies and build script**

Add to workspace dependencies:

```toml
slint = "=1.17.1"
slint-build = "=1.17.1"
```

Add to `crates/borderless-app/Cargo.toml`:

```toml
[dependencies]
slint.workspace = true

[build-dependencies]
slint-build.workspace = true
```

Create `build.rs`:

```rust
fn main() {
    slint_build::compile("ui/main.slint").expect("compile Slint UI");
}
```

Create `src/ui.rs`:

```rust
slint::include_modules!();
```

Create the smallest valid `ui/main.slint`:

```slint
import { Button, VerticalBox } from "std-widgets.slint";

export component AppWindow inherits Window {
    title: "Borderless";
    preferred-width: 1080px;
    preferred-height: 660px;

    VerticalBox {
        Text {
            text: "Borderless";
            font-size: 20px;
            font-weight: 700;
        }
        Button { text: "界面加载成功"; }
    }
}
```

- [ ] **Step 4: Run the smoke test**

Run:

```powershell
cargo test -p borderless-app generated_slint_window_type_is_available
```

Expected: PASS; the eframe app still remains the actual entry point.

- [ ] **Step 5: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/borderless-app/Cargo.toml crates/borderless-app/build.rs crates/borderless-app/ui/main.slint crates/borderless-app/src/ui.rs crates/borderless-app/src/main.rs
git commit -m "build: add Slint UI compiler"
```

---

### Task 2: Build a Pure Rust UI View Model

**Files:**
- Create: `crates/borderless-app/src/ui_model.rs`
- Modify: `crates/borderless-app/src/main.rs`
- Test: `crates/borderless-app/src/ui_model.rs`

- [ ] **Step 1: Write projection and config-draft tests**

Create `ui_model.rs` with tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{AppStatus, RunState};
    use borderless_core::config::{AppConfig, RemotePosition, Role};

    #[test]
    fn connected_remote_status_projects_to_chinese_labels() {
        let status = AppStatus {
            run_state: RunState::RemoteControl,
            recent_rtt_ms: Some(3),
            transfer_active: true,
            transfer_bytes_done: 248,
            transfer_bytes_total: 400,
            transfer_current_file: Some("design.pdf".to_string()),
            ..AppStatus::default()
        };

        let view = UiSnapshot::from_status(&status);
        assert_eq!(view.connection_label, "已连接");
        assert_eq!(view.control_label, "远程电脑");
        assert_eq!(view.latency_label, "3 ms");
        assert_eq!(view.transfer_progress, 0.62);
        assert_eq!(view.transfer_file, "design.pdf");
    }

    #[test]
    fn config_draft_updates_only_visible_connection_fields() {
        let mut config = AppConfig::default();
        let draft = ConfigDraft {
            role: Role::Agent,
            target_host: "192.168.1.20".to_string(),
            target_port: 24880,
            listen_host: "0.0.0.0".to_string(),
            listen_port: 24900,
            remote_position: RemotePosition::Left,
            clipboard_text: false,
            clipboard_html: true,
            clipboard_images: true,
            file_copy_paste: true,
            file_drag_drop: true,
        };

        draft.apply_to(&mut config);
        assert_eq!(config.role, Role::Agent);
        assert_eq!(config.agent.listen_host, "0.0.0.0");
        assert_eq!(config.agent.listen_port, 24900);
        assert_eq!(config.controller.agent_host, "192.168.1.20");
        assert_eq!(config.controller.agent_port, 24880);
        assert_eq!(config.controller.remote_position, RemotePosition::Left);
        assert!(!config.sharing.clipboard_text);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```powershell
cargo test -p borderless-app ui_model::tests
```

Expected: FAIL because `UiSnapshot` and `ConfigDraft` are not defined.

- [ ] **Step 3: Implement the view model**

Add these concrete types and mappings above the tests:

```rust
use borderless_core::config::{AppConfig, RemotePosition, Role};

use crate::status::{AppStatus, RunState};

#[derive(Clone, Debug, PartialEq)]
pub struct UiSnapshot {
    pub connection_label: String,
    pub control_label: String,
    pub latency_label: String,
    pub connected: bool,
    pub running: bool,
    pub transfer_active: bool,
    pub transfer_progress: f32,
    pub transfer_file: String,
    pub transfer_detail: String,
    pub transfer_destination: String,
    pub last_error: String,
}

impl UiSnapshot {
    pub fn from_status(status: &AppStatus) -> Self {
        let connection_label = match status.run_state {
            RunState::Stopped => "已停止",
            RunState::Waiting => "等待连接",
            RunState::Connecting => "正在连接",
            RunState::Connected | RunState::LocalControl | RunState::RemoteControl => "已连接",
            RunState::Reconnecting => "正在重连",
            RunState::Error => "连接错误",
        }
        .to_string();
        let control_label = match status.run_state {
            RunState::RemoteControl => "远程电脑",
            _ => "本机",
        }
        .to_string();
        let transfer_progress = if status.transfer_bytes_total == 0 {
            0.0
        } else {
            status.transfer_bytes_done as f32 / status.transfer_bytes_total as f32
        }
        .clamp(0.0, 1.0);

        Self {
            connection_label,
            control_label,
            latency_label: status
                .recent_rtt_ms
                .map(|value| format!("{value} ms"))
                .unwrap_or_else(|| "-".to_string()),
            connected: matches!(
                status.run_state,
                RunState::Connected | RunState::LocalControl | RunState::RemoteControl
            ),
            running: status.run_state != RunState::Stopped,
            transfer_active: status.transfer_active,
            transfer_progress,
            transfer_file: status.transfer_current_file.clone().unwrap_or_default(),
            transfer_detail: format!(
                "{} / {} bytes",
                status.transfer_bytes_done, status.transfer_bytes_total
            ),
            transfer_destination: status.drag_drop_destination.clone().unwrap_or_default(),
            last_error: status.last_error.clone().unwrap_or_default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigDraft {
    pub role: Role,
    pub target_host: String,
    pub target_port: u16,
    pub listen_host: String,
    pub listen_port: u16,
    pub remote_position: RemotePosition,
    pub clipboard_text: bool,
    pub clipboard_html: bool,
    pub clipboard_images: bool,
    pub file_copy_paste: bool,
    pub file_drag_drop: bool,
}

impl ConfigDraft {
    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            role: config.role.clone(),
            target_host: config.controller.agent_host.clone(),
            target_port: config.controller.agent_port,
            listen_host: config.agent.listen_host.clone(),
            listen_port: config.agent.listen_port,
            remote_position: config.controller.remote_position.clone(),
            clipboard_text: config.sharing.clipboard_text,
            clipboard_html: config.sharing.clipboard_html,
            clipboard_images: config.sharing.clipboard_images,
            file_copy_paste: config.sharing.file_copy_paste,
            file_drag_drop: config.sharing.file_drag_drop,
        }
    }

    pub fn apply_to(&self, config: &mut AppConfig) {
        config.role = self.role.clone();
        config.controller.agent_host = self.target_host.clone();
        config.controller.agent_port = self.target_port;
        config.controller.remote_position = self.remote_position.clone();
        config.agent.listen_host = self.listen_host.clone();
        config.agent.listen_port = self.listen_port;
        config.sharing.clipboard_text = self.clipboard_text;
        config.sharing.clipboard_html = self.clipboard_html;
        config.sharing.clipboard_images = self.clipboard_images;
        config.sharing.file_copy_paste = self.file_copy_paste;
        config.sharing.file_drag_drop = self.file_drag_drop;
    }
}
```

Expose `mod ui_model;` from `main.rs`.

- [ ] **Step 4: Run tests**

Run:

```powershell
cargo test -p borderless-app ui_model::tests
```

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add crates/borderless-app/src/ui_model.rs crates/borderless-app/src/main.rs
git commit -m "feat: add Chinese UI view model"
```

---

### Task 3: Implement the Approved Split Control Desk

**Files:**
- Modify: `crates/borderless-app/ui/main.slint`
- Create: `crates/borderless-app/assets/icons/settings.svg`
- Create: `crates/borderless-app/assets/icons/save.svg`
- Create: `crates/borderless-app/assets/icons/refresh-cw.svg`
- Create: `crates/borderless-app/assets/icons/play.svg`
- Create: `crates/borderless-app/assets/icons/square.svg`
- Create: `crates/borderless-app/assets/icons/x.svg`

- [ ] **Step 1: Vendor official Lucide SVG assets**

Run:

```powershell
New-Item -ItemType Directory -Force crates/borderless-app/assets/icons | Out-Null
Invoke-WebRequest https://raw.githubusercontent.com/lucide-icons/lucide/main/icons/settings.svg -OutFile crates/borderless-app/assets/icons/settings.svg
Invoke-WebRequest https://raw.githubusercontent.com/lucide-icons/lucide/main/icons/save.svg -OutFile crates/borderless-app/assets/icons/save.svg
Invoke-WebRequest https://raw.githubusercontent.com/lucide-icons/lucide/main/icons/refresh-cw.svg -OutFile crates/borderless-app/assets/icons/refresh-cw.svg
Invoke-WebRequest https://raw.githubusercontent.com/lucide-icons/lucide/main/icons/play.svg -OutFile crates/borderless-app/assets/icons/play.svg
Invoke-WebRequest https://raw.githubusercontent.com/lucide-icons/lucide/main/icons/square.svg -OutFile crates/borderless-app/assets/icons/square.svg
Invoke-WebRequest https://raw.githubusercontent.com/lucide-icons/lucide/main/icons/x.svg -OutFile crates/borderless-app/assets/icons/x.svg
```

Expected: six SVGs from the official Lucide repository are stored locally; no runtime network request is needed.

- [ ] **Step 2: Define the complete UI contract**

Replace the root component declaration with these properties and callbacks:

```slint
import {
    Button, CheckBox, ComboBox, LineEdit, ListView, ProgressIndicator,
    SpinBox, Switch
} from "std-widgets.slint";

export struct ActivityRow {
    time: string,
    kind: string,
    message: string,
}

export component AppWindow inherits Window {
    title: "Borderless";
    preferred-width: 1080px;
    preferred-height: 660px;
    min-width: 920px;
    min-height: 600px;
    background: #f3f5f6;

    in-out property <bool> controller-role: true;
    in-out property <string> target-host: "192.168.1.2";
    in-out property <int> target-port: 24800;
    in-out property <string> listen-host: "0.0.0.0";
    in-out property <int> listen-port: 24800;
    in-out property <int> remote-position-index: 1;
    in-out property <bool> clipboard-text: true;
    in-out property <bool> clipboard-html: true;
    in-out property <bool> clipboard-images: true;
    in-out property <bool> file-copy-paste: true;
    in-out property <bool> file-drag-drop: true;

    in property <string> connection-label: "已停止";
    in property <string> latency-label: "-";
    in property <string> control-label: "本机";
    in property <bool> connected: false;
    in property <bool> running: false;
    in property <bool> transfer-active: false;
    in property <float> transfer-progress: 0;
    in property <string> transfer-file: "";
    in property <string> transfer-detail: "";
    in property <string> transfer-destination: "";
    in-out property <string> config-error-message: "";
    in property <string> runtime-error-message: "";
    in property <[ActivityRow]> activities: [];

    in-out property <bool> advanced-visible: false;
    in-out property <int> bulk-port: 24802;
    in-out property <string> cache-directory: "";
    in-out property <string> max-clipboard-bytes: "33554432";
    in-out property <string> max-file-bytes: "21474836480";
    in-out property <int> edge-trigger-px: 2;
    in-out property <bool> debug-logging: false;

    callback save-requested();
    callback start-requested();
    callback stop-requested();
    callback reconnect-requested();
    callback cancel-transfer-requested();
    callback advanced-save-requested();
}
```

- [ ] **Step 3: Build the split layout from stable full-width bands**

Implement the root visual tree with this structure. Use the exact palette and dimensions shown; do not add nested decorative cards or gradients:

```slint
VerticalLayout {
    spacing: 0px;
    Rectangle {
        height: 58px;
        background: #172126;
        HorizontalLayout {
            padding-left: 20px;
            padding-right: 20px;
            alignment: space-between;
            Text { text: "Borderless"; color: white; font-size: 18px; font-weight: 700; }
            HorizontalLayout {
                spacing: 10px;
                Text {
                    text: root.connection-label + " · TCP · " + root.latency-label;
                    color: root.connected ? #bdebd8 : #d7dde0;
                    vertical-alignment: center;
                }
                Button {
                    icon: @image-url("../assets/icons/settings.svg");
                    accessible-label: "高级设置";
                    clicked => { root.advanced-visible = true; }
                }
                Button {
                    visible: root.running;
                    icon: @image-url("../assets/icons/refresh-cw.svg");
                    accessible-label: "重新连接";
                    clicked => { root.reconnect-requested(); }
                }
                Button {
                    text: root.running ? "停止共享" : "开始共享";
                    icon: root.running ? @image-url("../assets/icons/square.svg") : @image-url("../assets/icons/play.svg");
                    clicked => {
                        if root.running { root.stop-requested(); }
                        else { root.start-requested(); }
                    }
                }
            }
        }
    }
    HorizontalLayout {
        spacing: 0px;
        Rectangle {
            width: 370px;
            background: white;
            VerticalLayout {
                padding: 20px;
                spacing: 12px;
                Text { text: "连接设置"; font-size: 16px; font-weight: 700; }
                HorizontalLayout {
                    Button { text: "控制端"; enabled: !root.controller-role; clicked => { root.controller-role = true; } }
                    Button { text: "被控端"; enabled: root.controller-role; clicked => { root.controller-role = false; } }
                }
                Text { text: root.controller-role ? "目标电脑" : "监听地址"; color: #55646b; }
                if root.controller-role : LineEdit { text <=> root.target-host; }
                if !root.controller-role : LineEdit { text <=> root.listen-host; }
                Text { text: "TCP 端口"; color: #55646b; }
                if root.controller-role : SpinBox { value <=> root.target-port; minimum: 1; maximum: 65535; }
                if !root.controller-role : SpinBox { value <=> root.listen-port; minimum: 1; maximum: 65535; }
                if root.controller-role : VerticalLayout {
                    Text { text: "目标电脑的位置"; color: #55646b; }
                    ComboBox {
                        model: ["左侧", "右侧", "上方", "下方"];
                        current-index <=> root.remote-position-index;
                    }
                }
                Rectangle { height: 1px; background: #e2e6e8; }
                Text { text: "共享内容"; font-size: 16px; font-weight: 700; }
                Switch { text: "文本"; checked <=> root.clipboard-text; }
                Switch { text: "富文本"; checked <=> root.clipboard-html; }
                Switch { text: "图片"; checked <=> root.clipboard-images; }
                Switch { text: "复制文件"; checked <=> root.file-copy-paste; }
                Switch { text: "文件拖放"; checked <=> root.file-drag-drop; }
                Rectangle { vertical-stretch: 1; }
                Button {
                    text: "保存设置";
                    icon: @image-url("../assets/icons/save.svg");
                    clicked => { root.save-requested(); }
                }
            }
        }
        Rectangle { width: 1px; background: #d4dade; }
        VerticalLayout {
            padding: 22px;
            spacing: 12px;
            HorizontalLayout {
                alignment: space-between;
                Text { text: "实时状态"; font-size: 16px; font-weight: 700; }
                Text { text: root.connected ? "两台电脑运行正常" : root.connection-label; color: #68777e; }
            }
            Rectangle {
                height: 118px;
                background: white;
                border-width: 1px;
                border-color: #d2d9dc;
                border-radius: 6px;
                HorizontalLayout {
                    padding: 16px;
                    alignment: space-between;
                    Text { text: "本机\n" + (root.controller-role ? "控制端" : "被控端"); }
                    Text { text: "TCP"; color: #137d66; font-weight: 700; }
                    Text { text: "远程电脑\n" + root.control-label; }
                }
            }
            Rectangle {
                visible: root.transfer-active;
                height: 104px;
                background: white;
                border-width: 1px;
                border-color: #d7dddf;
                VerticalLayout {
                    padding: 14px;
                    HorizontalLayout {
                        alignment: space-between;
                        Text { text: root.transfer-file; font-weight: 700; }
                        Button { text: "取消"; clicked => { root.cancel-transfer-requested(); } }
                    }
                    ProgressIndicator { progress: root.transfer-progress; }
                    Text { text: root.transfer-detail + " · " + root.transfer-destination; color: #68777e; }
                }
            }
            Rectangle {
                background: white;
                border-width: 1px;
                border-color: #d2d9dc;
                border-radius: 6px;
                VerticalLayout {
                    padding: 14px;
                    Text { text: "最近活动"; font-weight: 700; }
                    ListView {
                        for row in root.activities : HorizontalLayout {
                            Text { width: 72px; text: row.time; color: #68777e; }
                            Text { width: 80px; text: row.kind; color: #137d66; }
                            Text { text: row.message; overflow: elide; }
                        }
                    }
                }
            }
            Text {
                visible: root.config-error-message != "" || root.runtime-error-message != "";
                text: root.config-error-message != ""
                    ? root.config-error-message
                    : root.runtime-error-message;
                color: #b43e3e;
                wrap: word-wrap;
            }
        }
    }
}
```

Add this overlay as the final child of `AppWindow`, after the main `VerticalLayout`:

```slint
if root.advanced-visible : Rectangle {
    x: 0px;
    y: 0px;
    width: parent.width;
    height: parent.height;
    background: #17212699;

    Rectangle {
        width: 430px;
        height: 500px;
        x: (parent.width - self.width) / 2;
        y: (parent.height - self.height) / 2;
        background: white;
        border-radius: 6px;
        VerticalLayout {
            padding: 20px;
            spacing: 10px;
            HorizontalLayout {
                alignment: space-between;
                Text { text: "高级设置"; font-size: 18px; font-weight: 700; }
                Button {
                    icon: @image-url("../assets/icons/x.svg");
                    clicked => { root.advanced-visible = false; }
                }
            }
            Text { text: "批量传输 TCP 端口"; color: #55646b; }
            SpinBox { value <=> root.bulk-port; minimum: 1; maximum: 65535; }
            Text { text: "接收缓存目录"; color: #55646b; }
            LineEdit { text <=> root.cache-directory; }
            Text { text: "最大剪贴板字节数"; color: #55646b; }
            LineEdit { text <=> root.max-clipboard-bytes; input-type: number; }
            Text { text: "最大文件字节数"; color: #55646b; }
            LineEdit { text <=> root.max-file-bytes; input-type: number; }
            Text { text: "边缘触发宽度"; color: #55646b; }
            SpinBox { value <=> root.edge-trigger-px; minimum: 1; maximum: 32; }
            CheckBox { text: "调试日志"; checked <=> root.debug-logging; }
            Rectangle { vertical-stretch: 1; }
            Button {
                text: "保存高级设置";
                icon: @image-url("../assets/icons/save.svg");
                clicked => {
                    root.advanced-save-requested();
                    root.advanced-visible = false;
                }
            }
        }
    }
}
```

- [ ] **Step 4: Compile the Slint file**

Run:

```powershell
cargo check -p borderless-app
```

Expected: PASS with the pinned Slint `1.17.1` compiler.

- [ ] **Step 5: Commit**

```powershell
git add crates/borderless-app/ui/main.slint crates/borderless-app/assets/icons
git commit -m "feat: build Slint split control desk"
```

---

### Task 4: Wire Config Commands and Persistence

**Files:**
- Create: `crates/borderless-app/src/ui_bridge.rs`
- Modify: `crates/borderless-app/src/main.rs`
- Test: `crates/borderless-app/src/ui_bridge.rs`

- [ ] **Step 1: Add parsing tests for UI numeric fields**

Create `ui_bridge.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_advanced_numeric_settings() {
        assert_eq!(parse_u64_setting("33554432", "最大剪贴板大小").unwrap(), 33_554_432);
        assert_eq!(parse_u64_setting("21474836480", "最大文件大小").unwrap(), 21_474_836_480);
    }

    #[test]
    fn rejects_empty_or_non_numeric_advanced_settings() {
        assert!(parse_u64_setting("", "最大文件大小").is_err());
        assert!(parse_u64_setting("20GB", "最大文件大小").is_err());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```powershell
cargo test -p borderless-app ui_bridge::tests
```

Expected: FAIL because the parser does not exist.

- [ ] **Step 3: Implement parsing and window/config mapping**

Add:

```rust
use std::{cell::RefCell, path::Path, rc::Rc, time::Duration};

use anyhow::{anyhow, Context};
use borderless_core::config::{AppConfig, RemotePosition, Role};
use slint::ComponentHandle;

use crate::{
    runtime::{RuntimeCommand, RuntimeHandle},
    status::AppStatus,
    ui::AppWindow,
    ui_model::ConfigDraft,
};

const CONFIG_PATH: &str = "config.toml";

fn parse_u64_setting(value: &str, label: &str) -> anyhow::Result<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("{label}不能为空"));
    }
    trimmed
        .parse::<u64>()
        .with_context(|| format!("{label}必须是正整数"))
}

fn position_from_index(index: i32) -> RemotePosition {
    match index {
        0 => RemotePosition::Left,
        2 => RemotePosition::Top,
        3 => RemotePosition::Bottom,
        _ => RemotePosition::Right,
    }
}

fn position_index(position: &RemotePosition) -> i32 {
    match position {
        RemotePosition::Left => 0,
        RemotePosition::Right => 1,
        RemotePosition::Top => 2,
        RemotePosition::Bottom => 3,
    }
}
```

Implement the generated-property mapping explicitly:

```rust
fn draft_from_window(window: &AppWindow) -> anyhow::Result<ConfigDraft> {
    Ok(ConfigDraft {
        role: if window.get_controller_role() {
            Role::Controller
        } else {
            Role::Agent
        },
        target_host: window.get_target_host().to_string(),
        target_port: u16::try_from(window.get_target_port())
            .context("目标 TCP 端口超出范围")?,
        listen_host: window.get_listen_host().to_string(),
        listen_port: u16::try_from(window.get_listen_port())
            .context("监听 TCP 端口超出范围")?,
        remote_position: position_from_index(window.get_remote_position_index()),
        clipboard_text: window.get_clipboard_text(),
        clipboard_html: window.get_clipboard_html(),
        clipboard_images: window.get_clipboard_images(),
        file_copy_paste: window.get_file_copy_paste(),
        file_drag_drop: window.get_file_drag_drop(),
    })
}

fn apply_config_to_window(window: &AppWindow, config: &AppConfig) {
    window.set_controller_role(matches!(&config.role, Role::Controller));
    window.set_target_host(config.controller.agent_host.clone().into());
    window.set_target_port(i32::from(config.controller.agent_port));
    window.set_listen_host(config.agent.listen_host.clone().into());
    window.set_listen_port(i32::from(config.agent.listen_port));
    window.set_remote_position_index(position_index(&config.controller.remote_position));
    window.set_clipboard_text(config.sharing.clipboard_text);
    window.set_clipboard_html(config.sharing.clipboard_html);
    window.set_clipboard_images(config.sharing.clipboard_images);
    window.set_file_copy_paste(config.sharing.file_copy_paste);
    window.set_file_drag_drop(config.sharing.file_drag_drop);
    window.set_bulk_port(i32::from(config.sharing.bulk_transfer_port));
    window.set_cache_directory(config.sharing.incoming_cache_dir.clone().into());
    window.set_max_clipboard_bytes(config.sharing.max_clipboard_bytes.to_string().into());
    window.set_max_file_bytes(config.sharing.max_file_transfer_bytes.to_string().into());
    window.set_edge_trigger_px(config.edge_trigger_px);
    window.set_debug_logging(config.debug_logging);
}

fn apply_advanced_from_window(
    window: &AppWindow,
    config: &mut AppConfig,
) -> anyhow::Result<()> {
    config.sharing.bulk_transfer_port = u16::try_from(window.get_bulk_port())
        .context("批量传输 TCP 端口超出范围")?;
    config.sharing.incoming_cache_dir = window.get_cache_directory().to_string();
    config.sharing.max_clipboard_bytes = parse_u64_setting(
        &window.get_max_clipboard_bytes(),
        "最大剪贴板大小",
    )?;
    config.sharing.max_file_transfer_bytes = parse_u64_setting(
        &window.get_max_file_bytes(),
        "最大文件大小",
    )?;
    config.edge_trigger_px = window.get_edge_trigger_px();
    config.debug_logging = window.get_debug_logging();
    config.validate().map_err(anyhow::Error::from)
}
```

Wire callbacks using `Rc<RefCell<AppConfig>>` and cloned `RuntimeHandle`. Use this shared save helper from both regular and advanced callbacks:

```rust
fn save_next_config(
    window: &AppWindow,
    shared: &Rc<RefCell<AppConfig>>,
    mutate: impl FnOnce(&mut AppConfig) -> anyhow::Result<()>,
) -> Option<AppConfig> {
    let mut next = shared.borrow().clone();
    let result = mutate(&mut next)
        .and_then(|()| next.validate().map_err(anyhow::Error::from))
        .and_then(|()| next.save_to_path(CONFIG_PATH).map_err(anyhow::Error::from));
    match result {
        Ok(()) => {
            *shared.borrow_mut() = next;
            window.set_config_error_message("".into());
            Some(shared.borrow().clone())
        }
        Err(error) => {
            window.set_config_error_message(error.to_string().into());
            None
        }
    }
}
```

Add one helper that applies every editable field before starting or reconnecting:

```rust
fn apply_all_from_window(window: &AppWindow, config: &mut AppConfig) -> anyhow::Result<()> {
    draft_from_window(window)?.apply_to(config);
    apply_advanced_from_window(window, config)
}
```

Wire every declared callback explicitly. `status` is an `Rc<RefCell<AppStatus>>` initialized in `run_app`; Task 5 updates it from runtime events:

```rust
fn wire_callbacks(
    window: &AppWindow,
    runtime: &RuntimeHandle,
    config: &Rc<RefCell<AppConfig>>,
    status: &Rc<RefCell<AppStatus>>,
) {
    let weak = window.as_weak();
    let config_for_save = Rc::clone(config);
    window.on_save_requested(move || {
        let Some(window) = weak.upgrade() else { return };
        let _ = save_next_config(&window, &config_for_save, |next| {
            draft_from_window(&window)?.apply_to(next);
            Ok(())
        });
    });

    let weak = window.as_weak();
    let config_for_advanced = Rc::clone(config);
    window.on_advanced_save_requested(move || {
        let Some(window) = weak.upgrade() else { return };
        let _ = save_next_config(&window, &config_for_advanced, |next| {
            apply_advanced_from_window(&window, next)
        });
    });

    let weak = window.as_weak();
    let config_for_start = Rc::clone(config);
    let runtime_for_start = runtime.clone();
    window.on_start_requested(move || {
        let Some(window) = weak.upgrade() else { return };
        if let Some(next) = save_next_config(&window, &config_for_start, |config| {
            apply_all_from_window(&window, config)
        }) {
            runtime_for_start.send(RuntimeCommand::Start(next));
        }
    });

    let weak = window.as_weak();
    let config_for_reconnect = Rc::clone(config);
    let runtime_for_reconnect = runtime.clone();
    window.on_reconnect_requested(move || {
        let Some(window) = weak.upgrade() else { return };
        if let Some(next) = save_next_config(&window, &config_for_reconnect, |config| {
            apply_all_from_window(&window, config)
        }) {
            runtime_for_reconnect.send(RuntimeCommand::Reconnect(next));
        }
    });

    let runtime_for_stop = runtime.clone();
    window.on_stop_requested(move || runtime_for_stop.send(RuntimeCommand::Stop));

    let runtime_for_cancel = runtime.clone();
    let status_for_cancel = Rc::clone(status);
    window.on_cancel_transfer_requested(move || {
        if let Some(transfer_id) = status_for_cancel.borrow().transfer_id {
            runtime_for_cancel.send(RuntimeCommand::CancelTransfer(transfer_id));
        }
    });
}
```

Create `run_app` with an explicit missing-file fallback. Existing invalid files remain visible as configuration errors:

```rust
pub fn run_app() -> Result<(), slint::PlatformError> {
    let window = AppWindow::new()?;
    let (config, load_error) = match AppConfig::load_from_path(CONFIG_PATH) {
        Ok(config) => (config, None),
        Err(_) if !Path::new(CONFIG_PATH).exists() => (AppConfig::default(), None),
        Err(error) => (
            AppConfig::default(),
            Some(format!("无法加载 {CONFIG_PATH}：{error}")),
        ),
    };
    apply_config_to_window(&window, &config);
    if let Some(error) = load_error {
        window.set_config_error_message(error.into());
    }

    let runtime = RuntimeHandle::spawn();
    let config = Rc::new(RefCell::new(config));
    let status = Rc::new(RefCell::new(AppStatus::default()));
    wire_callbacks(&window, &runtime, &config, &status);
    window.run()
}
```

Expose `mod ui_bridge;` from `main.rs` while keeping the eframe entry point active until Task 6. Never perform blocking file transfer work inside a callback.

- [ ] **Step 4: Run parser and config tests**

Run:

```powershell
cargo test -p borderless-app ui_bridge::tests
cargo test -p borderless-core config::tests
```

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add crates/borderless-app/src/ui_bridge.rs crates/borderless-app/src/main.rs
git commit -m "feat: connect Slint settings to runtime"
```

---

### Task 5: Bind Live Runtime Status and Activity

**Files:**
- Modify: `crates/borderless-app/src/ui_bridge.rs`
- Modify: `crates/borderless-app/src/ui_model.rs`
- Modify: `crates/borderless-app/ui/main.slint`
- Test: `crates/borderless-app/src/ui_model.rs`

- [ ] **Step 1: Add activity projection tests**

Add:

```rust
#[test]
fn event_log_projects_newest_items_first_with_kind_labels() {
    let mut status = AppStatus::default();
    status.push_log("connected 192.168.1.2 via TCP");
    status.push_log("bulk transfer started: design.pdf");

    let rows = activity_rows(&status);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].kind, "文件");
    assert!(rows[0].message.contains("design.pdf"));
    assert_eq!(rows[1].kind, "连接");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:

```powershell
cargo test -p borderless-app event_log_projects_newest_items_first_with_kind_labels
```

Expected: FAIL because `activity_rows` and `ActivityView` are absent.

- [ ] **Step 3: Add activity projection**

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityView {
    pub time: String,
    pub kind: String,
    pub message: String,
}

pub fn activity_rows(status: &AppStatus) -> Vec<ActivityView> {
    status
        .events
        .iter()
        .rev()
        .take(100)
        .map(|message| {
            let lower = message.to_ascii_lowercase();
            let kind = if lower.contains("transfer") || lower.contains("file") {
                "文件"
            } else if lower.contains("clipboard") {
                "剪贴板"
            } else if lower.contains("control") {
                "控制"
            } else if lower.contains("connect") {
                "连接"
            } else {
                "系统"
            };
            ActivityView {
                time: String::new(),
                kind: kind.to_string(),
                message: message.clone(),
            }
        })
        .collect()
}
```

Do not invent timestamps for existing log strings. A later runtime event may carry structured timestamps; until then the UI time column remains empty.

- [ ] **Step 4: Poll runtime events on the UI thread**

In `run_app`, keep a `slint::Timer` alive for the duration of `window.run()`:

```rust
let timer = slint::Timer::default();
let weak = window.as_weak();
let runtime_for_timer = runtime.clone();
let status_for_timer = Rc::clone(&status);
timer.start(
    slint::TimerMode::Repeated,
    Duration::from_millis(100),
    move || {
        let Some(window) = weak.upgrade() else { return };
        for event in runtime_for_timer.drain_events() {
            match event {
                RuntimeEvent::Status(mut next) => {
                    next.events = status_for_timer.borrow().events.clone();
                    *status_for_timer.borrow_mut() = next;
                }
                RuntimeEvent::Log(message) => status_for_timer.borrow_mut().push_log(message),
            }
        }
        apply_status_to_window(&window, &status_for_timer.borrow());
    },
);
```

`apply_status_to_window` sets every live property and constructs a `VecModel<ActivityRow>`:

```rust
fn apply_status_to_window(window: &AppWindow, status: &AppStatus) {
    let view = UiSnapshot::from_status(status);
    window.set_connection_label(view.connection_label.into());
    window.set_latency_label(view.latency_label.into());
    window.set_control_label(view.control_label.into());
    window.set_connected(view.connected);
    window.set_running(view.running);
    window.set_transfer_active(view.transfer_active);
    window.set_transfer_progress(view.transfer_progress);
    window.set_transfer_file(view.transfer_file.into());
    window.set_transfer_detail(view.transfer_detail.into());
    window.set_transfer_destination(view.transfer_destination.into());
    window.set_runtime_error_message(view.last_error.into());

    let rows = activity_rows(status)
        .into_iter()
        .map(|row| crate::ui::ActivityRow {
            time: row.time.into(),
            kind: row.kind.into(),
            message: row.message.into(),
        })
        .collect::<Vec<_>>();
    window.set_activities(slint::ModelRc::new(slint::VecModel::from(rows)));
}
```

- [ ] **Step 5: Run model and app tests**

Run:

```powershell
cargo test -p borderless-app ui_model::tests
cargo test -p borderless-app
```

Expected: PASS.

- [ ] **Step 6: Commit**

```powershell
git add crates/borderless-app/src/ui_bridge.rs crates/borderless-app/src/ui_model.rs crates/borderless-app/ui/main.slint
git commit -m "feat: show live runtime status in Slint"
```

---

### Task 6: Switch the Entry Point and Remove eframe/egui

**Files:**
- Modify: `crates/borderless-app/src/main.rs`
- Delete: `crates/borderless-app/src/app.rs`
- Modify: `crates/borderless-app/Cargo.toml`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `README.md`

- [ ] **Step 1: Add a shutdown-order unit test**

Extract a small helper in `ui_bridge.rs` and test it:

```rust
#[test]
fn closing_the_ui_requests_runtime_stop() {
    let (tx, rx) = crossbeam_channel::unbounded();
    request_runtime_stop(|command| tx.send(command).unwrap());
    assert!(matches!(rx.recv().unwrap(), RuntimeCommand::Stop));
}

fn request_runtime_stop(send: impl FnOnce(RuntimeCommand)) {
    send(RuntimeCommand::Stop);
}
```

- [ ] **Step 2: Replace `main` with Slint startup**

Use:

```rust
#![cfg_attr(windows, windows_subsystem = "windows")]

mod logging;
mod runtime;
mod status;
mod ui;
mod ui_bridge;
mod ui_model;

fn main() -> Result<(), slint::PlatformError> {
    let _logging_guard = logging::init_logging(false).ok();
    let _ = borderless_win::dpi::enable_per_monitor_dpi_awareness();
    ui_bridge::run_app()
}
```

At the end of `run_app`, stop the runtime before returning:

```rust
let result = window.run();
request_runtime_stop(|command| runtime.send(command));
result
```

- [ ] **Step 3: Delete eframe UI and dependencies**

Delete `src/app.rs`. Remove these two lines from the app manifest and workspace dependency declarations:

```toml
eframe.workspace = true
egui.workspace = true
```

Regenerate `Cargo.lock` with `cargo check --workspace`.

- [ ] **Step 4: Run all tests and launch the window**

Run:

```powershell
cargo test --workspace
cargo run -p borderless-app
```

Expected: tests pass; a native Chinese Slint window opens with the approved split control desk. Closing the window returns local Hook/input state and exits without a hanging Tokio task.

- [ ] **Step 5: Commit**

```powershell
git add Cargo.toml Cargo.lock crates/borderless-app README.md
git commit -m "refactor: replace egui with Slint control desk"
```

---

### Task 7: Visual and Release Quality Gate

**Files:**
- No planned file changes.

- [ ] **Step 1: Verify desktop scaling and stable layout**

Run the app at Windows scaling values 100%, 125%, 150%, and 200%. At each scale verify:

- no text clips in buttons, segmented role controls, direction options, status, or activity rows;
- the 370 px settings column remains stable;
- the activity list scrolls without resizing the window;
- the advanced settings overlay fits the minimum window size;
- the transfer row does not shift the top connection band.

Capture screenshots at 100% and 150% and inspect them before continuing.

- [ ] **Step 2: Verify all expected UI states**

Exercise these states through runtime tests or a local controller/agent pair:

```text
已停止
等待连接
正在连接
已连接 / 本机控制
远程电脑控制
正在重连
文件传输进行中
传输失败
权限错误
```

Expected: every state has a clear Chinese label; no control is enabled when its command is invalid.

- [ ] **Step 3: Run the full quality gate**

Run:

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release -p borderless-app
```

Expected: all commands exit `0`.

- [ ] **Step 4: Confirm the phase is clean**

Run:

```powershell
git status --short
```

Expected: no uncommitted files from the Slint tasks. If visual verification found a defect, return to the task that owns that layout or bridge behavior, add its concrete correction and verification there, then rerun this gate.

---

## Phase Acceptance

- No `eframe` or `egui` dependency remains.
- The app runs as the approved Chinese native Slint split control desk.
- Every prior configuration and runtime action remains reachable.
- The UI thread performs no network or file transfer work.
- Window close sends Stop and restores input state.
- Formatting, tests, strict lint, release build, and DPI screenshots pass.
- Proceed to `docs/superpowers/plans/2026-07-13-bidirectional-targeted-drag-drop.md`.
