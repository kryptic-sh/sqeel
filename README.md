# SQEEL

[![CI](https://github.com/sqeel-sql/sqeel/actions/workflows/ci.yml/badge.svg)](https://github.com/sqeel-sql/sqeel/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/sqeel-sql/sqeel)](https://github.com/sqeel-sql/sqeel/releases/latest)

Fast, vim-native SQL client. No Electron. No JVM.

## Features

- Native Rust — instant startup
- Vim bindings — first class
- Mouse support in all panes
- Two UIs: terminal (`sqeel`) or native GUI (`sqeel-gui`)
- MySQL, SQLite, PostgreSQL via sqlx
- tree-sitter SQL syntax highlighting (dialect-aware)
- LSP integration (`sqls`) — completions + diagnostics
- Schema browser — click or keyboard to expand/collapse
- Editor tabs with lazy loading and 5-min RAM eviction
- Auto-save SQL buffers, result history, query history
- tmux-aware pane navigation
- Vim-style status bar + command mode (`:`)

## Layout

```
┌──────────┬─────────────────────────────┐
│          │  [tab1] [tab2]              │
│  Schema  │         Editor              │
│  (15%)   │         (85%)               │
│          │                             │
│          ├─────────────────────────────┤
│          │         Results             │
│          │      (shows on query)       │
└──────────┴─────────────────────────────┘
```

Results hidden → editor fills right pane. Query runs → results expand to 50%.

## Install

```sh
cargo install --git https://github.com/sqeel-sql/sqeel --bin sqeel
cargo install --git https://github.com/sqeel-sql/sqeel --bin sqeel-gui
```

Or build from source:

```sh
git clone https://github.com/sqeel-sql/sqeel
cd sqeel
cargo build --release
```

Binaries land in `target/release/sqeel` and `target/release/sqeel-gui`.

## Config

### Main — `~/.config/sqeel/config.toml`

```toml
[editor]
keybindings = "vim"

# Path to the SQL LSP binary (sqls recommended: https://github.com/sqls-server/sqls)
lsp_binary = "sqls"

# Lines scrolled per mouse wheel tick (all panes)
mouse_scroll_lines = 3
```

### Connections — `~/.config/sqeel/conns/<name>.toml`

Each file is one connection. Filename = display name in UI.

```toml
url = "mysql://localhost/mydb"
```

```toml
url = "postgres://user:pass@host/db"
```

sqeel scans `conns/` on startup and loads all `.toml` files.

## Keybindings

Press `?` in normal mode to open the help overlay.

### Global

| Key          | Action                                |
| ------------ | ------------------------------------- |
| `Ctrl+Enter` | Execute query                         |
| `Ctrl+W`     | Connection switcher                   |
| `?`          | Help overlay                          |
| `q`          | Quit (normal mode / schema / results) |

### Pane Focus

| Key              | Action        |
| ---------------- | ------------- |
| `Ctrl+H` / click | Focus schema  |
| `Ctrl+L` / click | Focus editor  |
| `Ctrl+J` / click | Focus results |
| `Ctrl+K` / click | Focus editor  |

### Tabs

| Key            | Action          |
| -------------- | --------------- |
| `Ctrl+T`       | New scratch tab |
| `Ctrl+Right`   | Next tab        |
| `Ctrl+Left`    | Prev tab        |
| Click tab name | Switch to tab   |

### Editor — Vim

| Key                 | Action                    |
| ------------------- | ------------------------- |
| `i`                 | Insert mode               |
| `Esc`               | Normal mode               |
| `v`                 | Visual mode               |
| `:`                 | Command mode              |
| `/`                 | Search                    |
| `Ctrl+P` / `Ctrl+N` | Query history prev / next |

### Explorer Pane

| Key           | Action                 |
| ------------- | ---------------------- |
| `j` / `k`     | Navigate down / up     |
| `Enter` / `l` | Expand / collapse node |
| `/`           | Search                 |

### Results Pane

| Key            | Action           |
| -------------- | ---------------- |
| `j` / `k`      | Scroll down / up |
| `q` / `Ctrl+C` | Dismiss results  |

### Connection Switcher

| Key       | Action            |
| --------- | ----------------- |
| `j` / `k` | Navigate          |
| `Enter`   | Connect           |
| `n`       | New connection    |
| `e`       | Edit connection   |
| `d`       | Delete connection |
| `Esc`     | Close             |

## Data

```
~/.local/share/sqeel/
  queries/    # auto-saved SQL buffers (grouped by connection)
  results/    # last 10 successful results (JSON, grouped by connection)
```

## Workspace

```
sqeel-core/   # state, DB, query runner, schema, config
sqeel-tui/    # ratatui terminal provider
sqeel-gui/    # iced native GUI provider
sqeel/        # binaries: sqeel + sqeel-gui
```

## License

[MIT](LICENSE)
