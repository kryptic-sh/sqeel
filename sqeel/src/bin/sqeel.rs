use sqeel_core::{AppState, UiProvider};
use sqeel_tui::TuiProvider;

fn main() -> anyhow::Result<()> {
    let state = AppState::new();
    TuiProvider::run(state)
}
