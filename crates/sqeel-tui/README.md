# sqeel-tui

Ratatui TUI front-end for [sqeel](https://sqeel.kryptic.sh) — the vim-native SQL
client.

[![CI](https://github.com/kryptic-sh/sqeel-tui/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/sqeel-tui/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/sqeel-tui.svg)](https://crates.io/crates/sqeel-tui)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Vim-modal SQL editing on top of
[hjkl-engine](https://crates.io/crates/hjkl-engine) and
[ratatui](https://crates.io/crates/ratatui). Handles the full event loop,
keybindings, pane layout, syntax highlighting, LSP hover, splash screen, and
results rendering.

Library crate, consumed by the `sqeel` binary. Part of the
[sqeel](https://github.com/kryptic-sh/sqeel) workspace.

## Entry point

```rust
use sqeel_core::AppState;
use std::sync::{Arc, Mutex};

let state = Arc::new(Mutex::new(AppState::default()));

// Run the TUI. show_splash = false skips the startup animation.
sqeel_tui::run(state, /* show_splash */ true).await?;
```

The `sqeel` binary passes `--no-splash` through to the `show_splash` bool.

## Modules

| Module   | Purpose                                                            |
| -------- | ------------------------------------------------------------------ |
| `splash` | `SqeelStartScreen` — hjkl-splash powered startup animation.        |
| `host`   | `SqeelHost` — hjkl-engine `Host` impl bridging editor → app state. |

## Key types

| Type               | Purpose                                                      |
| ------------------ | ------------------------------------------------------------ |
| `SqeelHost`        | `hjkl_engine::Host` impl: tab ops, clipboard, LSP dispatch.  |
| `SqeelIntent`      | App-level intents emitted by `SqeelHost` and consumed by the |
|                    | run loop (run query, switch connection, open picker, etc.).  |
| `SqeelBufferId`    | Newtype over buffer identity used by the host.               |
| `SqeelStartScreen` | Splash screen: block-cursor animation tracing the sqeel art. |

## License

[MIT](LICENSE)
