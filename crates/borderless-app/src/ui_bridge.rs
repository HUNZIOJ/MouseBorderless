use std::{cell::RefCell, path::Path, rc::Rc, time::Duration};

use anyhow::{anyhow, Context};
use borderless_core::config::{AppConfig, RemotePosition, Role};
use slint::ComponentHandle;

use crate::{
    runtime::{RuntimeCommand, RuntimeEvent, RuntimeHandle},
    status::AppStatus,
    ui::AppWindow,
    ui_model::{activity_rows, ConfigDraft, UiSnapshot},
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

fn draft_from_window(window: &AppWindow) -> anyhow::Result<ConfigDraft> {
    Ok(ConfigDraft {
        role: if window.get_controller_role() {
            Role::Controller
        } else {
            Role::Agent
        },
        target_host: window.get_target_host().to_string(),
        target_port: u16::try_from(window.get_target_port()).context("目标 TCP 端口超出范围")?,
        listen_host: window.get_listen_host().to_string(),
        listen_port: u16::try_from(window.get_listen_port()).context("监听 TCP 端口超出范围")?,
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

fn apply_advanced_from_window(window: &AppWindow, config: &mut AppConfig) -> anyhow::Result<()> {
    config.sharing.bulk_transfer_port =
        u16::try_from(window.get_bulk_port()).context("批量传输 TCP 端口超出范围")?;
    config.sharing.incoming_cache_dir = window.get_cache_directory().to_string();
    config.sharing.max_clipboard_bytes =
        parse_u64_setting(&window.get_max_clipboard_bytes(), "最大剪贴板大小")?;
    config.sharing.max_file_transfer_bytes =
        parse_u64_setting(&window.get_max_file_bytes(), "最大文件大小")?;
    config.edge_trigger_px = window.get_edge_trigger_px();
    config.debug_logging = window.get_debug_logging();
    config.validate().map_err(anyhow::Error::from)
}

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

fn apply_all_from_window(window: &AppWindow, config: &mut AppConfig) -> anyhow::Result<()> {
    draft_from_window(window)?.apply_to(config);
    apply_advanced_from_window(window, config)
}

fn wire_callbacks(
    window: &AppWindow,
    runtime: &RuntimeHandle,
    config: &Rc<RefCell<AppConfig>>,
    status: &Rc<RefCell<AppStatus>>,
) {
    let weak = window.as_weak();
    let config_for_save = Rc::clone(config);
    window.on_save_requested(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let _ = save_next_config(&window, &config_for_save, |next| {
            draft_from_window(&window)?.apply_to(next);
            Ok(())
        });
    });

    let weak = window.as_weak();
    let config_for_advanced = Rc::clone(config);
    window.on_advanced_save_requested(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let _ = save_next_config(&window, &config_for_advanced, |next| {
            apply_advanced_from_window(&window, next)
        });
    });

    let weak = window.as_weak();
    let config_for_start = Rc::clone(config);
    let runtime_for_start = runtime.clone();
    window.on_start_requested(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
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
        let Some(window) = weak.upgrade() else {
            return;
        };
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
    window.set_drag_state(view.drag_state.into());
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

fn request_runtime_stop(send: impl FnOnce(RuntimeCommand)) {
    send(RuntimeCommand::Stop);
}

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
    apply_status_to_window(&window, &status.borrow());

    let timer = slint::Timer::default();
    let weak = window.as_weak();
    let runtime_for_timer = runtime.clone();
    let status_for_timer = Rc::clone(&status);
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            for event in runtime_for_timer.drain_events() {
                match event {
                    RuntimeEvent::Status(mut next) => {
                        next.events = status_for_timer.borrow().events.clone();
                        *status_for_timer.borrow_mut() = next;
                    }
                    RuntimeEvent::Log(message) => {
                        status_for_timer.borrow_mut().push_log(message);
                    }
                }
            }
            apply_status_to_window(&window, &status_for_timer.borrow());
        },
    );
    let result = window.run();
    request_runtime_stop(|command| runtime.send(command));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_advanced_numeric_settings() {
        assert_eq!(
            parse_u64_setting("33554432", "最大剪贴板大小").unwrap(),
            33_554_432
        );
        assert_eq!(
            parse_u64_setting("21474836480", "最大文件大小").unwrap(),
            21_474_836_480
        );
    }

    #[test]
    fn rejects_empty_or_non_numeric_advanced_settings() {
        assert!(parse_u64_setting("", "最大文件大小").is_err());
        assert!(parse_u64_setting("20GB", "最大文件大小").is_err());
    }

    #[test]
    fn closing_the_ui_requests_runtime_stop() {
        let (tx, rx) = crossbeam_channel::unbounded();
        request_runtime_stop(|command| tx.send(command).unwrap());
        assert!(matches!(rx.recv().unwrap(), RuntimeCommand::Stop));
    }
}
