fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("ramtop — RAM monitor")
            .with_app_id("ramtop")
            .with_inner_size([1600.0, 1000.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ramtop",
        options,
        Box::new(|cc| {
            ramtop::gui::apply_style(&cc.egui_ctx);
            Ok(Box::new(ramtop::gui::RamApp::default()))
        }),
    )
}
