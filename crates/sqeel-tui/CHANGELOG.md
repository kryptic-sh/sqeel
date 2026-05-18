# Changelog

All notable changes to this project will be documented in this file. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.4.17] - 2026-05-15

### Added

- **TLS form fields in Add/Edit Connection dialog** (issue #23 phase 1). When
  the URL scheme is `mysql://`, `mariadb://`, `postgres://`, or `postgresql://`,
  four new rows render below the Password field:
  - `CA Cert` — path to PEM-encoded CA root certificate.
  - `Clt Cert` — path to PEM-encoded client certificate (mutual TLS).
  - `Clt Key` — path to client private key.
  - `Verify` — toggle between `Full` (full hostname + chain verification,
    default) and `Skip` (accept any cert, suitable for self-signed dev). Tab
    cycles
    `Name → URL → Password → CaCert → ClientCert → ClientKey → VerifyMode → Name`.
    SQLite and DuckDB URLs continue with the 3-field form. Space / Enter / Left
    / Right toggle the `Verify` chip when focused; contextual hint row shows the
    toggle binding. Backed by `sqeel-core 0.4.13` (form state + sqlx pool TLS)
    and `sqeel-config 0.2.8` (`save_connection` accepts `Option<&TlsConfig>`).

### Changed

- New direct dependency on `sqeel-config` (`0.2`) so the renderer can match on
  `TlsVerifyMode` variants directly. Re-exporting through `sqeel-core` is not
  viable because `sqeel-core::config` only re-exports connection-management
  types, not the TLS enum.

## [0.4.16] - 2026-05-15

### Added

- **DuckDB scheme hint in Add/Edit Connection form.** When the URL field is
  focused, the bottom hint row now shows scheme examples including
  `duckdb::memory:` and `duckdb:/path`. Backed by `sqeel-core 0.4.11`.
  (kryptic-sh/sqeel#27)

## [0.4.15] - 2026-05-15

### Added

- **Password field in Add/Edit Connection dialog.** A third `Password` field is
  rendered below the URL field, masked with `*` characters. Tab now cycles
  `Name → URL → Password → Name`. The password is passed to
  `sqeel_config::save_connection` which stores it in the OS keyring and writes
  the password-stripped URL to TOML. The plaintext-password warning toast now
  only fires when the URL itself contains an inline password (user typed it
  directly) and the Password field was left blank; the hint text now includes
  "use the Password field or run `:migrate-secrets`". (kryptic-sh/sqeel#26)
- **`:migrate-secrets` ex-command.** Walks all connections returned by
  `load_connections`, calls `sqeel_config::migrate_connection_to_keyring` for
  each, and surfaces per-connection toasts (migrated / failed) plus a summary
  toast. Connections with no inline password are silently skipped. On keyring
  failure the connection file is left unchanged and an error toast is shown.
- Bumped `sqeel-core` dep to `"0.4"` (picks up 0.4.10 password-field state).

## [0.4.14] - 2026-05-15

### Fixed

- **`<leader>h` history picker now opens picked query in a fresh scratch tab.**
  Previously `editor.set_content(&query)` clobbered the active editor buffer
  with no tab-bar update, leaving the tab name pointing at the old file while
  the editor displayed the history query. The handler now calls
  `AppState::new_tab_with_content` so the picked query lands in a new
  `scratch_N.sql` tab — tab bar refreshes, previous buffer preserved. Requires
  `sqeel-core` 0.4.9.

### Changed

- **Engine 0.7 migration.** Bumped `hjkl-engine` 0.6 → 0.7, `hjkl-bonsai` 0.5 →
  0.6. Added direct `hjkl-vim = "0.19"` dep. Engine 0.7 removed
  `Editor::handle_key` (and the other crossterm/Input shims) from the editor
  surface — the four call sites here now route through `hjkl_vim::handle_key`
  per the engine 0.7 migration guide. Tracks `hjkl-form 0.3.7`'s caret-minor
  engine-pin bump that dragged two engine majors into any consumer graph still
  on 0.6.
- v0.4.13 was tagged but failed to publish on the two-engine graph; same content
  (UX fix) ships here on engine 0.7.

## [0.4.12] - 2026-05-15

### Added

- **`<leader>h` query history picker.** Opens a fuzzy picker populated from
  `AppState::query_history` (newest entry first). Each row shows the first line
  of the query (truncated to ~60 chars) plus a relative age ("5s ago", "3m ago",
  "2h ago", "4d ago", "2w ago", capped at 52w). Fuzzy matching runs over the
  full query text, not just the label. Selecting an entry replaces the active
  editor buffer content. Esc dismisses without change. Adjacent-duplicate
  deduplication is handled by `push_history` in sqeel-core. (#17)
- `format_relative_time(now, then) -> String` helper (sec/min/hr/day/week
  buckets, 52w cap). Unit-tested at every boundary.

### Changed

- Bumped `sqeel-core` dependency to `0.4` (picks up `HistoryEntry` struct and
  `query_history: Vec<HistoryEntry>` field change in 0.4.6).

### Fixed

- **CI: force fresh rustc on `rust-toolchain.toml` repos.** Added
  `toolchain: stable` to each `actions-rust-lang/setup-rust-toolchain@v1`
  invocation and an explicit `rustup update --no-self-update stable` step.
  `setup-rust-toolchain@v1` reads `channel = "stable"` from
  `rust-toolchain.toml` but reuses the runner's pre-cached rustc (1.94.1),
  causing `cargo` to reject deps that pin `rust-version = "1.95"`. Fix confirmed
  in sqeel-core v0.4.7.

## [0.4.11] - 2026-05-15

### Added

- **`:export csv|json [<path>]` ex-command.** Writes the active results tab to
  disk via the existing `sqeel-core::persistence::export_{csv,json}` backend.
  Bare form (no path) defaults to
  `~/.local/share/sqeel/results/<conn>/<utc_timestamp>.{csv,json}` (parent
  directories created on demand). On success, surfaces an Info toast with the
  row count and resolved path. Tab-completion deferred to engine-level work
  (#53). (#16)
- **`:refreshschema` / `:refresh` ex-command + `<leader>R` binding.** Busts the
  300s schema TTL cache without re-opening the DB pool — clears
  `tables_loaded_at` / `columns_loaded_at` on previously-loaded subtrees and
  re-fires `request_schema_load`. Discoverable from any focus; help text added.
  Backed by the new `AppState::refresh_schema()` in sqeel-core 0.4.5. (#18)
- **`:describe <table>` / `:desc <table>` ex-command.** Dialect-aware column
  schema dump rendered into the active results pane. MySQL: native
  `DESCRIBE \`<table>\``. Postgres: `information_schema.columns`SELECT. SQLite:`PRAGMA
  table_info(<table>)`. Generic / no-connection: error toast. Rejects
  single-quotes in the table name as a defensive guard. (#21)

### Changed

- Bumped `sqeel-core` from 0.4.4 to 0.4.5 (new `refresh_schema` public method).

## [0.4.10] - 2026-05-14

### Changed

- **Migrated to hjkl-engine 0.6 stack.** Engine-0.5 ecosystem snapshot rotted a
  day after release — `hjkl-form 0.3.6`, `hjkl-editor 0.4.5+`,
  `hjkl-picker 0.5.1+`, `hjkl-ratatui 0.3.6` all caret-minor-bumped their engine
  pin to 0.6, dragging engine 0.6 alongside our pinned 0.5 → two engines in
  graph → Input / VimMode mismatches. Bumped `hjkl-engine` from 0.5 to 0.6 with
  caret-minor pins; no source-level API breakage. Requires `sqeel-core = 0.4.4`.
  v0.4.9 was tagged with the cursor fix below but failed to publish on the
  rotted graph; this release ships the same fix on the engine-0.6 graph.

### Fixed

- **Cursor off-by-one for buffers with <10 lines.** Renderer used
  `gutter_width = digits + 2` which under-reserved by one cell vs the engine's
  `Editor::cursor_screen_pos` formula `max(digits + 1, numberwidth)` (vim's
  `numberwidth=4` floor). Cursor landed one column right of where text started.
  Renderer now mirrors the engine formula. Tracks
  [hjkl#96](https://github.com/kryptic-sh/hjkl/issues/96) to replace with
  `editor.lnum_width()` once the engine helper ships.

## [0.4.9] - 2026-05-14

### Fixed

- **Cursor off-by-one for buffers with <10 lines.** Renderer used
  `gutter_width = digits + 2` which under-reserved by one cell vs the engine's
  `Editor::cursor_screen_pos` formula `max(digits + 1, numberwidth)` (vim's
  `numberwidth=4` floor). Cursor landed one column right of where text started.
  Renderer now mirrors the engine formula. Tracks
  [hjkl#96](https://github.com/kryptic-sh/hjkl/issues/96) to replace with
  `editor.lnum_width()` once the engine helper ships.

### Changed

- Loosened over-pinned deps to caret-minor: `sqeel-core`, `hjkl-editor`,
  `hjkl-form`, `hjkl-ratatui`.

## [0.4.8] - 2026-05-13

### Changed

- **Migrated to hjkl-engine 0.5 stack.** Dropped the engine-0.3 lockdown pins
  introduced in v0.4.7. Now that `sqeel-config` (0.2.3) and `sqeel-core` (0.4.3)
  also speak engine 0.5, the dependency graph collapses to a single engine
  version and dependabot can resume caret-minor updates without breaking type
  identity. Bumped: `hjkl-engine` 0.3 → 0.5, `hjkl-editor` 0.4.1 → 0.4.4,
  `hjkl-buffer` 0.3.5 → 0.6, `hjkl-picker` 0.4.0 → 0.5, `hjkl-form` 0.3.3 →
  0.3.5, `hjkl-ratatui` 0.3.3 → 0.3.5. `sqeel-core` pinned to `0.4.3`.

### Fixed

- `PickerLogic::preview` updated to engine-0.5 trait shape (2-tuple
  `(Buffer, String)`; `PreviewSpans` removed upstream).
- `hjkl_buffer::Gutter` gains `numbers` field; `hjkl_buffer::BufferView` gains
  `non_text_style`, `diag_overlays`, `colorcolumn_cols`, `colorcolumn_style`.
- Fully-qualify `hjkl_engine::VimMode` at compare sites that previously used the
  bare `VimMode` (sqeel_core re-exports the same type now).

## [0.4.7] - 2026-05-13

### Fixed

- **Pin hjkl-stack to engine-0.3-compatible versions.** Downstream hjkl crates
  caret-minor-bumped their internal `hjkl-engine` pin to 0.4 / 0.5 without
  bumping their own major, leaking new engine types through their public
  re-exports (`hjkl_form::Input`, `hjkl_form::VimMode`,
  `FormFieldHost::viewport_mut`, etc.). Local builds with cached lockfiles kept
  passing; fresh CI resolution picked the newer transitives and failed with
  type-mismatch + missing-method errors. Pins applied: `hjkl-engine = "=0.3.8"`,
  `hjkl-editor = "=0.4.1"`, `hjkl-buffer = "=0.3.5"`, `hjkl-form = "=0.3.3"`,
  `hjkl-picker = "=0.4.0"`, `hjkl-ratatui = "=0.3.3"`. v0.4.5 (Input/VimMode
  mismatch in form) and v0.4.6 (viewport_mut missing in ratatui) were tagged but
  never published to crates.io; v0.4.7 ships the same feature set with the full
  pin set.

## [0.4.5] - 2026-05-13

### Added

- **`sqls` install prompt modal.** On startup, when the configured `lsp_binary`
  (default `sqls`) is missing from `$PATH` and `editor.lsp_auto_install = true`,
  a centred `[y/N]` modal asks whether to install via `hjkl-anvil`. `y`/`Y`/
  `Enter` triggers the install (same code path as `:Anvil install sqls`);
  `n`/`N`/`Esc` dismisses with the `LSP: sqls missing` banner. Letter keys
  accept both `KeyModifiers::NONE` and `KeyModifiers::SHIFT` so terminals that
  emit uppercase with SHIFT still match. (kryptic-sh/sqeel#14)
- **PATH-aware LSP detection + `:Anvil` / `:LspInfo` ex-commands.** Resolves
  `editor.lsp_binary` via `which` first — found binaries are used untouched.
  Missing binaries surface the modal (above) when `lsp_auto_install = true`. New
  ex-commands mirror the hjkl convention: `:Anvil` (usage hint),
  `:Anvil install <name>`, `:Anvil uninstall <name>`, `:Anvil update [name]`,
  `:LspInfo` (reports state + source `PATH` vs `anvil` + binary path).
  Background `InstallPool` runs `go install` for sqls; terminal status clears
  `active_install` so subsequent installs aren't blocked; `Installing` toast is
  debounced. (kryptic-sh/sqeel#13)
- **Comment-marker highlights via `hjkl-bonsai::CommentMarkerPass`.** TODO /
  FIXME / NOTE / WARN / X / XTODO markers in SQL comments now highlight via the
  shared bonsai pass instead of a bespoke overlay. Theme slots pick up the
  `comment.marker.{kind}` and `comment.marker.tail.{kind}` captures. Trailing
  text after each marker renders distinctly. (kryptic-sh/sqeel#8)
- **Cursor-line + cursor-column highlight options.** `:set cursorline` /
  `:set cursorcolumn` (and aliases `:set cul` / `:set cuc`) plus the new
  `cursorline` / `cursorcolumn` TOML knobs. Both default to `false` — previously
  the highlights were always on. `:set <opt>=on|off|true|false|...`
  value-assign, `:set <opt>!` toggle, and `:set <opt>?` query forms supported;
  query result surfaces via an Info toast and suppresses the engine's bare-set
  dump. The cursor-column highlight uses a dedicated theme slot
  (`sql_cursor_column_bg`) distinct from the cursor-line slot.
  (kryptic-sh/sqeel#10)

### Fixed

- Cursor-line blend on comment markers was forcing `comment.marker.*` fg to
  match the cursor-line bg, hiding the marker text under the cursor. Blend
  dropped — markers stay readable on the active line.

### Changed

- New deps: `hjkl-anvil = "0.2"`, `which = "7"`.
- `sqeel-config` 0.2.1 → 0.2.2 (picks up `cursorline` / `cursorcolumn` /
  `lsp_auto_install` config knobs).

## [0.4.4] - 2026-05-07

### Changed

- CI: collapsed `ci.yml` + `release.yml` + `_tests.yml` into a single `ci.yml`;
  added dependabot config for Cargo and GitHub Actions (weekly).

## [0.4.3] - 2026-05-06

### Added

- Tmux/SSH-friendly alternate bindings for query execution: `<leader><CR>` runs
  the statement under the cursor (alt for `Ctrl+Enter`) and `<leader><Tab>` runs
  all statements in the file (alt for `Ctrl+Shift+Enter`). The modifier+Enter
  combos rely on terminal protocols (modifyOtherKeys / CSI-u) that don't pass
  through tmux passthrough cleanly; the leader-chord variants use plain bytes
  that transmit over any pipe.

### Changed

- Internal deduplication against upstream hjkl crates: the local `spinner`
  module (verbatim copy of `hjkl_ratatui::spinner`) has been removed and call
  sites now delegate directly to `hjkl_ratatui::spinner::frame`. The
  comment-marker overlay (`find_comment_markers`, `apply_marker_overlay`,
  `seed_active_color`, et al.) was evaluated against
  `hjkl_bonsai::CommentMarkerPass` but not migrated — the bonsai API operates on
  `HighlightSpan` byte ranges requiring a theme resolver step, whereas sqeel
  applies colour after ratatui span construction; the shapes do not compose
  without a larger refactor (tracked in
  [kryptic-sh/sqeel#8](https://github.com/kryptic-sh/sqeel/issues/8)).
- Internal: extracted `run_statement_under_cursor` and `run_all_statements` free
  fns from the four near-identical handler bodies (`Ctrl+Enter`,
  `Ctrl+Shift+Enter`, `<leader><CR>`, `<leader><Tab>`). Net -73 lines in
  `lib.rs`; behaviour byte-identical.

## [0.4.2] - 2026-05-06

### Added

- Startup splash screen powered by `hjkl-splash`. On TUI launch a block-cursor
  animation traces the `sqeel` letterforms until the user presses any key. Hint
  text below the art shows `:e <file>`, `:c <conn>`, and `:q` as quick-start
  reminders.
- `pub fn sqeel_tui::run(state, show_splash: bool)` free function — callers that
  want fine-grained control over the splash (e.g. `--no-splash`) use this
  instead of going through `UiProvider::run`.

### Changed

- **`hjkl-splash` 0.1 → 0.2.** Adopts the clock-owned `Splash` API: removes
  manual `advance()` calls, drops the public `tick` field on `SqeelStartScreen`,
  and unmuts the splash binding. Animation cadence is now 120ms (8 Hz) driven by
  `Splash`'s internal wall clock; redraw cadence stays at the event-poll rate
  (50ms / 20 Hz). Decoupling them fixes a class of bugs where high-frequency
  events would starve the timeout branch and freeze the animation.

## [0.4.1] - 2026-05-05

### Changed

- Bumps `sqeel-core` 0.3 → 0.4 (picks up the connection-storage move,
  theme-loader path resolution, and `AsyncGrammarLoader` adoption).
- Render loop now uses `Highlighter::new_async()` + per-tick `try_upgrade()` for
  SQL syntax highlighting. The first-frame freeze on a fresh grammar install is
  gone: the editor renders plain text until the background
  `hjkl_bonsai::AsyncGrammarLoader` finishes the clone + compile, then switches
  to full highlighting automatically on the next tick. Three call sites updated:
  the main highlighter constructed in `run_loop`, plus the thread-local helpers
  in `highlight_sql_lines` and `highlight_query_line`.
- Theme loader (`theme.rs`) now resolves `theme.toml` through
  `sqeel_core::config::config_dir()` (the sqeel-config-backed central path
  resolver) instead of calling `dirs::config_dir()` directly. Two effects: the
  sandbox override (`--sandbox`) now applies to theme loading like it does for
  every other config file, and the `dirs` crate is dropped from `sqeel-tui`'s
  direct deps (still pulled transitively via `sqeel-core`).

## [0.4.0] - 2026-05-05

### Added

- Connection state badge in the connection switcher. Each row now shows a
  colored glyph reflecting the live state of the active connection: `●` green
  for a live connection, `◌` yellow while the handshake is in flight, `✗` red
  when the last attempt failed. Non-active connections show a blank badge (no
  tracked state this session). The active connection name is bolded so it
  remains identifiable when the cursor is on a different row. Replaces the old
  `*` prefix.

### Changed

- **`hjkl-picker` 0.3 → 0.4.** `PickerAction` reduced to
  `Custom(Box<dyn Any + Send>) | None`; app-specific variants removed. sqeel-tui
  now defines a local `SqeelFileAction::OpenPath` enum and wraps `FileSource` in
  a `SqeelFileSource` adapter so `select()` emits
  `PickerAction::Custom(Box::new(SqeelFileAction::OpenPath(path)))`. The
  dispatch arm downcasts on receive. Behavior preserved.
- **`hjkl-editor` 0.3 → 0.4.** `ExEffect::Substituted` now carries
  `lines_changed: usize` alongside `count`. The substitute toast now renders
  vim-accurate `"N substitution(s) on M line(s)"` (e.g.
  `"1 substitution on 1 line"`, `"3 substitutions on 2 lines"`). Old text was
  `"N substitution(s)"`.
- **`hjkl-clipboard` 0.4 → 0.5.** Additive-only upgrade. New public `Backend`
  trait, `BackendKind` enum, `Capabilities` bitflags, async variants
  (`set_async` / `get_async` / `clear_async` / `available_async`),
  `Clipboard::with_backend`, `Clipboard::kind()`, and
  `Clipboard::capabilities()` are now available. sqeel-tui's clipboard usage
  (text `set`/`get` on `Selection::Clipboard`) is unchanged; no call-site
  updates were required.
- **`hjkl-bonsai` 0.3 → 0.5.** Picks up the 0.4 `ManifestMeta` loader API and
  the 0.5 `highlight_range_with_injections` viewport-scoped method. The
  `InputEdit` / `Point` re-exports used here are unchanged.

## [0.3.0] - 2026-05-03

### Changed

- **`hjkl-bonsai` 0.2 → 0.3.** Grammar storage subdir renamed `hjkl/` →
  `bonsai/`; macOS/Windows now follow XDG-everywhere. Existing grammars re-fetch
  on first use. See `crates/hjkl-bonsai/CHANGELOG.md` for detail.
- **`sqeel-core` 0.2 → 0.3.** Pulls in the breaking `leader_key` `String` →
  `char` change + the hjkl-config 0.2 path migration. See
  `crates/sqeel-core/CHANGELOG.md`.

## [0.2.4] - 2026-05-03

### Changed

- Replaced sqeel's bespoke `FilePicker` (subsequence fuzzy scorer + custom
  dialog) with `hjkl-picker`. The leader+space picker now enumerates every saved
  `.sql` buffer in `~/.local/share/sqeel/queries/` via
  `hjkl_picker::FileSource`, ranked by `hjkl_picker::score`. Selecting a file
  opens it as a tab if not already open; otherwise switches to the existing tab.
  Match positions render with the editor search-bg highlight.

### Removed

- `FilePicker` struct + `fuzzy_score` helper (~50 lines) — superseded by
  `hjkl_picker::Picker` + built-in `FileSource`.

## [0.2.3] - 2026-05-03

### Changed

- Bumped `hjkl-bonsai` 0.1 → 0.2. Only `InputEdit` / `Point` re-exports are
  consumed here; sqeel-tui's behaviour is unchanged. The runtime grammar
  pipeline is exercised through `sqeel-core` 0.2.3.

## [0.2.2] - 2026-05-03

### Changed

- `deny.toml`: allow `CDLA-Permissive-2.0` (transitive via webpki-roots, through
  sqeel-core) and ignore RUSTSEC-2023-0071 (rsa Marvin attack — transitive via
  sqlx-mysql, no fix available).
- CI: extracted shared lint/test jobs (`fmt`, `clippy`, `test`, `deny`) into a
  reusable `_tests.yml` workflow called by both `ci.yml` and `release.yml`.

## [0.2.1] - 2026-05-03

### Changed

- Migrated `sqeel-tui` from the `kryptic-sh/sqeel` monorepo into its own
  repository ([kryptic-sh/sqeel-tui](https://github.com/kryptic-sh/sqeel-tui))
  with full git history preserved.
- Bumped hjkl deps from 0.2 to 0.3 (`hjkl-engine`, `hjkl-buffer`, `hjkl-editor`,
  `hjkl-form`, `hjkl-ratatui`) and `hjkl-clipboard` 0.2 → 0.4.
- Replaced removed `hjkl-tree-sitter` with `hjkl-bonsai` 0.1.
- Bumped `ratatui` 0.29 → 0.30 and `crossterm` 0.28 → 0.29.
- Adapted to `hjkl-clipboard` 0.4 generic API (`set`/`get` with `Selection` +
  `MimeType`; `Clipboard::new` now returns `Result`).
- Adapted to `hjkl_buffer::Gutter` new `line_offset` field.
- `sqeel-core` resolved from crates.io (was sibling path dep).
- Loosened dep pins from `=0.X.Y` exact to `"0.X"` caret-minor.

### Added

- Standalone `LICENSE`, `.gitignore`, `deny.toml`, `rust-toolchain.toml`, and CI
  workflows at the repo root.

[Unreleased]: https://github.com/kryptic-sh/sqeel-tui/compare/v0.4.17...HEAD
[0.4.17]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.17
[0.4.16]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.16
[0.4.15]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.15
[0.4.14]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.14
[0.4.13]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.13
[0.4.12]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.12
[0.4.11]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.11
[0.4.10]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.10
[0.4.9]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.9
[0.4.8]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.8
[0.4.7]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.7
[0.4.6]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.6
[0.4.5]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.5
[0.4.4]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.4
[0.4.3]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.3
[0.4.2]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.2
[0.4.1]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.1
[0.4.0]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.4.0
[0.3.0]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.3.0
[0.2.4]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.2.4
[0.2.3]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.2.3
[0.2.2]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.2.2
[0.2.1]: https://github.com/kryptic-sh/sqeel-tui/releases/tag/v0.2.1
