#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod capture;
mod chrome;
mod discovery;
mod host;
mod input;
mod preview;
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
                .with_inner_size([1080.0, 720.0])
                .with_min_inner_size([480.0, 360.0])
                .with_transparent(cfg!(target_os = "windows"))
                .with_fullsize_content_view(true)
                .with_title_shown(false)
                .with_titlebar_shown(false)
                .with_titlebar_buttons_shown(true),
            ..Default::default()
        },
        Box::new(|cc| {
            let mut app = ui::Localnet::new(&cc.egui_ctx);
            app.chrome = chrome::Chrome::install(cc);
            app.enable_discovery();
            Ok(Box::new(app))
        }),
    )
}
