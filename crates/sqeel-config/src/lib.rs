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

pub mod pgpass;
pub use pgpass::{PgpassEntry, load_pgpass, load_pgpass_from};

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
    /// Highlight the row where the cursor sits (vim's `cursorline`). Can be
    /// toggled at runtime via `:set cursorline` / `:set nocursorline`.
    /// Default `false`.
    pub cursorline: bool,
    /// Highlight the column where the cursor sits (vim's `cursorcolumn`). Can
    /// be toggled at runtime via `:set cursorcolumn` / `:set nocursorcolumn`.
    /// Default `false`.
    pub cursorcolumn: bool,
    /// Automatically prompt the user to install `sqls` via `hjkl-anvil` when
    /// it is not found on `$PATH`. When `true` (default), a toast-with-instruction
    /// appears on launch suggesting `:Anvil install sqls`. When `false`, only a
    /// status-line banner is shown; no prompt, no network call.
    #[serde(default = "default_lsp_auto_install")]
    pub lsp_auto_install: bool,
    /// Ask for a y/N confirm before running a destructive statement:
    /// `UPDATE` / `DELETE` with no top-level `WHERE`, `DROP`, `TRUNCATE`.
    /// Default `true`. Set `false` to dispatch everything unprompted.
    #[serde(default = "default_confirm_destructive")]
    pub confirm_destructive: bool,
    /// Auto-append ` LIMIT <n>` to bare SELECT / WITH statements that
    /// don't limit themselves (LIMIT / FETCH / TOP). `0` disables.
    /// Default `100`.
    #[serde(default = "default_row_limit")]
    pub default_row_limit: usize,
}

fn default_row_limit() -> usize {
    100
}

fn default_lsp_auto_install() -> bool {
    true
}

fn default_confirm_destructive() -> bool {
    true
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

/// TLS certificate verification mode for a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsVerifyMode {
    /// Full hostname + certificate-chain verification (the default).
    #[default]
    Full,
    /// Accept any certificate — suitable for self-signed / dev endpoints.
    Skip,
}

/// Optional TLS settings for a connection.
///
/// All fields are optional; an absent `[tls]` block means "use the driver's
/// default TLS behaviour" (typically full verification with the system CA
/// bundle).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct TlsConfig {
    /// Path to a PEM-encoded CA root certificate to trust.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<PathBuf>,
    /// Path to a PEM-encoded client certificate for mutual TLS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert: Option<PathBuf>,
    /// Path to the PEM-encoded private key matching `client_cert`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<PathBuf>,
    /// Certificate verification mode. `None` lets the driver choose (usually
    /// equivalent to [`TlsVerifyMode::Full`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_mode: Option<TlsVerifyMode>,
}

/// Per-connection TOML record stored under `<config_dir>/conns/<name>.toml`.
///
/// The `name` field is derived from the filename at load time and is never
/// written to the TOML file (hence `skip_serializing`).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ConnectionConfig {
    pub url: String,
    /// Derived from filename at load time; not present in the .toml file itself.
    #[serde(default, skip_serializing)]
    pub name: String,
    /// Optional TLS configuration block. Absent → driver defaults apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,
}

/// Result of attempting to migrate a connection's password to the OS keyring.
#[derive(Debug, PartialEq, Eq)]
pub enum MigrationResult {
    /// Password was successfully moved to the keyring; TOML now has no password.
    Migrated,
    /// The connection URL had no inline password — nothing to migrate.
    NoPassword,
    /// Keyring write failed; URL left unchanged. Contains the error message.
    KeyringFailed(String),
}

/// Keyring service name for all sqeel credentials.
const KEYRING_SERVICE: &str = "sqeel";

/// Build a keyring `Entry` for the given connection `name`.
fn keyring_entry(name: &str) -> anyhow::Result<keyring_core::Entry> {
    keyring_core::Entry::new(KEYRING_SERVICE, name)
        .map_err(|e| anyhow::anyhow!("keyring entry creation failed: {e}"))
}

/// Strip the password out of a URL, returning `(url_without_password, password)`.
///
/// Returns `(original_url, None)` when the URL has no inline password or cannot
/// be parsed by the `url` crate. The returned URL-without-password preserves all
/// other components (host, path, query).
fn split_url_password(raw: &str) -> (String, Option<String>) {
    let Ok(mut parsed) = url::Url::parse(raw) else {
        return (raw.to_string(), None);
    };
    let password = parsed
        .password()
        .filter(|p| !p.is_empty())
        .map(String::from);
    if password.is_some() {
        // Silently ignore errors — if we cannot clear the password field
        // (e.g. cannot-be-a-base URLs) we fall back to the original URL.
        let _ = parsed.set_password(None);
    }
    (parsed.to_string(), password)
}

/// Splice `password` back into `url` at the userinfo position.
///
/// Returns `url` unmodified when the URL cannot be parsed or has no username
/// (we can't meaningfully place a password without a user component).
fn splice_password_into_url(url: &str, password: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    // Only splice if there is already a username; otherwise the URL doesn't
    // have a natural home for the password segment.
    if parsed.username().is_empty() {
        return url.to_string();
    }
    let _ = parsed.set_password(Some(password));
    parsed.to_string()
}

/// Load all connections from `<config_dir>/conns/*.toml`.
///
/// Returns an empty `Vec` if the directory does not exist yet. Each file's
/// stem (e.g. `prod` from `prod.toml`) becomes `ConnectionConfig::name`.
///
/// For each connection whose URL has no inline password, the keyring is
/// queried; if an entry exists the password is spliced back into the URL
/// before returning. Failures (no keyring daemon, no entry) leave the URL
/// as-is.
pub fn load_connections() -> anyhow::Result<Vec<ConnectionConfig>> {
    let conns_dir = config_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine config dir"))?
        .join("conns");

    if !conns_dir.exists() {
        return Ok(vec![]);
    }

    let mut conns = Vec::new();
    for entry in std::fs::read_dir(&conns_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();
            let content = std::fs::read_to_string(&path)?;
            let mut conn: ConnectionConfig = toml::from_str(&content)?;
            conn.name = name.clone();

            // If the stored URL has no inline password, check the keyring.
            let (_, inline_pw) = split_url_password(&conn.url);
            if inline_pw.is_none()
                && let Ok(kr) = keyring_entry(&name)
                && let Ok(pw) = kr.get_password()
            {
                conn.url = splice_password_into_url(&conn.url, &pw);
            }

            conns.push(conn);
        }
    }
    Ok(conns)
}

/// Save a connection to `<config_dir>/conns/<name>.toml`.
///
/// `name` must contain only alphanumeric characters, `-`, or `_`.
/// The directory is created if it does not exist.
///
/// When `password` is `Some(non-empty)`:
/// 1. The password segment is stripped from `url` before writing to TOML.
/// 2. The password is stored in the OS keyring under `("sqeel", name)`.
/// 3. If the keyring write fails (e.g. no dbus on Linux), a warning is
///    logged and the URL with the inline password is written to TOML as a
///    fallback (graceful degradation per issue #26).
///
/// When `password` is `None` or empty the URL is written as-is (existing
/// behaviour preserved).
///
/// When `tls` is `Some`, the TLS configuration is serialised into the
/// `[tls]` block of the TOML file. `None` means no TLS override — the
/// driver's default behaviour applies (usually full verification with the
/// system CA bundle).
pub fn save_connection(
    name: &str,
    url: &str,
    password: Option<&str>,
    tls: Option<&TlsConfig>,
) -> anyhow::Result<()> {
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!("Connection name may only contain letters, digits, - and _");
    }

    let (url_no_pw, existing_inline_pw) = split_url_password(url);

    // Decide which password to persist to keyring (explicit arg wins, then
    // any password that was already inline in the URL).
    let pw_to_store: Option<&str> = match password {
        Some(p) if !p.is_empty() => Some(p),
        _ => existing_inline_pw.as_deref(),
    };

    // Determine the URL we actually write to disk: always strip the password.
    let url_to_write = &url_no_pw;

    // Attempt keyring write when we have a password.
    let keyring_ok = if let Some(pw) = pw_to_store {
        match keyring_entry(name).and_then(|e| {
            e.set_password(pw)
                .map_err(|e| anyhow::anyhow!("keyring set_password failed: {e}"))
        }) {
            Ok(()) => true,
            Err(e) => {
                tracing_warn_or_eprintln(&format!(
                    "sqeel: keyring unavailable for '{name}': {e}; falling back to plaintext"
                ));
                false
            }
        }
    } else {
        // No password → nothing to store in keyring; always write URL as-is.
        true
    };

    // If keyring failed and we have a password, fall back to plaintext URL.
    let final_url = if !keyring_ok {
        url.to_string()
    } else {
        url_to_write.to_string()
    };

    let conns_dir = config_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine config dir"))?
        .join("conns");
    std::fs::create_dir_all(&conns_dir)?;
    let conn = ConnectionConfig {
        url: final_url,
        name: String::new(),
        tls: tls.cloned(),
    };
    let content = toml::to_string(&conn)?;
    std::fs::write(conns_dir.join(format!("{name}.toml")), content)?;
    Ok(())
}

/// Attempt to migrate an existing connection's inline password to the OS keyring.
///
/// Reads the current TOML for `name`, checks whether the URL has an inline
/// password, and if so:
/// 1. Writes the password to the keyring.
/// 2. Overwrites the TOML file with the password-stripped URL.
///
/// Returns [`MigrationResult`] to indicate what happened. The connection file
/// is never modified unless the keyring write succeeds.
pub fn migrate_connection_to_keyring(name: &str) -> anyhow::Result<MigrationResult> {
    let path = config_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine config dir"))?
        .join("conns")
        .join(format!("{name}.toml"));
    let content = std::fs::read_to_string(&path)?;
    let conn: ConnectionConfig = toml::from_str(&content)?;
    let (url_no_pw, pw) = split_url_password(&conn.url);
    let Some(pw) = pw else {
        return Ok(MigrationResult::NoPassword);
    };
    let entry = match keyring_entry(name) {
        Ok(e) => e,
        Err(e) => return Ok(MigrationResult::KeyringFailed(e.to_string())),
    };
    if let Err(e) = entry.set_password(&pw) {
        return Ok(MigrationResult::KeyringFailed(e.to_string()));
    }
    // Keyring write succeeded — overwrite TOML with password-stripped URL,
    // preserving any existing TLS configuration.
    let updated = ConnectionConfig {
        url: url_no_pw,
        name: String::new(),
        tls: conn.tls,
    };
    let updated_content = toml::to_string(&updated)?;
    std::fs::write(&path, updated_content)?;
    Ok(MigrationResult::Migrated)
}

/// Remove the keyring entry for `name`, ignoring "no entry" errors.
pub fn delete_keyring_entry(name: &str) {
    if let Ok(entry) = keyring_entry(name) {
        match entry.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => {}
            Err(e) => {
                tracing_warn_or_eprintln(&format!(
                    "sqeel: failed to delete keyring entry for '{name}': {e}"
                ));
            }
        }
    }
}

/// Remove `<config_dir>/conns/<name>.toml` if it exists.
/// Also cleans up any keyring entry for `name`.
pub fn delete_connection(name: &str) -> anyhow::Result<()> {
    let path = config_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine config dir"))?
        .join("conns")
        .join(format!("{name}.toml"));
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    delete_keyring_entry(name);
    Ok(())
}

/// Emit a tracing warning when the `tracing` crate is wired up; otherwise
/// fall back to `eprintln!` so the message isn't silently swallowed in
/// non-TUI contexts (tests, CLI tools).
fn tracing_warn_or_eprintln(msg: &str) {
    // We don't have a hard dep on `tracing` in this crate, so use eprintln.
    // sqeel-tui wires up the tracing subscriber; any log emitted here
    // propagates through the normal mechanism when the subscriber exists.
    eprintln!("WARN {msg}");
}

/// Install the `keyring-core` mock store as the process-wide default.
///
/// Call this **once** at the top of any test module that exercises keyring
/// paths. The mock store is in-process and leaves the user's real OS keyring
/// untouched. Uses a `Once` guard so repeated calls across parallel test
/// threads are safe.
#[cfg(test)]
pub fn install_mock_keyring() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let store = keyring_core::mock::Store::new().expect("mock keyring store creation failed");
        keyring_core::set_default_store(store);
    });
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
        assert!(!cfg.editor.cursorline);
        assert!(!cfg.editor.cursorcolumn);
        assert!(cfg.editor.lsp_auto_install);
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

    #[test]
    fn connection_config_parse() {
        let conn: ConnectionConfig = toml::from_str(
            r#"
url = "mysql://user:pass@localhost/mydb"
name = "local"
"#,
        )
        .unwrap();
        assert_eq!(conn.url, "mysql://user:pass@localhost/mydb");
    }

    /// Test save/load/delete round-trip for connection files.
    ///
    /// All filesystem-touching connection tests run here in sequence so they
    /// share a single `set_config_dir_override` call — the underlying
    /// `OnceLock` is first-call-wins and cannot be reset between parallel tests.
    ///
    /// Keyring tests are included here because they also need `config_dir`
    /// to point at the same shared tempdir.
    #[test]
    fn connection_filesystem_roundtrip() {
        install_mock_keyring();
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap();

        let dir = tempfile::tempdir().unwrap();
        set_config_dir_override(dir.path().to_path_buf());

        // --- basic save + load roundtrip (no password) ---
        save_connection("test_db", "postgres://localhost/test", None, None).unwrap();
        let conns = load_connections().unwrap();
        assert_eq!(conns.len(), 1);
        assert_eq!(conns[0].name, "test_db");
        assert_eq!(conns[0].url, "postgres://localhost/test");

        // --- delete removes the file ---
        save_connection("to_delete", "sqlite::memory:", None, None).unwrap();
        assert_eq!(load_connections().unwrap().len(), 2);
        delete_connection("to_delete").unwrap();
        assert_eq!(load_connections().unwrap().len(), 1);

        // --- save with password strips URL and stores in keyring ---
        save_connection(
            "kr_test",
            "postgres://alice@dbhost/mydb",
            Some("s3cret"),
            None,
        )
        .unwrap();
        // TOML must not contain the password.
        let toml_content =
            std::fs::read_to_string(dir.path().join("conns").join("kr_test.toml")).unwrap();
        assert!(
            !toml_content.contains("s3cret"),
            "TOML should not contain plaintext password; got: {toml_content}"
        );
        // load splices the keyring password back into the URL.
        let conns = load_connections().unwrap();
        let conn = conns.iter().find(|c| c.name == "kr_test").unwrap();
        assert!(
            conn.url.contains("s3cret"),
            "loaded URL should contain spliced password; got: {}",
            conn.url
        );

        // --- save without explicit password, URL has no password: unchanged ---
        save_connection("no_pw", "sqlite::memory:", None, None).unwrap();
        let conns = load_connections().unwrap();
        let conn = conns.iter().find(|c| c.name == "no_pw").unwrap();
        assert_eq!(conn.url, "sqlite::memory:");

        // --- load splices keyring password for manually-created file ---
        let conns_dir = dir.path().join("conns");
        std::fs::write(
            conns_dir.join("splice_test.toml"),
            "url = \"mysql://user@dbhost/db\"\n",
        )
        .unwrap();
        let entry = keyring_core::Entry::new("sqeel", "splice_test").unwrap();
        entry.set_password("secretpw").unwrap();
        let conns = load_connections().unwrap();
        let conn = conns.iter().find(|c| c.name == "splice_test").unwrap();
        assert!(
            conn.url.contains("secretpw"),
            "URL should have spliced password; got: {}",
            conn.url
        );

        // --- migrate_connection_to_keyring moves inline password ---
        std::fs::write(
            conns_dir.join("migrate_me.toml"),
            "url = \"postgres://bob:hunter2@dbhost/db\"\n",
        )
        .unwrap();
        let result = migrate_connection_to_keyring("migrate_me").unwrap();
        assert_eq!(result, MigrationResult::Migrated);
        let toml_content = std::fs::read_to_string(conns_dir.join("migrate_me.toml")).unwrap();
        assert!(
            !toml_content.contains("hunter2"),
            "TOML should not contain password after migration; got: {toml_content}"
        );
        let conns = load_connections().unwrap();
        let conn = conns.iter().find(|c| c.name == "migrate_me").unwrap();
        assert!(
            conn.url.contains("hunter2"),
            "loaded URL should have spliced password; got: {}",
            conn.url
        );

        // --- migrate returns NoPassword when no inline password ---
        std::fs::write(
            conns_dir.join("no_pw_conn.toml"),
            "url = \"postgres://bob@dbhost/db\"\n",
        )
        .unwrap();
        let result = migrate_connection_to_keyring("no_pw_conn").unwrap();
        assert_eq!(result, MigrationResult::NoPassword);

        // --- delete_connection cleans keyring entry ---
        save_connection("del_kr", "mysql://user@host/db", Some("topsecret"), None).unwrap();
        let entry = keyring_core::Entry::new("sqeel", "del_kr").unwrap();
        assert_eq!(entry.get_password().unwrap(), "topsecret");
        delete_connection("del_kr").unwrap();
        assert!(
            matches!(entry.get_password(), Err(keyring_core::Error::NoEntry)),
            "keyring entry should be deleted after delete_connection"
        );
    }

    #[test]
    fn save_connection_rejects_invalid_name() {
        install_mock_keyring();
        // Validation fires before any FS access, so no config dir override needed.
        let err = save_connection("bad name!", "postgres://localhost/x", None, None).unwrap_err();
        assert!(err.to_string().contains("letters, digits"));
    }

    /// cursorline defaults to false.
    #[test]
    fn default_cursorline_is_false() {
        let cfg = MainConfig::default();
        assert!(!cfg.editor.cursorline);
    }

    /// cursorcolumn defaults to false.
    #[test]
    fn default_cursorcolumn_is_false() {
        let cfg = MainConfig::default();
        assert!(!cfg.editor.cursorcolumn);
    }

    /// lsp_auto_install defaults to true.
    #[test]
    fn default_lsp_auto_install_is_true() {
        let cfg = MainConfig::default();
        assert!(cfg.editor.lsp_auto_install);
    }

    /// lsp_auto_install can be disabled via user TOML.
    #[test]
    fn lsp_auto_install_user_override_false() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, b"[editor]\nlsp_auto_install = false\n").unwrap();
        let cfg: MainConfig = hjkl_config::load_layered_from(DEFAULTS_TOML, f.path()).unwrap();
        assert!(!cfg.editor.lsp_auto_install);
    }

    /// cursorline / cursorcolumn can be enabled via user TOML.
    #[test]
    fn cursorline_cursorcolumn_user_override() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(
            &mut f,
            b"[editor]\ncursorline = true\ncursorcolumn = true\n",
        )
        .unwrap();
        let cfg: MainConfig = hjkl_config::load_layered_from(DEFAULTS_TOML, f.path()).unwrap();
        assert!(cfg.editor.cursorline);
        assert!(cfg.editor.cursorcolumn);
    }

    /// All four TLS fields survive a serialize → deserialize round-trip.
    #[test]
    fn tls_config_round_trip() {
        let conn = ConnectionConfig {
            url: "postgres://localhost/mydb".to_string(),
            name: String::new(),
            tls: Some(TlsConfig {
                ca_cert: Some(PathBuf::from("/etc/ssl/ca.pem")),
                client_cert: Some(PathBuf::from("/etc/ssl/client.crt")),
                client_key: Some(PathBuf::from("/etc/ssl/client.key")),
                verify_mode: Some(TlsVerifyMode::Full),
            }),
        };
        let toml_str = toml::to_string(&conn).unwrap();
        let back: ConnectionConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(conn.url, back.url);
        assert_eq!(conn.tls, back.tls);
    }

    /// When `tls` is `None` the serialized TOML must not contain a `[tls]` table.
    #[test]
    fn tls_config_omitted_when_absent() {
        let conn = ConnectionConfig {
            url: "postgres://localhost/mydb".to_string(),
            name: String::new(),
            tls: None,
        };
        let toml_str = toml::to_string(&conn).unwrap();
        assert!(
            !toml_str.contains("[tls]"),
            "TOML should have no [tls] section; got:\n{toml_str}"
        );
    }

    /// `TlsVerifyMode` serializes to the expected lowercase strings.
    #[test]
    fn tls_verify_mode_serde() {
        #[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
        struct Wrapper {
            mode: TlsVerifyMode,
        }
        let full_str = toml::to_string(&Wrapper {
            mode: TlsVerifyMode::Full,
        })
        .unwrap();
        assert!(
            full_str.contains("\"full\""),
            "expected 'full', got: {full_str}"
        );

        let skip_str = toml::to_string(&Wrapper {
            mode: TlsVerifyMode::Skip,
        })
        .unwrap();
        assert!(
            skip_str.contains("\"skip\""),
            "expected 'skip', got: {skip_str}"
        );

        // Round-trip both variants.
        let back_full: Wrapper = toml::from_str(&full_str).unwrap();
        assert_eq!(back_full.mode, TlsVerifyMode::Full);
        let back_skip: Wrapper = toml::from_str(&skip_str).unwrap();
        assert_eq!(back_skip.mode, TlsVerifyMode::Skip);
    }

    /// A `[tls]` block with only `ca_cert` set must round-trip cleanly;
    /// the other three fields stay `None`.
    #[test]
    fn tls_partial_config() {
        let toml_str = r#"
url = "postgres://localhost/mydb"

[tls]
ca_cert = "/etc/ssl/ca.pem"
"#;
        let conn: ConnectionConfig = toml::from_str(toml_str).unwrap();
        let tls = conn.tls.as_ref().expect("tls block must be present");
        assert_eq!(tls.ca_cert, Some(PathBuf::from("/etc/ssl/ca.pem")));
        assert!(tls.client_cert.is_none());
        assert!(tls.client_key.is_none());
        assert!(tls.verify_mode.is_none());

        // Re-serialize and parse again — stable round-trip.
        let back: ConnectionConfig = toml::from_str(&toml::to_string(&conn).unwrap()).unwrap();
        assert_eq!(conn.tls, back.tls);
    }
}
