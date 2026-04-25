# sqeel — Feature Plans

Living doc. Not a roadmap, not a promise. Captures features worth building.
Everything here ships free under the OSS license. Donors get priority on
bug/feature requests — see [DONATING.md](DONATING.md).

For vim-parity backlog (motions, operators, folding, etc.) see
[TODO.md](TODO.md). This file tracks product-level features.

---

## Connections & infra

### Multi-connection workspaces

- Split editor panes, each bound to a different connection
- Run same query across N DBs, diff result sets inline
- Connection groups (`prod`, `staging`, `dev`) with one-key swap
- Per-pane history + per-pane buffer

### More dialects

- MSSQL / SQL Server
- Oracle
- ClickHouse
- DuckDB
- Snowflake
- BigQuery
- MongoDB (read-only SQL via Atlas SQL or `mongosql`)
- Redis (read-only)

### Secrets integration

- 1Password CLI
- HashiCorp Vault
- AWS Secrets Manager
- sops / age-encrypted connection files
- macOS Keychain, GNOME Keyring, KWallet
- No plaintext passwords in `~/.config/sqeel/conns/`

### SSH tunnel manager

- Built-in SSH tunnel with multi-hop bastion support
- Per-connection key + agent forwarding
- Auto-reconnect on drop

### Connection load balancing

- Round-robin across read replicas
- Failover on connection error
- Latency-aware routing

---

## Query authoring

### Saved query library

- Tag, fuzzy-find, fold by folder
- Shared via git repo
- Parameterize with `:bind` placeholders
- Quick-run from any buffer with `<leader>q`

### AI query assist (BYO key)

- Natural language → SQL, schema-aware
- Explain selected query in plain English
- Refactor / optimize selected query
- Convert dialect (MySQL → Postgres, etc.)
- Bring-your-own API key (Claude / OpenAI / local Ollama)
- No data leaves machine without explicit confirm

### Macro / session recording

- Record a query session (queries + cell edits + exports)
- Replay against another DB / time / snapshot
- Export as portable `.sqeel-session` file
- Useful for repeatable reports, ETL spot-checks

---

## Performance & analysis

### EXPLAIN visualizer

- Interactive plan tree, collapsible nodes
- Hot-path highlighting (cost, rows, time)
- Cross-engine: MySQL `EXPLAIN ANALYZE`, Postgres JSON plan, MSSQL XML
- Suggest missing indexes from plan analysis

### Query timing & profiling

- Built-in query timer with percentile tracking
- Per-connection slow-query log
- Compare two query variants (A/B run, mean / p95 / p99)
- Flame-graph view of repeated runs

### Schema diff & migration generation

- Compare two live DBs, two snapshots, or DB vs SQL file
- Generate forward + rollback migration SQL
- Dialect-aware output (MySQL vs Postgres vs MSSQL)
- Side-by-side vimdiff view of schema files

---

## Results

### Result set transforms

- Pivot / unpivot in-place
- Group / aggregate without re-querying
- Apply ad-hoc filter on result without round-trip
- Pipe result through external command (`!sort`, `!jq`, `!awk`)

### Diff mode for results

- Run query twice, diff result sets row-by-row
- Highlight added / removed / changed rows
- Useful for verifying migrations, deploys, ETL

### Snapshot & time-travel

- Snapshot a query result locally (compressed)
- Re-open later, compare against fresh run
- Browse historical results in jumplist-style view

### Visualizations

- Inline sparklines per numeric column
- Quick chart preview (bar / line / hist) over result set
- Export chart as SVG / PNG
- Terminal-native rendering (no GUI required)

### Advanced exports

- Parquet, JSON-lines, XLSX, Avro
- Clipboard as markdown table, HTML table, LaTeX tabular
- Streaming export for huge result sets (no full RAM load)
- Custom templates (Jinja-style) for arbitrary text formats

### CSV / TSV / Parquet as table

- Open local file as if it were a SQL table (DuckDB-backed)
- Query across files + live DB in one statement
- Useful for log spelunking, CSV joins

---

## Ops

### Backup / restore wizards

- mysqldump / pg_dump UI with progress
- Selective table backup
- Restore preview before commit
