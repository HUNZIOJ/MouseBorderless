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

#[cfg(test)]
mod ui_compile_tests {
    #[test]
    fn generated_slint_window_type_is_available() {
        fn accepts_window(_: Option<crate::ui::AppWindow>) {}
        accepts_window(None);
    }

    #[test]
    fn generated_slint_window_exposes_control_desk_contract() {
        fn accepts_contract(window: &crate::ui::AppWindow) {
            window.set_controller_role(true);
            window.set_target_host("192.168.1.2".into());
            window.set_target_port(24800);
            window.set_connection_label("已停止".into());
            window.set_transfer_active(false);
            window.set_drag_state("".into());
            window.on_start_requested(|| {});
            window.on_stop_requested(|| {});
        }

        let _ = accepts_contract;
    }

    #[test]
    fn slint_bridge_entry_point_is_available() {
        let _ = crate::ui_bridge::run_app as fn() -> Result<(), slint::PlatformError>;
    }
}
