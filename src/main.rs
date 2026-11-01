#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod capture;
mod host;
mod input;
mod protocol;
mod transport;
mod ui;
mod viewer;

fn main() -> eframe::Result {
    #[cfg(target_os = "windows")]
    let _ = enigo::set_dpi_awareness();
    eframe::run_native(
        "Localnet",
        eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default()
                .with_inner_size([1100.0, 760.0])
                .with_min_inner_size([640.0, 480.0]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(ui::Localnet::new(&cc.egui_ctx)))),
    )
}
