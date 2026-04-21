use std::sync::{Arc, Mutex};
use crate::AppState;

pub trait UiProvider {
    fn run(state: Arc<Mutex<AppState>>) -> anyhow::Result<()>
    where
        Self: Sized;
}
