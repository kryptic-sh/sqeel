# sqeel-config

Config and connection storage for [sqeel](https://sqeel.kryptic.sh) — the
vim-native SQL client.

[![CI](https://github.com/kryptic-sh/sqeel-config/actions/workflows/ci.yml/badge.svg)](https://github.com/kryptic-sh/sqeel-config/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/sqeel-config.svg)](https://crates.io/crates/sqeel-config)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Wraps [`hjkl-config`](https://crates.io/crates/hjkl-config)'s `AppConfig` trait
to provide deep-merge TOML loading for `~/.config/sqeel/config.toml`, XDG path
resolution, and the connection-file store (`~/.config/sqeel/conns/<name>.toml`).

Library crate, consumed by `sqeel-core`. Part of the
[sqeel](https://github.com/kryptic-sh/sqeel) workspace.

## Public API

### Config loading

```rust
use sqeel_config::{load_main_config, MainConfig, EditorConfig};

// Loads ~/.config/sqeel/config.toml merged over bundled defaults.
// Missing file → bundled defaults. Never writes to disk.
let cfg: MainConfig = load_main_config()?;

println!("lsp: {}", cfg.editor.lsp_binary);
println!("leader: {:?}", cfg.editor.leader_key);
```

### Connection storage

```rust
use sqeel_config::{load_connections, save_connection, delete_connection};

// List all saved connections.
for conn in load_connections()? {
    println!("{}: {}", conn.name, conn.url);
}

// Add or update a connection (file: ~/.config/sqeel/conns/<name>.toml).
save_connection("local", "postgres://localhost/mydb")?;

// Remove a connection.
delete_connection("local")?;
```

### Path resolution

```rust
use sqeel_config::{config_dir, set_config_dir_override};

// Override config dir (used by --sandbox mode).
set_config_dir_override("/tmp/sqeel-sandbox-abc".into());

// Resolves to the override or ~/.config/sqeel/.
let dir = config_dir();
```

## Key types

| Type / fn             | Purpose                                               |
| --------------------- | ----------------------------------------------------- |
| `MainConfig`          | Top-level config struct (`[editor]` section).         |
| `EditorConfig`        | Editor settings: LSP binary, leader key, scroll, etc. |
| `ConnectionConfig`    | A single saved connection: `name` + `url`.            |
| `load_main_config()`  | Load and merge config from disk.                      |
| `load_connections()`  | List all saved connections.                           |
| `save_connection()`   | Write/update a connection file.                       |
| `delete_connection()` | Remove a connection file.                             |
| `config_dir()`        | Resolve the active config directory.                  |
| `DEFAULTS_TOML`       | Bundled default config string.                        |

## License

[MIT](LICENSE)
