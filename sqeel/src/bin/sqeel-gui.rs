use sqeel_gui::{GuiApp, find_sqeel_binary};

fn main() -> anyhow::Result<()> {
    let sqeel_bin = find_sqeel_binary();

    eframe::run_native(
        "sqeel",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1024.0, 768.0])
                .with_min_inner_size([400.0, 300.0])
                .with_title("sqeel"),
            ..Default::default()
        },
        Box::new(move |cc| Ok(Box::new(GuiApp::new(cc, sqeel_bin)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
