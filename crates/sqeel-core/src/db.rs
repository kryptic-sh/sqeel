use crate::schema::SchemaNode;
use crate::state::QueryResult;
use sqeel_config::{TlsConfig, TlsVerifyMode};
use sqlx::{
    Column, Row, TypeInfo,
    mysql::{MySqlConnectOptions, MySqlPool, MySqlSslMode},
    postgres::{PgConnectOptions, PgPool, PgSslMode},
    sqlite::{SqliteConnectOptions, SqlitePool},
};
use std::str::FromStr;
#[cfg(feature = "duckdb")]
use std::sync::{Arc, Mutex};

/// Outcome of `DbConnection::execute`. Row-returning queries (SELECT,
/// SHOW, EXPLAIN, …) produce a `Rows` result; statements that don't
/// produce a result set (INSERT/UPDATE/DELETE, CREATE/DROP/ALTER, …)
/// produce a `NonQuery` summary the UI can render as a status line
/// instead of an empty table.
#[non_exhaustive]
pub enum ExecOutcome {
    Rows(QueryResult),
    NonQuery { verb: String, rows_affected: u64 },
}

/// Classification of a failed `DbConnection::connect`. Lets the
/// sidebar render a short headline ("Auth failed" vs "Host not
/// found") and the details popup show the underlying message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConnectErrorKind {
    /// Could reach the network but auth was rejected (bad password,
    /// access denied, role doesn't exist).
    Auth,
    /// TCP refused / unreachable / reset.
    Network,
    /// DNS lookup failed — host doesn't resolve.
    Dns,
    /// TLS handshake / cert validation failure.
    Tls,
    /// URL is unparseable or scheme is unsupported.
    Config,
    /// Anything else — sqlx surfaced an error we don't classify.
    Other,
}

impl ConnectErrorKind {
    /// Headline shown in the sidebar placeholder.
    pub fn headline(self) -> &'static str {
        match self {
            ConnectErrorKind::Auth => "Auth failed",
            ConnectErrorKind::Network => "Network unreachable",
            ConnectErrorKind::Dns => "Host not found",
            ConnectErrorKind::Tls => "TLS error",
            ConnectErrorKind::Config => "Bad connection URL",
            ConnectErrorKind::Other => "Connection failed",
        }
    }

    /// Short tag used as the popup title prefix.
    pub fn label(self) -> &'static str {
        match self {
            ConnectErrorKind::Auth => "Auth",
            ConnectErrorKind::Network => "Network",
            ConnectErrorKind::Dns => "DNS",
            ConnectErrorKind::Tls => "TLS",
            ConnectErrorKind::Config => "Config",
            ConnectErrorKind::Other => "Connection",
        }
    }
}

#[derive(Debug)]
pub struct ConnectError {
    pub kind: ConnectErrorKind,
    pub detail: String,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.label(), self.detail)
    }
}

impl std::error::Error for ConnectError {}

impl From<sqlx::Error> for ConnectError {
    fn from(e: sqlx::Error) -> Self {
        // sqlx 0.8's Error is non_exhaustive — match the variants we
        // can classify and let everything else fall through to
        // `Other`. Heuristic message-sniffing inside `Io` covers DNS,
        // which sqlx doesn't surface as its own variant.
        match &e {
            sqlx::Error::Io(io_err) => classify_io(io_err, &e),
            sqlx::Error::Database(db_err) => {
                let msg = db_err.message().to_string();
                let lower = msg.to_lowercase();
                let kind = if lower.contains("password")
                    || lower.contains("authentication")
                    || lower.contains("access denied")
                    || lower.contains("role")
                    || lower.contains("permission denied")
                {
                    ConnectErrorKind::Auth
                } else {
                    ConnectErrorKind::Other
                };
                ConnectError { kind, detail: msg }
            }
            sqlx::Error::Tls(t) => ConnectError {
                kind: ConnectErrorKind::Tls,
                detail: t.to_string(),
            },
            sqlx::Error::Configuration(c) => ConnectError {
                kind: ConnectErrorKind::Config,
                detail: c.to_string(),
            },
            sqlx::Error::PoolTimedOut => ConnectError {
                kind: ConnectErrorKind::Network,
                detail: "pool timed out".into(),
            },
            _ => ConnectError {
                kind: ConnectErrorKind::Other,
                detail: e.to_string(),
            },
        }
    }
}

fn classify_io(io_err: &std::io::Error, sqlx_err: &sqlx::Error) -> ConnectError {
    use std::io::ErrorKind as K;
    let detail = sqlx_err.to_string();
    let lower = detail.to_lowercase();
    // sqlx wraps the resolver error in an opaque `Io(Other)` whose
    // formatted form contains "failed to lookup address" / "name
    // resolution" / "nodename nor servname". Match the message
    // before we fall back on the io kind.
    if lower.contains("lookup address")
        || lower.contains("name resolution")
        || lower.contains("nodename nor servname")
        || lower.contains("temporary failure in name resolution")
        || lower.contains("no such host")
    {
        return ConnectError {
            kind: ConnectErrorKind::Dns,
            detail,
        };
    }
    let kind = match io_err.kind() {
        K::ConnectionRefused
        | K::ConnectionReset
        | K::ConnectionAborted
        | K::NotConnected
        | K::TimedOut
        | K::HostUnreachable
        | K::NetworkUnreachable => ConnectErrorKind::Network,
        K::NotFound => ConnectErrorKind::Dns,
        _ => ConnectErrorKind::Network,
    };
    ConnectError { kind, detail }
}

/// Per-engine connection pool. Sqeel dispatches typed queries through the
/// matching variant so each engine can decode its native column types
/// (DATETIME, DECIMAL, JSON, BYTEA, UUID, …) without going through `sqlx::Any`.
pub enum Pool {
    MySql(MySqlPool),
    Pg(PgPool),
    Sqlite(SqlitePool),
    #[cfg(feature = "duckdb")]
    DuckDb(Arc<Mutex<duckdb::Connection>>),
}

pub struct DbConnection {
    pool: Pool,
    pub url: String,
}

impl DbConnection {
    pub async fn connect(url: &str, tls: Option<&TlsConfig>) -> Result<Self, ConnectError> {
        let pool = if url.starts_with("mysql://") || url.starts_with("mariadb://") {
            let mut opts = MySqlConnectOptions::from_str(url)?;
            if let Some(tls_cfg) = tls {
                let ssl_mode = match tls_cfg.verify_mode {
                    Some(TlsVerifyMode::Skip) => MySqlSslMode::Required,
                    // Full or unspecified → full hostname + chain verification
                    Some(TlsVerifyMode::Full) | None => MySqlSslMode::VerifyIdentity,
                };
                opts = opts.ssl_mode(ssl_mode);
                if let Some(ca) = &tls_cfg.ca_cert {
                    opts = opts.ssl_ca(ca);
                }
                if let Some(cert) = &tls_cfg.client_cert {
                    opts = opts.ssl_client_cert(cert);
                }
                if let Some(key) = &tls_cfg.client_key {
                    opts = opts.ssl_client_key(key);
                }
            }
            Pool::MySql(MySqlPool::connect_with(opts).await?)
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            let mut opts = PgConnectOptions::from_str(url)?;
            if let Some(tls_cfg) = tls {
                let ssl_mode = match tls_cfg.verify_mode {
                    Some(TlsVerifyMode::Skip) => PgSslMode::Require,
                    // Full or unspecified → full hostname + chain verification
                    Some(TlsVerifyMode::Full) | None => PgSslMode::VerifyFull,
                };
                opts = opts.ssl_mode(ssl_mode);
                if let Some(ca) = &tls_cfg.ca_cert {
                    opts = opts.ssl_root_cert(ca);
                }
                if let Some(cert) = &tls_cfg.client_cert {
                    opts = opts.ssl_client_cert(cert);
                }
                if let Some(key) = &tls_cfg.client_key {
                    opts = opts.ssl_client_key(key);
                }
            }
            Pool::Pg(PgPool::connect_with(opts).await?)
        } else if url.starts_with("sqlite://") || url.starts_with("sqlite:") {
            // Match what every other SQL client does for sqlite: create
            // the DB file if it doesn't exist yet. Stops `--sandbox`
            // and "open my new project DB" both from failing on
            // first launch with a confusing "file not found" error.
            // Users who want strict "must exist" semantics can pass
            // `?mode=ro` or `?mode=rw` in the URL to override.
            let opts = SqliteConnectOptions::from_str(url)?.create_if_missing(true);
            Pool::Sqlite(SqlitePool::connect_with(opts).await?)
        } else if url.starts_with("duckdb:") {
            #[cfg(feature = "duckdb")]
            {
                let url_owned = url.to_string();
                let conn = tokio::task::spawn_blocking(
                    move || -> Result<duckdb::Connection, ConnectError> {
                        // `duckdb::memory:` → in-memory; `duckdb:/path` or `duckdb://path` → file.
                        let rest = url_owned.strip_prefix("duckdb:").unwrap_or("");
                        let path = rest.trim_start_matches('/');
                        // `:memory:` is the duckdb-rs in-memory sentinel (after stripping `duckdb:`
                        // from `duckdb::memory:` the rest is `:memory:`).
                        if path == ":memory:" || path.is_empty() {
                            duckdb::Connection::open_in_memory().map_err(|e| ConnectError {
                                kind: ConnectErrorKind::Config,
                                detail: e.to_string(),
                            })
                        } else {
                            duckdb::Connection::open(path).map_err(|e| ConnectError {
                                kind: ConnectErrorKind::Config,
                                detail: e.to_string(),
                            })
                        }
                    },
                )
                .await
                .map_err(|e| ConnectError {
                    kind: ConnectErrorKind::Other,
                    detail: e.to_string(),
                })??;
                Pool::DuckDb(Arc::new(Mutex::new(conn)))
            }
            #[cfg(not(feature = "duckdb"))]
            {
                return Err(ConnectError {
                    kind: ConnectErrorKind::Config,
                    detail: "DuckDB support not compiled in (enable the `duckdb` feature)".into(),
                });
            }
        } else {
            return Err(ConnectError {
                kind: ConnectErrorKind::Config,
                detail: format!("Unsupported URL scheme: {url}"),
            });
        };
        Ok(Self {
            pool,
            url: url.to_string(),
        })
    }

    pub fn is_sqlite(&self) -> bool {
        matches!(self.pool, Pool::Sqlite(_))
    }

    pub fn is_duckdb(&self) -> bool {
        #[cfg(feature = "duckdb")]
        {
            matches!(self.pool, Pool::DuckDb(_))
        }
        #[cfg(not(feature = "duckdb"))]
        {
            false
        }
    }

    /// Load just the database/schema names as collapsed nodes with no tables.
    /// This is fast and lets the UI show the structure before tables are loaded.
    pub async fn load_schema_databases(&self) -> anyhow::Result<Vec<SchemaNode>> {
        if self.is_sqlite() || self.is_duckdb() {
            return Ok(vec![SchemaNode::Database {
                name: "main".into(),
                expanded: true,
                tables: vec![],
                tables_loaded_at: None,
            }]);
        }
        let databases = self.list_databases().await?;
        Ok(databases
            .into_iter()
            .map(|name| SchemaNode::Database {
                name,
                expanded: false,
                tables: vec![],
                tables_loaded_at: None,
            })
            .collect())
    }

    /// Fetch columns, indexes, and foreign keys for a table concurrently.
    /// Uses `tokio::join!` so a failed index/FK query doesn't abort columns.
    pub async fn list_table_relations(
        &self,
        database: &str,
        table: &str,
    ) -> (
        Vec<crate::db::ColumnInfo>,
        Vec<IndexInfo>,
        Vec<ForeignKeyInfo>,
    ) {
        let (cols, idxs, fks) = tokio::join!(
            self.list_columns(database, table),
            self.list_indexes(database, table),
            self.list_foreign_keys(database, table),
        );
        (
            cols.unwrap_or_default(),
            idxs.unwrap_or_default(),
            fks.unwrap_or_default(),
        )
    }

    pub async fn execute(&self, query: &str) -> anyhow::Result<ExecOutcome> {
        // Non-row statements (INSERT/UPDATE/DELETE/CREATE/DROP/…) go
        // through sqlx's `execute()` so we can surface rows_affected
        // in a dedicated results pane instead of pretending the empty
        // result set means "nothing happened".
        if let Some(verb) = non_query_verb(query) {
            let rows_affected = match &self.pool {
                Pool::MySql(p) => sqlx::query(query).execute(p).await?.rows_affected(),
                Pool::Pg(p) => sqlx::query(query).execute(p).await?.rows_affected(),
                Pool::Sqlite(p) => sqlx::query(query).execute(p).await?.rows_affected(),
                #[cfg(feature = "duckdb")]
                Pool::DuckDb(c) => {
                    let conn = Arc::clone(c);
                    let q = query.to_string();
                    tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
                        let conn = conn
                            .lock()
                            .map_err(|e| anyhow::anyhow!("mutex poisoned: {e}"))?;
                        let n = conn.execute(&q, [])?;
                        Ok(n as u64)
                    })
                    .await??
                }
            };
            return Ok(ExecOutcome::NonQuery {
                verb,
                rows_affected,
            });
        }

        let owned;
        let query = match apply_default_limit(query, DEFAULT_ROW_LIMIT) {
            Some(q) => {
                owned = q;
                owned.as_str()
            }
            None => query,
        };
        let (columns, rows) = match &self.pool {
            Pool::MySql(p) => {
                let rs = sqlx::query(query).fetch_all(p).await?;
                let cols = rs
                    .first()
                    .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
                    .unwrap_or_default();
                let data = rs
                    .iter()
                    .map(|r| (0..r.columns().len()).map(|i| decode_mysql(r, i)).collect())
                    .collect();
                (cols, data)
            }
            Pool::Pg(p) => {
                let rs = sqlx::query(query).fetch_all(p).await?;
                let cols = rs
                    .first()
                    .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
                    .unwrap_or_default();
                let data = rs
                    .iter()
                    .map(|r| (0..r.columns().len()).map(|i| decode_pg(r, i)).collect())
                    .collect();
                (cols, data)
            }
            Pool::Sqlite(p) => {
                let rs = sqlx::query(query).fetch_all(p).await?;
                let cols = rs
                    .first()
                    .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
                    .unwrap_or_default();
                let data = rs
                    .iter()
                    .map(|r| {
                        (0..r.columns().len())
                            .map(|i| decode_sqlite(r, i))
                            .collect()
                    })
                    .collect();
                (cols, data)
            }
            #[cfg(feature = "duckdb")]
            Pool::DuckDb(c) => {
                let conn = Arc::clone(c);
                let q = query.to_string();
                tokio::task::spawn_blocking(
                    move || -> anyhow::Result<(Vec<String>, Vec<Vec<String>>)> {
                        let conn = conn
                            .lock()
                            .map_err(|e| anyhow::anyhow!("mutex poisoned: {e}"))?;
                        let mut stmt = conn.prepare(&q)?;
                        // `query([])` executes the statement and returns Rows.
                        // Column metadata is only available after execution, so we
                        // read column names through `rows.as_ref()` rather than
                        // calling `stmt.column_count()` before the query runs.
                        let mut rows = stmt.query([])?;
                        let cols: Vec<String> =
                            rows.as_ref().map(|s| s.column_names()).unwrap_or_default();
                        let col_count = cols.len();
                        let mut data: Vec<Vec<String>> = Vec::new();
                        while let Some(row) = rows.next()? {
                            let mut cells = Vec::with_capacity(col_count);
                            for i in 0..col_count {
                                let v: duckdb::types::Value = row.get(i)?;
                                cells.push(duck_value_to_string(v));
                            }
                            data.push(cells);
                        }
                        Ok((cols, data))
                    },
                )
                .await??
            }
        };

        Ok(ExecOutcome::Rows(QueryResult {
            columns,
            rows,
            col_widths: vec![],
        }))
    }

    pub async fn list_databases(&self) -> anyhow::Result<Vec<String>> {
        match &self.pool {
            Pool::Sqlite(p) => {
                let rows = sqlx::query("PRAGMA database_list").fetch_all(p).await?;
                Ok(rows
                    .iter()
                    .map(|r| r.try_get::<String, _>(1).unwrap_or_else(|_| "main".into()))
                    .collect())
            }
            Pool::MySql(p) => {
                let rows = sqlx::query("SHOW DATABASES").fetch_all(p).await?;
                Ok(rows.iter().map(|r| mysql_string(r, 0)).collect())
            }
            Pool::Pg(p) => {
                let rows =
                    sqlx::query("SELECT datname FROM pg_database WHERE datistemplate = false")
                        .fetch_all(p)
                        .await?;
                Ok(rows
                    .iter()
                    .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
                    .collect())
            }
            #[cfg(feature = "duckdb")]
            Pool::DuckDb(_) => Ok(vec!["main".into()]),
        }
    }

    pub async fn list_tables(&self, database: &str) -> anyhow::Result<Vec<String>> {
        match &self.pool {
            Pool::MySql(p) => {
                let safe_db = database.replace('`', "``");
                let rows = sqlx::query(&format!("SHOW TABLES FROM `{safe_db}`"))
                    .fetch_all(p)
                    .await?;
                Ok(rows.iter().map(|r| mysql_string(r, 0)).collect())
            }
            Pool::Sqlite(p) => {
                let rows =
                    sqlx::query("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                        .fetch_all(p)
                        .await?;
                Ok(rows
                    .iter()
                    .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
                    .collect())
            }
            Pool::Pg(p) => {
                let rows = sqlx::query(
                    "SELECT tablename FROM pg_tables WHERE schemaname = $1 ORDER BY tablename",
                )
                .bind(database)
                .fetch_all(p)
                .await?;
                Ok(rows
                    .iter()
                    .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
                    .collect())
            }
            #[cfg(feature = "duckdb")]
            Pool::DuckDb(c) => {
                let conn = Arc::clone(c);
                let _db = database.to_string();
                tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<String>> {
                    let conn = conn
                        .lock()
                        .map_err(|e| anyhow::anyhow!("mutex poisoned: {e}"))?;
                    let mut stmt = conn.prepare(
                        "SELECT table_name FROM information_schema.tables \
                         WHERE table_schema = 'main' ORDER BY table_name",
                    )?;
                    let names = stmt.query_map([], |row| row.get::<_, String>(0))?;
                    names.map(|r| r.map_err(Into::into)).collect()
                })
                .await?
            }
        }
    }

    pub async fn list_columns(
        &self,
        database: &str,
        table: &str,
    ) -> anyhow::Result<Vec<ColumnInfo>> {
        match &self.pool {
            Pool::MySql(p) => {
                let rows = sqlx::query(
                    "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY \
                     FROM information_schema.COLUMNS \
                     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                     ORDER BY ORDINAL_POSITION",
                )
                .bind(database)
                .bind(table)
                .fetch_all(p)
                .await?;
                Ok(rows
                    .iter()
                    .map(|r| ColumnInfo {
                        name: mysql_string(r, 0),
                        type_name: mysql_string(r, 1),
                        nullable: mysql_string(r, 2) == "YES",
                        is_pk: mysql_string(r, 3) == "PRI",
                    })
                    .collect())
            }
            Pool::Sqlite(p) => {
                let safe_table = table.replace('"', "\"\"");
                let rows = sqlx::query(&format!("PRAGMA table_info(\"{safe_table}\")"))
                    .fetch_all(p)
                    .await?;
                Ok(rows
                    .iter()
                    .map(|r| ColumnInfo {
                        name: r.try_get::<String, _>(1).unwrap_or_default(),
                        type_name: r.try_get::<String, _>(2).unwrap_or_default(),
                        nullable: r.try_get::<i64, _>(3).unwrap_or(0) == 0,
                        is_pk: r.try_get::<i64, _>(5).unwrap_or(0) != 0,
                    })
                    .collect())
            }
            Pool::Pg(p) => {
                let rows = sqlx::query(
                    "SELECT c.column_name, c.data_type, c.is_nullable, \
                     COALESCE((SELECT 1 FROM information_schema.table_constraints tc \
                       JOIN information_schema.key_column_usage kcu \
                         ON tc.constraint_name = kcu.constraint_name \
                       WHERE tc.table_schema = $1 AND tc.table_name = $2 \
                         AND kcu.column_name = c.column_name \
                         AND tc.constraint_type = 'PRIMARY KEY' LIMIT 1), 0) AS is_pk \
                     FROM information_schema.columns c \
                     WHERE c.table_schema = $1 AND c.table_name = $2 \
                     ORDER BY c.ordinal_position",
                )
                .bind(database)
                .bind(table)
                .fetch_all(p)
                .await?;
                Ok(rows
                    .iter()
                    .map(|r| ColumnInfo {
                        name: r.try_get::<String, _>(0).unwrap_or_default(),
                        type_name: r.try_get::<String, _>(1).unwrap_or_default(),
                        nullable: r.try_get::<String, _>(2).unwrap_or_default() == "YES",
                        is_pk: r.try_get::<i32, _>(3).unwrap_or(0) != 0,
                    })
                    .collect())
            }
            #[cfg(feature = "duckdb")]
            Pool::DuckDb(c) => {
                let conn = Arc::clone(c);
                let tbl = table.to_string();
                tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<ColumnInfo>> {
                    let conn = conn
                        .lock()
                        .map_err(|e| anyhow::anyhow!("mutex poisoned: {e}"))?;
                    let mut stmt = conn.prepare(
                        "SELECT column_name, data_type, is_nullable \
                         FROM information_schema.columns \
                         WHERE table_schema = 'main' AND table_name = ? \
                         ORDER BY ordinal_position",
                    )?;
                    let infos = stmt.query_map([tbl.as_str()], |row| {
                        Ok(ColumnInfo {
                            name: row.get::<_, String>(0)?,
                            type_name: row.get::<_, String>(1)?,
                            nullable: row.get::<_, String>(2).map(|s| s == "YES").unwrap_or(true),
                            is_pk: false,
                        })
                    })?;
                    infos.map(|r| r.map_err(Into::into)).collect()
                })
                .await?
            }
        }
    }

    /// Fetch all indexes for the given table.
    ///
    /// For MySQL the schema parameter is the database name. For Postgres it is
    /// the schema name (same convention as `list_columns`).
    /// SQLite and DuckDB return empty vecs (best-effort; no standard catalog).
    pub async fn list_indexes(
        &self,
        database: &str,
        table: &str,
    ) -> anyhow::Result<Vec<IndexInfo>> {
        match &self.pool {
            Pool::MySql(p) => {
                // One row per (index, column) ordered by SEQ_IN_INDEX; GROUP_CONCAT
                // aggregates columns into a comma-separated string.
                let rows = sqlx::query(
                    "SELECT INDEX_NAME, \
                     GROUP_CONCAT(COLUMN_NAME ORDER BY SEQ_IN_INDEX SEPARATOR ',') AS cols, \
                     NON_UNIQUE \
                     FROM information_schema.STATISTICS \
                     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                     GROUP BY INDEX_NAME, NON_UNIQUE \
                     ORDER BY INDEX_NAME",
                )
                .bind(database)
                .bind(table)
                .fetch_all(p)
                .await?;
                Ok(rows
                    .iter()
                    .map(|r| {
                        let name = mysql_string(r, 0);
                        let cols_raw = mysql_string(r, 1);
                        let cols: Vec<String> = cols_raw
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        // NON_UNIQUE == 0 → unique
                        let non_unique: i64 = r.try_get::<i64, _>(2).unwrap_or(1);
                        IndexInfo {
                            name,
                            cols,
                            unique: non_unique == 0,
                        }
                    })
                    .collect())
            }
            Pool::Pg(p) => {
                let rows = sqlx::query(
                    "SELECT \
                       i.relname  AS index_name, \
                       ix.indisunique AS is_unique, \
                       a.attname  AS col_name, \
                       array_position(ix.indkey, a.attnum) AS col_pos \
                     FROM pg_class t \
                     JOIN pg_index ix    ON t.oid = ix.indrelid \
                     JOIN pg_class i     ON i.oid = ix.indexrelid \
                     JOIN pg_attribute a ON a.attrelid = t.oid \
                                        AND a.attnum = ANY(ix.indkey) \
                     JOIN pg_namespace n ON n.oid = t.relnamespace \
                     WHERE n.nspname = $1 AND t.relname = $2 \
                     ORDER BY index_name, col_pos",
                )
                .bind(database)
                .bind(table)
                .fetch_all(p)
                .await?;
                // Aggregate by index_name — use a Vec to preserve insertion order.
                // Each entry: (index_name, is_unique, [(col_pos, col_name), ...])
                #[allow(clippy::type_complexity)]
                let mut agg: Vec<(String, bool, Vec<(i32, String)>)> = Vec::new();
                for r in &rows {
                    let idx_name: String = r.try_get(0).unwrap_or_default();
                    let is_unique: bool = r.try_get(1).unwrap_or(false);
                    let col_name: String = r.try_get(2).unwrap_or_default();
                    let col_pos: i32 = r.try_get(3).unwrap_or(0);
                    if let Some(entry) = agg.iter_mut().find(|(n, _, _)| n == &idx_name) {
                        entry.2.push((col_pos, col_name));
                    } else {
                        agg.push((idx_name, is_unique, vec![(col_pos, col_name)]));
                    }
                }
                Ok(agg
                    .into_iter()
                    .map(|(name, unique, mut col_pairs)| {
                        col_pairs.sort_by_key(|(pos, _)| *pos);
                        let cols = col_pairs.into_iter().map(|(_, c)| c).collect();
                        IndexInfo { name, cols, unique }
                    })
                    .collect())
            }
            // SQLite: best-effort via PRAGMA index_list + PRAGMA index_info.
            Pool::Sqlite(p) => {
                let safe_table = table.replace('"', "\"\"");
                let list_rows = sqlx::query(&format!("PRAGMA index_list(\"{safe_table}\")"))
                    .fetch_all(p)
                    .await
                    .unwrap_or_default();
                let mut infos = Vec::new();
                for lr in &list_rows {
                    let idx_name: String = lr.try_get::<String, _>(1).unwrap_or_default();
                    let unique_int: i64 = lr.try_get::<i64, _>(2).unwrap_or(0);
                    let safe_idx = idx_name.replace('"', "\"\"");
                    let col_rows = sqlx::query(&format!("PRAGMA index_info(\"{safe_idx}\")"))
                        .fetch_all(p)
                        .await
                        .unwrap_or_default();
                    let cols: Vec<String> = col_rows
                        .iter()
                        .map(|cr| cr.try_get::<String, _>(2).unwrap_or_default())
                        .collect();
                    infos.push(IndexInfo {
                        name: idx_name,
                        cols,
                        unique: unique_int != 0,
                    });
                }
                Ok(infos)
            }
            #[cfg(feature = "duckdb")]
            Pool::DuckDb(_) => {
                // DuckDB has no standard PRAGMA index catalog; return empty.
                Ok(vec![])
            }
        }
    }

    /// Fetch all foreign key constraints for the given table.
    ///
    /// For MySQL the schema parameter is the database name. For Postgres it is
    /// the schema name (same convention as `list_columns`).
    /// SQLite returns constraints via `PRAGMA foreign_key_list`.
    /// DuckDB returns empty (no FK catalog).
    pub async fn list_foreign_keys(
        &self,
        database: &str,
        table: &str,
    ) -> anyhow::Result<Vec<ForeignKeyInfo>> {
        match &self.pool {
            Pool::MySql(p) => {
                let rows = sqlx::query(
                    "SELECT CONSTRAINT_NAME, COLUMN_NAME, \
                     REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME, ORDINAL_POSITION \
                     FROM information_schema.KEY_COLUMN_USAGE \
                     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                     AND REFERENCED_TABLE_NAME IS NOT NULL \
                     ORDER BY CONSTRAINT_NAME, ORDINAL_POSITION",
                )
                .bind(database)
                .bind(table)
                .fetch_all(p)
                .await?;
                // Aggregate by CONSTRAINT_NAME — Vec preserves insertion order.
                let mut agg: Vec<(String, String, Vec<String>, Vec<String>)> = Vec::new();
                for r in &rows {
                    let constraint: String = mysql_string(r, 0);
                    let col: String = mysql_string(r, 1);
                    let ref_table: String = mysql_string(r, 2);
                    let ref_col: String = mysql_string(r, 3);
                    if let Some(entry) = agg.iter_mut().find(|(n, _, _, _)| n == &constraint) {
                        entry.2.push(col);
                        entry.3.push(ref_col);
                    } else {
                        agg.push((constraint, ref_table, vec![col], vec![ref_col]));
                    }
                }
                Ok(agg
                    .into_iter()
                    .map(|(name, ref_table, cols, ref_cols)| ForeignKeyInfo {
                        name,
                        cols,
                        ref_table,
                        ref_cols,
                    })
                    .collect())
            }
            Pool::Pg(p) => {
                let rows = sqlx::query(
                    "SELECT \
                       con.conname AS name, \
                       a.attname AS col, \
                       af.attname AS ref_col, \
                       cl.relname AS ref_table, \
                       u.ord \
                     FROM pg_constraint con \
                     JOIN unnest(con.conkey) WITH ORDINALITY u(attnum, ord) ON TRUE \
                     JOIN pg_attribute a  ON a.attrelid = con.conrelid \
                                         AND a.attnum = u.attnum \
                     JOIN pg_attribute af ON af.attrelid = con.confrelid \
                                         AND af.attnum = con.confkey[u.ord] \
                     JOIN pg_class cl     ON cl.oid = con.confrelid \
                     JOIN pg_class t      ON t.oid = con.conrelid \
                     JOIN pg_namespace n  ON n.oid = t.relnamespace \
                     WHERE con.contype = 'f' AND n.nspname = $1 AND t.relname = $2 \
                     ORDER BY name, u.ord",
                )
                .bind(database)
                .bind(table)
                .fetch_all(p)
                .await?;
                let mut agg: Vec<(String, String, Vec<String>, Vec<String>)> = Vec::new();
                for r in &rows {
                    let name: String = r.try_get(0).unwrap_or_default();
                    let col: String = r.try_get(1).unwrap_or_default();
                    let ref_col: String = r.try_get(2).unwrap_or_default();
                    let ref_table: String = r.try_get(3).unwrap_or_default();
                    if let Some(entry) = agg.iter_mut().find(|(n, _, _, _)| n == &name) {
                        entry.2.push(col);
                        entry.3.push(ref_col);
                    } else {
                        agg.push((name, ref_table, vec![col], vec![ref_col]));
                    }
                }
                Ok(agg
                    .into_iter()
                    .map(|(name, ref_table, cols, ref_cols)| ForeignKeyInfo {
                        name,
                        cols,
                        ref_table,
                        ref_cols,
                    })
                    .collect())
            }
            Pool::Sqlite(p) => {
                // PRAGMA foreign_key_list returns one row per (fk, column).
                let safe_table = table.replace('"', "\"\"");
                let rows = sqlx::query(&format!("PRAGMA foreign_key_list(\"{safe_table}\")"))
                    .fetch_all(p)
                    .await
                    .unwrap_or_default();
                // Columns: id, seq, table, from, to, ...
                let mut agg: Vec<(i64, String, Vec<String>, Vec<String>)> = Vec::new();
                for r in &rows {
                    let id: i64 = r.try_get::<i64, _>(0).unwrap_or(0);
                    let ref_table: String = r.try_get::<String, _>(2).unwrap_or_default();
                    let from_col: String = r.try_get::<String, _>(3).unwrap_or_default();
                    let to_col: String = r.try_get::<String, _>(4).unwrap_or_default();
                    if let Some(entry) = agg.iter_mut().find(|(i, _, _, _)| *i == id) {
                        entry.2.push(from_col);
                        entry.3.push(to_col);
                    } else {
                        agg.push((id, ref_table, vec![from_col], vec![to_col]));
                    }
                }
                Ok(agg
                    .into_iter()
                    .map(|(id, ref_table, cols, ref_cols)| ForeignKeyInfo {
                        name: format!("fk_{id}"),
                        cols,
                        ref_table,
                        ref_cols,
                    })
                    .collect())
            }
            #[cfg(feature = "duckdb")]
            Pool::DuckDb(_) => {
                // DuckDB has no FK catalog yet; return empty.
                Ok(vec![])
            }
        }
    }

    /// Load the schema tree: databases + tables only (no columns — too slow to
    /// load eagerly for large schemas). Columns can be loaded on demand later.
    pub async fn load_schema(&self) -> anyhow::Result<Vec<SchemaNode>> {
        if self.is_sqlite() || self.is_duckdb() {
            let tables = self.list_tables("main").await.unwrap_or_default();
            let table_nodes = tables
                .into_iter()
                .map(|t| SchemaNode::Table {
                    name: t,
                    expanded: false,
                    columns: vec![],
                    columns_loaded_at: None,
                    indexes: vec![],
                    foreign_keys: vec![],
                    relations_loaded_at: None,
                    indexes_expanded: false,
                    foreign_keys_expanded: false,
                })
                .collect();
            return Ok(vec![SchemaNode::Database {
                name: "main".into(),
                expanded: true,
                tables: table_nodes,
                tables_loaded_at: Some(std::time::Instant::now()),
            }]);
        }

        let databases = self.list_databases().await?;
        let mut nodes = Vec::new();
        for db in databases {
            let tables = self.list_tables(&db).await.unwrap_or_default();
            let table_nodes = tables
                .into_iter()
                .map(|t| SchemaNode::Table {
                    name: t,
                    expanded: false,
                    columns: vec![],
                    columns_loaded_at: None,
                    indexes: vec![],
                    foreign_keys: vec![],
                    relations_loaded_at: None,
                    indexes_expanded: false,
                    foreign_keys_expanded: false,
                })
                .collect();
            nodes.push(SchemaNode::Database {
                name: db,
                expanded: false,
                tables: table_nodes,
                tables_loaded_at: Some(std::time::Instant::now()),
            });
        }
        Ok(nodes)
    }
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub is_pk: bool,
}

/// Metadata for a single index on a table.
#[derive(Debug, Clone)]
pub struct IndexInfo {
    pub name: String,
    pub cols: Vec<String>,
    pub unique: bool,
}

/// Metadata for a single foreign key constraint on a table.
#[derive(Debug, Clone)]
pub struct ForeignKeyInfo {
    pub name: String,
    pub cols: Vec<String>,
    pub ref_table: String,
    pub ref_cols: Vec<String>,
}

macro_rules! raw_is_null {
    ($row:expr, $idx:expr) => {{
        use sqlx::ValueRef;
        $row.try_get_raw($idx).map(|v| v.is_null()).unwrap_or(false)
    }};
}

/// Decode a MySQL column as a String, falling back to raw bytes (utf8) for
/// columns returned as binary (e.g. `SHOW DATABASES`/`SHOW TABLES` on some
/// servers return VARBINARY).
fn mysql_string(row: &sqlx::mysql::MySqlRow, idx: usize) -> String {
    if let Ok(s) = row.try_get::<String, _>(idx) {
        return s;
    }
    if let Ok(b) = row.try_get::<Vec<u8>, _>(idx) {
        return bytes_to_display(&b);
    }
    String::new()
}

fn bytes_to_display(v: &[u8]) -> String {
    match std::str::from_utf8(v) {
        Ok(s) => s.to_string(),
        Err(_) => v.iter().map(|b| format!("{b:02x}")).collect(),
    }
}

fn decode_mysql(row: &sqlx::mysql::MySqlRow, idx: usize) -> String {
    if raw_is_null!(row, idx) {
        return "NULL".into();
    }
    let ty = row.columns()[idx].type_info().name().to_ascii_uppercase();
    match ty.as_str() {
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" => {
            if let Ok(v) = row.try_get::<i64, _>(idx) {
                return v.to_string();
            }
        }
        "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED"
        | "BIGINT UNSIGNED" => {
            if let Ok(v) = row.try_get::<u64, _>(idx) {
                return v.to_string();
            }
        }
        "BOOLEAN" => {
            if let Ok(v) = row.try_get::<bool, _>(idx) {
                return v.to_string();
            }
        }
        "FLOAT" | "DOUBLE" => {
            if let Ok(v) = row.try_get::<f64, _>(idx) {
                return v.to_string();
            }
        }
        "DECIMAL" | "NUMERIC" => {
            if let Ok(v) = row.try_get::<bigdecimal::BigDecimal, _>(idx) {
                return v.to_string();
            }
        }
        "DATE" => {
            if let Ok(v) = row.try_get::<chrono::NaiveDate, _>(idx) {
                return v.to_string();
            }
        }
        "TIME" => {
            if let Ok(v) = row.try_get::<chrono::NaiveTime, _>(idx) {
                return v.to_string();
            }
        }
        "DATETIME" => {
            if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
                return v.to_string();
            }
        }
        "TIMESTAMP" => {
            if let Ok(v) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(idx) {
                return v.to_rfc3339();
            }
            if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
                return v.to_string();
            }
        }
        "JSON" => {
            if let Ok(v) = row.try_get::<serde_json::Value, _>(idx) {
                return v.to_string();
            }
        }
        "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BINARY" | "VARBINARY" => {
            if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
                return bytes_to_display(&v);
            }
        }
        "CHAR" | "VARCHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" | "SET" => {
            if let Ok(v) = row.try_get::<String, _>(idx) {
                return v;
            }
        }
        _ => {}
    }
    // Fallback probe ladder — bool moved after numerics so integer columns
    // with unknown type names don't get stringified as true/false.
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return v;
    }
    if let Ok(v) = row.try_get::<i64, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<u64, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<bigdecimal::BigDecimal, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::NaiveDate, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::NaiveTime, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(idx) {
        return v.to_rfc3339();
    }
    if let Ok(v) = row.try_get::<serde_json::Value, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<bool, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
        return bytes_to_display(&v);
    }
    "?".into()
}

fn decode_pg(row: &sqlx::postgres::PgRow, idx: usize) -> String {
    if raw_is_null!(row, idx) {
        return "NULL".into();
    }
    let ty = row.columns()[idx].type_info().name().to_ascii_uppercase();
    match ty.as_str() {
        "BOOL" => {
            if let Ok(v) = row.try_get::<bool, _>(idx) {
                return v.to_string();
            }
        }
        "INT2" => {
            if let Ok(v) = row.try_get::<i16, _>(idx) {
                return v.to_string();
            }
        }
        "INT4" => {
            if let Ok(v) = row.try_get::<i32, _>(idx) {
                return v.to_string();
            }
        }
        "INT8" => {
            if let Ok(v) = row.try_get::<i64, _>(idx) {
                return v.to_string();
            }
        }
        "FLOAT4" => {
            if let Ok(v) = row.try_get::<f32, _>(idx) {
                return v.to_string();
            }
        }
        "FLOAT8" => {
            if let Ok(v) = row.try_get::<f64, _>(idx) {
                return v.to_string();
            }
        }
        "NUMERIC" => {
            if let Ok(v) = row.try_get::<bigdecimal::BigDecimal, _>(idx) {
                return v.to_string();
            }
        }
        "UUID" => {
            if let Ok(v) = row.try_get::<uuid::Uuid, _>(idx) {
                return v.to_string();
            }
        }
        "DATE" => {
            if let Ok(v) = row.try_get::<chrono::NaiveDate, _>(idx) {
                return v.to_string();
            }
        }
        "TIME" => {
            if let Ok(v) = row.try_get::<chrono::NaiveTime, _>(idx) {
                return v.to_string();
            }
        }
        "TIMESTAMP" => {
            if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
                return v.to_string();
            }
        }
        "TIMESTAMPTZ" => {
            if let Ok(v) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(idx) {
                return v.to_rfc3339();
            }
        }
        "JSON" | "JSONB" => {
            if let Ok(v) = row.try_get::<serde_json::Value, _>(idx) {
                return v.to_string();
            }
        }
        "BYTEA" => {
            if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
                return bytes_to_display(&v);
            }
        }
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CITEXT" => {
            if let Ok(v) = row.try_get::<String, _>(idx) {
                return v;
            }
        }
        _ => {}
    }
    // Fallback probe ladder — bool moved after numerics.
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return v;
    }
    if let Ok(v) = row.try_get::<i64, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<i32, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<i16, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<f32, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<bigdecimal::BigDecimal, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<uuid::Uuid, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::NaiveDate, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::NaiveTime, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(idx) {
        return v.to_rfc3339();
    }
    if let Ok(v) = row.try_get::<serde_json::Value, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<bool, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
        return bytes_to_display(&v);
    }
    "?".into()
}

fn decode_sqlite(row: &sqlx::sqlite::SqliteRow, idx: usize) -> String {
    if raw_is_null!(row, idx) {
        return "NULL".into();
    }
    let ty = row.columns()[idx].type_info().name().to_ascii_uppercase();
    match ty.as_str() {
        "INTEGER" => {
            if let Ok(v) = row.try_get::<i64, _>(idx) {
                return v.to_string();
            }
        }
        "REAL" => {
            if let Ok(v) = row.try_get::<f64, _>(idx) {
                return v.to_string();
            }
        }
        "TEXT" => {
            if let Ok(v) = row.try_get::<String, _>(idx) {
                return v;
            }
        }
        "BLOB" => {
            if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
                return bytes_to_display(&v);
            }
        }
        "BOOLEAN" => {
            if let Ok(v) = row.try_get::<bool, _>(idx) {
                return v.to_string();
            }
        }
        _ => {}
    }
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return v;
    }
    if let Ok(v) = row.try_get::<i64, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<bool, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
        return bytes_to_display(&v);
    }
    "?".into()
}

#[cfg(feature = "duckdb")]
fn duck_value_to_string(v: duckdb::types::Value) -> String {
    use duckdb::types::Value;
    match v {
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::TinyInt(n) => n.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::HugeInt(n) => n.to_string(),
        Value::UTinyInt(n) => n.to_string(),
        Value::USmallInt(n) => n.to_string(),
        Value::UInt(n) => n.to_string(),
        Value::UBigInt(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(f) => f.to_string(),
        Value::Decimal(d) => d.to_string(),
        Value::Text(s) => s,
        Value::Blob(b) => b.iter().map(|byte| format!("{byte:02x}")).collect(),
        other => format!("{other:?}"),
    }
}

/// Rows added automatically when a SELECT/WITH query has no LIMIT clause.
pub const DEFAULT_ROW_LIMIT: usize = 100;

/// Returns the leading uppercase keyword if `query` is a non-row
/// statement (DML / DDL / transaction control / etc), else `None`.
/// Used by `execute` to dispatch to sqlx's `execute()` and surface
/// rows_affected instead of an empty result set.
///
/// Row-returning verbs we leave for the fetch_all path:
/// SELECT, WITH, VALUES, SHOW, EXPLAIN, DESC[RIBE], TABLE, PRAGMA.
/// Anything else with a recognisable verb is treated as non-row.
/// An unrecognisable / empty query falls through to fetch_all so
/// sqlx surfaces its own parse error.
fn non_query_verb(query: &str) -> Option<String> {
    let stripped = skip_leading_whitespace_and_comments(query.trim_start());
    let kw = leading_keyword(stripped)?.to_ascii_uppercase();
    let row_returning = matches!(
        kw.as_str(),
        "SELECT"
            | "WITH"
            | "VALUES"
            | "SHOW"
            | "EXPLAIN"
            | "DESC"
            | "DESCRIBE"
            | "TABLE"
            | "PRAGMA"
    );
    if row_returning { None } else { Some(kw) }
}

/// If `query` is a top-level SELECT or WITH statement with no LIMIT clause,
/// return a rewritten query with ` LIMIT <limit>` appended. Returns `None`
/// when the query already limits itself or isn't a row-producing statement.
pub fn apply_default_limit(query: &str, limit: usize) -> Option<String> {
    let trimmed = strip_trailing_semicolons(query).trim();
    if trimmed.is_empty() {
        return None;
    }
    let after_comments = skip_leading_whitespace_and_comments(trimmed);
    let first_kw = leading_keyword(after_comments)?.to_ascii_uppercase();
    if first_kw != "SELECT" && first_kw != "WITH" {
        return None;
    }
    if has_top_level_keyword(trimmed, "LIMIT")
        || has_top_level_keyword(trimmed, "FETCH")
        || has_top_level_keyword(trimmed, "TOP")
    {
        return None;
    }
    Some(format!("{trimmed} LIMIT {limit}"))
}

fn strip_trailing_semicolons(q: &str) -> &str {
    q.trim_end().trim_end_matches(';').trim_end()
}

fn skip_leading_whitespace_and_comments(mut s: &str) -> &str {
    loop {
        let before = s;
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map(|(_, r)| r).unwrap_or("");
        }
        if s == before {
            return s;
        }
    }
}

fn leading_keyword(s: &str) -> Option<&str> {
    let end = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_alphabetic())
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    if end == 0 { None } else { Some(&s[..end]) }
}

/// Scan `q` for `needle` (case-insensitive, whole word) appearing at
/// paren-depth 0 and outside of string/identifier literals and comments.
fn has_top_level_keyword(q: &str, needle: &str) -> bool {
    let bytes = q.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    let mut depth: i32 = 0;
    while i < n {
        let b = bytes[i];
        match b {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
            }
            b'\'' | b'"' | b'`' => {
                let quote = b;
                i += 1;
                while i < n {
                    if bytes[i] == b'\\' && i + 1 < n {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if i + 1 < n && bytes[i + 1] == b'-' => {
                while i < n && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < n && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(n);
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < n && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                if depth == 0 && q[start..i].eq_ignore_ascii_case(needle) {
                    return true;
                }
            }
            _ => i += 1,
        }
    }
    false
}

#[cfg(test)]
mod limit_tests {
    use super::*;

    fn apply(q: &str) -> Option<String> {
        apply_default_limit(q, 100)
    }

    #[test]
    fn appends_to_bare_select() {
        assert_eq!(
            apply("SELECT * FROM t"),
            Some("SELECT * FROM t LIMIT 100".into())
        );
    }

    #[test]
    fn strips_trailing_semicolon_before_appending() {
        assert_eq!(
            apply("select id from users;"),
            Some("select id from users LIMIT 100".into())
        );
    }

    #[test]
    fn leaves_query_that_already_limits() {
        assert_eq!(apply("SELECT * FROM t LIMIT 5"), None);
        assert_eq!(apply("select * from t limit 5 offset 10"), None);
    }

    #[test]
    fn ignores_limit_inside_subquery_paren() {
        let q = "SELECT * FROM (SELECT id FROM t LIMIT 5) x";
        assert_eq!(
            apply(q),
            Some("SELECT * FROM (SELECT id FROM t LIMIT 5) x LIMIT 100".into())
        );
    }

    #[test]
    fn ignores_limit_inside_string_literal() {
        assert!(apply("SELECT 'has LIMIT in string' AS x").is_some());
    }

    #[test]
    fn handles_with_cte() {
        let q = "WITH x AS (SELECT 1) SELECT * FROM x";
        assert_eq!(
            apply(q),
            Some("WITH x AS (SELECT 1) SELECT * FROM x LIMIT 100".into())
        );
    }

    #[test]
    fn skips_non_select() {
        assert_eq!(apply("INSERT INTO t VALUES (1)"), None);
        assert_eq!(apply("UPDATE t SET x = 1"), None);
        assert_eq!(apply("DELETE FROM t"), None);
        assert_eq!(apply("EXPLAIN SELECT * FROM t"), None);
    }

    #[test]
    fn skips_leading_comments() {
        let q = "-- fetch users\nSELECT * FROM users";
        let out = apply(q).unwrap();
        assert!(out.ends_with(" LIMIT 100"));
        assert!(out.contains("SELECT * FROM users"));
    }

    #[test]
    fn leaves_query_with_fetch_first() {
        // FETCH FIRST N ROWS ONLY is SQL standard pagination — don't double-limit
        assert_eq!(apply("SELECT * FROM t FETCH FIRST 10 ROWS ONLY"), None);
        assert_eq!(apply("SELECT * FROM t FETCH NEXT 5 ROWS ONLY"), None);
    }

    #[test]
    fn leaves_query_with_top() {
        // TOP N is SQL Server/T-SQL pagination — don't double-limit
        assert_eq!(apply("SELECT TOP 10 * FROM t"), None);
        assert_eq!(apply("select top 100 id from users"), None);
    }
}

#[cfg(all(test, feature = "duckdb"))]
mod duckdb_tests {
    use super::*;

    #[tokio::test]
    async fn duckdb_connect_in_memory() {
        let conn = DbConnection::connect("duckdb::memory:", None)
            .await
            .unwrap();
        assert!(conn.is_duckdb());
        assert!(!conn.is_sqlite());
    }

    #[tokio::test]
    async fn duckdb_roundtrip_create_insert_select() {
        let conn = DbConnection::connect("duckdb::memory:", None)
            .await
            .unwrap();
        let create = conn
            .execute("CREATE TABLE items (id INTEGER, name TEXT)")
            .await
            .unwrap();
        assert!(matches!(create, ExecOutcome::NonQuery { .. }));
        let insert = conn
            .execute("INSERT INTO items VALUES (1, 'alpha'), (2, 'beta')")
            .await
            .unwrap();
        assert!(matches!(
            insert,
            ExecOutcome::NonQuery {
                rows_affected: 2,
                ..
            }
        ));
        let select = conn
            .execute("SELECT id, name FROM items ORDER BY id")
            .await
            .unwrap();
        let ExecOutcome::Rows(qr) = select else {
            panic!("expected rows")
        };
        assert_eq!(qr.columns, vec!["id", "name"]);
        assert_eq!(qr.rows.len(), 2);
        assert_eq!(qr.rows[0], vec!["1", "alpha"]);
        assert_eq!(qr.rows[1], vec!["2", "beta"]);
    }

    #[tokio::test]
    async fn duckdb_list_databases() {
        let conn = DbConnection::connect("duckdb::memory:", None)
            .await
            .unwrap();
        let dbs = conn.list_databases().await.unwrap();
        assert_eq!(dbs, vec!["main"]);
    }

    #[tokio::test]
    async fn duckdb_list_tables() {
        let conn = DbConnection::connect("duckdb::memory:", None)
            .await
            .unwrap();
        conn.execute("CREATE TABLE alpha (x INTEGER)")
            .await
            .unwrap();
        conn.execute("CREATE TABLE beta (y TEXT)").await.unwrap();
        let tables = conn.list_tables("main").await.unwrap();
        assert!(tables.contains(&"alpha".to_string()), "tables: {tables:?}");
        assert!(tables.contains(&"beta".to_string()), "tables: {tables:?}");
    }

    #[tokio::test]
    async fn duckdb_list_columns() {
        let conn = DbConnection::connect("duckdb::memory:", None)
            .await
            .unwrap();
        conn.execute("CREATE TABLE people (id INTEGER, name TEXT, score DOUBLE)")
            .await
            .unwrap();
        let cols = conn.list_columns("main", "people").await.unwrap();
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "name", "score"]);
    }

    #[tokio::test]
    async fn duckdb_csv_read() {
        let dir = tempfile::tempdir().unwrap();
        let csv_path = dir.path().join("data.csv");
        std::fs::write(&csv_path, "id,name\n1,alice\n2,bob\n").unwrap();
        let conn = DbConnection::connect("duckdb::memory:", None)
            .await
            .unwrap();
        let q = format!("SELECT * FROM read_csv_auto('{}')", csv_path.display());
        let result = conn.execute(&q).await.unwrap();
        let ExecOutcome::Rows(qr) = result else {
            panic!("expected rows")
        };
        assert_eq!(qr.columns, vec!["id", "name"]);
        assert_eq!(qr.rows.len(), 2);
    }

    #[tokio::test]
    async fn duckdb_null_renders_as_null_string() {
        let conn = DbConnection::connect("duckdb::memory:", None)
            .await
            .unwrap();
        conn.execute("CREATE TABLE nulls (v TEXT)").await.unwrap();
        conn.execute("INSERT INTO nulls VALUES (NULL)")
            .await
            .unwrap();
        let result = conn.execute("SELECT v FROM nulls").await.unwrap();
        let ExecOutcome::Rows(qr) = result else {
            panic!("expected rows")
        };
        assert_eq!(qr.rows[0][0], "NULL");
    }

    #[tokio::test]
    async fn connect_duckdb_ignores_tls() {
        use sqeel_config::{TlsConfig, TlsVerifyMode};
        let tls = TlsConfig {
            ca_cert: Some(std::path::PathBuf::from("/nonexistent/ca.pem")),
            client_cert: None,
            client_key: None,
            verify_mode: Some(TlsVerifyMode::Full),
        };
        // DuckDB silently ignores TLS; connection must succeed.
        let conn = DbConnection::connect("duckdb::memory:", Some(&tls))
            .await
            .unwrap();
        assert!(conn.is_duckdb());
    }
}

#[cfg(test)]
mod tls_tests {
    use super::*;
    use sqeel_config::{TlsConfig, TlsVerifyMode};

    #[tokio::test]
    async fn connect_mysql_with_tls_skip() {
        let tls = TlsConfig {
            ca_cert: None,
            client_cert: None,
            client_key: None,
            verify_mode: Some(TlsVerifyMode::Skip),
        };
        // Bogus host — connection must fail (not panic) with TLS options applied.
        let err = DbConnection::connect("mysql://user:pass@127.0.0.1:19999/db", Some(&tls))
            .await
            .map(|_| ())
            .unwrap_err();
        // Any connection-level error is acceptable; what matters is it didn't panic.
        let msg = err.to_string().to_lowercase();
        assert!(
            matches!(
                err.kind,
                ConnectErrorKind::Network | ConnectErrorKind::Tls | ConnectErrorKind::Other
            ),
            "unexpected error kind for mysql tls skip: {msg}"
        );
    }

    #[tokio::test]
    async fn connect_postgres_with_tls_full_and_paths() {
        let tls = TlsConfig {
            ca_cert: Some(std::path::PathBuf::from("/nonexistent/ca.pem")),
            client_cert: Some(std::path::PathBuf::from("/nonexistent/client.crt")),
            client_key: Some(std::path::PathBuf::from("/nonexistent/client.key")),
            verify_mode: Some(TlsVerifyMode::Full),
        };
        // Bogus CA path — sqlx must reject before even attempting TCP.
        let err = DbConnection::connect("postgres://user:pass@127.0.0.1:19999/db", Some(&tls))
            .await
            .map(|_| ())
            .unwrap_err();
        // Accept Config (file-not-found at options build) or Network/Tls (connect-time).
        assert!(
            matches!(
                err.kind,
                ConnectErrorKind::Config
                    | ConnectErrorKind::Tls
                    | ConnectErrorKind::Network
                    | ConnectErrorKind::Other
            ),
            "unexpected error kind for postgres tls full: {}",
            err
        );
    }

    #[tokio::test]
    async fn connect_sqlite_ignores_tls() {
        let tls = TlsConfig {
            ca_cert: Some(std::path::PathBuf::from("/nonexistent/ca.pem")),
            client_cert: None,
            client_key: None,
            verify_mode: Some(TlsVerifyMode::Full),
        };
        // SQLite silently ignores TLS; in-memory connection must succeed.
        let conn = DbConnection::connect("sqlite::memory:", Some(&tls))
            .await
            .unwrap();
        assert!(conn.is_sqlite());
    }
}
