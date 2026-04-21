use crate::AppState;
use std::sync::{Arc, Mutex};

pub trait UiProvider {
    fn run(state: Arc<Mutex<AppState>>) -> anyhow::Result<()>
    where
        Self: Sized;
}
