# Backlog

Open work items, findings and deferred decisions. Finished items are deleted
(see `git log` for the record).

## Work 2026-08-11

Worked from the backlog: all five open correctness/security findings from the
2026-08-10 Review/Audit passes landed (commits 9b7fc16..3cff1eb; each verified
by the full gate plus a test that goes red on the old code):

- Headless `-e` table/CSV **header rows** now run through `sanitize_cell` — a
  raw ESC byte in a column name no longer reaches stdout (Review-3 / Audit-1).
- `:export csv|json` and `:w <path>` write via `write_private` (0600) instead of
  bare `std::fs::write` (0644) (Audit-2).
- `push_history` stamps the connection the query RAN on (threaded through
  `apply_exec_outcome`), not `active_connection` at completion time (Work
  2026-08-07 / Review re-verified / Audit-5).
- LSP diagnostic underlines convert the server's UTF-16 columns to byte offsets
  against the line before painting (Review-2 / Audit-4).
- Runs are gated on `query_in_flight()` — a second run while one is in flight
  toasts instead of both aliasing result-tab index 0 (Review-1 / Audit-3).

Still open: the Review/Audit "Hardening" blocks below (deliberate fragility —
each needs a decision before touching), and the Tidy / Perf passes' findings.

## Release pipeline 2026-08-06 (v0.6.1)

Cut v0.6.1; GitHub release + homebrew published. Three tag-run jobs failed.
Fixed after the cut (see `git log`):

- **crates.io publish** — the `sqeel-config`/`sqeel-core`/`sqeel-tui` crates
  were absorbed into this monorepo (432b055) but never re-published; crates.io
  still held stale 0.33-era builds that no longer compiled, and the publish job
  only shipped the umbrella. Fixed: crates bumped to 0.4.0 / 0.6.0 / 0.6.0,
  `hjkl-*` pinned `=0.41.0`, and `publish-crates` now ships all four in
  dependency order.
- **Alpine apk** — abuild rejected the uncompressed man page; the APKBUILD now
  gzips `sqeel.1` before install.
- **AUR sqeel-bin** — failed on "The AUR is down due to maintenance" (external,
  transient; v0.6.0's AUR publish succeeded).

## Review 2026-08-06

Correctness review of the whole codebase (4 crates + app + tests). All 10
findings fixed (see `git log`); suspected-and-disproved items cleared. Open
items below are deliberate fragility, not defects.

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
passes; every item above re-verified by me (tracing the failure path and reading
the cited lines) before inclusion. Not reviewed: `pkg/` packaging scripts,
`.github/` workflows, the tree-sitter grammar fixtures and
`crates/sqeel-core/src/config.toml` bundled defaults.

## Audit 2026-08-06

Security audit of the whole codebase (local TUI SQL client; attack surface: CLI
args, config/connection/session/result files, the local sqls LSP child process,
pgpass, keyring, DB server responses). The user's own SQL is trusted by design.
All 4 findings fixed (see `git log`); suspected-and-disproved items cleared.
Open items below are deliberate fragility, not defects.

### Hardening (correct today, fragile)

- `load_result_for` (`persistence.rs:178`) joins `session.toml`-sourced names
  into the results dir without a `components()` check — self-inflicted content
  only; add the check anyway.
- No size limits on session.toml / result JSON reads — unbounded allocation on
  planted huge files (same-user DoS).
- `duckdb:///abs/path` resolves to a _relative_ path (leading slashes all
  trimmed, db.rs:240-241); single-slash form works.
- Main TUI connection ignores the saved connection's TLS block —
  `DbConnection::connect(url, None)` (sqeel.rs:847) skips CA/mTLS/verify
  settings from `conns/*.toml` in the TUI path (they apply in `-e`).
- `Clipboard::new().expect(...)` aborts startup with no clipboard backend.
- No per-cell size cap — a single multi-GB cell is held fully in memory and
  persisted before rendering truncates it.
- `Mutex<AppState>` + `unwrap()` everywhere — a panic on any thread while
  holding the lock (e.g. the byte/char slice panic) poisons it and cascades.
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

## Tidy 2026-08-06

Quality/cleanup pass over the whole codebase (behavior-preserving only).
Dead-code claims verified by workspace-wide grep (each symbol below has zero
callers outside its own file); duplication claims traced against both copies.
The dead-code deletions and the `ensure_dir` inline landed (567898b); the items
below remain open.

### Duplicated logic (drift risk — fix in one place)

- `crates/sqeel-tui/src/ex.rs:289-305` — inline `~/` expansion duplicates
  `syntax.rs` `expand_tilde`, but NOT a drop-in: ex.rs errors on
  `home_dir() == None` and leaves bare `~` literal, `expand_tilde` passes
  through / expands. Unify only if the error path is kept.

### Allocation nits

- `crates/sqeel-core/src/lsp.rs` — `from_value(result.clone())` ×3 then a
  consuming final attempt is already the minimum for 4 deserialization tries.
  The proposed "consume on the first attempt" is impossible:
  `serde_json::from_value` consumes the `Value` on failure and does not return
  it. Any further saving needs shape-based pre-dispatch, which risks behaviour
  change — decision needed.

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
frequency. (One core-agent finding — `refresh_schema_nodes` — was dropped: it
was deleted as dead code in 567898b.)

### Findings

#### 1. Search-state work per frame: whole-buffer materialization + regex scan

Every redraw while a `/` search was ever committed, `search_label`
(render.rs:129-144) calls `buffer_lines(editor.buffer())` (all lines as owned
Strings) and `re.find_iter` over every line to compute the status-bar match
counts. The per-frame `regex::Regex::new` recompile in `draw_editor` is already
gone (the installed `search_state().pattern` is reused when the query is
unchanged). Fix: cache `(total, current)` keyed on the buffer's `dirty_gen` +
pattern + cursor, so steady-state frames skip the scan.

#### 2. Schema filter: label lowercasing + no result cache while filtering

`filter_items` (schema.rs) now tests descendants via prefix probes into a
matched-path set — O(depth) per item, the O(N·M) linear scan is gone. Remaining
per-frame costs while the filter box is active: a lowercased `String` alloc per
label (`label_matches`, schema.rs:454) and no result cache. Fix: pre-lower
labels, and cache the filtered list keyed on query + schema generation (needs an
invalidation key from the schema refresh path).

#### 3. Retained-tree walk + two full newline-offset scans per highlight pass

`crates/sqeel-core/src/highlight.rs:563-568` — `highlight_range` walks the
ENTIRE retained tree (`collect_block_ranges` + `node.walk()` per node) on every
pass, including scroll-only passes where the tree is unchanged (fires on every
keystroke and every scroll past the margin, lib.rs:941-951). And
`compute_newline_offsets(source)` runs twice per pass — once at highlight.rs:511
and again inside `promote_uncovered_dialect_keywords_in_range` (:825) for every
non-Generic dialect. Fix: cache block ranges on a tree-generation counter; pass
the :511 offsets into the promotion function.

#### 4. Statement runs pay 1–2 full cold tree-sitter parses each

`crates/sqeel-core/src/highlight.rs:996-1031` / `:1110-1115` —
`statement_ranges` builds a fresh `Parser` and parses the whole buffer;
`first_syntax_error` parses again. Every Ctrl+Enter runs `statement_at_byte`
then `first_syntax_error` (exec.rs:93, 119); Ctrl+Shift+Enter runs
`split_statements` plus `first_syntax_error` over the whole content
(exec.rs:151, 163) — while the `Highlighter` already maintains an incremental
tree of this exact buffer (lib.rs:954-1018). Fix: route statement-finding
through the retained tree.

#### 6. K-hover does linear schema scans with per-name allocations

`crates/sqeel-core/src/state.rs:1291-1315` (`find_table`) and `:1322-1404`
(`hover_table_from_cache`) loop every db × table with a `to_lowercase()` alloc
per name, per `K` press. Fix: lowercase-name → table map built in
`rebuild_schema_cache` (off the render loop).

#### 7. Query/DDL line re-parsed on every redraw

`crates/sqeel-core/src/highlight.rs:627-741` — `highlight_shared` does
`inner.reset()` + a cold full parse + full-tree walk on every call, from
`highlight_query_line`/`highlight_sql_lines` inside `draw_results` every frame
(render.rs:1592-1593, 1696, 2029). The DDL body is a stable String across
frames. Fix: cache `(source identity, dialect) → spans` in the `Highlighter`.

#### 8. Whole-buffer materialization for single-line reads

`buffer_lines(editor.buffer())` allocates every line to serve one row:
lib.rs:1121-1129 (per content publish, ~75 ms debounce), lib.rs:3981
(`word_at_cursor`, per `K`), exec.rs:92 (`cursor_byte_offset`, per Ctrl+Enter).
Fix: rope-walking variants of `word_prefix_at`/`row_col_to_byte` that read only
up to the cursor row.

#### 9. Frame-global lock + LSP-event redraws multiply everything above

`crates/sqeel-tui/src/lib.rs:1538-1541` holds `state` across the whole
`terminal.draw`, so every cost above serializes under one mutex; and lib.rs:1368
forces a redraw for every LSP event, including diagnostics-only publishes. Fix
after 1-8: snapshot render inputs outside the lock; skip redraws that change no
visible state.

### Minor

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

## Review 2026-08-10

Correctness review of the whole codebase (4 crates + app + tests; clean tree,
depth low). The 3 findings and the re-verified `push_history` item were all
fixed on 2026-08-11 (see `## Work 2026-08-11`); the Cleared / Hardening /
Coverage records below remain as written.

### Cleared

- `apply_default_limit` vs trailing comments/semicolons: `split_statements`
  drops comment tails that follow a `;` (they land in their own, then
  comment-filtered, range), and comments are stripped before the LIMIT append —
  no `…; LIMIT n` multi-statement text ever reaches the executor
  (db.rs:1543-1563, highlight.rs:984-991).
- CSV cell injection: rows are sanitized before quoting (sqeel.rs:756) — only
  the header path misses (finding 3).
- `statement_at_byte` over a leading comment: the comment-only range is filtered
  by the strip-comments check in `run_statement_under_cursor` (exec.rs:119-121).
- `results_find` / `hover_find` backward-wrap math
  `(start + i·(total−1)) % total`: correct for every total ≥ 2
  (state.rs:1830-1834, 1871-1875).
- `:describe` / `:desc` injection via backtick/paren table names
  (ex.rs:390-409): user-typed input, trusted by design (audit scope).
- Switch-away-from-dirty-tab and `evict_cold_tabs`: dirty tabs keep their
  in-memory content in `tab.content` and are never evicted (state.rs:3620-3657,
  3803-3816).
- `SqlsConfigFile` drop lifecycle: the credential-bearing sqls config is removed
  on drop and on replacement (lib.rs:120-126).
- UTF-8/char-index conversions in TextInput, `row_col_to_byte`,
  `char_col_to_byte`: all char-index carets converted via `char_indices`;
  multibyte cases pinned by tests.

### Hardening (correct today, fragile — not defects)

- Editing a connection and leaving the Password field empty cannot clear the
  keyring entry: `save_connection` with `password=None` leaves the old
  credential in place (sqeel-config lib.rs:380-405), so `load_connections`
  splices the stale password back on the next startup — including into a URL the
  user edited to point at a different host (lib.rs:329-336). No UI path removes
  a stored password short of deleting the connection.
- Ctrl+N at the end of query history replaces the buffer with "" (lib.rs:
  3948-3955), and Ctrl+P replaces the whole buffer with the recalled query with
  no guard — unsaved edits in the buffer are discarded. Documented recall
  semantics, but the wipe-on-`None` arm is vim-divergent (vim restores the
  in-progress line at history end).
- History semantics differ by failure path: `dispatch_pending_run` records the
  query when the send fails (exec.rs:58), the executor's Error arm does not
  (sqeel.rs:1117-1119) — "channel full" and "SQL error" leave different history
  trails.

### Coverage

Scope: entire workspace (clean tree), depth low. Reviewed in full: sqeel-config
(pgpass.rs, lib.rs); sqeel-core (config, safety, ddl, completion_ctx, db,
highlight, lsp, schema, persistence, state); sqeel-tui (lib.rs, render.rs,
exec.rs, ex.rs, host.rs, syntax.rs, picker.rs, completion_thread.rs, splash.rs,
theme.rs lines 1-120 + structure); apps/sqeel (bin/sqeel.rs, tests/headless.rs,
tests/e2e.rs, pty harness — the 11 smoke tests' `openpty` failure confirmed
environmental at harness.rs:78). GAPS: theme.rs 121-849 (color tables) and the
bundled theme TOMLs; tree-sitter grammar fixtures and hjkl-bonsai internals;
pkg/ packaging scripts; .github workflows. The test suite was NOT run (task
instruction; 11 PTY e2e tests fail environmentally in this sandbox).
MySQL/Postgres/DuckDB paths are static reads only — no live server exercised;
the sqls position-encoding behaviour (finding 2's operative assumption) was not
exercised against a real server.

## Audit 2026-08-10

Security + correctness audit of the whole codebase (clean tree, depth low). All
5 findings were fixed on 2026-08-11 (see `## Work 2026-08-11`); the Cleared /
Hardening / Coverage records below remain as written.

### Cleared

- LSP server `line`/`col` → `editor.jump_to(line+1, col+1)`: clamps in
  hjkl-engine 0.41.0 `editor.rs:4388` (row min'd to last, col min'd to line char
  count) — a hostile `u32::MAX` position cannot panic or go OOB.
- LSP diagnostic row/col out of range: `start_row >= buffer_rows` returns early,
  `stop`/cols clamped (`syntax.rs:199-224`) — no OOB indexing.
- Terminal-escape injection from DB cells / LSP hover / completions / status:
  all TUI text renders through ratatui, and ratatui-core 0.1.2's
  `Buffer::set_stringn` drops control characters at buffer-fill time
  (`buffer.rs:350-353`); the editor pane additionally maps them to Control
  Pictures (hjkl-buffer-tui 0.41.0 `render.rs`). Only the headless stdout path
  (finding 1) leaks.
- hjkl-lsp untrusted-server wire parsing: message payload capped at 16 MiB,
  header at 64 KiB (`codec.rs:6-11`) — no unbounded allocation from a malicious
  sqls.
- `:describe`/`:desc` SQLi via backtick/paren/`;` table names (`ex.rs:390-409`):
  user-typed command input — the trusted-by-design doctrine of both prior audits
  applies; single-quote check is belt-and- braces. MySQL multi-statement text is
  additionally rejected by sqlx.
- Introspection-query injection: every catalog query binds its parameters
  (`db.rs:574, 617-618, 654-667, 730-733, 773-776, 860-866, 897-918`); the three
  SQLite PRAGMA sites escape `"` → `""` (`:632, :805, :816, :945`) and the one
  MySQL `SHOW TABLES FROM` site backtick-doubles (`:551`). Traced: a table name
  containing `"`/`` ` ``/`)` stays inside the quoted identifier.
- LIMIT injection: `apply_default_limit` appends a formatted `usize`
  (`db.rs:1562`), and multi-statement text never reaches the executor
  (`split_statements` + comment stripping — re-verified, matches Review
  2026-08-10's cleared item).
- pgpass: world-readable files are skipped before any parse (`pgpass.rs:92-98`,
  pinned by tests) — a planted 0644 `.pgpass` yields zero entries, not
  credentials.
- Keyring splice of attacker-planted `conns/*.toml` URLs
  (`sqeel-config lib.rs:329-336`): requires config-dir write access, which
  already implies full file compromise — not a boundary.
- Tab-name path traversal: `rename_active_tab` restricts to `[A-Za-z0-9-_.]` +
  `.sql` (`state.rs:3732-3739`); `:saveas` rejects multi-component names
  (`lib.rs:2709`); `:e` strips to `file_name()` (`lib.rs:2808-2811`) — a name
  can never escape the queries dir.
- `theme::switch_colorscheme`: matches bundled names only, no file reads from
  user input (`theme.rs:108-121`).
- `evict_old_results`: `read_dir` never yields `..`; `remove_file` on a planted
  symlink removes the link, not the target (`persistence.rs:223- 235`).
- `mask_db_url_password`: `replacen(":{encoded}@")` matches both raw and
  percent-encoded passwords (tests `sqeel.rs:1425-1462`); malformed URLs
  returned unchanged.
- DuckDB `:memory:` and path handling: `duckdb::memory:`/empty → in-memory,
  spawn_blocking isolates panics (`db.rs:238-262`).
- hjkl-buffer `jump_to`/`set_cursor` and `row_col_to_byte`: all clamp or fall
  back to `String::new()` — no panic on out-of-range cursor math.

### Hardening (correct today, fragile — not defects)

- `write_sqls_config` interpolates the DSN raw into YAML (`lsp.rs:36-38`): a `"`
  or newline in a URL/password yields an unparseable sqls config (LSP silently
  off); a password containing a newline could inject a second `connections:`
  entry — same-user (config dir / keyring access already implies compromise),
  but the file is written to world-visible `/tmp` with 0600 and the injection
  target is a config that then contains the password.
- `splice_password_into_url` silently drops a saved password for URLs with no
  username (`sqeel-config lib.rs:290`) — form-saved password for a userless URL
  is lost on reload → auth failure with no explanation.
- `save_connection` with `password=None`/empty cannot clear a stale keyring
  entry (`sqeel-config lib.rs:380-405`) — the old credential is spliced back
  into a URL the user edited to point at a different host.
- `duckdb:///abs/path` resolves to a _relative_ path — leading slashes all
  trimmed (`db.rs:241-242`).
- `load_result_for` joins a session.toml-sourced filename without a
  `components()` check (`persistence.rs:199-206`) — self-inflicted content only;
  read fails (JSON parse) before anything loads.
- TUI connection path ignores the saved connection's TLS block —
  `DbConnection::connect(url, None)` (`sqeel.rs:847`) skips CA/mTLS/verify in
  the TUI; they apply in `-e`.
- No size limits on session.toml / result JSON reads (`config.rs:126`,
  `persistence.rs:202`) — planted huge files are an unbounded read (same-user
  DoS).
- `export_csv` applies no control-char sanitization (`persistence.rs:239- 250`)
  — the headless CSV row path sanitizes (`sqeel.rs:756`), so `:export csv` is
  the one CSV path that writes raw ESC bytes into the file (inert on disk;
  injects only when the file is catted).
- CSV formula injection: leading `=`, `+`, `-`, `@` cells are exported verbatim
  (headless CSV + `:export csv`) — a spreadsheet-opened export of DB-sourced
  text can execute formulas. Data is the user's own; flagged for awareness only.
- `retry_connection` status fallback can show the raw URL-with-password
  (`state.rs:2931` `unwrap_or_else(|| url.clone())`) when the failed URL matches
  no saved connection — in-TUI on the user's own screen, but the one spot the
  masked-string discipline is bypassed.
- `results/` and `queries/` dirs are created 0755 (`create_dir_all`): result
  filenames (`<timestamp>_<fnv16>.json`) are world-listable — query
  timing/metadata leak to other local users; file contents stay 0600.

### Coverage

Walked and traced: `apps/sqeel/src/bin/sqeel.rs` in full (CLI args, `-e`
headless, sandbox, env: `DATABASE_URL`, `SQEEL_SANDBOX_AUTOCLEAN`,
`SQEEL_DEBUG_HL_DUMP`, `PGPASSFILE`); `sqeel-config` (lib.rs, pgpass.rs) in
full; `sqeel-core`: db.rs in full (connect, execution, LIMIT rewrite, statement
splitting, introspection), lsp.rs in full + the event consumers in sqeel-tui
(lib.rs diagnostics/definition/hover/completion/signature arms, syntax.rs
underline + clamps), persistence.rs, config.rs, safety.rs, ddl.rs,
completion_ctx.rs in full; state.rs security-relevant paths (URL masking,
connection add/edit/delete forms, tabs/save/rename/delete, query history,
persist_result, results-pane math, LSP event state, retry); sqeel- tui: lib.rs
(LSP startup/restart/`SqlsConfigFile` lifecycle, ex-command dispatch incl.
`:w`/`:saveas`/`:e`/`:export`/`:describe`/`:Anvil`, highlight window,
clipboard), exec.rs, ex.rs, host.rs, picker.rs, completion_thread.rs in full;
render.rs result/hover/status/export paths; theme.rs (colorscheme resolution +
structure); dependency sides: hjkl-lsp 0.41.0 codec caps, hjkl-engine 0.41.0
`jump_to`, hjkl-buffer 0.41.0 `set_cursor`, ratatui-core 0.1.2 control-char
filtering. GAPs: tree-sitter grammar fixtures + bundled `config.toml` defaults;
theme.rs 121-849 color tables and bundled theme TOMLs; hjkl-ex registry
internals (how `:w`/ `:saveas`/`:e` parse before sqeel's arms) and hjkl-anvil
install machinery; `pkg/` packaging scripts; `.github/` workflows. No live
MySQL/Postgres/ DuckDB server exercised (static reads only) and the sqls binary
was not run — server responses traced against `lsp_types` + the codec caps
above. Test suite not run per task instruction (11 PTY e2e tests fail
environmentally in this sandbox; CI runs them).

### Summary

5 findings, all LOW (1 security-correctness: headless header escape; 1
data-at-rest perms: 0644 exports; 3 re-verified open correctness items:
result-tab aliasing, UTF-16 diagnostic cols, history connection stamp), 0
critical/high/medium. Overall risk for this local tool is low: no remote attack
surface, credentials handled carefully (0600 state files, keyring with plaintext
fallback only on keyring failure, URL masking, O_EXCL 0600 sqls config), and
untrusted-server (LSP) inputs are size-capped and clamped. Top fixes: (1) run
table/CSV headers through `sanitize_cell` to close the headless escape leak; (2)
switch `:export` and `:w <path>` to the 0600 `write_private` path; (3) land the
three open correctness items (thread `conn_slug` into `push_history`, fix the
UTF-16→byte diagnostic conversion, gate runs on `query_in_flight()`).

## Tidy 2026-08-10

Quality/cleanup pass over the whole codebase (behavior-preserving only; clean
tree; no code changed). Every duplication below was traced against both copies;
the dead-branch claim (item 13) was checked against the helper's own fallback.
The 2026-08-06 tidy items 1–10 were re-verified still open and are re-listed
with current line numbers (nothing landed since). New this pass: items 2, 11–24.

### Duplicated logic (extract a helper — drift risk)

- `crates/sqeel-core/src/state.rs` — the add-connection caret methods
  (`add_connection_type_char`/`backspace`/`delete`/`left`/`right`/`home`/`end`,
  3335-3408) reimplement `TextInput`'s char-index arithmetic (lib.rs:195-287)
  against the `(&mut String, &mut usize)` field pairs. Hoisting `TextInput` into
  sqeel-core and storing it for the add-connection fields removes ~30 lines of
  duplicated caret math (AppState is not Serialize, so no wire impact —
  cross-crate decision).
- `crates/sqeel-tui/src/ex.rs:286-299` — inline `~/` expansion still duplicates
  `syntax.rs:402` `expand_tilde` (still open from 2026-08-06). NOT a drop-in:
  ex.rs errors on `home_dir() == None` / leaves bare `~` literal, `expand_tilde`
  passes through. Unify only if the error path is kept.

### Coverage

Walked in full (non-test code): sqeel-config (lib.rs, pgpass.rs); sqeel-core
(state.rs 1-4173, db.rs 1-1640, highlight.rs 1-1226, lsp.rs 1-496, schema.rs
1-700, persistence.rs, config.rs, safety.rs, ddl.rs, completion_ctx.rs);
sqeel-tui (lib.rs, render.rs, ex.rs, exec.rs, host.rs, syntax.rs, picker.rs,
completion_thread.rs, splash.rs, theme.rs 1-130 + structure); apps/sqeel
(bin/sqeel.rs, tests/headless.rs).
`cargo clippy --all-targets --all-features -- -D warnings` green — no dead
private code; every dead-code/dup claim verified by workspace grep incl. tests.
GAP: theme.rs 131-849 (color tables) and the bundled theme TOMLs; tree-sitter
grammar fixtures + bundled config.toml; `pkg/` and `.github/`; the test modules
of the core/tui crates (grepped for symbol usage, not read for cleanups). Test
suite NOT run per task instruction (11 PTY e2e tests fail environmentally in
this sandbox; CI runs them).

## Perf 2026-08-10

Performance pass over the whole codebase (clean tree, depth low; no code
changed). Second perf pass: every finding from `## Perf 2026-08-06` was
re-verified against the current tree — all 9 numbered + 3 minor are STILL OPEN,
line numbers refreshed below. New this pass: findings 1 and 3. Frame-rate
context unchanged from the 06-06 pass: one redraw runs per event (keystroke,
mouse, resize), per content change, per LSP event, and up to 20 Hz while a hover
loads; each redraw runs `draw` while holding the global `state` mutex
(lib.rs:1567-1568). Every cost below is traced to a named caller + frequency.

### Findings

#### 9. Frame-global lock serializes all per-frame work

lib.rs:1567-1568 holds `state` across the whole `terminal.draw`, so every cost
above serializes under one mutex and any background holder (finding 1) blocks
frames. Fix after 1-8: snapshot render inputs outside the lock.

### Minor

- `tmux_navigate` spawns a `tmux` process per nav keystroke (exec.rs:192-198).

### Coverage

Traced to a named frequency: draw-frame path (search label, schema filter,
results grid + query-line highlight, status bar), highlight resubmit path
(keystroke / scroll past half-margin / dialect flip), completion publish path
(~75 ms debounce: buffer_lines + parse_context + pool build + thread filter),
run path (Ctrl+Enter / Ctrl+Shift+Enter), hover path (K), LSP event drain (per
message), executor task (per query: decode off-lock, persist + col-widths under
lock), schema refresh (per load + 1 s stale sweep), db.rs execute/decode (per
query, row-bounded; DuckDB uses spawn_blocking, sqlx paths async). Verified NOT
hot: ddl/safety (per run), completion_ctx (statement scan capped at 64 KB),
persistence load paths (per tab load), sqeel-config (startup), picker/host/ex
(user-initiated or delegated to hjkl crates), splash (startup only). GAPS:
hjkl-engine/buffer/tui internals (buffer render, wrap segments, search regex,
`content_arc` — read the editor.rs API surface, not the buffer render path);
tree-sitter parse costs (could not settle without profiling which of parse vs.
block-ranges walk vs. newline scans dominates on large buffers — same gap as the
06-06 pass); the operative assumption that sqls publishes a diagnostics message
per didChange (standard LSP behaviour, not exercised against a live server);
mouse-move event rates (would inflate every per-frame cost). Test suite NOT run
per task instruction (11 PTY e2e tests fail environmentally in this sandbox; CI
runs them).
