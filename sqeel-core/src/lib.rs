pub mod config;
pub mod db;
pub mod highlight;
pub mod lsp;
pub mod persistence;
pub mod provider;
pub mod schema;
pub mod state;

pub use provider::UiProvider;
pub use state::AppState;
