mod app;
mod logging;
mod runtime;
mod status;

fn main() -> eframe::Result<()> {
    let _ = borderless_win::dpi::enable_per_monitor_dpi_awareness();

    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Borderless",
        native_options,
        Box::new(|_cc| Ok(Box::new(app::BorderlessApp::new()))),
    )
}
