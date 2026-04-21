use sqeel_core::{AppState, UiProvider};
use sqeel_gui::GuiProvider;

fn main() -> anyhow::Result<()> {
    let state = AppState::new();
    GuiProvider::run(state)
}
