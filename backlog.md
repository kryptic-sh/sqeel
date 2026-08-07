# Backlog

Open work items, findings and deferred decisions. Finished items are deleted
(see `git log` for the record).

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
