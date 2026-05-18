use crate::state::Focus;
use serde::{Deserialize, Serialize};

// MainConfig, EditorConfig, connection helpers, and the loader entry points now
// live in sqeel-config. Re-export them here so existing call sites
// (`sqeel_core::config::MainConfig`, `sqeel_core::config::ConnectionConfig`, etc.)
// continue to work without any changes.
pub use sqeel_config::{
    ConnectionConfig, DEFAULTS_TOML, EditorConfig, KeybindingMode, MainConfig, MigrationResult,
    PgpassEntry, config_dir, delete_connection, delete_keyring_entry, load_connections,
    load_main_config, load_pgpass, migrate_connection_to_keyring, save_connection,
    set_config_dir_override,
};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct TabCursor {
    pub name: String,
    pub row: usize,
    pub col: usize,
}

/// Lightweight pointer persisted in session.toml for a single results tab.
/// Success rows live in a separate JSON under
/// `~/.local/share/sqeel/results/<conn>/<filename>.json`. Error + cancelled
/// outcomes are stored inline.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, Default)]
pub struct SavedResultRef {
    /// Present only for success tabs — on-disk JSON payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub scroll: usize,
    #[serde(default)]
    pub col_scroll: usize,
    /// Error text captured when the query failed. `None` for success /
    /// cancelled tabs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// True for tabs whose batch slot was skipped after an earlier error.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct Session {
    connection: String,
    #[serde(default)]
    schema_cursor: usize,
    #[serde(default)]
    schema_cursor_path: Option<String>,
    #[serde(default)]
    schema_expanded_paths: Vec<String>,
    #[serde(default)]
    focus: Focus,
    #[serde(default)]
    schema_search: Option<String>,
    #[serde(default)]
    tab_cursors: Vec<TabCursor>,
    #[serde(default)]
    active_tab: usize,
    #[serde(default)]
    result_tabs: Vec<SavedResultRef>,
    #[serde(default)]
    active_result_tab: usize,
}

/// Data restored from session.toml.
#[derive(Debug, Default)]
pub struct SessionData {
    pub connection: Option<String>,
    /// Numeric fallback cursor — used only when `schema_cursor_path` lookup fails.
    pub schema_cursor: usize,
    /// Preferred cursor: `"db/table/col"` path string for stable restore across schema changes.
    pub schema_cursor_path: Option<String>,
    /// Expanded node paths, e.g. `["mydb", "mydb/users"]`.
    pub schema_expanded_paths: Vec<String>,
    pub focus: Focus,
    pub schema_search: Option<String>,
    /// Per-tab editor cursor positions, keyed by tab name.
    pub tab_cursors: Vec<TabCursor>,
    pub active_tab: usize,
    pub result_tabs: Vec<SavedResultRef>,
    pub active_result_tab: usize,
}

/// Save session state to session.toml.
#[allow(clippy::too_many_arguments)]
pub fn save_session(
    name: &str,
    schema_cursor: usize,
    schema_cursor_path: Option<String>,
    schema_expanded_paths: Vec<String>,
    focus: Focus,
    schema_search: Option<String>,
    tab_cursors: Vec<TabCursor>,
    active_tab: usize,
    result_tabs: Vec<SavedResultRef>,
    active_result_tab: usize,
) -> anyhow::Result<()> {
    let dir = config_dir().ok_or_else(|| anyhow::anyhow!("cannot determine config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let content = toml::to_string(&Session {
        connection: name.to_string(),
        schema_cursor,
        schema_cursor_path,
        schema_expanded_paths,
        focus,
        schema_search,
        tab_cursors,
        active_tab,
        result_tabs,
        active_result_tab,
    })?;
    std::fs::write(dir.join("session.toml"), content)?;
    Ok(())
}

fn load_session_inner() -> Option<Session> {
    let path = config_dir()?.join("session.toml");
    let content = std::fs::read_to_string(path).ok()?;
    toml::from_str(&content).ok()
}

/// Load full session data (connection name + schema cursor).
pub fn load_session_data() -> SessionData {
    let Some(s) = load_session_inner() else {
        return SessionData::default();
    };
    SessionData {
        connection: if s.connection.is_empty() {
            None
        } else {
            Some(s.connection)
        },
        schema_cursor: s.schema_cursor,
        schema_cursor_path: s.schema_cursor_path,
        schema_expanded_paths: s.schema_expanded_paths,
        focus: s.focus,
        schema_search: s.schema_search,
        tab_cursors: s.tab_cursors,
        active_tab: s.active_tab,
        result_tabs: s.result_tabs,
        active_result_tab: s.active_result_tab,
    }
}

/// Load only the last-used connection name from session.toml.
pub fn load_session() -> Option<String> {
    load_session_inner().and_then(|s| {
        if s.connection.is_empty() {
            None
        } else {
            Some(s.connection)
        }
    })
}
