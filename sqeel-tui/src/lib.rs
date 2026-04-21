use std::sync::{Arc, Mutex};
use sqeel_core::{AppState, UiProvider};

pub struct TuiProvider;

impl UiProvider for TuiProvider {
    fn run(state: Arc<Mutex<AppState>>) -> anyhow::Result<()> {
        let _ = state;
        // M1: ratatui app loop goes here
        println!("sqeel TUI — not yet implemented");
        Ok(())
    }
}
