//! Parser for PostgreSQL `.pgpass` credential files.
//!
//! Follows the libpq spec: five colon-separated fields, backslash
//! escapes for `:` and `\`, comments and blank lines skipped,
//! Unix permission check (0600 or stricter).

use std::path::{Path, PathBuf};

/// One credential record from a `.pgpass` file.
///
/// All fields are raw strings; `*` is the libpq wildcard and is
/// preserved literally so the caller decides how to handle it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgpassEntry {
    pub host: String,
    pub port: String,
    pub database: String,
    pub user: String,
    pub password: String,
}

/// Parse a single `.pgpass` line into up to five fields.
///
/// Returns `None` when the line should be skipped (comment, blank,
/// wrong field count, or an unrecoverable parse error).
fn parse_line(line: &str) -> Option<PgpassEntry> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    // Walk char-by-char handling backslash escapes.
    let mut fields: Vec<String> = Vec::with_capacity(5);
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                match chars.peek() {
                    Some(':') => {
                        chars.next();
                        current.push(':');
                    }
                    Some('\\') => {
                        chars.next();
                        current.push('\\');
                    }
                    _ => {
                        // Lone backslash — pass through unchanged (lenient).
                        current.push('\\');
                    }
                }
            }
            ':' => {
                fields.push(current.clone());
                current.clear();
            }
            other => current.push(other),
        }
    }
    fields.push(current);

    if fields.len() != 5 {
        return None;
    }

    Some(PgpassEntry {
        host: fields.remove(0),
        port: fields.remove(0),
        database: fields.remove(0),
        user: fields.remove(0),
        password: fields.remove(0),
    })
}

/// Resolve the default `.pgpass` path for the current platform.
fn default_pgpass_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        dirs::data_dir().map(|d| d.join("postgresql").join("pgpass.conf"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        dirs::home_dir().map(|h| h.join(".pgpass"))
    }
}

/// Check whether a file's Unix permissions are 0600 or stricter.
///
/// Returns `true` on non-Unix platforms (permission check not applicable).
#[cfg(unix)]
fn permissions_ok(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(m) => m.mode() & 0o177 == 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn permissions_ok(_path: &Path) -> bool {
    true
}

/// Load and parse `.pgpass` entries from `path`.
///
/// Used directly by tests (avoids env-var interaction); the public
/// [`load_pgpass`] delegates here after resolving the path.
pub fn load_pgpass_from(path: &Path) -> Vec<PgpassEntry> {
    if !path.exists() {
        return vec![];
    }
    if !permissions_ok(path) {
        tracing::warn!(
            path = %path.display(),
            "pgpass file has insecure permissions (must be 0600); skipping"
        );
        return vec![];
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    content.lines().filter_map(parse_line).collect()
}

/// Load and parse `.pgpass` entries.
///
/// Discovery order:
/// 1. `$PGPASSFILE` environment variable (when set and non-empty).
/// 2. `~/.pgpass` on Unix / `%APPDATA%\postgresql\pgpass.conf` on Windows.
///
/// Returns an empty `Vec` on missing file, bad permissions, or any I/O error.
/// Never panics.
pub fn load_pgpass() -> Vec<PgpassEntry> {
    let path = if let Ok(v) = std::env::var("PGPASSFILE") {
        if v.is_empty() {
            match default_pgpass_path() {
                Some(p) => p,
                None => return vec![],
            }
        } else {
            PathBuf::from(v)
        }
    } else {
        match default_pgpass_path() {
            Some(p) => p,
            None => return vec![],
        }
    };
    load_pgpass_from(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn write_pgpass(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        f.write_all(content.as_bytes()).expect("write");
        // Ensure 0600 permissions on unix so the permission check passes.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(f.path(), std::fs::Permissions::from_mode(0o600))
                .expect("chmod");
        }
        f
    }

    #[test]
    fn basic_line_parsed() {
        let f = write_pgpass("localhost:5432:mydb:alice:s3cret\n");
        let entries = load_pgpass_from(f.path());
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.host, "localhost");
        assert_eq!(e.port, "5432");
        assert_eq!(e.database, "mydb");
        assert_eq!(e.user, "alice");
        assert_eq!(e.password, "s3cret");
    }

    #[test]
    fn comment_skipped() {
        let f = write_pgpass("# this is a comment\nlocalhost:5432:db:user:pw\n");
        let entries = load_pgpass_from(f.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].user, "user");
    }

    #[test]
    fn blank_line_skipped() {
        let f = write_pgpass("\n\nlocalhost:5432:db:user:pw\n\n");
        let entries = load_pgpass_from(f.path());
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn wildcard_preserved_literally() {
        let f = write_pgpass("*:*:*:*:wildcardpw\n");
        let entries = load_pgpass_from(f.path());
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.host, "*");
        assert_eq!(e.port, "*");
        assert_eq!(e.database, "*");
        assert_eq!(e.user, "*");
        assert_eq!(e.password, "wildcardpw");
    }

    #[test]
    fn escaped_colon_in_password() {
        let f = write_pgpass("localhost:5432:db:user:pass\\:word\n");
        let entries = load_pgpass_from(f.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].password, "pass:word");
    }

    #[test]
    fn escaped_backslash_in_password() {
        let f = write_pgpass("localhost:5432:db:user:pass\\\\word\n");
        let entries = load_pgpass_from(f.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].password, "pass\\word");
    }

    #[test]
    fn four_field_line_rejected() {
        let f = write_pgpass("localhost:5432:db:user\n");
        let entries = load_pgpass_from(f.path());
        assert!(entries.is_empty(), "4-field line must be skipped");
    }

    #[test]
    fn six_field_line_rejected() {
        let f = write_pgpass("localhost:5432:db:user:pw:extra\n");
        let entries = load_pgpass_from(f.path());
        assert!(entries.is_empty(), "6-field line must be skipped");
    }

    #[test]
    fn missing_file_returns_empty() {
        let entries = load_pgpass_from(Path::new("/nonexistent/__pgpass_does_not_exist__"));
        assert!(entries.is_empty());
    }

    #[test]
    fn pgpassfile_env_override() {
        let f = write_pgpass("remotehost:5433:prod:admin:topsecret\n");
        // Use load_pgpass_from directly to avoid env-var races in parallel tests.
        let entries = load_pgpass_from(f.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].host, "remotehost");
        assert_eq!(entries[0].port, "5433");
        assert_eq!(entries[0].password, "topsecret");
    }

    #[test]
    fn multiple_entries_parsed_in_order() {
        let f = write_pgpass(
            "host1:5432:db1:u1:pw1\n\
             # skip me\n\
             host2:5433:db2:u2:pw2\n",
        );
        let entries = load_pgpass_from(f.path());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].host, "host1");
        assert_eq!(entries[1].host, "host2");
    }

    #[cfg(unix)]
    #[test]
    fn insecure_permissions_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let f = write_pgpass("localhost:5432:db:user:pw\n");
        std::fs::set_permissions(f.path(), std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let entries = load_pgpass_from(f.path());
        assert!(
            entries.is_empty(),
            "file with 0644 permissions must be skipped"
        );
    }
}
