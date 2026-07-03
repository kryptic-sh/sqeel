//! Headless-mode (`-e`) integration tests.
//!
//! Plain subprocess runs against a temp SQLite database — no pty, no TUI —
//! so this suite runs on every platform (unlike the pty e2e suite) and
//! exercises the connect → execute → format → stdout path directly.

use std::process::Command;

fn sqeel() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sqeel"))
}

/// Temp SQLite URL, unique per test.
fn db_url(tag: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("{tag}.db"));
    let url = format!("sqlite://{}", path.display());
    (dir, url)
}

const SEED: &str = "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT); \
                    INSERT INTO users (email) VALUES ('alice@example.com'); \
                    INSERT INTO users (email) VALUES ('bob@example.com');";

#[test]
fn table_format_prints_header_and_rows() {
    let (_dir, url) = db_url("table");
    let out = sqeel()
        .args([
            "--url",
            &url,
            "-e",
            SEED,
            "-e",
            "SELECT id, email FROM users ORDER BY id;",
        ])
        .output()
        .expect("spawn sqeel");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("id"), "missing header:\n{stdout}");
    assert!(
        stdout.contains("alice@example.com"),
        "missing row:\n{stdout}"
    );
    assert!(stdout.contains("bob@example.com"), "missing row:\n{stdout}");
    // Non-query summaries stay off stdout (pipe purity).
    assert!(
        !stdout.contains("rows affected"),
        "summary leaked to stdout:\n{stdout}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("rows affected"),
        "summary missing from stderr:\n{stderr}"
    );
}

#[test]
fn csv_format_quotes_and_joins() {
    let (_dir, url) = db_url("csv");
    let out = sqeel()
        .args([
            "--url",
            &url,
            "-e",
            "CREATE TABLE t (a TEXT, b TEXT); INSERT INTO t VALUES ('x,y', 'plain');",
            "-e",
            "SELECT a, b FROM t;",
            "--format",
            "csv",
        ])
        .output()
        .expect("spawn sqeel");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("a,b"), "missing csv header:\n{stdout}");
    assert!(
        stdout.contains("\"x,y\",plain"),
        "comma cell not quoted:\n{stdout}"
    );
}

#[test]
fn json_format_emits_row_objects() {
    let (_dir, url) = db_url("json");
    let out = sqeel()
        .args([
            "--url",
            &url,
            "-e",
            "CREATE TABLE t (a TEXT); INSERT INTO t VALUES ('hello');",
            "-e",
            "SELECT a FROM t;",
            "--format",
            "json",
        ])
        .output()
        .expect("spawn sqeel");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("stdout not JSON");
    assert_eq!(v[0]["a"], "hello", "unexpected JSON: {v}");
}

#[test]
fn sql_error_exits_nonzero_with_stderr() {
    let (_dir, url) = db_url("err");
    let out = sqeel()
        .args(["--url", &url, "-e", "SELECT * FROM missing_table;"])
        .output()
        .expect("spawn sqeel");
    assert!(!out.status.success(), "bad SQL must exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sqeel -e:"),
        "error missing from stderr:\n{stderr}"
    );
    assert!(out.stdout.is_empty(), "stdout should stay empty on error");
}

#[test]
fn no_connection_exits_2() {
    let out = sqeel()
        .env_remove("DATABASE_URL")
        .args(["-e", "SELECT 1;"])
        .output()
        .expect("spawn sqeel");
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit 2 without a connection"
    );
}

#[test]
fn error_stops_remaining_statements() {
    let (_dir, url) = db_url("stop");
    let out = sqeel()
        .args([
            "--url",
            &url,
            "-e",
            "CREATE TABLE t (a TEXT); SELECT * FROM missing; INSERT INTO t VALUES ('never');",
            "-e",
            "SELECT count(*) AS n FROM t;",
        ])
        .output()
        .expect("spawn sqeel");
    assert!(!out.status.success());
    // The INSERT after the failing SELECT must not have run.
    let check = sqeel()
        .args([
            "--url",
            &url,
            "-e",
            "SELECT count(*) AS n FROM t;",
            "--format",
            "csv",
        ])
        .output()
        .expect("spawn sqeel");
    let stdout = String::from_utf8_lossy(&check.stdout);
    assert!(
        stdout.contains("\n0"),
        "statements after the error still ran:\n{stdout}"
    );
}

#[test]
fn completions_flag_emits_shell_script() {
    for shell in ["bash", "zsh", "fish", "nushell"] {
        let out = sqeel()
            .args(["--completions", shell])
            .output()
            .expect("spawn sqeel");
        assert!(out.status.success(), "--completions {shell} failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("sqeel") && stdout.contains("format"),
            "{shell} completions missing expected tokens:\n{stdout}"
        );
    }
}

#[test]
fn man_flag_emits_troff() {
    let out = sqeel().arg("--man").output().expect("spawn sqeel");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(".TH sqeel 1"),
        "man output missing .TH header:\n{}",
        &stdout[..stdout.len().min(200)]
    );
    assert!(
        stdout.contains("headless") || stdout.contains("execute"),
        "man output missing option docs"
    );
}
