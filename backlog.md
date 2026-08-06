# Backlog

Open work items, findings and deferred decisions. Finished items are deleted
(see `git log` for the record).

## Review 2026-08-06

Correctness review of the whole codebase (4 crates + app + tests). Every finding
below was re-traced against the real code after the initial pass. Ranked
most-severe first.

### Findings

#### 1. Any multi-byte input panics the app — cursor column is a char index, sliced as bytes

`crates/sqeel-tui/src/lib.rs:1123` and `crates/sqeel-tui/src/syntax.rs:438` do
`&line[..col.min(line.len())]` where `col` comes from `editor.cursor()`, which
hjkl-buffer 0.41 clamps to a **char** index (`set_cursor` clamps to
`rope_line_char_count`). A `String` slice needs a **byte** boundary, so the
completion block (runs on every published edit, any mode) panics the main thread
on the first non-ASCII character typed.

Repro: open a session, Insert mode, type `é`. Expect: completion prefix
computed, app keeps running. Actual:
`byte index 1 is not a char boundary; it is inside 'é' (bytes 0..2)` — process
aborts, terminal left in alternate screen.

Same pattern in `word_prefix_at` (syntax.rs:438, runs on the same edit
pipeline). Fixes should byte-index via `line[..byte_offset]` using the cursor's
byte position (the engine exposes one) or clamp to the char-boundary floor.

#### 2. `evict_cold_tabs` drops the in-memory content of DIRTY tabs — unsaved edits silently lost

`crates/sqeel-core/src/state.rs:3827-3839` clears `tab.content = None` for any
non-active tab untouched for 5 minutes; `tab.dirty` is never consulted. Called
every TUI tick (lib.rs:735). `switch_to_tab` (state.rs:3658-3667) cold-reloads
from disk when content is `None`, and `prepare_save_all_dirty`
(state.rs:3944-3945) skips dirty tabs whose content is `None` — so no exit path
writes the lost edits.

Repro: edit tab A (unsaved), switch to tab B, leave B active > 5 min, switch
back to A. Expect: A shows the unsaved edits. Actual: A cold-loads from disk
(scratch files are only written by explicit save); the edits are gone, `dirty`
still true, quit-time save skips A.

Fix: never evict a dirty tab (or flush it to disk before eviction).

#### 3. Auto-LIMIT rewrite + destructive guard misfire on WITH-prefixed DML

- `apply_default_limit` (`crates/sqeel-core/src/db.rs`, the
  `first_kw != "SELECT" && first_kw != "WITH"` check) treats every `WITH`
  statement as row-producing, and `non_query_verb` sends `WITH` down the
  `fetch_all` path — so `execute_with_limit` appends ` LIMIT 100` to a
  `WITH … DELETE/UPDATE/INSERT`.
- `destructive_kind` (`crates/sqeel-core/src/safety.rs:43-44`) classifies by the
  first top-level word only, so `WITH cte AS (…) DELETE FROM users` yields
  `None` — no destructive confirmation.

Repro (Postgres): run `WITH x AS (SELECT 1) DELETE FROM users WHERE id = 1`.
Expect: runs, "1 row affected". Actual: executor sends
`… DELETE FROM users WHERE id = 1 LIMIT 100` → PG syntax error; the valid
statement never runs.

Repro (MySQL 8+, where `WITH … DELETE … LIMIT` is valid): run
`WITH x AS (SELECT 1) DELETE FROM users` with the confirm guard on. Expect:
destructive-confirm modal (no top-level WHERE). Actual: no confirm; the rewrite
silently caps the write at 100 rows — an unconfirmed, partial delete.

Fix: the rewrite and the guard must look past a leading `WITH … SELECT`/CTE
block to the statement's actual verb; only a top-level `WITH … SELECT` should
get the auto-LIMIT.

#### 4. Postgres schema browser binds the DATABASE name as the SCHEMA in every catalog query — sidebar always empty

Tree nodes come from `pg_database` (`db.rs:509-517`), but the per-db fetches
bind that name as a schema: `list_tables` `WHERE schemaname = $1` (db.rs:546),
`list_columns` `WHERE c.table_schema = $1` (db.rs:630), `list_indexes`
`WHERE n.nspname = $1` (db.rs:736), `list_foreign_keys` (db.rs:873). The loader
passes the database name straight through (`apps/sqeel/src/bin/sqeel.rs:1302`,
`SchemaLoadRequest::Tables { db }`).

Repro: connect to PG database `app` whose tables live in schema `public`; expand
the `app` node in the sidebar. Expect: the table list. Actual:
`SELECT tablename FROM pg_tables WHERE schemaname = 'app'` → zero rows → "(no
tables)". Works only when a schema happens to be named exactly like the
database. Needs a schema dimension (list schemas via `pg_namespace`/search_path
and query per schema) or at least fall back to the session's `search_path`.

#### 5. `is_show_create` byte-slices a UTF-8 string without a char-boundary check — render-path panic

`crates/sqeel-core/src/highlight.rs:1222` —
`trimmed.len() >= 11 && trimmed[..11].eq_ignore_ascii_case("show create")`.
`[..11]` panics when byte 11 falls inside a multi-byte char. Reached from the
results render path (`render.rs:1577`) and `AppState::active_ddl_text`
(state.rs:781) with the user's executed query text.

Repro: run a query whose first 11 bytes end mid-char, e.g. `éééééé` (6 × 2
bytes) or any 11+ bytes of leading non-ASCII text. Expect: renders the error
result normally. Actual: `byte index 11 is not a char boundary` panic in the TUI
main loop.

Fix: compare on a char boundary (`trimmed[..11.min(…) ]` with
`floor_char_boundary`, or `starts_with` after a char-slice).

#### 6. Hover popup width `clamp` panics on narrow terminals

`crates/sqeel-tui/src/render.rs:2257` —
`(natural_w as u16).saturating_add(4).clamp(40, area.width.saturating_sub(4).min(100))`
(and the text-hover twin at render.rs:2500 with min 30). `u16::clamp` panics
when min > max. The guards reject only `area.width < 20` (table) / `< 10`
(text), so widths 20–43 (table) / 10–33 (text) reach the clamp inverted.

Repro: press `K` on an identifier, then resize the terminal to ≤ 43 columns (or
open the hover while 40 wide). Expect: popup sized to fit. Actual: `clamp` panic
(`min > max. min = 40, max = 36`), process aborts.

Fix: `max(clamp_min)` on the upper bound, or clamp to `(min..=max.max(min))`.

#### 7. Keyring password round-trip double-percent-encodes already-encoded passwords

`url::Url::password()` returns the raw percent-**encoded** slice (url 2.5.8),
and `set_password` percent-encodes its input again. `save_connection`
(`crates/sqeel-config/src/lib.rs`) stores the encoded form in the keyring when
the password came from the URL (`pw_to_store = existing_inline_pw`), and
`load_connections` splices it back with a second encode; sqlx then decodes once,
yielding a literal `%XX` string. Same corruption via
`migrate_connection_to_keyring`.

Repro: save connection with URL `postgres://alice:p%40ss@dbhost/db` (any
password needing URL-encoding: `@`, `:`, `%`, non-ASCII) and no Password field.
Expect: connects with `p@ss`. Actual: keyring stores `p%40ss`, reload splices
`p%2540ss`, sqlx decodes to the literal string `p%40ss` → auth failure.

Fix: percent-decode before storing in the keyring, or store the raw decoded
password (the keyring is the safe place for it).

#### 8. Edit-connection rename deletes the original BEFORE validating/saving the new name

`crates/sqeel-core/src/state.rs:3492` — `delete_connection(original)?` runs
before `save_connection(&name, …)` (3494), which is where the name charset is
validated (sqeel-config lib.rs) and where disk/keyring writes happen.

Repro: edit connection `alpha`, rename it to `bad name!`, save. Expect:
validation error with `alpha` intact. Actual: `alpha`'s file + keyring entry
deleted, then `save_connection` bails — `alpha` is gone from disk and only
lingers in the in-memory list until restart. Any disk-write failure between the
two calls loses the original the same way.

Fix: validate the new name first, and write the new file before deleting the old
one (or delete only after the save succeeds).

#### 9. LSP `LabelOffsets` sliced without a char-boundary check — panics the detached LSP bridge thread

`crates/sqeel-core/src/lsp.rs:401-409` — `ParameterLabel::LabelOffsets` is
bounds-checked (`s <= label.len() && e <= label.len() && s <= e`) but never
boundary-checked before `&label[s..e]`. The comment claiming "byte-safe via char
boundary checks" is wrong — they are bounds checks. LSP expresses these offsets
in UTF-16 code units; a spec-conforming server with a multi-byte character
before the active parameter produces offsets that are not UTF-8 byte boundaries.
The panic lands in `bridge_loop` (lsp.rs:174, detached thread), so every LSP
feature stops silently for the rest of the session.

Repro: any signatureHelp `LabelOffsets` pair splitting a multi-byte char (e.g.
label `"fn(α int, b text)"` with UTF-16 offsets). Expect: label text wrapped in
brackets. Actual: `byte index … is not a char boundary` panic; LSP bridge thread
dies, no error surfaces.

Fix: clamp to char boundaries (or use the `Simple` label when offsets are
suspect). Also consider `catch_unwind` around `translate_event` so one bad
message can't kill the whole bridge.

#### 10. LSP positions sent as character indices, not UTF-16/UTF-8 units (LOW)

The TUI passes the engine's char-indexed column as `Position.character`
(`lib.rs:1200-1216`, `4046-4056`); LSP expects UTF-16 code units (or the
negotiated encoding — never a plain char count). Coincides with UTF-16 for BMP
text, so it only misfires with astral-plane characters (emoji, some CJK) before
the cursor: completions/hover/definition resolve at a shifted position.

Repro: buffer line `a😀 SELECT`, press `K` on `SELECT`. Expect: hover for
`SELECT`. Actual: position sent as char col 4 instead of UTF-16 col 5 → wrong
resolution.

### Cleared (suspected and disproved)

- `completion_ctx::parse_context` mid-char `byte_offset` — the only caller
  derives the offset through `row_col_to_byte` (char→byte conversion); no path
  hands it a non-boundary offset. (The `&line[..col]` slice in the same block is
  finding 1.)
- `split_top_level_semicolons` / `has_top_level_keyword` / `strip_sql_comments`
  doubled-quote handling — close-then-reopen keeps `;` inside strings; same
  final token set.
- `statement_ranges` tree-sitter walk — root children + semicolon split handles
  DESC, comments, unterminated strings; test-covered.
- `results_find`/`hover_find` backward wrap arithmetic — probes cover all cells
  exactly once.
- `strip_sql_comments` byte stripping — all stripped regions delimited by ASCII
  bytes; multibyte sequences never split.
- `evict_old_results_dir` timestamp parsing — non-matching names skipped, not
  mis-deleted.
- Tampered `session.toml` / result JSON — restore clamps indices; grid access is
  `.get()`-based.
- `:export csv out.csv` bare filename → `create_dir_all("")` — succeeds on Linux
  (empty path = cwd).
- Batch tab indexing in `dispatch_pending_run` — `dismiss_results()` clears
  `result_tabs` before loading tabs are pushed.
- Executor/channel leak on reconnect — dropping senders winds the old executor
  down.
- `run_statement_under_cursor` `content[s..e]` — offsets computed against the
  same joined string the slice indexes.
- pgpass parsing (5-field rule, `\:`/`\\` escapes, permission check) — matches
  libpq; test-pinned.

### Hardening (correct today, fragile — not defects)

- `ddl.rs` doesn't recognize `DROP INDEX` / `DROP VIEW` / `DROP FUNCTION` /
  `DROP TRIGGER` — sidebar stays stale after those statements (documented
  heuristic). Verified: only TABLE / DATABASE / SCHEMA shapes handled.
- `write_sqls_config` interpolates the DSN raw into YAML
  (`crates/sqeel-core/src/lsp.rs`, `dataSourceName: "{dsn}"`) — a `"` or newline
  in a URL/password yields an unparseable sqls config and silently disables LSP.
  Verified: raw `format!` interpolation.
- `splice_password_into_url` silently skips URLs with no username — a password
  saved via the form for a userless URL is dropped on reload → auth failure with
  no explanation. Verified: `if parsed.username().is_empty() { return }`.
- Editor tab-bar mouse hit-test uses `tab.name.len()` bytes (lib.rs:1736) while
  rendering counts chars — multi-byte tab names mis-click. Verified.
- `AppState::persist_result` keys the results file by `active_connection` at
  completion time, not the connection the query ran on — a switch mid-query
  files the result under the wrong slug (session restore can't find it).
- `save_result` filename is `{1s-timestamp}_{fnv-1a-32bit}.json` — colliding
  32-bit hashes in the same second overwrite each other.
- `sanitize_conn_slug` maps `my conn` and `my_conn` to the same slug — colliding
  results dirs.
- Session watcher has a 1 s debounce with no final flush on quit — the last <1 s
  of edits may not reach disk.
- Ctrl-C fired while a query is still queued is wiped by the executor's
  `cancel.reset()` before the query starts — cancel does nothing in the
  dispatch→dequeue window.
- Batch results focus lands on the last tab, not the first (comment says first).
- `take_clipboard_writes().pop()` drops all but the newest queued write —
  multi-yank commands (macros) lose earlier writes.
- Out-of-window LSP diagnostics get a 1-cell underline in empty rows — cosmetic,
  disappears on scroll re-highlight.
- `next_scratch_name` check-then-create race — benign under the current
  single-threaded event loop.

### Coverage

Reviewed: all of `crates/sqeel-config`, `crates/sqeel-core` (incl. state.rs 5891
lines), `crates/sqeel-tui` (lib.rs 5545 lines, render.rs 3680 lines),
`apps/sqeel` src + tests. Read in full, not skimmed, by two read-only review
passes; every finding above re-verified by me (tracing the failure path and
reading the cited lines) before inclusion. Not reviewed: `pkg/` packaging
scripts, `.github/` workflows, the tree-sitter grammar fixtures and
`crates/sqeel-core/src/config.toml` bundled defaults. The LSP-position-unit
finding (10) and the LabelOffsets reachability (9) depend on server behaviour
(sqls) and were not exercised against a live server.

## Audit 2026-08-06

Security audit of the whole codebase (local TUI SQL client; attack surface: CLI
args, config/connection/session/result files, the local sqls LSP child process,
pgpass, keyring, DB server responses). The user's own SQL is trusted by design.
Two read-only passes; every finding below re-traced against the real code before
inclusion.

### Findings (ranked)

#### 1. MEDIUM — sqls config written via a predictable, symlinkable `/tmp` path; DB password lands in an attacker-readable file

`crates/sqeel-core/src/lsp.rs:29-36` —
`std::env::temp_dir().join(format!("sqeel-sqls-config-{}.yml", std::process::id()))`,
then `std::fs::write` (follows symlinks) and `set_permissions(0o600)` (follows
symlinks). The DSN written there carries the DB password: MySQL
`{userpass}@tcp({host})/{db}` (lsp.rs:53) or the Postgres URL verbatim
(lsp.rs:55), called with the live, keyring-spliced connection URL
(`apps/sqeel/src/bin/sqeel.rs:875` ← URL from `load_connections` at
sqeel.rs:420-423, which splices the keyring password). Any local user can learn
the PID (`pgrep`) and pre-plant the symlink; between `write` and
`set_permissions` the file is also briefly 0644.

Repro: `pgrep -x sqeel` → 4242; as another local user,
`ln -s /tmp/steal.yml /tmp/sqeel-sqls-config-4242.yml`; next connection writes
the DSN (with password) through the link. Expect: A's DB password stays secret.
Actual: it lands in `/tmp/steal.yml`, readable.

Fix: create with `O_EXCL`/`NamedTempFile` (or a private dir), and delete the
file on drop.

#### 2. MEDIUM — plaintext passwords and query content written world-readable (0644) under 0755 dirs

`crates/sqeel-config/src/lib.rs:417` (keyring-fallback connection TOML with the
inline password, when `keyring_ok` is false),
`crates/sqeel-core/src/config.rs:120` (session.toml — query text and errors),
`crates/sqeel-core/src/persistence.rs:168` (result JSON — full row data), all
via `std::fs::write` with default 0666&~umask = 0644 permissions inside
`create_dir_all` (0755) dirs. The app demands 0600 for the files it _reads_
(pgpass) and chmods the sqls config — but writes its own secrets 0644.

Repro: no keyring daemon (common headless); save connection
`postgres://alice:p@ss@db/prod`. Expect: password only in the keyring. Actual:
`~/.config/sqeel/conns/prod.toml` is 0644 containing the URL with the password;
any local user can read it (and query text from session.toml, row data from
results/\*.json).

Fix: write state files with 0o600 (or a private-dir / restrictive umask).

#### 3. LOW — headless `-e` prints DB cell content raw to the terminal (terminal escape injection, incl. OSC 52 clipboard hijack)

`apps/sqeel/src/bin/sqeel.rs:733` (table `println!("{}", fmt_row(&display))`)
and `:756` (CSV) format cells with only `{:<w$}` padding / quote-comma escaping;
`cell_display` (sqeel-core/src/state.rs:247) does no control-char filtering, so
a DB cell containing `ESC ]52;c;<base64> ESC \` is emitted raw into the user's
terminal. The TUI paths are safe (ratatui and hjkl-buffer-tui filter control
chars) — headless-only.

Repro: SQLite cell `char(27)||']52;c;TUlTQ0g='||char(27)||'\'`, run
`sqeel --url sqlite://x.db -e "SELECT * FROM t;"`. Expect: the cell text prints.
Actual: the OSC 52 sequence executes and sets the clipboard.

Fix: reuse the TUI's Control-Pictures mapping in the headless formatters.

#### 4. LOW — DB credentials shown unmasked (status bar, connection switcher, argv)

`apps/sqeel/src/bin/sqeel.rs:464` `set_status(format!("Connecting to {url}…"))`
and `crates/sqeel-tui/src/render.rs:3032`
(`Span::styled(format!(" — {}", c.url))`) put the full URL including password on
screen; `--url postgres://user:pass@…` is visible in `ps` for the process
lifetime, and nothing in README/clap help documents this or steers users to
`$DATABASE_URL` / connection files (the `$DATABASE_URL` prompt itself is masked,
sqeel.rs:339-343).

Impact: same-user shoulder-surfing / scrollback / `ps` exposure, not
cross-privilege. The `mask_db_url_password` helper (sqeel.rs:127) does mask
percent-encoded passwords correctly (`url::password()` returns the encoded form,
which the `replacen` matches) — it is simply not used on these two paths.

### Cleared (suspected, disproved)

- SQL injection via DB-server-controlled identifiers — backtick/quote-doubling
  on the MySQL/SQLite introspection queries; the Postgres/MySQL/DuckDB catalog
  queries are bound parameters.
- Command injection — no shell is ever built from data; child processes use
  `Command::new().args(...)` with fixed args (sqls `-config <path>`, tmux
  `select-pane -{L|R|D|U}`); pkg/ scripts use fixed URLs + pinned checksums.
- LSP child leak — hjkl-lsp 0.41 uses `kill_on_drop(true)` + grace period.
- pgpass permissions — `mode & 0o177 == 0` check matches libpq semantics.
- Path traversal via connection name / tab filenames — name charset restricted
  to `[A-Za-z0-9_-]`.
- TUI terminal escape injection — ratatui and hjkl-buffer-tui filter control
  graphemes on every render path.
- Malformed session.toml / result JSON — `.ok()` fallbacks, no panic.
- Theme/config parsing — errors become toasts, never panics.
- LSP diagnostic ranges — clamped before use.
- `--sandbox` — fresh mkdtemp, dirs redirected before config reads, cleanup
  default no.

### Hardening (correct today, fragile)

- sqls config file never deleted — credential-bearing file accumulates per
  process in /tmp (0600). Delete on drop.
- `load_result_for` (`persistence.rs:178`) joins `session.toml`-sourced names
  into the results dir without a `components()` check — self-inflicted content
  only; add the check anyway.
- No size limits on session.toml / result JSON reads — unbounded allocation on
  planted huge files (same-user DoS).
- Auto-LIMIT can be nullified by a trailing comment: `SELECT * FROM t -- x`
  becomes `SELECT * FROM t -- x LIMIT 100` — the cap is commented out, a large
  table materializes fully in memory. Verified (db.rs `apply_default_limit`).
- `duckdb:///abs/path` resolves to a _relative_ path (leading slashes all
  trimmed, db.rs:240-241); single-slash form works.
- Main TUI connection ignores the saved connection's TLS block —
  `DbConnection::connect(url, None)` (sqeel.rs:847) skips CA/mTLS/verify
  settings from `conns/*.toml` in the TUI path (they apply in `-e`).
- `Clipboard::new().expect(...)` aborts startup with no clipboard backend.
- No per-cell size cap — a single multi-GB cell is held fully in memory and
  persisted before rendering truncates it.
- `Mutex<AppState>` + `unwrap()` everywhere — a panic on any thread while
  holding the lock (e.g. review finding 1) poisons it and cascades.
- Keyring splice feeds attacker-planted URLs — requires config-dir write access
  (which already implies full file compromise).
- Error strings may embed the URL — displayed in-TUI only, not persisted.

### Coverage

Walked: CLI args (incl. `--sandbox`, headless `-e`), config/connection/session/
results file load+save+perms, keyring + pgpass, sqls YAML write + LSP
spawn/probe/shutdown, all DB introspection queries + user-SQL dispatch, all TUI
render/host/keybinding paths, the executor/schema-loader tasks, tests, and the
three `pkg/` packaging scripts. GAPS: `crates/sqeel-core` was reviewed (see the
Review section) but the LSP wire protocol internals (hjkl-lsp) and the
tree-sitter grammar fixtures were not audited in depth; the release workflow's
checksum substitution step was not verified; MySQL/Postgres/DuckDB paths are
static reads only — no live server exercised.

Summary: 0 critical, 0 high, 2 medium, 2 low (plus 2 duplicates of review
findings 1 and 5). Fix first: the `/tmp` sqls-config symlink race, and 0600
permissions on conns/session/results files. The byte/char slice panic (review
finding 1) and the headless escape injection (audit finding 3) are the crash and
injection fixes respectively.

## Tidy 2026-08-06

Quality/cleanup pass over the whole codebase (behavior-preserving only).
Dead-code claims verified by workspace-wide grep (each symbol below has zero
callers outside its own file); duplication claims traced against both copies.
Nothing was changed.

### Duplicated logic (drift risk — fix in one place)

- `crates/sqeel-core/src/highlight.rs` — the parse-error harvesting `filter_map`
  block (skip native statements → byte→row/col → `ParseError`) is byte-identical
  at 538-561, 588-611, 698-722. Extract
  `harvest_parse_errors(source, dialect, errors, nl_offsets)`.
- `crates/sqeel-core/src/state.rs` — header+rows column-width computation
  duplicated at 1262-1273 (`parse_hover_table`) and 1378-1389
  (`hover_table_from_cache`). Extract `grid_col_widths(header, rows)`. Do NOT
  fold into `QueryResult::compute_col_widths` — that one uses byte `.len()`,
  this uses `chars().count()`; they differ for non-ASCII cells.
- `crates/sqeel-core/src/state.rs` — find-scan loop duplicated at `results_find`
  1823-1838 and `hover_find` 1864-1881. Extract one `find_in_grid` helper.
- `crates/sqeel-core/src/state.rs` — mouse-hit column walk duplicated at
  `results_click_to_cell` 1600-1611 vs `hover_click_to_cell` 1677-1688; drag
  edge-stepping at `results_drag_to_cell` 1560-1571 vs `hover_drag_to_cell`
  1638-1649. Extract `col_at_x(col_widths, col_scroll, rel_x, col_count)` + a
  step helper.
- `crates/sqeel-tui/src/render.rs` — `cursor_byte_offset` (245-260) is a byte-
  for-byte reimplementation of `syntax.rs` `row_col_to_byte` (417-431); delete
  it and call the existing helper from exec.rs:92. Verified identical on
  empty/EOL/past-EOL/past-last-row inputs.
- `crates/sqeel-tui/src/render.rs` — `extract_results_left_click` per-pane
  query-row blocks at 811-813, 834-836, 862-864 are dead duplicates of the outer
  check at 737-742 (the caller bounds clicks inside the results area, so the
  outer fires first); each also re-clones `query` the outer already holds.
  Delete the three blocks + per-arm clones, keep `has_q`/`body_start`.
- `crates/sqeel-tui/src/render.rs` — `highlight_sql_lines` (2113-2129) and
  `highlight_query_line` (2183-2202) duplicate the TLS highlighter bootstrap;
  extract one `highlight_spans(source, dialect)`.
- `crates/sqeel-tui/src/lib.rs` — schema-refresh-with-toast block duplicated at
  2119-2138 and 2608-2628; extract `refresh_schema_with_toast`.
- `crates/sqeel-tui/src/lib.rs` — Anvil `ToolSpec` literal tripled (679-687,
  2358-2368, 2397-2407); Install/Update arms share the unknown-tool and
  already-in-progress toasts.
- `crates/sqeel-tui/src/lib.rs` — tab-content apply block (set_content +
  take_dirty + reset last_highlight_top) ×5 at 1759-1764, 2153-2158, 2994-2999,
  3404-3409, 3424-3428.
- `crates/sqeel-tui/src/render.rs` — column-scroll char-offset prefix sum ×3
  (752-757, 1653-1658, 2346-2351); extract
  `col_scroll_char_offset(col_widths, skip)`.
- `crates/sqeel-tui/src/ex.rs:289-305` — inline `~/` expansion duplicates
  `syntax.rs` `expand_tilde`, but NOT a drop-in: ex.rs errors on
  `home_dir() == None` and leaves bare `~` literal, `expand_tilde` passes
  through / expands. Unify only if the error path is kept.

### Dead code (delete; each is pub-in-lib with zero callers, verified by grep)

- `crates/sqeel-core/src/persistence.rs` — `results_dir()` (53),
  `list_results()` (215), `load_result()` (238) — dead trio; `results_dir_for`
  is the live variant.
- `crates/sqeel-core/src/state.rs` — `close_active_result_tab` (876),
  `schema_toggle_path` (2424), `append_db_tables` (2663), `refresh_schema_nodes`
  (2858, not even used by tests), `update_active_tab_cursor` (3694),
  `save_all_dirty` (3973; the TUI uses the `prepare_save_all_dirty` +
  `PendingSave::commit` split).
- `crates/sqeel-core/src/config.rs:154` — `load_session()` (binary uses
  `load_session_data`/`save_session`; `load_session_inner` stays).
- `crates/sqeel-core/src/schema.rs:121` — `is_expanded(&self)`.
- `apps/sqeel/src/bin/sqeel.rs:1022/1025/1039/1060` — `cancelled` local in the
  batch loop is write-only; drop the var and both assignments.
- `crates/sqeel-tui/src/ex.rs:251` — `handle_export_cmd`'s `_toasts` param
  unused (1 caller lib.rs:2604 + 5 test calls to update).
- `crates/sqeel-tui/src/theme.rs:144` — `Theme.name` read only by tests; drop
  field + the `#[allow(dead_code)]`, or surface it.

### Over-abstraction

- `crates/sqeel-core/src/persistence.rs:61-64` — `ensure_dir` is a pure
  `create_dir_all` passthrough at 3 in-file call sites; inline it.

### Allocation nits

- `crates/sqeel-core/src/highlight.rs:633` — `highlight()` clones the whole
  string into an `Arc` for a callee that only uses `&str`; change
  `highlight_shared` to take `&str`.
- `crates/sqeel-core/src/lsp.rs:316,333,346` — `from_value(result.clone())` ×3;
  consume on the first attempt, clone only for later ones.
- `crates/sqeel-core/src/completion_ctx.rs:106,141` — `Token.upper: String`
  per-token alloc; `eq_ignore_ascii_case` on `tokens[idx].text` drops the field.
- `crates/sqeel-core/src/highlight.rs:621-623` — `block_ranges()` clones; return
  `&[(usize, usize)]` (single caller iterates immediately).
- `crates/sqeel-tui/src/render.rs:128` — `re.as_str().to_string()` only
  interpolated into `format!`s; bind `re.as_str()` instead.

### Coverage

sqeel-config + sqeel-core read in full (state.rs non-test code 1-4173);
sqeel-tui + apps/sqeel read in full by the sibling tidy pass; tests grepped for
every symbol claimed unused. Deliberately NOT unified (different semantics): the
three decode probe ladders in db.rs, schema.rs flatten/expand helpers, and the
comment/string scanners in highlight.rs vs db.rs.

## Perf 2026-08-06

Performance pass over the whole codebase. Frame-rate context: one redraw runs
per event (keystroke, mouse, resize), per content change, per LSP event, and up
to 20 Hz while a hover loads; each redraw runs `draw` while holding the global
`state` mutex. Findings ranked by impact; every cost traced to a named caller +
frequency. (One core-agent finding — `refresh_schema_nodes` — was dropped: it is
dead code per the Tidy section.)

### Findings

#### 1. Results grid rebuilt end-to-end every frame — O(rows-after-scroll × cols) per redraw

`crates/sqeel-tui/src/render.rs:1918-1960` — `render_grid_lines` skips
`body_skip` then `.collect()`s every remaining row as ratatui `Line`s, with a
`format!` String alloc per cell (1930) and `"│".to_string()` per gap (1955); no
`.take()` bounds the visible window. Called by `draw_results`
(render.rs:1681-1694) and `draw_hover_table` (2331-2344) on every redraw. With
`default_row_limit = 0` a 100k-row result means 100k×cols String allocs per
keystroke. Fix: `.skip(body_skip).take(body_height)` (ratatui clips vertically;
only the horizontal scroll matters).

#### 2. Search-state work per frame: whole-buffer materialization + regex scan + `Regex::new` recompile

Three related costs, all in `draw_status_bar` / `draw_editor` (every redraw
while a `/` search was ever committed):

- `search_label` (render.rs:129-144) calls `buffer_lines(editor.buffer())` (all
  lines as owned Strings) and `re.find_iter` over every line, per frame.
- `draw_editor` (render.rs:1368-1370) recompiles `regex::Regex::new(q)` from
  `last_editor_search` on every frame — the engine already holds the compiled
  regex in `search_state().pattern`. Fix: cache the match counts keyed on the
  buffer's `dirty_gen` + pattern, and reuse `search_state().pattern` instead of
  recompiling.

#### 3. Schema filter is O(N·M) per frame while the search box is active

`crates/sqeel-core/src/schema.rs:473-491` — `filter_items` runs a linear scan of
all matched paths for every item (`is_descendant` closure), plus a lowercased
`String` alloc per label (`label_matches`, 464-468). Called from `draw_schema`
(render.rs:1179) on every redraw with a filter. Fix: `HashSet` of matched paths
with prefix probes (depth ≤ 5), cache the filtered list keyed on query,
pre-lower search labels.

#### 4. Retained-tree walk + two full newline-offset scans per highlight pass

`crates/sqeel-core/src/highlight.rs:563-568` — `highlight_range` walks the
ENTIRE retained tree (`collect_block_ranges` + `node.walk()` per node) on every
pass, including scroll-only passes where the tree is unchanged (fires on every
keystroke and every scroll past the margin, lib.rs:941-951). And
`compute_newline_offsets(source)` runs twice per pass — once at highlight.rs:511
and again inside `promote_uncovered_dialect_keywords_in_range` (:825) for every
non-Generic dialect. Fix: cache block ranges on a tree-generation counter; pass
the :511 offsets into the promotion function.

#### 5. Statement runs pay 1–2 full cold tree-sitter parses each

`crates/sqeel-core/src/highlight.rs:996-1031` / `:1110-1115` —
`statement_ranges` builds a fresh `Parser` and parses the whole buffer;
`first_syntax_error` parses again. Every Ctrl+Enter runs `statement_at_byte`
then `first_syntax_error` (exec.rs:93, 119); Ctrl+Shift+Enter runs
`split_statements` plus `first_syntax_error` over the whole content
(exec.rs:151, 163) — while the `Highlighter` already maintains an incremental
tree of this exact buffer (lib.rs:954-1018). Fix: route statement-finding
through the retained tree.

#### 6. Completion pipeline: per-keystroke O(schema) allocs under the state lock

`crates/sqeel-core/src/state.rs:2110-2190` — `completions_for_context` runs with
`prefix = ""` (lib.rs:1148, 1487) so every `starts_with` is true, yet each
candidate pays `to_lowercase()` + `to_owned()`×2 (3 heap allocs) plus an
`out.sort()`; the `Any` arm re-processes `schema_identifier_cache` which is
already sorted + deduped (:702-704). Fix: skip lowercase/prefix work for empty
prefix; return the cache clone directly for `Any`.

#### 7. K-hover does linear schema scans with per-name allocations

`crates/sqeel-core/src/state.rs:1291-1315` (`find_table`) and `:1322-1404`
(`hover_table_from_cache`) loop every db × table with a `to_lowercase()` alloc
per name, per `K` press. Fix: lowercase-name → table map built in
`rebuild_schema_cache` (off the render loop).

#### 8. Query/DDL line re-parsed on every redraw

`crates/sqeel-core/src/highlight.rs:627-741` — `highlight_shared` does
`inner.reset()` + a cold full parse + full-tree walk on every call, from
`highlight_query_line`/`highlight_sql_lines` inside `draw_results` every frame
(render.rs:1592-1593, 1696, 2029). The DDL body is a stable String across
frames. Fix: cache `(source identity, dialect) → spans` in the `Highlighter`.

#### 9. Whole-buffer materialization for single-line reads

`buffer_lines(editor.buffer())` allocates every line to serve one row:
lib.rs:1121-1129 (per content publish, ~75 ms debounce), lib.rs:3981
(`word_at_cursor`, per `K`), exec.rs:92 (`cursor_byte_offset`, per Ctrl+Enter).
Fix: rope-walking variants of `word_prefix_at`/`row_col_to_byte` that read only
up to the cursor row.

#### 10. Frame-global lock + LSP-event redraws multiply everything above

`crates/sqeel-tui/src/lib.rs:1538-1541` holds `state` across the whole
`terminal.draw`, so every cost above serializes under one mutex; and lib.rs:1368
forces a redraw for every LSP event, including diagnostics-only publishes. Fix
after 1-8: snapshot render inputs outside the lock; skip redraws that change no
visible state.

### Minor

- `draw_completions` builds `Vec<ListItem>` over all completions every frame
  though ≤12 show (render.rs:2687-2702) — `.take(popup_h)` first.
- `tmux_navigate` spawns a `tmux` process per nav keystroke (exec.rs:188-193).
- `merge_db_list` sorts by `position()` — O(D²) per db-list load
  (state.rs:2716-2721).
- `results_find`/`hover_find` allocate `to_lowercase()` per cell
  (state.rs:1833, 1875) — user-initiated, row-capped.

### Coverage

Traced to a named frequency: draw-frame path (schema filter, results grid,
search label, query-line highlight), highlight resubmit path (keystroke/scroll),
completion path (insert keystroke), run path (Ctrl+Enter/Ctrl+Shift+Enter),
hover path (K), schema refresh (per load). Verified NOT hot: db.rs
execute/decode (row-bounded), ddl/safety/completion_ctx (bounded), lsp (per
response), persistence (writes on spawn_blocking), sqeel-config (startup). Could
not settle without profiling: relative weights of tree-sitter parse vs. the
block-ranges walk vs. newline scans on large buffers; whether mouse-move events
are enabled (which would inflate every per-frame cost above).
