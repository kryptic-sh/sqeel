use egui::Vec2;
use egui_term::{BackendSettings, PtyEvent, TerminalBackend, TerminalView};
use std::sync::mpsc::Receiver;

pub struct GuiApp {
    terminal_backend: TerminalBackend,
    pty_receiver: Receiver<(u64, PtyEvent)>,
}

impl GuiApp {
    pub fn new(cc: &eframe::CreationContext<'_>, sqeel_bin: String) -> Self {
        let (pty_sender, pty_receiver) = std::sync::mpsc::channel();
        let terminal_backend = TerminalBackend::new(
            0,
            cc.egui_ctx.clone(),
            pty_sender,
            BackendSettings {
                shell: sqeel_bin,
                ..Default::default()
            },
        )
        .expect("failed to create terminal backend");

        Self {
            terminal_backend,
            pty_receiver,
        }
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Ok((_, PtyEvent::Exit)) = self.pty_receiver.try_recv() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            let terminal = TerminalView::new(ui, &mut self.terminal_backend)
                .set_focus(true)
                .set_size(Vec2::new(ui.available_width(), ui.available_height()));
            ui.add(terminal);
        });
    }
}

/// Find the `sqeel` binary: prefer sibling next to current exe, fall back to PATH.
pub fn find_sqeel_binary() -> String {
    if let Ok(mut exe) = std::env::current_exe() {
        exe.pop();
        let candidate = exe.join("sqeel");
        if candidate.exists() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    "sqeel".to_string()
}
