use std::sync::{Arc, Mutex};
use sqeel_core::{AppState, UiProvider};

pub struct GuiProvider;

impl UiProvider for GuiProvider {
    fn run(state: Arc<Mutex<AppState>>) -> anyhow::Result<()> {
        let _ = state;
        // M6: iced app loop goes here
        println!("sqeel GUI — not yet implemented");
        Ok(())
    }
}
