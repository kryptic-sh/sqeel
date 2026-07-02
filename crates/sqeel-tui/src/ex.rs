//! sqeel-specific ex-command handling: `:set` cursor-opt interception,
//! `:Anvil`, `:export`, `:describe`, `:migrate-secrets`.

use super::*;

/// Parse `on/off/true/false/yes/no/1/0` as a bool. Case-insensitive.
pub(crate) fn parse_bool_value(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

pub(crate) fn apply_cursor_opts<'a>(
    cmd: &'a str,
    cursorline: &mut bool,
    cursorcolumn: &mut bool,
) -> CursorOptsResult<'a> {
    // Only handle `:set …` commands.
    let body = if let Some(rest) = cmd.strip_prefix("set").map(str::trim_start) {
        rest
    } else if let Some(rest) = cmd.strip_prefix("se").map(str::trim_start) {
        rest
    } else {
        return CursorOptsResult {
            forward: std::borrow::Cow::Borrowed(cmd),
            info: None,
        };
    };

    let mut residual: Vec<&str> = Vec::new();
    let mut consumed_any = false;
    let mut info_lines: Vec<String> = Vec::new();
    for token in body.split_whitespace() {
        // Strip a trailing `?` (query form) or `!` (toggle form) before
        // matching the name. `=value` is split on the `=` separately.
        let (name, suffix, value) = if let Some(eq) = token.find('=') {
            (&token[..eq], None, Some(&token[eq + 1..]))
        } else if let Some(rest) = token.strip_suffix('?') {
            (rest, Some('?'), None)
        } else if let Some(rest) = token.strip_suffix('!') {
            (rest, Some('!'), None)
        } else {
            (token, None, None)
        };

        // Resolve which option (if any) this name refers to.
        let target = match name {
            "cursorline" | "cul" => Some('l'),
            "nocursorline" | "nocul" => Some('L'), // no-prefix variant
            "cursorcolumn" | "cuc" => Some('c'),
            "nocursorcolumn" | "nocuc" => Some('C'),
            _ => None,
        };

        match (target, suffix, value) {
            // Bare bool: `:set cul` / `:set nocul`
            (Some('l'), None, None) => {
                *cursorline = true;
                consumed_any = true;
            }
            (Some('L'), None, None) => {
                *cursorline = false;
                consumed_any = true;
            }
            (Some('c'), None, None) => {
                *cursorcolumn = true;
                consumed_any = true;
            }
            (Some('C'), None, None) => {
                *cursorcolumn = false;
                consumed_any = true;
            }
            // Toggle: `:set cul!` / `:set cuc!`
            (Some('l' | 'L'), Some('!'), None) => {
                *cursorline = !*cursorline;
                consumed_any = true;
            }
            (Some('c' | 'C'), Some('!'), None) => {
                *cursorcolumn = !*cursorcolumn;
                consumed_any = true;
            }
            // Query: `:set cul?` / `:set cuc?`
            (Some('l' | 'L'), Some('?'), None) => {
                let label = if *cursorline {
                    "cursorline"
                } else {
                    "nocursorline"
                };
                info_lines.push(label.to_string());
                consumed_any = true;
            }
            (Some('c' | 'C'), Some('?'), None) => {
                let label = if *cursorcolumn {
                    "cursorcolumn"
                } else {
                    "nocursorcolumn"
                };
                info_lines.push(label.to_string());
                consumed_any = true;
            }
            // Value assign: `:set cul=on` / `:set cuc=off`
            (Some('l' | 'L'), None, Some(v)) => {
                if let Some(b) = parse_bool_value(v) {
                    *cursorline = b;
                    consumed_any = true;
                } else {
                    residual.push(token);
                }
            }
            (Some('c' | 'C'), None, Some(v)) => {
                if let Some(b) = parse_bool_value(v) {
                    *cursorcolumn = b;
                    consumed_any = true;
                } else {
                    residual.push(token);
                }
            }
            // Anything else (unknown name, `cul=?`, etc.) → forward to engine.
            _ => residual.push(token),
        }
    }

    let info = if info_lines.is_empty() {
        None
    } else {
        Some(info_lines.join("  "))
    };

    if !consumed_any {
        return CursorOptsResult {
            forward: std::borrow::Cow::Borrowed(cmd),
            info,
        };
    }

    let forward = if residual.is_empty() {
        // All tokens consumed; the engine needs a no-op `:set` that
        // succeeds silently.  Return `"set"` which emits an Info dump
        // (harmless; the caller suppresses it when we already have our
        // own info to surface).
        std::borrow::Cow::Owned("set".to_string())
    } else {
        std::borrow::Cow::Owned(format!("set {}", residual.join(" ")))
    };

    CursorOptsResult { forward, info }
}

/// Parsed form of an `:Anvil …` ex-command.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AnvilCmd<'a> {
    /// Bare `:Anvil` — show usage hint.
    Usage,
    /// `:Anvil install <name>`
    Install(&'a str),
    /// `:Anvil update [name]`  (None = all)
    Update(Option<&'a str>),
    /// `:Anvil uninstall <name>`
    Uninstall(&'a str),
    /// Unrecognized sub-command.
    Unknown,
}

/// Parse the body of an `:Anvil …` command (the part after "Anvil").
pub(crate) fn parse_anvil_cmd(rest: &str) -> AnvilCmd<'_> {
    let args: Vec<&str> = rest.split_whitespace().collect();
    match args.as_slice() {
        [] => AnvilCmd::Usage,
        ["install", name] => AnvilCmd::Install(name),
        ["update"] => AnvilCmd::Update(None),
        ["update", name] => AnvilCmd::Update(Some(name)),
        ["uninstall", name] => AnvilCmd::Uninstall(name),
        _ => AnvilCmd::Unknown,
    }
}

/// Execute the `:migrate-secrets` ex-command.
///
/// Walks all known connections via `load_connections`, identifies those with
/// inline passwords, and attempts to migrate each one to the OS keyring via
/// `migrate_connection_to_keyring`. Returns a list of toast messages (one per
/// connection attempted, plus a summary).
pub(crate) fn run_migrate_secrets() -> Vec<(String, ToastKind)> {
    use sqeel_core::config::{MigrationResult, migrate_connection_to_keyring};

    let conns = match sqeel_core::config::load_connections() {
        Ok(c) => c,
        Err(e) => {
            return vec![(
                format!(":migrate-secrets: failed to load connections: {e}"),
                ToastKind::Error,
            )];
        }
    };

    let mut migrated = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut msgs: Vec<(String, ToastKind)> = Vec::new();

    for conn in &conns {
        match migrate_connection_to_keyring(&conn.name) {
            Ok(MigrationResult::Migrated) => {
                migrated += 1;
                msgs.push((
                    format!("Migrated `{}` password to keyring", conn.name),
                    ToastKind::Info,
                ));
            }
            Ok(MigrationResult::NoPassword) => {
                skipped += 1;
            }
            Ok(MigrationResult::KeyringFailed(reason)) => {
                failed += 1;
                msgs.push((
                    format!(
                        "`{}` keyring write failed: {} (left as plaintext)",
                        conn.name, reason
                    ),
                    ToastKind::Error,
                ));
            }
            Err(e) => {
                failed += 1;
                msgs.push((
                    format!(":migrate-secrets error for `{}`: {e}", conn.name),
                    ToastKind::Error,
                ));
            }
        }
    }

    let summary = format!(
        ":migrate-secrets done — {migrated} migrated, {skipped} skipped (no password), {failed} failed"
    );
    msgs.push((summary, ToastKind::Info));
    msgs
}

/// Parse and execute a `:export csv|json [<path>]` ex-command.
///
/// Returns `Some((message, kind))` that the caller should push as a toast, or
/// `None` when a toast was already pushed into `toasts` directly.  In
/// practice this always returns `Some`; the `Option` makes the calling site
/// tidy.
pub(crate) fn handle_export_cmd(
    cmd: &str,
    state: &AppState,
    _toasts: &mut Vec<(String, ToastKind, std::time::Instant)>,
) -> Option<(String, ToastKind)> {
    let mut parts = cmd.split_whitespace();
    // skip the "export" token we already matched on
    let _ = parts.next();

    let format = match parts.next() {
        Some(f) => f,
        None => {
            return Some((
                "usage: :export csv|json [<path>]".to_string(),
                ToastKind::Error,
            ));
        }
    };

    let ext = match format {
        "csv" => "csv",
        "json" => "json",
        other => {
            return Some((format!("unknown export format: {other}"), ToastKind::Error));
        }
    };

    // Resolve the active QueryResult.
    let result = match state.active_result() {
        Some(tab) => match &tab.kind {
            sqeel_core::state::ResultsPane::Results(r) => r,
            _ => {
                return Some(("no query result to export".to_string(), ToastKind::Error));
            }
        },
        None => {
            return Some(("no active result tab".to_string(), ToastKind::Error));
        }
    };

    // Resolve the output path.
    let path: std::path::PathBuf = match parts.next() {
        Some(raw) => {
            // Expand a leading `~/`.
            if let Some(rest) = raw.strip_prefix("~/") {
                match dirs::home_dir() {
                    Some(home) => home.join(rest),
                    None => {
                        return Some((
                            "could not resolve home directory".to_string(),
                            ToastKind::Error,
                        ));
                    }
                }
            } else {
                std::path::PathBuf::from(raw)
            }
        }
        None => {
            // Default: ~/.local/share/sqeel/results/<conn_name>/<timestamp>.<ext>
            let conn_name = sqeel_core::persistence::sanitize_conn_slug(
                state.active_connection.as_deref().unwrap_or("scratch"),
            );
            let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
            let home = match dirs::home_dir() {
                Some(h) => h,
                None => {
                    return Some((
                        "could not resolve home directory".to_string(),
                        ToastKind::Error,
                    ));
                }
            };
            home.join(".local/share/sqeel/results")
                .join(&conn_name)
                .join(format!("{ts}.{ext}"))
        }
    };

    // Ensure parent directory exists.
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return Some((format!("mkdir failed: {e}"), ToastKind::Error));
    }

    // Serialize the result.
    let content = match ext {
        "csv" => sqeel_core::persistence::export_csv(result),
        "json" => match sqeel_core::persistence::export_json(result) {
            Ok(s) => s,
            Err(e) => return Some((format!("JSON serialization failed: {e}"), ToastKind::Error)),
        },
        _ => unreachable!(),
    };

    // Write to disk.
    if let Err(e) = std::fs::write(&path, &content) {
        return Some((format!("write failed: {e}"), ToastKind::Error));
    }

    let row_count = result.rows.len();
    Some((
        format!("Exported {row_count} rows to {}", path.display()),
        ToastKind::Info,
    ))
}

// ── :describe / :desc ex-command ────────────────────────────────────────────

/// Handle `:describe <table>` / `:desc <table>`.
///
/// Returns `(Option<toast>, bool sent)`.  When `sent` is `true` the query was
/// dispatched and the results pane is the feedback — no success toast is
/// emitted (mirrors the pattern used by run-query).  The caller drops the
/// state lock before calling `send_query` if needed; here we take `tab_idx`
/// separately so the lock can be held for the read-only parts then released.
pub(crate) fn handle_describe_cmd(
    cmd: &str,
    state: &AppState,
    tab_idx: usize,
) -> (Option<(String, ToastKind)>, bool) {
    let mut parts = cmd.split_whitespace();
    // skip the "describe" / "desc" token
    let _ = parts.next();

    let table = match parts.next() {
        Some(t) => t,
        None => {
            return (
                Some(("usage: :describe <table>".to_string(), ToastKind::Error)),
                false,
            );
        }
    };

    // Reject embedded single-quotes — cheap SQLi guard.
    if table.contains('\'') {
        return (
            Some((
                "table name must not contain single quotes".to_string(),
                ToastKind::Error,
            )),
            false,
        );
    }

    let sql = match state.active_dialect {
        Dialect::MySql => format!("DESCRIBE `{table}`"),
        Dialect::Postgres => format!(
            "SELECT column_name, data_type, is_nullable, column_default, \
             character_maximum_length \
             FROM information_schema.columns \
             WHERE table_name = '{table}' \
             ORDER BY ordinal_position"
        ),
        Dialect::Sqlite => format!("PRAGMA table_info({table})"),
        Dialect::Generic => {
            return (
                Some((
                    "No dialect selected — connect to a DB first.".to_string(),
                    ToastKind::Error,
                )),
                false,
            );
        }
    };

    let sent = state.send_query(sql, tab_idx);
    if sent {
        (None, true)
    } else {
        (
            Some((
                "No DB connected. Use <leader>c to switch connections.".to_string(),
                ToastKind::Error,
            )),
            false,
        )
    }
}
