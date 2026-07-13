#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod logging;
mod runtime;
mod status;
mod ui;
mod ui_model;

fn main() -> eframe::Result<()> {
    let _logging_guard = logging::init_logging(false).ok();
    let _ = borderless_win::dpi::enable_per_monitor_dpi_awareness();

    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Borderless",
        native_options,
        Box::new(|_cc| Ok(Box::new(app::BorderlessApp::new()))),
    )
}

#[cfg(test)]
mod ui_compile_tests {
    #[test]
    fn generated_slint_window_type_is_available() {
        fn accepts_window(_: Option<crate::ui::AppWindow>) {}
        accepts_window(None);
    }
}
