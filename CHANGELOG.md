# Changelog

All notable changes to this project will be documented in this file. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). This
project adheres to [Semantic Versioning](https://semver.org/) once it reaches
0.1.0; the 0.0.x series is a churn phase where breaking changes may land on
patch bumps.

## [Unreleased]

### Added

- Alpine .apk packaging pipeline (release CI builds .apk in `alpine:latest` and
  uploads it as a release asset; install with
  `apk add --allow-untrusted sqeel-*.apk`).
- Homebrew tap auto-publish for `sqeel` on tag push. New
  `pkg/homebrew/sqeel.rb.in` template + `brew-tap` job in `release.yml` renders
  the formula with the just-uploaded macOS sha256s and pushes it to
  `kryptic-sh/homebrew-tap`. Install with `brew install kryptic-sh/tap/sqeel`.

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

[Unreleased]: https://github.com/kryptic-sh/sqeel/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.3.0
[0.2.4]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.2.4
[0.2.3]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.2.3
[0.2.2]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.2.2
[0.2.1]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.2.1
[0.2.0]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.2.0
[0.1.1]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.1.1
[0.1.0]: https://github.com/kryptic-sh/sqeel/releases/tag/v0.1.0
