# sqeel-core

Core SQL engine for [sqeel](https://sqeel.kryptic.sh) — the vim-native SQL
client.

[![CI](https://github.com/kryptic-sh/sqeel-core/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/sqeel-core/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/sqeel-core.svg)](https://crates.io/crates/sqeel-core)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

sqlx-backed connection pool, tree-sitter SQL parser, schema browser, LSP
integration, and shared `AppState`. Pre-1.0 — API churns with sqeel.

Library crate, consumed by `sqeel-tui`. Part of the
[sqeel](https://github.com/kryptic-sh/sqeel) workspace.

## Modules

| Module           | Purpose                                                              |
| ---------------- | -------------------------------------------------------------------- |
| `state`          | `AppState` — shared mutable state behind `Arc<Mutex<AppState>>`.     |
| `db`             | sqlx connection pool, query execution, result streaming.             |
| `highlight`      | tree-sitter SQL highlighter: `Highlighter`, `HighlightSpan`.         |
| `schema`         | Schema browser: databases, tables, columns with TTL-based caching.   |
| `lsp`            | LSP client wiring: `sqls` process management, completions, hover.    |
| `completion_ctx` | Completion context extraction from the SQL AST.                      |
| `config`         | Re-exports from `sqeel-config`: `MainConfig`, `config_dir`, etc.     |
| `persistence`    | Auto-save SQL buffers and result history to `~/.local/share/sqeel/`. |
| `ddl`            | DDL introspection helpers for schema-cache fast paths.               |

## Key types

| Type            | Purpose                                                        |
| --------------- | -------------------------------------------------------------- |
| `AppState`      | Central state: active connection, tabs, results, schema cache. |
| `UiProvider`    | Trait implemented by `sqeel-tui`; drives the render loop.      |
| `Highlighter`   | Async-first tree-sitter SQL highlighter with `try_upgrade()`.  |
| `HighlightSpan` | A single styled byte-range produced by the highlighter.        |

## Quick start

```rust
use sqeel_core::{AppState, UiProvider};
use std::sync::{Arc, Mutex};

let state = Arc::new(Mutex::new(AppState::default()));

// UiProvider is implemented by sqeel-tui; run the TUI loop.
// sqeel_tui::run(state, /* show_splash */ true).await?;
```

## License

[MIT](LICENSE)
