# Changelog

All notable changes to this project will be documented in this file. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.4.13] - 2026-05-15

### Added

- **TLS support in `DbConnection::connect`** (issue #23 phase 1). Signature
  changes from `connect(url: &str)` to
  `connect(url: &str, tls: Option<&TlsConfig>)`. When `Some`, MySQL and Postgres
  pools are built via `connect_with` on `MySqlConnectOptions` /
  `PgConnectOptions` with `ssl_mode`, `ssl_ca` / `ssl_root_cert`,
  `ssl_client_cert`, `ssl_client_key` mapped from the supplied
  `sqeel_config::TlsConfig`. SQLite and DuckDB silently ignore `tls` (no network
  TLS). `TlsVerifyMode::Full` maps to `VerifyIdentity` (MySQL) / `VerifyFull`
  (Postgres); `Skip` maps to `Required` (MySQL) / `Require` (Postgres). Absent
  (`None`) → driver defaults apply (same as before).
- **TLS form-state machinery in `AppState`** (issue #23 phase 1, sqeel-tui
  rendering follows). New `AddConnectionField` variants: `CaCert`, `ClientCert`,
  `ClientKey`, `VerifyMode`. New `AppState` fields: `add_connection_ca_cert` /
  `_cursor`, `add_connection_client_cert` / `_cursor`,
  `add_connection_client_key` / `_cursor`, `add_connection_verify_mode`
  (defaults to `Full`). Tab cycle includes the TLS fields only for
  mysql/mariadb/postgres URLs (`url_supports_tls` helper).
  `open_edit_connection` populates fields from the existing
  `ConnectionConfig.tls`. `save_new_connection` builds a `TlsConfig` from the
  form state (or `None` when all paths empty + `Full` mode) and threads it
  through to `sqeel_config::save_connection`.

### Changed

- Bumped `sqeel-config` dep to `0.2` (resolves to 0.2.8 for the new
  `save_connection(name, url, password, tls)` signature).

## [0.4.12] - 2026-05-15

### Fixed

- **Windows linker error with DuckDB feature.** Added `build.rs` that emits
  `cargo:rustc-link-lib=rstrtmgr` on Windows when the `duckdb` feature is
  enabled. DuckDB's bundled build references `RmStartSession` / `RmEndSession` /
  `RmRegisterResources` / `RmGetList` (Windows Restart Manager) but
  `libduckdb-sys` does not add `rstrtmgr.lib` to the link set automatically.

## [0.4.11] - 2026-05-15

### Added

- **DuckDB backend** (`duckdb` feature, default-on). `Pool` gains a
  `DuckDb(Arc<Mutex<duckdb::Connection>>)` variant backed by `duckdb = "1"` with
  the `bundled` feature so no system library is required. URL schemes:
  `duckdb::memory:` (in-memory) and `duckdb:/path/to/file.duckdb` (file). Schema
  introspection uses `information_schema.tables` / `.columns`. CSV and Parquet
  files are readable out of the box via DuckDB's auto-loaders
  (`SELECT * FROM read_csv_auto('file.csv')`). Disabling the feature compiles
  cleanly and returns `ConnectErrorKind::Config` for `duckdb:` URLs.
  `is_duckdb()` added alongside the existing `is_sqlite()`.
  (kryptic-sh/sqeel#27)
- `validate_connection_url` in `state.rs` now accepts `duckdb:` URLs
  (`duckdb::memory:` and `duckdb:/path` forms, no `//` required).

## [0.4.10] - 2026-05-15

### Added

- **Password field in connection form state.** `AppState` gains
  `add_connection_password: String` and `add_connection_password_cursor: usize`.
  `AddConnectionField` enum gains the `Password` variant; Tab now cycles
  `Name → Url → Password → Name`. `open_add_connection`, `open_edit_connection`,
  and `close_add_connection` clear the new fields on every open/close.
- **Keyring-aware save.** `save_new_connection` extracts the password from the
  new field and passes it as `Option<&str>` to `sqeel_config::save_connection`,
  which handles keyring storage and URL stripping. The old plaintext-password
  warning toast now only fires when the URL itself contains an inline password
  and the Password field was left blank; the message includes a
  `:migrate-secrets` hint. (kryptic-sh/sqeel#26)
- Bumped `sqeel-config` dependency to `"0.2"` (picks up 0.2.6 keyring APIs).

## [0.4.9] - 2026-05-15

### Changed

- Bumped `hjkl-engine` dependency from 0.6 to 0.7. Re-exported
  `{KeybindingMode, VimMode}` now resolve to engine 0.7. Tracks the engine churn
  — `hjkl-form 0.3.7` caret-minor-bumped its engine pin to 0.7, dragging two
  engine majors into any consumer graph still on 0.6. Requires
  `sqeel-config = 0.2.5`.

## [0.4.8] - 2026-05-15

### Added

- **`AppState::new_tab_with_content(content: String)`** — fresh scratch tab
  pre-seeded with the given content (disk + memory + `tab_content_pending`
  consistent). Backs sqeel-tui's `<leader>h` history-picker UX fix: a picked
  history entry now lands in its own tab instead of clobbering the active buffer
  with no tab-bar update. `new_tab()` is now a thin wrapper around
  `new_tab_with_content(String::new())`.

## [0.4.7] - 2026-05-15

### Fixed

- **CI: pin `toolchain: stable` + add `rustup update --no-self-update stable`
  step after `actions-rust-lang/setup-rust-toolchain@v1`.** The new action reads
  the repo's `rust-toolchain.toml` (`channel = "stable"`) but reuses the
  runner's pre-cached rustc 1.94.1 — `cargo` then rejects building against deps
  that pin `rust-version = "1.95"`. The explicit `rustup update` step forces a
  fresh stable install. v0.4.6 was tagged but failed to publish for this reason;
  same content ships here.

## [0.4.6] - 2026-05-15

### Changed

- **`query_history` field type changed from `Vec<String>` to
  `Vec<HistoryEntry>`**. `HistoryEntry { query: String, timestamp: SystemTime }`
  pairs each recorded query with a wall-clock timestamp. `push_history` stamps
  `SystemTime::now()` on insertion; `history_prev` / `history_next` return
  `Option<&str>` over `entry.query` — external call-site contract unchanged.
  Backs the `<leader>h` history picker in sqeel-tui. (#17)

## [0.4.5] - 2026-05-15

### Added

- `AppState::refresh_schema()` — busts the schema TTL cache and re-fires
  `request_schema_load` for previously-loaded subtrees without re-opening the DB
  pool. Returns `true` when a refresh was queued, `false` when no connection is
  active. Backs sqeel-tui's `:refreshschema` / `<leader>R` binding. (#18)

## [0.4.4] - 2026-05-14

### Changed

- Bumped `hjkl-engine` from 0.5 to 0.6. Re-exported `{KeybindingMode, VimMode}`
  now resolve to engine 0.6. Tracks the engine 0.6 rollout in the hjkl
  ecosystem; rotted 0.5 snapshot caused two-engines-in-graph builds. Requires
  `sqeel-config = 0.2.4`.

## [0.4.3] - 2026-05-13

### Changed

- Bumped `hjkl-engine` from 0.3 to 0.5 and `hjkl-bonsai` from 0.5 to 0.6.
  Re-exported `{KeybindingMode, VimMode}` now resolve to engine 0.5, collapsing
  the dependency graph for downstream consumers that previously straddled engine
  majors. No source-level API change — engine 0.5's variants and bonsai 0.6's
  runtime module are API-compatible with the previous versions.
- `sqeel-config` dependency bumped to `0.2.3` (engine-0.5).

## [0.4.2] - 2026-05-13

### Changed

- **LSP client ported to the shared `hjkl-lsp` crate.** Replaced 796 LOC of
  hand-rolled codec / server-lifecycle / text-sync plumbing with a 253-LOC thin
  adapter over `hjkl_lsp::LspManager`. Public surface unchanged — `LspClient`,
  `LspWriter`, `LspEvent`, `Diagnostic`, `write_sqls_config` keep identical
  signatures; consumers (`sqeel-tui`) recompile untouched. (kryptic-sh/sqeel#12)
- Internal `DidChangeQueue` dispatcher tests dropped — the queue is now
  hjkl-lsp's responsibility. All public-API tests retained.

### Added

- `hjkl-bonsai` `CommentMarkerPass` integration: TODO / FIXME / NOTE / WARN / X
  markers in SQL comments now highlight via the shared bonsai pass instead of a
  bespoke overlay. (kryptic-sh/sqeel#8)
- README module table and key-type reference.

### Fixed

- Clippy `collapsible_if` warnings in the new LSP adapter.

## [0.4.1] - 2026-05-07

### Changed

- CI: collapsed `ci.yml` + `release.yml` + `_tests.yml` into a single `ci.yml`;
  added dependabot config for Cargo and GitHub Actions (weekly).

## [0.4.0] - 2026-05-05

### Added

- **`sql_grammar_blocking() -> anyhow::Result<Arc<Grammar>>`** — new public
  function for sync callers (tests, CLI one-shots). Wraps the async loader with
  `recv_blocking`; caches the result in the shared `SQL_GRAMMAR` static.
- **`Highlighter::new_async() -> Highlighter`** — constructs a `Highlighter`
  immediately without blocking. When the grammar is not yet ready, all highlight
  calls return empty spans (plain-text fallback). The grammar attaches on the
  first successful `try_upgrade()` tick.
- **`Highlighter::try_upgrade(&mut self)`** — polls the non-blocking grammar
  cache each tick and attaches the inner `hjkl_bonsai::Highlighter` once the
  background load resolves. No-op once the highlighter is ready.
- **`Highlighter::is_ready() -> bool`** — returns whether the inner highlighter
  is attached.
- Added `tracing` dependency for warn-level diagnostics on async load failures.

### Changed

- **`sql_grammar()` signature changed from `anyhow::Result<Arc<Grammar>>` to
  `Option<Arc<Grammar>>`** (breaking for direct callers). Returns `None` on
  first call while the background grammar load is in-flight; returns `Some(arc)`
  once the grammar is cached. Callers that need a `Result` should use
  `sql_grammar_blocking()` instead.
- **`Highlighter::inner` is now `Option<hjkl_bonsai::Highlighter>`**. All
  highlight/parse methods short-circuit gracefully to empty vecs when `None`.
  `Highlighter::new()` continues to block (calls `sql_grammar_blocking`) and is
  still appropriate for tests. `Highlighter::default()` now uses `new_async()`
  instead of `new()`.
- Process-wide async grammar loader (`AsyncGrammarLoader`) stored in
  `OnceLock<AsyncGrammarLoader>`; in-flight handle in
  `Mutex<Option<LoadHandle>>`. Once the grammar resolves, the handle is cleared
  and subsequent calls fast-return from the `SQL_GRAMMAR` cache.
- `MainConfig`, `EditorConfig`, `load_main_config`, `config_dir`, and
  `set_config_dir_override` extracted into the new `sqeel-config` crate.
  Re-exported from `sqeel_core::config` so all existing call sites are
  unaffected. `sqeel-config` is now a direct dependency.
- `ConnectionConfig`, `load_connections`, `save_connection`, and
  `delete_connection` moved to `sqeel-config`. Re-exported from
  `sqeel_core::config` — all call sites in `sqeel-core::state`, `sqeel-tui`, and
  `apps/sqeel` are unaffected.

## [0.3.1] - 2026-05-05

### Changed

- **`hjkl-bonsai` 0.3 → 0.5.** Migrated the two 0.4 breaking call sites in
  `sql_grammar()`: `GrammarLoader::user_default` and `Grammar::load` now receive
  `registry.meta()` (`&ManifestMeta`). No `LangSpec` is constructed manually and
  sqeel ships no `bonsai.toml`, so those breaking changes had no further impact.
  The 0.5 `highlight_range_with_injections` API is available for future
  viewport-scoped adoption; `sqeel-core`'s `Highlighter` wrapper continues to
  use `highlight_range` internally, which is correct for its caller-driven parse
  model.

## [0.3.0] - 2026-05-03

### Changed

- **`hjkl-bonsai` 0.2 → 0.3.** Grammar storage subdir renamed `hjkl/` →
  `bonsai/`, and macOS/Windows now follow XDG-everywhere instead of
  `~/Library/Application Support` / `%APPDATA%`. Existing grammars under the old
  paths are not migrated — sqeel re-fetches and re-compiles them into the new
  `~/.local/share/bonsai/grammars/` location on first use.
- **Config loading migrated to `hjkl-config` 0.2.** Defaults now live in
  `crates/sqeel-core/src/config.toml`, bundled via `include_str!()` and parsed
  at runtime as the single source of truth. The user file at
  `<config_dir>/config.toml` is **deep-merged** on top via
  `hjkl_config::load_layered_from` — only the fields you want to override need
  to appear there. Unknown keys are an error.
- Dropped `DEFAULT_CONFIG` const, `default_*` helper fns, and per-field
  `#[serde(default = "...")]` attrs (no default values live in Rust code
  anymore). `MainConfig::default()` parses the bundled TOML.
- `MainConfig` and `EditorConfig` are now `#[serde(deny_unknown_fields)]`.
- `load_main_config` no longer auto-writes a default config file when one is
  missing — returns bundled defaults in memory instead. Use
  `hjkl_config::write_default` explicitly if a starter file is needed.
- `load_main_config` now validates the merged config and returns an error if
  `editor.lsp_binary` is empty or `editor.mouse_scroll_lines` is `0`. Multi-char
  or empty `leader_key` values are caught at parse time by serde's `char`
  deserializer (see breaking change below).
- **Breaking:** `EditorConfig::leader_key` is now `char` (was `String`). Made
  the invariant unrepresentable at the type level — TOML strings of length != 1
  fail to deserialize with a span-aware error from hjkl-config. Existing user
  files keep working: `leader_key = " "` still parses (a single space is a valid
  `char`). Multi-char strings like `"ab"` now fail at parse time rather than
  being silently truncated or surfaced via a runtime validation message.
- **XDG-everywhere path migration on macOS/Windows.** Both `config_dir()` and
  `data_dir()` (the latter in `persistence.rs`) now route through hjkl-config
  0.2's XDG-everywhere resolver. Linux paths are unchanged (`~/.config/sqeel/`,
  `~/.local/share/sqeel/`). macOS moves from
  `~/Library/Application Support/sqeel/` to `~/.config/sqeel/` (config) and
  `~/.local/share/sqeel/` (data). Windows moves from `%APPDATA%\sqeel\` to
  `~/.config/sqeel/` and `~/.local/share/sqeel/`. Existing macOS/Windows users
  will need to move their `config.toml`, `conns/`, `session.toml`, and
  `queries/`/`results/` data to the new locations. `$XDG_CONFIG_HOME` and
  `$XDG_DATA_HOME` are now honored on every platform — no more per-platform
  conditionals in dotfile setups. Sandbox override (`set_config_dir_override` /
  `set_data_dir_override`) for `--sandbox` is preserved.

### Added

- `hjkl-config = "0.2"` dependency.
- `MainConfig` impls `hjkl_config::AppConfig` (`APPLICATION = "sqeel"`) and
  `hjkl_config::Validate` (with `ValidationError` as the associated error). The
  `Validate` hook composes the shared `ensure_non_empty_str` / `ensure_non_zero`
  helpers so error messages carry field names.
- `pub const DEFAULTS_TOML: &str` — exposes the bundled defaults string for
  downstream tooling.
- 11 new tests covering bundled-defaults parse, partial user overrides
  preserving defaults, unknown-key rejection, validation boundaries (empty
  `lsp_binary`, zero `mouse_scroll_lines`), and parse-level rejection of
  multi-char / empty / unicode-single-char `leader_key`.

## [0.2.3] - 2026-05-03

### Changed

- Migrated to `hjkl-bonsai` 0.2 (was 0.1). 0.2 swaps the bundled-grammar
  registry for a runtime `dlopen`-based loader; first invocation for an unseen
  language clones `derekstride/tree-sitter-sql` and compiles `parser.so` into
  the user's XDG data dir. Subsequent runs hit the cache.
- Cache the resolved `Arc<Grammar>` in a process-wide `Mutex<Option<...>>` so
  concurrent `Highlighter::new()` callers (parallel tests, multi-buffer editor
  sessions) share one dlopen handle instead of racing.

## [0.2.2] - 2026-05-03

### Changed

- `deny.toml`: allow `CDLA-Permissive-2.0` (transitive via webpki-roots) and
  ignore RUSTSEC-2023-0071 (rsa Marvin attack — transitive via sqlx-mysql, no
  fix available, threat model is remote timing).
- CI: extracted shared lint/test jobs (`fmt`, `clippy`, `test`, `deny`) into a
  reusable `_tests.yml` workflow called by both `ci.yml` and `release.yml`.

## [0.2.1] - 2026-05-03

### Changed

- Migrated `sqeel-core` from the `kryptic-sh/sqeel` monorepo into its own
  repository ([kryptic-sh/sqeel-core](https://github.com/kryptic-sh/sqeel-core))
  with full git history preserved.
- Bumped hjkl deps from 0.2 to 0.3 (`hjkl-engine`).
- Replaced removed `hjkl-tree-sitter` with `hjkl-bonsai` 0.1 for syntax
  highlighting.
- Loosened dep pins from `=0.X.Y` exact to `"0.X"` caret-minor, matching the
  hjkl-\* sibling pattern.

### Added

- Standalone `LICENSE`, `.gitignore`, `deny.toml`, `rust-toolchain.toml`, and CI
  workflows at the repo root.

[Unreleased]: https://github.com/kryptic-sh/sqeel-core/compare/v0.4.13...HEAD
[0.4.13]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.4.13
[0.4.12]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.4.12
[0.4.11]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.4.11
[0.4.11]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.4.11
[0.4.10]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.4.10
[0.4.9]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.4.9
[0.4.8]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.4.8
[0.4.7]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.4.7
[0.4.6]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.4.6
[0.4.5]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.4.5
[0.4.4]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.4.4
[0.4.3]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.4.3
[0.4.2]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.4.2
[0.4.1]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.4.1
[0.4.0]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.4.0
[0.3.1]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.3.1
[0.3.0]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.3.0
[0.2.3]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.2.3
[0.2.2]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.2.2
[0.2.1]: https://github.com/kryptic-sh/sqeel-core/releases/tag/v0.2.1
