# Changelog

All notable changes to this project will be documented in this file. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This
project adheres to [Semantic Versioning](https://semver.org/) once it reaches
0.1.0; the 0.0.x series is a churn phase where breaking changes may land on
patch bumps.

## [Unreleased]

## [0.4.12] - 2026-05-15

### Added

- **`:export csv|json [<path>]` ex-command** (via sqeel-tui v0.4.11). Writes the
  active results tab to disk via the existing
  `sqeel-core::persistence::export_{csv,json}` backend. Bare form (no path)
  defaults to `~/.local/share/sqeel/results/<conn>/<utc_timestamp>.{csv,json}`.
  Tab-completion deferred to engine-level work (#53). (#16)
- **`:refreshschema` / `:refresh` ex-command + `<leader>R` binding** (via
  sqeel-tui v0.4.11 + sqeel-core v0.4.5). Busts the 300s schema TTL cache
  without re-opening the DB pool. Backed by the new `refresh_schema()` public
  method on `AppState`. (#18)
- **`:describe <table>` / `:desc <table>` ex-command** (via sqeel-tui v0.4.11).
  Dialect-aware column schema dump rendered into the active results pane. MySQL:
  native `DESCRIBE`. Postgres: `information_schema.columns`. SQLite:
  `PRAGMA table_info`. Rejects single-quotes in the table name. (#21)

### Changed

- Submodule bumps: `sqeel-core` 0.4.4 → 0.4.5 (new `refresh_schema` public
  method); `sqeel-tui` 0.4.10 → 0.4.11 (three new ex-commands above).

## [0.4.11] - 2026-05-14

### Changed

- **Engine-0.5 → 0.6 migration train across the sqeel stack.** The engine-0.5
  hjkl ecosystem snapshot rotted within a day of v0.4.10 — `hjkl-form 0.3.6`,
  `hjkl-editor 0.4.5+`, `hjkl-picker 0.5.1+`, `hjkl-ratatui 0.3.6` all
  caret-minor-bumped their engine pin to 0.6 without majoring themselves,
  dragging engine 0.6 alongside any consumer pinned to 0.5 → two engines in
  graph → `Input` / `VimMode` mismatches. Bumps all three submodule pointers in
  lockstep so the graph holds exactly one engine major:
  - `sqeel-config` 0.2.3 → 0.2.4 (hjkl-engine 0.5 → 0.6).
  - `sqeel-core` 0.4.3 → 0.4.4 (hjkl-engine 0.5 → 0.6).
  - `sqeel-tui` 0.4.8 → 0.4.10 (hjkl-engine 0.5 → 0.6; full hjkl stack on
    engine-0.6-compatible caret-minor pins; no source-level API breakage).
- v0.4.9 (umbrella) and sqeel-tui v0.4.9 were tagged on the rotted graph and did
  not publish; this release ships the equivalent fix on engine 0.6.

### Fixed

- **Cursor off-by-one for buffers with <10 lines** (via sqeel-tui v0.4.10).
  Renderer used `gutter_width = digits + 2` which under-reserved by one cell vs
  the engine's `Editor::cursor_screen_pos` formula
  `max(digits + 1, numberwidth)` (vim's `numberwidth=4` floor). Cursor landed
  one column right of where text started. Tracks
  [hjkl#96](https://github.com/kryptic-sh/hjkl/issues/96) to replace with
  `editor.lnum_width()` once the engine helper ships.

## [0.4.10] - 2026-05-13

### Changed

- **Engine-0.5 migration across the sqeel stack.** Bumps all three submodule
  pointers in lockstep so the dependency graph holds exactly one `hjkl-engine`
  major:
  - `sqeel-config` 0.2.2 → 0.2.3 (hjkl-engine 0.3 → 0.5).
  - `sqeel-core` 0.4.2 → 0.4.3 (hjkl-engine 0.3 → 0.5, hjkl-bonsai 0.5 → 0.6).
  - `sqeel-tui` 0.4.7 → 0.4.8 (drops engine-0.3 lockdown pins; full hjkl stack
    on engine-0.5-compatible versions).
- Resolves the caret-minor rot that forced exact pins in v0.4.9 and was keeping
  dependabot bumps perma-red.

## [0.4.9] - 2026-05-13

### Fixed

- `sqeel-tui` 0.4.6 added partial hjkl-stack pins to fix the engine-version
  conflict but missed `hjkl-ratatui`. v0.4.7 adds `hjkl-ratatui = "=0.3.3"` so
  the full pin set lands and fresh CI builds resolve to a single `hjkl-engine`
  0.3.8 in the graph.

## [0.4.8] - 2026-05-13

### Fixed

- `sqeel-tui` 0.4.5 was tagged but never published — fresh CI dep resolution
  picked up `hjkl-form` 0.3.5 / `hjkl-editor` 0.4.4 which silently caret-minor
  bumped to `hjkl-engine` 0.5, breaking the type contract. `sqeel-tui` 0.4.6
  pins the hjkl-stack to `hjkl-engine = "=0.3.8"`, `hjkl-editor = "=0.4.1"`,
  `hjkl-buffer = "=0.3.5"`, `hjkl-form = "=0.3.3"`, `hjkl-picker = "=0.4.0"`.
  Bump these together when migrating to hjkl-engine 0.5.

## [0.4.7] - 2026-05-13

### Added

- `sqls` auto-install via `hjkl-anvil` with PATH-aware detection. On startup
  sqeel resolves `editor.lsp_binary` (default `sqls`) via `which` — if present,
  uses it untouched. If missing and the new `editor.lsp_auto_install = true`
  (default) config knob is on, a modal `[y/N]` prompt asks the user to install
  via `hjkl-anvil`. The ex-commands `:Anvil` / `:Anvil install <name>` /
  `:Anvil uninstall <name>` / `:Anvil update [name]` / `:LspInfo` mirror the
  hjkl convention. `:LspInfo` reports whether the active binary came from
  `$PATH` or `hjkl-anvil`. Set `lsp_auto_install = false` to silence the prompt
  (banner only). (#13, #14)

### Changed

- Submodule bumps: `sqeel-config` 0.2.1 → 0.2.2 (cursor opts + lsp_auto_install
  config knobs); `sqeel-tui` 0.4.4 → 0.4.5 (anvil + modal + comment markers +
  cursor opts).

## [0.4.6] - 2026-05-13

### Changed

- LSP client (`sqeel-core::lsp`) ported to the shared `hjkl-lsp` crate. The
  hand-rolled codec / server-lifecycle / text-sync plumbing (796 LOC) is now a
  253-LOC adapter over `hjkl_lsp::LspManager`. Public surface unchanged —
  `LspClient`, `LspWriter`, `LspEvent`, `Diagnostic`, `write_sqls_config` keep
  the same signatures, so `sqeel-tui` consumers are untouched. (#12)
- Cursor-line and cursor-column highlights are now opt-in (`cursorline = false`,
  `cursorcolumn = false` by default in `~/.config/sqeel/config.toml` under
  `[editor]`). Enable via TOML or at runtime via `:set cursorline` /
  `:set cursorcolumn` (aliases `:set cul` / `:set cuc`). Previously both were
  always-on. The cursor-column highlight now uses a dedicated theme slot
  (`sql_cursor_column_bg`) distinct from the cursor-line slot.
- Pinned `mlugg/setup-zig` to zig 0.15.1 to skip `build.zig.zon` lookup and fix
  post-step CI noise.
- Submodule bumps: `sqeel-core` 0.4.1 → 0.4.2 (hjkl-lsp adapter port).

### Fixed

- CI: `cargo-deny` job now checks out submodules — previously it skipped them
  and broke on workspace member resolution when new submodule-resident deps
  landed.

## [0.4.5] - 2026-05-07

### Changed

- CI: collapsed `ci.yml` + `release.yml` into a single `ci.yml` with tag-gated
  release jobs; added dependabot config for Cargo and GitHub Actions (weekly).
  Submodules (`sqeel-config` 0.2.1, `sqeel-core` 0.4.1, `sqeel-tui` 0.4.4) cut
  matching patch releases.

## [0.4.4] - 2026-05-06

### Fixed

- Re-cut of v0.4.3. The v0.4.3 umbrella release failed at the cross-platform
  binary-build step because the tag's submodule pointer for `crates/sqeel-tui`
  was left at the pre-bump SHA (sqeel-tui 0.4.2), while parent `Cargo.lock` had
  already been refreshed against sqeel-tui 0.4.3. CI's `--locked` build refused
  to reconcile. v0.4.4 ships the same content as v0.4.3 with the submodule
  pointer correctly aligned. `sqeel-tui` v0.4.3 published cleanly during the
  v0.4.3 attempt, so the regression is umbrella-only.

### Added

- Tmux/SSH-friendly alternate bindings for query execution: `<leader><CR>` runs
  the statement under the cursor (alt for `Ctrl+Enter`) and `<leader><Tab>` runs
  all statements in the file (alt for `Ctrl+Shift+Enter`). The modifier+Enter
  combos rely on terminal protocols that don't pass cleanly through tmux
  passthrough; the leader-chord variants use plain bytes that transmit over any
  pipe.

### Changed

- `sqeel-tui` 0.4.2 → 0.4.3. Picks up the alternate binds above plus internal
  dedup against upstream hjkl crates (local `spinner` module removed in favor of
  `hjkl_ratatui::spinner::frame`) and a refactor extracting
  `run_statement_under_cursor` / `run_all_statements` free fns from four
  near-identical handler bodies.

## [0.4.2] - 2026-05-06

### Added

- Startup splash screen: on TUI launch the `sqeel` letterform animation (powered
  by `hjkl-splash`) plays until the user presses any key.
- `--no-splash` CLI flag to skip the splash screen.

## [0.4.1] - 2026-05-05

### Added

- **`sqeel-config` extracted to its own repo + submodule**
  ([kryptic-sh/sqeel-config](https://github.com/kryptic-sh/sqeel-config),
  published v0.1.0 → v0.2.0). Hosts `MainConfig`, `EditorConfig`, and the
  connection-storage API on top of `hjkl_config::AppConfig`, re-exported from
  `sqeel-core::config` for backwards compatibility. Mirrors the buffr-config /
  sqeel-core / sqeel-tui standalone-repo pattern: own ci.yml + release.yml,
  depended on by `sqeel-core` from crates.io and patched to local path in the
  umbrella workspace for development.
- **No first-highlight freeze.** The SQL grammar (tree-sitter-sql) is now loaded
  in the background via `hjkl_bonsai::AsyncGrammarLoader`. On a fresh install
  the 1–3 s git clone + `cc` compile no longer blocks the TUI main loop. The
  editor renders in plain text until the grammar resolves, then switches to full
  syntax highlighting automatically on the next render tick.

### Changed

- **Connection storage moved to `sqeel-config`** (`ConnectionConfig`,
  `load_connections`, `save_connection`, `delete_connection`). All sqeel TOML
  I/O now lives in one crate. `sqeel-core` re-exports the symbols, so
  `sqeel-tui` and `apps/sqeel` are unaffected.
- `sqeel-tui` theme loader (`theme.rs`) now resolves `theme.toml` through
  `sqeel_core::config::config_dir()` (the sqeel-config-backed central path
  resolver) instead of `dirs::config_dir()` directly. The `--sandbox` override
  now applies to theme loading too. Drops the direct `dirs` dep from
  `sqeel-tui`.

## [0.4.0] - 2026-05-05

### Changed

- **`sqeel-tui`: `hjkl-editor` 0.3 → 0.4.** Status toast after `:s/…/…/` now
  renders vim-accurate `"N substitutions on M lines"` using the new
  `ExEffect::Substituted { count, lines_changed }` shape. Old text was
  `"N substitution(s)"`.
- **`sqeel-tui`: `hjkl-clipboard` 0.4 → 0.5.** Additive upgrade — public
  `Backend` trait, `Capabilities` bitflags, `BackendKind` enum, and async
  variants land upstream. sqeel-tui's text copy/paste paths are unchanged.
- **`hjkl-bonsai` 0.3 → 0.5** (both `sqeel-core` and `sqeel-tui`). Migrates
  through two major releases: 0.4 introduced `ManifestMeta` as a required
  argument to `GrammarLoader::user_default` and `Grammar::load`; 0.5 adds
  `Highlighter::highlight_range_with_injections` for viewport-scoped
  highlighting. The 0.4 call-site updates are applied; 0.5 adoption is available
  but not yet wired into the render path (SQL grammars don't ship injection
  rules, so the perf win is deferred until injections land).

### Added

- Connection state badge in the connection switcher. Each row now shows a
  colored glyph: `●` green for a live connection, `◌` yellow while the handshake
  is in flight, `✗` red when the last attempt failed. The active connection name
  is bolded. The old `*` prefix is removed.
- Alpine .apk packaging pipeline (release CI builds .apk in `alpine:latest` and
  uploads it as a release asset; install with
  `apk add --allow-untrusted sqeel-*.apk`).
- Homebrew tap auto-publish for `sqeel` on tag push. New
  `pkg/homebrew/sqeel.rb.in` template + `brew-tap` job in `release.yml` renders
  the formula with the just-uploaded macOS sha256s and pushes it to
  `kryptic-sh/homebrew-tap`. Install with `brew install kryptic-sh/tap/sqeel`.

### Removed

- `sqeel-gui` binary and `crates/sqeel-gui` crate removed pending a shared GUI
  adapter layer (`hjkl-editor-gui`, tracked in
  [kryptic-sh/sqeel#3](https://github.com/kryptic-sh/sqeel/issues/3)). The
  previously published `sqeel-gui` crate on crates.io stays frozen at 0.3.0 and
  will not be re-published from this state.

## [0.3.0] - 2026-05-03

### Changed

- **`sqeel-core` 0.2 → 0.3, `sqeel-tui` 0.2 → 0.3.** Submodules bumped for the
  `hjkl-bonsai` 0.3 + `hjkl-config` 0.2 + `leader_key: char` cascade. See
  `crates/sqeel-core/CHANGELOG.md` and `crates/sqeel-tui/CHANGELOG.md`.
  User-facing fallout: macOS / Windows users move from
  `~/Library/Application Support/sqeel/` / `%APPDATA%\sqeel\` to
  `~/.config/sqeel/` + `~/.local/share/sqeel/` (Linux unchanged), and
  tree-sitter grammars re-fetch on first use into `~/.local/share/bonsai/`.
  Config files with `leader_key = "ab"` (multi-char) now fail at parse time.

### Added

- `sqeel --help` now renders an ASCII-art banner (figlet "ANSI Regular" font)
  with the package version inline. Banner lives in `apps/sqeel/src/bin/art.txt`,
  embedded via `include_str!`. Regenerate with
  `figlet -f "ANSI Regular" sqeel > apps/sqeel/src/bin/art.txt`.
- `--version` flag (clap auto-derive from `CARGO_PKG_VERSION`).
- CLI smoke tests: `--version` returns `CARGO_PKG_VERSION`, long-form help
  contains the embedded art block and the version string.

## [0.2.4] - 2026-05-03

### Added

- `workflow_dispatch:` trigger on the release workflow for manual re-runs.

### Changed

- `sqeel-tui` submodule bumped to v0.2.4 (rustfmt fix).
- `sqeel-core` submodule bumped to v0.2.3 — picks up `hjkl-bonsai` 0.2 (runtime
  grammar loading; no more baked tree-sitter grammar crates).

## [0.2.3] - 2026-05-03

### Changed

- Release workflow streamlined: `fmt` + `clippy` steps moved to per-submodule CI
  (already gated there). Tag pushes now go straight to build + publish.

## [0.2.2] - 2026-05-03

### Changed

- **`sqeel-core` and `sqeel-tui` extracted into their own submodule repos**
  ([kryptic-sh/sqeel-core](https://github.com/kryptic-sh/sqeel-core),
  [kryptic-sh/sqeel-tui](https://github.com/kryptic-sh/sqeel-tui)). Mirrors the
  hjkl + buffr pattern: each crate publishes independently, the umbrella `sqeel`
  repo carries `[patch.crates-io]` overrides for local dev.

## [0.2.1] - 2026-05-03

### Changed

- `hjkl` 0.2 → 0.3.
- Migrated from `hjkl-tree-sitter` to `hjkl-bonsai` (runtime grammar loading
  instead of baked-in grammars — same shrink path the umbrella hjkl binary took,
  applied here for SQL highlighting).
- **`sqeel-tui` palette + search prompt** now use `TextFieldEditor` from
  `hjkl-form` instead of bespoke input handling. Same FSM as hjkl's `:` and `/`
  prompts.
- **Incremental tree-sitter highlighting** in `sqeel-core` — dropped the
  background highlight thread; reparse on edit instead.

## [0.2.0] - 2026-04-27

### Changed

- **Breaking: bumped to hjkl 0.2.0** — generic `Editor<B, H>` with explicit
  buffer + host type params. Public API on `sqeel-tui` and `sqeel-core` reshaped
  to match.
- **`sqeel-tui` consumes `hjkl-clipboard`** for yank / paste registers (Phase F
  of the hjkl-stack adoption). Replaces ad-hoc `arboard` calls.
- **Breaking: `sqeel-core` consumes `hjkl-tree-sitter`** for SQL syntax
  highlighting. Removes the bespoke highlighter; sqeel now uses the same
  Neovim-flavoured themes as hjkl + buffr.

## [0.1.1] - 2026-04-27

### Added

- **Auto-publish to crates.io on tag push.** `release.yml` watches `v*` tags and
  ships `sqeel-core`, `sqeel-tui`, and the umbrella `sqeel` crate.

### Fixed

- Release workflow greps the `[workspace.package]` `version` field directly
  instead of relying on `workspace.true`, which the crate-level lookup couldn't
  resolve.

### Docs

- README refreshed for the 0.1.0 crates.io publish.

## [0.1.0] - 2026-04-27

First tagged release. Vim-native SQL client for MySQL, Postgres, and SQLite.
Per-file TOML connections, sqls LSP integration, tree-sitter highlighting,
ratatui TUI + iced GUI from a shared `sqeel-core`.

### Highlights of the 0.0.x → 0.1.0 churn

- **Migrated to `hjkl` 0.1.0** generic `Editor<'a>` (after 16 churn bumps
  through `hjkl =0.0.24` → `=0.0.42`). Span types and search-pattern helpers
  relocated; viewport moved onto the `Host` trait.
- **Engine**: vim FSM, motions, registers, ex commands, page-mode dispatch
  shared with hjkl + buffr.
- **CI scaffolding** for `[patch.crates-io]` sibling-clone of hjkl removed once
  `hjkl` 0.1.0 published — sqeel now resolves it from crates.io.
- Publish metadata added; `pre-hjkl-extraction` retained as a historical
  reference tag for the pre-split monorepo state.

[Unreleased]: https://github.com/kryptic-sh/sqeel/compare/v0.4.12...HEAD
[0.4.12]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.4.12
[0.4.11]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.4.11
[0.4.10]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.4.10
[0.4.9]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.4.9
[0.4.8]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.4.8
[0.4.7]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.4.7
[0.4.6]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.4.6
[0.4.5]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.4.5
[0.4.4]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.4.4
[0.4.2]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.4.2
[0.4.1]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.4.1
[0.4.0]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.4.0
[0.3.0]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.3.0
[0.2.4]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.2.4
[0.2.3]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.2.3
[0.2.2]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.2.2
[0.2.1]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.2.1
[0.2.0]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.2.0
[0.1.1]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.1.1
[0.1.0]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.1.0
