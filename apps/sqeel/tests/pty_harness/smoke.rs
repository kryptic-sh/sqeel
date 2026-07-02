//! Sandbox smoke suite: launch → editor renders → query → results grid.
//!
//! Everything runs against the `--sandbox` seed (SQLite `sample` connection,
//! `sample_users.sql` buffer: CREATE TABLE users + 2 INSERTs + SELECT), so
//! the whole query round-trip exercises the real sqlx backend offline.

use super::harness::TerminalSession;

/// The seeded buffer must render on launch — catches startup regressions in
/// session restore, tab loading, and the BufferView render path.
#[test]
fn startup_renders_sample_buffer() {
    let s = TerminalSession::spawn_sandbox();
    assert!(
        s.wait_for_text("CREATE TABLE IF NOT EXISTS users", 5_000),
        "sample buffer never rendered\n{}",
        s.screen_dump()
    );
}

/// Typing in insert mode must show up on screen — the render-sync bug class
/// (engine state moves, display doesn't).
#[test]
fn insert_mode_typing_renders() {
    let mut s = TerminalSession::spawn_sandbox();
    assert!(
        s.wait_for_text("CREATE TABLE", 5_000),
        "editor never rendered\n{}",
        s.screen_dump()
    );
    s.keys("ggO-- e2e marker line<Esc>");
    assert!(
        s.wait_for_text("-- e2e marker line", 3_000),
        "typed text never rendered\n{}",
        s.screen_dump()
    );
}

/// `<leader><Tab>` (run all statements) must execute the seeded buffer against
/// the sandbox SQLite database and render the SELECT's rows in the results
/// grid — the full editor → executor → sqlx → results-pane round-trip.
#[test]
fn run_all_statements_renders_select_rows() {
    let mut s = TerminalSession::spawn_sandbox();
    assert!(
        s.wait_for_text("CREATE TABLE", 5_000),
        "editor never rendered\n{}",
        s.screen_dump()
    );
    s.keys("<Space><Tab>");
    // The SELECT lands last; its grid shows the two seeded rows.
    assert!(
        s.wait_for_text("alice@example.com", 10_000),
        "SELECT results never rendered\n{}",
        s.screen_dump()
    );
    assert!(
        s.screen_contains("bob@example.com"),
        "second row missing from results grid\n{}",
        s.screen_dump()
    );
}

/// `/` search must jump the cursor and `<leader><CR>` must run just the
/// statement under the cursor.
#[test]
fn run_statement_under_cursor_via_search() {
    let mut s = TerminalSession::spawn_sandbox();
    assert!(
        s.wait_for_text("CREATE TABLE", 5_000),
        "editor never rendered\n{}",
        s.screen_dump()
    );
    // Create the table + rows first so the SELECT has something to return.
    s.keys("<Space><Tab>");
    assert!(
        s.wait_for_text("alice@example.com", 10_000),
        "seed run never produced results\n{}",
        s.screen_dump()
    );
    // Jump to the SELECT statement and run only it.
    s.keys("gg/SELECT<Enter>");
    s.keys("<Space><Enter>");
    assert!(
        s.wait_for_text("bob@example.com", 10_000),
        "statement-under-cursor run never rendered\n{}",
        s.screen_dump()
    );
}

/// A `DELETE` without `WHERE` must hit the destructive-run guard: confirm
/// modal appears, `n` cancels (no results), re-run + `y` executes.
#[test]
fn destructive_guard_prompts_and_confirms() {
    let mut s = TerminalSession::spawn_sandbox();
    assert!(
        s.wait_for_text("CREATE TABLE", 5_000),
        "editor never rendered\n{}",
        s.screen_dump()
    );
    // Seed the table + rows first.
    s.keys("<Space><Tab>");
    assert!(
        s.wait_for_text("alice@example.com", 10_000),
        "seed run never produced results\n{}",
        s.screen_dump()
    );
    // Replace the buffer with a guarded statement and run it.
    s.keys("ggVGc"); // select-all, change → insert mode with empty buffer
    s.keys("DELETE FROM users;<Esc>");
    s.keys("<Space><CR>");
    assert!(
        s.wait_for_text("Run DELETE without WHERE?", 5_000),
        "guard modal never appeared\n{}",
        s.screen_dump()
    );
    // `n` cancels: modal gone, nothing dispatched.
    s.keys("n");
    assert!(
        !s.screen_contains("Run DELETE without WHERE?"),
        "guard modal still up after n\n{}",
        s.screen_dump()
    );
    // Run again and confirm with `y` — the DELETE goes through, and a
    // follow-up SELECT renders an empty grid (no alice row).
    s.keys("<Space><CR>");
    assert!(
        s.wait_for_text("Run DELETE without WHERE?", 5_000),
        "guard modal never re-appeared\n{}",
        s.screen_dump()
    );
    s.keys("y");
    s.keys("ggVGc");
    s.keys("SELECT * FROM users;<Esc>");
    s.keys("<Space><CR>");
    // The SELECT after the confirmed DELETE must come back empty: poll for
    // the result tab rendering, then assert no user rows.
    assert!(
        s.wait_for_text("SELECT * FROM users", 10_000),
        "post-delete SELECT never rendered\n{}",
        s.screen_dump()
    );
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        !s.screen_contains("alice@example.com"),
        "rows survived a confirmed DELETE\n{}",
        s.screen_dump()
    );
}

/// `:w <path>` must export the buffer to a filesystem path without touching
/// the tab's identity.
#[test]
fn w_path_exports_buffer_to_file() {
    let mut s = TerminalSession::spawn_sandbox();
    assert!(
        s.wait_for_text("CREATE TABLE", 5_000),
        "editor never rendered\n{}",
        s.screen_dump()
    );
    let out = std::env::temp_dir().join(format!("sqeel-e2e-export-{}.sql", std::process::id()));
    let _ = std::fs::remove_file(&out);
    s.keys(&format!(":w {}<Enter>", out.display()));
    assert!(
        s.wait_for_text("Written", 5_000),
        "export toast never appeared\n{}",
        s.screen_dump()
    );
    let written = std::fs::read_to_string(&out).expect("exported file missing");
    assert!(
        written.contains("CREATE TABLE IF NOT EXISTS users"),
        "exported content wrong: {written}"
    );
    let _ = std::fs::remove_file(&out);
}

/// Ctrl-C during a long-running query must cancel it: the "Query cancelled"
/// pane renders and the editor stays responsive. Uses an unbounded recursive
/// CTE — effectively infinite in SQLite — so without a working cancel the
/// executor would spin forever and the assertions below could never pass.
#[test]
fn ctrl_c_cancels_running_query() {
    let mut s = TerminalSession::spawn_sandbox();
    assert!(
        s.wait_for_text("CREATE TABLE", 5_000),
        "editor never rendered\n{}",
        s.screen_dump()
    );
    s.keys("ggVGc");
    s.keys("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT count(*) FROM c;<Esc>");
    s.keys("<Space><CR>");
    // The query is running (loading tab up); cancel it.
    std::thread::sleep(std::time::Duration::from_millis(300));
    s.keys("<C-c>");
    assert!(
        s.wait_for_text("Query cancelled", 5_000),
        "cancel never surfaced\n{}",
        s.screen_dump()
    );
    // The event loop must still be alive: typing renders.
    s.keys("ggO-- still alive<Esc>");
    assert!(
        s.wait_for_text("-- still alive", 3_000),
        "UI wedged after cancel\n{}",
        s.screen_dump()
    );
}

/// A run-all batch (`<leader><Tab>`) containing a destructive statement
/// must hit the guard as a whole: `n` cancels the ENTIRE batch (safe
/// statements included), `y` runs it in order.
#[test]
fn destructive_guard_covers_run_all_batch() {
    let mut s = TerminalSession::spawn_sandbox();
    assert!(
        s.wait_for_text("CREATE TABLE", 5_000),
        "editor never rendered\n{}",
        s.screen_dump()
    );
    // Seed the table + two rows.
    s.keys("<Space><Tab>");
    assert!(
        s.wait_for_text("alice@example.com", 10_000),
        "seed run never produced results\n{}",
        s.screen_dump()
    );
    // Mixed batch: a safe INSERT followed by a guarded DELETE.
    s.keys("ggVGc");
    s.keys("INSERT INTO users (email, display_name) VALUES ('carol@example.com', 'Carol'); DELETE FROM users;<Esc>");
    s.keys("<Space><Tab>");
    assert!(
        s.wait_for_text("Run DELETE without WHERE?", 5_000),
        "batch guard modal never appeared\n{}",
        s.screen_dump()
    );
    // `n` cancels the whole batch — the safe INSERT must not have run
    // either (all-or-nothing keeps statement order intact).
    s.keys("n");
    s.keys("ggVGc");
    s.keys("SELECT email FROM users ORDER BY id;<Esc>");
    s.keys("<Space><CR>");
    assert!(
        s.wait_for_text("alice@example.com", 10_000),
        "verify SELECT never rendered\n{}",
        s.screen_dump()
    );
    assert!(
        !s.screen_contains("carol@example.com"),
        "cancelled batch still ran its INSERT\n{}",
        s.screen_dump()
    );
    // Re-enter the batch and confirm with `y`: INSERT then DELETE run in
    // order, leaving the table empty.
    s.keys("ggVGc");
    s.keys("INSERT INTO users (email, display_name) VALUES ('carol@example.com', 'Carol'); DELETE FROM users;<Esc>");
    s.keys("<Space><Tab>");
    assert!(
        s.wait_for_text("Run DELETE without WHERE?", 5_000),
        "batch guard modal never re-appeared\n{}",
        s.screen_dump()
    );
    s.keys("y");
    s.keys("ggVGc");
    s.keys("SELECT count(*) AS remaining FROM users;<Esc>");
    s.keys("<Space><CR>");
    assert!(
        s.wait_for_text("remaining", 10_000),
        "post-batch SELECT never rendered\n{}",
        s.screen_dump()
    );
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        !s.screen_contains("alice@example.com"),
        "confirmed batch DELETE didn't run\n{}",
        s.screen_dump()
    );
}

/// `:set wrap` must soft-wrap long lines: text past the pane width becomes
/// visible on a continuation row instead of being clipped by `top_col`.
#[test]
fn set_wrap_renders_continuation_rows() {
    let mut s = TerminalSession::spawn_sandbox();
    assert!(
        s.wait_for_text("CREATE TABLE", 5_000),
        "editor never rendered\n{}",
        s.screen_dump()
    );
    // One long line: the tail marker sits past the ~73-cell text width.
    let filler = "x".repeat(80);
    s.keys("ggVGc");
    s.keys(&format!("-- {filler} WRAPTAIL<Esc>"));
    assert!(
        !s.screen_contains("WRAPTAIL"),
        "tail visible before wrap — line too short?\n{}",
        s.screen_dump()
    );
    s.keys(":set wrap<Enter>");
    assert!(
        s.wait_for_text("WRAPTAIL", 3_000),
        "continuation row never rendered after :set wrap\n{}",
        s.screen_dump()
    );
    // And back off again.
    s.keys(":set nowrap<Enter>");
    let gone = (0..150).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(20));
        !s.screen_contains("WRAPTAIL")
    });
    assert!(gone, ":set nowrap didn't unwrap\n{}", s.screen_dump());
}

/// `:q!` must exit the process cleanly — the graceful shutdown path (LSP
/// shutdown, session persist, terminal restore, sandbox autoclean). A hang
/// here means the event loop or an async worker is wedged on quit.
#[test]
fn quit_exits_cleanly() {
    let mut s = TerminalSession::spawn_sandbox();
    assert!(
        s.wait_for_text("CREATE TABLE", 5_000),
        "editor never rendered\n{}",
        s.screen_dump()
    );
    s.keys(":q!<Enter>");
    assert!(
        s.wait_for_exit(5_000),
        "process still running 5s after :q!\n{}",
        s.screen_dump()
    );
}
