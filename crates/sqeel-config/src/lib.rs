//! Config loading for sqeel.
//!
//! Wraps [`hjkl_config::AppConfig`] to provide path resolution and
//! deep-merge loading for sqeel's `~/.config/sqeel/config.toml`.
//!
//! # Usage
//!
//! ```no_run
//! use sqeel_config::load_main_config;
//!
//! let cfg = load_main_config().unwrap();
//! println!("{}", cfg.editor.lsp_binary);
//! ```
//!
//! Missing file → bundled defaults. Never writes to disk.

use std::path::PathBuf;

use hjkl_config::{AppConfig, Validate, ValidationError, ensure_non_empty_str, ensure_non_zero};
pub use hjkl_engine::KeybindingMode;
use serde::{Deserialize, Serialize};

/// Bundled default config — the single source of truth for default values.
/// User overrides are deep-merged on top via [`hjkl_config::load_layered_from`].
pub const DEFAULTS_TOML: &str = include_str!("config.toml");

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MainConfig {
    pub editor: EditorConfig,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EditorConfig {
    pub keybindings: KeybindingMode,
    pub lsp_binary: String,
    pub mouse_scroll_lines: usize,
    pub leader_key: char,
    /// Whether `Ctrl+Shift+Enter` (run-all) stops on the first query error.
    pub stop_on_error: bool,
    /// Seconds before cached schema data (databases / tables / columns) is
    /// considered stale and re-fetched in the background. `0` disables TTL.
    pub schema_ttl_secs: u64,
}

impl Default for MainConfig {
    /// Parses the bundled [`DEFAULTS_TOML`]. Panics if the bundled file is
    /// malformed — that's a build-time bug caught by [`tests::defaults_parse`].
    fn default() -> Self {
        toml::from_str(DEFAULTS_TOML)
            .expect("bundled sqeel-config/src/config.toml is invalid; build-time bug")
    }
}

impl AppConfig for MainConfig {
    const APPLICATION: &'static str = "sqeel";
}

impl Validate for MainConfig {
    type Error = ValidationError;

    fn validate(&self) -> Result<(), Self::Error> {
        ensure_non_empty_str(&self.editor.lsp_binary, "editor.lsp_binary")?;
        ensure_non_zero(self.editor.mouse_scroll_lines, "editor.mouse_scroll_lines")?;
        // leader_key is a `char` — multi-char and empty leaders are
        // already rejected at parse time by serde's char deserializer
        // (TOML strings of length != 1 fail to convert to `char`). No
        // additional validation needed here.
        Ok(())
    }
}

/// Process-wide override for the config dir, set by `--sandbox` so
/// dev-mode runs don't touch the user's real `~/.config/sqeel/`.
/// `None` (the default) falls back to [`hjkl_config::config_dir`] keyed
/// off the [`AppConfig`] impl on [`MainConfig`].
static CONFIG_DIR_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Install a sandbox config dir. Idempotent — first call wins.
/// Subsequent calls are silently ignored so a misconfigured caller
/// can't surprise the user mid-run by repointing the dir.
pub fn set_config_dir_override(path: PathBuf) {
    let _ = CONFIG_DIR_OVERRIDE.set(path);
}

/// Resolve the sqeel config root. Sandbox override (set via
/// [`set_config_dir_override`] from `--sandbox`) wins; otherwise routes
/// through [`hjkl_config::config_dir`] which is XDG-everywhere:
///
/// - Linux/macOS/Windows: `$XDG_CONFIG_HOME/sqeel`
///   (default `~/.config/sqeel` on every platform)
pub fn config_dir() -> Option<PathBuf> {
    if let Some(p) = CONFIG_DIR_OVERRIDE.get() {
        return Some(p.clone());
    }
    hjkl_config::config_dir(MainConfig::APPLICATION).ok()
}

/// Load + validate `MainConfig`.
///
/// Defaults are bundled into the binary via [`DEFAULTS_TOML`]; the user
/// file at `<config_dir>/config.toml` is **deep-merged** on top
/// (only overridden fields need to appear there). Unknown keys are
/// rejected. Validation is run on the merged result and surfaces
/// out-of-range values (empty `lsp_binary`, zero `mouse_scroll_lines`).
/// Multi-char or empty `leader_key` is caught at parse time by serde's
/// `char` deserializer (TOML strings of length != 1 fail to convert).
///
/// Missing user file → bundled defaults only. **Never writes to disk** —
/// callers that want to scaffold a starter config can use
/// [`hjkl_config::write_default`] explicitly.
pub fn load_main_config() -> anyhow::Result<MainConfig> {
    let cfg = match config_dir() {
        Some(dir) => {
            let path = dir.join("config.toml");
            if path.exists() {
                hjkl_config::load_layered_from::<MainConfig>(DEFAULTS_TOML, &path)?
            } else {
                MainConfig::default()
            }
        }
        None => MainConfig::default(),
    };
    cfg.validate().map_err(|e| anyhow::anyhow!(e))?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Build-time check: the bundled defaults must parse into `MainConfig`.
    /// If this fails, `MainConfig::default()` would panic at runtime.
    #[test]
    fn defaults_parse() {
        let cfg: MainConfig =
            toml::from_str(DEFAULTS_TOML).expect("bundled config.toml must parse");
        assert_eq!(cfg.editor.keybindings, KeybindingMode::Vim);
        assert_eq!(cfg.editor.lsp_binary, "sqls");
        assert_eq!(cfg.editor.mouse_scroll_lines, 3);
        assert_eq!(cfg.editor.leader_key, ' ');
        assert!(cfg.editor.stop_on_error);
        assert_eq!(cfg.editor.schema_ttl_secs, 300);
    }

    #[test]
    fn defaults_match_default_impl() {
        let parsed: MainConfig = toml::from_str(DEFAULTS_TOML).unwrap();
        let dflt = MainConfig::default();
        assert_eq!(parsed.editor.keybindings, dflt.editor.keybindings);
        assert_eq!(parsed.editor.lsp_binary, dflt.editor.lsp_binary);
        assert_eq!(parsed.editor.leader_key, dflt.editor.leader_key);
    }

    #[test]
    fn defaults_pass_validation() {
        MainConfig::default()
            .validate()
            .expect("bundled defaults must validate");
    }

    #[test]
    fn default_config_has_vim_bindings() {
        let config = MainConfig::default();
        assert_eq!(config.editor.keybindings, KeybindingMode::Vim);
    }

    #[test]
    fn default_config_has_sqls_lsp() {
        let config = MainConfig::default();
        assert_eq!(config.editor.lsp_binary, "sqls");
    }

    /// Partial user TOML over bundled defaults: only overridden fields appear
    /// in the user file; unspecified fields keep their bundled value.
    #[test]
    fn user_partial_override_keeps_defaults() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            "[editor]\nlsp_binary = \"/opt/sqls\"\nmouse_scroll_lines = 5"
        )
        .unwrap();
        let cfg: MainConfig = hjkl_config::load_layered_from(DEFAULTS_TOML, f.path()).unwrap();
        // User overrides took effect:
        assert_eq!(cfg.editor.lsp_binary, "/opt/sqls");
        assert_eq!(cfg.editor.mouse_scroll_lines, 5);
        // Non-overridden fields retain bundled values:
        assert_eq!(cfg.editor.keybindings, KeybindingMode::Vim);
        assert_eq!(cfg.editor.leader_key, ' ');
        assert!(cfg.editor.stop_on_error);
        assert_eq!(cfg.editor.schema_ttl_secs, 300);
    }

    #[test]
    fn user_unknown_key_is_rejected() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "[editor]\nbogus = 1").unwrap();
        let err =
            hjkl_config::load_layered_from::<MainConfig>(DEFAULTS_TOML, f.path()).unwrap_err();
        assert!(matches!(err, hjkl_config::ConfigError::Invalid { .. }));
    }

    #[test]
    fn validate_rejects_zero_mouse_scroll_lines() {
        let mut cfg = MainConfig::default();
        cfg.editor.mouse_scroll_lines = 0;
        let err = cfg.validate().unwrap_err();
        assert_eq!(err.field, "editor.mouse_scroll_lines");
    }

    #[test]
    fn validate_rejects_empty_lsp_binary() {
        let mut cfg = MainConfig::default();
        cfg.editor.lsp_binary = String::new();
        let err = cfg.validate().unwrap_err();
        assert_eq!(err.field, "editor.lsp_binary");
    }

    /// Multi-char leader strings must be rejected at parse time — serde's
    /// `char` deserializer fails on TOML strings of length != 1. This
    /// pins the contract: users who write `leader_key = "ab"` get a
    /// `ConfigError::Invalid` instead of a silently truncated leader.
    #[test]
    fn parse_rejects_multi_char_leader_key() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "[editor]\nleader_key = \"ab\"").unwrap();
        let err =
            hjkl_config::load_layered_from::<MainConfig>(DEFAULTS_TOML, f.path()).unwrap_err();
        assert!(matches!(err, hjkl_config::ConfigError::Invalid { .. }));
    }

    #[test]
    fn parse_rejects_empty_leader_key() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "[editor]\nleader_key = \"\"").unwrap();
        let err =
            hjkl_config::load_layered_from::<MainConfig>(DEFAULTS_TOML, f.path()).unwrap_err();
        assert!(matches!(err, hjkl_config::ConfigError::Invalid { .. }));
    }

    #[test]
    fn parse_accepts_unicode_single_char_leader_key() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "[editor]\nleader_key = \"α\"").unwrap();
        let cfg: MainConfig = hjkl_config::load_layered_from(DEFAULTS_TOML, f.path()).unwrap();
        assert_eq!(cfg.editor.leader_key, 'α');
    }
}
