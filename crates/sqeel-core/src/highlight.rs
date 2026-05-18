use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};
use tree_sitter::Parser;

use hjkl_bonsai::runtime::{
    AsyncGrammarLoader, Grammar, GrammarLoader, GrammarRegistry, LoadHandle,
};
use hjkl_bonsai::{HighlightSpan as InnerSpan, ParseError as InnerError};

// ── process-wide async loader ─────────────────────────────────────────────────

/// One `AsyncGrammarLoader` per process. Built lazily on first call to
/// `sql_grammar()` or `sql_grammar_blocking()`. The 2 worker threads it spawns
/// live for the rest of the process — that's fine.
static ASYNC_LOADER: OnceLock<AsyncGrammarLoader> = OnceLock::new();

fn async_loader() -> anyhow::Result<&'static AsyncGrammarLoader> {
    if let Some(al) = ASYNC_LOADER.get() {
        return Ok(al);
    }
    let registry = GrammarRegistry::embedded()?;
    let loader = GrammarLoader::user_default(registry.meta())?;
    let al = AsyncGrammarLoader::new(loader);
    // OnceLock::get_or_try_init is unstable; use set + get.
    let _ = ASYNC_LOADER.set(al);
    Ok(ASYNC_LOADER.get().expect("just set"))
}

// ── grammar cache + in-flight handle ─────────────────────────────────────────

/// Fully resolved grammar. Populated once the async load finishes.
static SQL_GRAMMAR: Mutex<Option<Arc<Grammar>>> = Mutex::new(None);

/// In-flight `LoadHandle` from `load_async`. Taken out (→ `None`) the moment
/// the handle resolves so subsequent ticks hit the cached `SQL_GRAMMAR` fast-path.
static SQL_GRAMMAR_HANDLE: Mutex<Option<LoadHandle>> = Mutex::new(None);

// ── helpers ───────────────────────────────────────────────────────────────────

/// Build the `(spec, meta)` pair needed to enqueue a load. Cheap — only reads
/// the embedded registry bytes; no I/O.
fn sql_spec_and_meta() -> anyhow::Result<(
    hjkl_bonsai::runtime::LangSpec,
    hjkl_bonsai::runtime::ManifestMeta,
)> {
    let registry = GrammarRegistry::embedded()?;
    let spec = registry
        .by_name("sql")
        .ok_or_else(|| anyhow::anyhow!("sql language not found in hjkl-bonsai registry"))?
        .clone();
    let meta = registry.meta().clone();
    Ok((spec, meta))
}

/// Materialise a `Grammar` from the path returned by the async loader (the `.so`
/// path, not its parent directory).
fn grammar_from_path(so_path: std::path::PathBuf) -> anyhow::Result<Arc<Grammar>> {
    Ok(Arc::new(Grammar::load_from_path("sql", &so_path)?))
}

// ── public API ────────────────────────────────────────────────────────────────

/// Non-blocking grammar accessor used by the TUI render loop.
///
/// - First call: kicks off the background clone+compile and returns `None`.
/// - Subsequent calls while pending: polls the handle; returns `None` when
///   still in-flight, caches + returns `Some(arc)` once it resolves.
/// - After caching: cheap fast-return of `Some(arc.clone())`.
/// - On load error: logs via `tracing::warn!` and returns `None` so callers
///   fall back to plain-text rendering gracefully. No retry this round.
pub fn sql_grammar() -> Option<Arc<Grammar>> {
    // Fast path: grammar already cached.
    {
        let guard = SQL_GRAMMAR.lock().ok()?;
        if let Some(g) = guard.as_ref() {
            return Some(g.clone());
        }
    }

    // Poll or kick off the async load.
    let mut handle_guard = SQL_GRAMMAR_HANDLE.lock().ok()?;

    if let Some(handle) = handle_guard.as_ref() {
        // Handle exists — poll for completion.
        match handle.try_recv() {
            None => return None, // still in-flight
            Some(Ok(so_path)) => {
                // Resolved — materialise Grammar, cache, return.
                *handle_guard = None;
                drop(handle_guard);
                match grammar_from_path(so_path) {
                    Ok(g) => {
                        let mut grammar_guard = SQL_GRAMMAR.lock().ok()?;
                        *grammar_guard = Some(g.clone());
                        return Some(g);
                    }
                    Err(e) => {
                        tracing::warn!("sql grammar materialise failed: {e:#}");
                        return None;
                    }
                }
            }
            Some(Err(e)) => {
                tracing::warn!("sql grammar async load failed: {e}");
                *handle_guard = None;
                return None;
            }
        }
    }

    // No handle yet — kick off async load.
    match (async_loader(), sql_spec_and_meta()) {
        (Ok(al), Ok((spec, meta))) => {
            *handle_guard = Some(al.load_async("sql".into(), spec, meta));
        }
        (Err(e), _) | (_, Err(e)) => {
            tracing::warn!("sql grammar async loader init failed: {e:#}");
        }
    }
    None
}

/// Blocking variant for tests and CLI one-shots. Waits until the grammar is
/// ready (may take 1–3 s on first run while the grammar is cloned + compiled).
/// Production render loops should use [`sql_grammar`] (non-blocking) instead.
pub fn sql_grammar_blocking() -> anyhow::Result<Arc<Grammar>> {
    // Fast path: grammar already cached.
    {
        let guard = SQL_GRAMMAR
            .lock()
            .map_err(|_| anyhow::anyhow!("sql grammar mutex poisoned"))?;
        if let Some(g) = guard.as_ref() {
            return Ok(g.clone());
        }
    }

    let (spec, meta) = sql_spec_and_meta()?;
    let al = async_loader()?;

    // Check if an in-flight handle exists; if so take it, else kick a new one.
    let handle = {
        let mut handle_guard = SQL_GRAMMAR_HANDLE
            .lock()
            .map_err(|_| anyhow::anyhow!("sql grammar handle mutex poisoned"))?;
        handle_guard
            .take()
            .unwrap_or_else(|| al.load_async("sql".into(), spec, meta))
    };

    let so_path = handle
        .recv_blocking()
        .map_err(|e| anyhow::anyhow!("sql grammar load failed: {e}"))?;
    let g = grammar_from_path(so_path)?;

    let mut grammar_guard = SQL_GRAMMAR
        .lock()
        .map_err(|_| anyhow::anyhow!("sql grammar mutex poisoned"))?;
    *grammar_guard = Some(g.clone());
    Ok(g)
}

/// SQL dialect the current connection is speaking. Drives per-dialect
/// keyword promotion in the highlighter so things like `ILIKE` show as
/// keywords on Postgres, `AUTO_INCREMENT` on MySQL, `PRAGMA` on SQLite,
/// etc. `Generic` means no dialect-specific extras — useful before any
/// connection has been established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    #[default]
    Generic,
    MySql,
    Postgres,
    Sqlite,
}

impl Dialect {
    /// Pick a dialect from a sqlx-style URL scheme, matching the
    /// dispatch in `DbConnection::connect`.
    pub fn from_url(url: &str) -> Self {
        if url.starts_with("mysql://") || url.starts_with("mariadb://") {
            Dialect::MySql
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            Dialect::Postgres
        } else if url.starts_with("sqlite://") || url.starts_with("sqlite:") {
            Dialect::Sqlite
        } else {
            Dialect::Generic
        }
    }

    /// Extra identifiers that should render as keywords in this dialect,
    /// but aren't part of the shared tree-sitter-sequel keyword set.
    /// Compared case-insensitively against the token text.
    fn extra_keywords(self) -> &'static [&'static str] {
        match self {
            Dialect::MySql => &[
                "AUTO_INCREMENT",
                "ENGINE",
                "CHARSET",
                "COLLATE",
                "ZEROFILL",
                "UNSIGNED",
                "ROW_FORMAT",
                "KEY_BLOCK_SIZE",
                "DELAYED",
                "STRAIGHT_JOIN",
                "SQL_CALC_FOUND_ROWS",
                "LOW_PRIORITY",
                "HIGH_PRIORITY",
                "IGNORE",
            ],
            Dialect::Postgres => &[
                "ILIKE",
                "RETURNING",
                "SERIAL",
                "BIGSERIAL",
                "SMALLSERIAL",
                "BYTEA",
                "JSONB",
                "TSQUERY",
                "TSVECTOR",
                "GENERATED",
                "MATERIALIZED",
                "LATERAL",
                "DISTINCT",
                "CONCURRENTLY",
                "SIMILAR",
                "OVERLAPS",
            ],
            Dialect::Sqlite => &[
                "PRAGMA",
                "AUTOINCREMENT",
                "WITHOUT",
                "ROWID",
                "VACUUM",
                "GLOB",
                "ATTACH",
                "DETACH",
                "REINDEX",
                "SAVEPOINT",
            ],
            Dialect::Generic => &[],
        }
    }

    /// True iff `text` (any case) is one of this dialect's extra keywords
    /// OR a native-statement-start keyword.
    fn is_extra_keyword(self, text: &str) -> bool {
        self.extra_keywords()
            .iter()
            .chain(self.native_statement_starts().iter())
            .any(|kw| kw.eq_ignore_ascii_case(text))
    }

    /// Statement-start tokens that tree-sitter-sequel doesn't parse as
    /// valid statements but that the target engine accepts natively.
    fn native_statement_starts(self) -> &'static [&'static str] {
        match self {
            Dialect::MySql => &[
                "DESC",
                "DESCRIBE",
                "SHOW",
                "EXPLAIN",
                "USE",
                "ANALYZE",
                "OPTIMIZE",
                "REPAIR",
                "CHECK",
                "FLUSH",
                "KILL",
                "RENAME",
                "SET",
                "START",
                "COMMIT",
                "ROLLBACK",
                "SAVEPOINT",
                "LOAD",
                "GRANT",
                "REVOKE",
                "CALL",
            ],
            Dialect::Postgres => &[
                "EXPLAIN",
                "ANALYZE",
                "VACUUM",
                "CLUSTER",
                "COPY",
                "LISTEN",
                "NOTIFY",
                "UNLISTEN",
                "REINDEX",
                "REFRESH",
                "SET",
                "SHOW",
                "RESET",
                "BEGIN",
                "COMMIT",
                "ROLLBACK",
                "SAVEPOINT",
                "GRANT",
                "REVOKE",
                "CALL",
            ],
            Dialect::Sqlite => &[
                "PRAGMA",
                "VACUUM",
                "ATTACH",
                "DETACH",
                "REINDEX",
                "ANALYZE",
                "EXPLAIN",
                "BEGIN",
                "COMMIT",
                "ROLLBACK",
                "SAVEPOINT",
                "RELEASE",
            ],
            Dialect::Generic => &[],
        }
    }

    /// True iff `stmt`'s first non-comment token is one of this dialect's
    /// engine-native statement starts.
    pub fn is_native_statement(self, stmt: &str) -> bool {
        let stripped = strip_sql_comments(stmt);
        let trimmed = stripped.trim_start();
        let first_word: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_alphabetic() || *c == '_')
            .collect();
        if first_word.is_empty() {
            return false;
        }
        self.native_statement_starts()
            .iter()
            .any(|w| w.eq_ignore_ascii_case(&first_word))
    }
}

/// True iff `capture` should render as a SQL keyword in sqeel's colour scheme.
///
/// tree-sitter-sequel assigns `@keyword` to primary SQL keywords and `@attribute`
/// to modifier keywords (ASC, DESC, AUTO_INCREMENT, DEFAULT, COLLATE, ENGINE, …).
/// Both groups are "keywords" from the user's perspective — bold, same colour.
/// `@storageclass` (TEMP, MATERIALIZED, …) and `@boolean` (TRUE/FALSE) likewise.
pub fn is_sql_keyword_capture(capture: &str) -> bool {
    capture.starts_with("keyword")
        || capture == "attribute"
        || capture == "storageclass"
        || capture == "boolean"
}

/// A highlight span enriched with row/column information for the TUI renderer.
///
/// The capture name replaces the old `TokenKind` enum. Map it to a style with
/// [`capture_style`](crate::highlight) or by matching prefixes:
/// `"keyword"` → bold magenta, `"string"` → green, `"comment"` → italic grey, etc.
#[derive(Debug, Clone)]
pub struct HighlightSpan {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
    /// Tree-sitter capture name, e.g. `"keyword"`, `"string"`, `"comment"`.
    pub capture: String,
}

/// A parse error span with row/column for inline diagnostic underlines.
#[derive(Debug, Clone)]
pub struct ParseError {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_row: usize,
    pub start_col: usize,
    pub end_row: usize,
    pub end_col: usize,
    pub message: String,
}

/// Thin wrapper around `hjkl_bonsai::Highlighter` that:
/// - adds dialect-specific keyword promotion for SQL.
/// - enriches spans with row/col from byte offsets.
/// - caches last errors and block ranges.
///
/// The inner `hjkl_bonsai::Highlighter` is held as `Option` so the wrapper can
/// be constructed before the grammar is ready. When `None`, all highlight calls
/// return empty span lists (plain-text rendering) without panicking.
pub struct Highlighter {
    inner: Option<hjkl_bonsai::Highlighter>,
    last_errors: Vec<ParseError>,
    last_block_ranges: Vec<(usize, usize)>,
}

impl Highlighter {
    /// Construct a `Highlighter`, loading the grammar synchronously.
    ///
    /// Suitable for tests and CLI one-shots where blocking is acceptable.
    /// In the TUI render loop use `Highlighter::new_async` so the first
    /// grammar load does not freeze the UI.
    pub fn new() -> anyhow::Result<Self> {
        let grammar = sql_grammar_blocking()?;
        let inner = hjkl_bonsai::Highlighter::new(grammar)?;
        Ok(Self {
            inner: Some(inner),
            last_errors: Vec::new(),
            last_block_ranges: Vec::new(),
        })
    }

    /// Construct a `Highlighter` using the non-blocking grammar accessor.
    ///
    /// Returns a `Highlighter` immediately regardless of whether the grammar
    /// is ready yet. When the grammar is not ready, `inner` is `None` and
    /// highlight calls return empty spans (plain-text fallback). The caller
    /// should call `try_upgrade()` each tick to attach the grammar once it
    /// resolves. Suitable for the TUI render loop.
    pub fn new_async() -> Self {
        let inner = sql_grammar().and_then(|g| hjkl_bonsai::Highlighter::new(g).ok());
        Self {
            inner,
            last_errors: Vec::new(),
            last_block_ranges: Vec::new(),
        }
    }

    /// Returns `true` if the inner `hjkl_bonsai::Highlighter` is present.
    pub fn is_ready(&self) -> bool {
        self.inner.is_some()
    }

    /// Poll the non-blocking grammar cache and attach the inner highlighter
    /// if it resolved since the last call. Cheap no-op once ready.
    pub fn try_upgrade(&mut self) {
        if self.inner.is_some() {
            return;
        }
        if let Some(g) = sql_grammar()
            && let Ok(h) = hjkl_bonsai::Highlighter::new(g)
        {
            self.inner = Some(h);
        }
    }

    /// Apply an `InputEdit` to the retained tree. Delegates to
    /// `hjkl_bonsai::Highlighter::edit`.
    pub fn edit(&mut self, edit: &tree_sitter::InputEdit) {
        if let Some(inner) = self.inner.as_mut() {
            inner.edit(edit);
        }
    }

    /// Cold parse `source` from scratch into the retained tree.
    pub fn parse_initial(&mut self, source: &str) {
        if let Some(inner) = self.inner.as_mut() {
            inner.parse_initial(source.as_bytes());
        }
    }

    /// Reparse `source` against the retained tree under the configured
    /// timeout. Returns `false` on timeout (callers must skip the
    /// highlight pass for this frame). Returns `true` when grammar is not
    /// yet available (no tree to invalidate — callers may proceed normally).
    pub fn parse_incremental(&mut self, source: &str) -> bool {
        self.inner
            .as_mut()
            .map(|inner| inner.parse_incremental(source.as_bytes()))
            .unwrap_or(true)
    }

    /// Drop the retained tree.
    pub fn reset(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            inner.reset();
        }
    }

    /// Read accessor for the retained tree.
    pub fn tree(&self) -> Option<&tree_sitter::Tree> {
        self.inner.as_ref().and_then(|inner| inner.tree())
    }

    /// Run the highlights query against the retained tree, scoped to
    /// `byte_range`. Enriches inner spans with row/col, runs dialect
    /// keyword promotion across the same byte_range, and refreshes the
    /// cached `last_errors` (range-scoped) + `last_block_ranges` (full
    /// retained tree).
    ///
    /// Returns an empty `Vec` when the grammar is not yet ready.
    pub fn highlight_range(
        &mut self,
        source: &str,
        dialect: Dialect,
        byte_range: Range<usize>,
    ) -> Vec<HighlightSpan> {
        let inner = match self.inner.as_mut() {
            Some(i) => i,
            None => {
                self.last_errors.clear();
                self.last_block_ranges.clear();
                return vec![];
            }
        };
        let bytes = source.as_bytes();
        let mut inner_spans = inner.highlight_range(bytes, byte_range.clone());
        hjkl_bonsai::CommentMarkerPass::new().apply(&mut inner_spans, bytes);

        // Build newline offsets once and reuse across all per-span row/col
        // lookups (O(N) build + O(log N) per lookup vs O(N) per lookup).
        let nl_offsets = compute_newline_offsets(source);

        let mut spans: Vec<HighlightSpan> = inner_spans
            .into_iter()
            .map(|s| {
                let (start_row, start_col) =
                    byte_to_rowcol_with_offsets(&nl_offsets, s.byte_range.start);
                let (end_row, end_col) = byte_to_rowcol_with_offsets(&nl_offsets, s.byte_range.end);
                HighlightSpan {
                    start_byte: s.byte_range.start,
                    end_byte: s.byte_range.end,
                    start_row,
                    start_col,
                    end_row,
                    end_col,
                    capture: s.capture,
                }
            })
            .collect();

        promote_uncovered_dialect_keywords_in_range(
            source,
            dialect,
            byte_range.clone(),
            &mut spans,
        );

        let inner_errors = inner.parse_errors_range(bytes, byte_range);
        self.last_errors = inner_errors
            .into_iter()
            .filter_map(|e| {
                let start_byte = e.byte_range.start;
                let end_byte = e.byte_range.end;
                if let Some(slice) = source.get(start_byte..end_byte)
                    && dialect.is_native_statement(slice.trim_start())
                {
                    return None;
                }
                let (start_row, start_col) = byte_to_rowcol_with_offsets(&nl_offsets, start_byte);
                let (end_row, end_col) = byte_to_rowcol_with_offsets(&nl_offsets, end_byte);
                Some(ParseError {
                    start_byte,
                    end_byte,
                    start_row,
                    start_col,
                    end_row,
                    end_col,
                    message: e.message,
                })
            })
            .collect();

        if let Some(tree) = self.inner.as_ref().and_then(|i| i.tree()) {
            let mut block_ranges = Vec::new();
            collect_block_ranges(tree.root_node(), &mut block_ranges);
            block_ranges.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
            block_ranges.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
            self.last_block_ranges = block_ranges;
        } else {
            self.last_block_ranges.clear();
        }

        spans
    }

    /// Harvest parse errors across the full source via the retained tree.
    /// Used by status-line "first error" jumps where range-scoped errors
    /// aren't sufficient.
    ///
    /// Returns an empty `Vec` when the grammar is not yet ready.
    pub fn parse_errors_full(&mut self, source: &str, dialect: Dialect) -> Vec<ParseError> {
        let inner = match self.inner.as_mut() {
            Some(i) => i,
            None => return vec![],
        };
        let bytes = source.as_bytes();
        let nl_offsets = compute_newline_offsets(source);
        let inner_errors = inner.parse_errors_range(bytes, 0..bytes.len());
        inner_errors
            .into_iter()
            .filter_map(|e| {
                let start_byte = e.byte_range.start;
                let end_byte = e.byte_range.end;
                if let Some(slice) = source.get(start_byte..end_byte)
                    && dialect.is_native_statement(slice.trim_start())
                {
                    return None;
                }
                let (start_row, start_col) = byte_to_rowcol_with_offsets(&nl_offsets, start_byte);
                let (end_row, end_col) = byte_to_rowcol_with_offsets(&nl_offsets, end_byte);
                Some(ParseError {
                    start_byte,
                    end_byte,
                    start_row,
                    start_col,
                    end_row,
                    end_col,
                    message: e.message,
                })
            })
            .collect()
    }

    /// Parse-error nodes collected on the most recent highlight call.
    pub fn last_errors(&self) -> &[ParseError] {
        &self.last_errors
    }

    /// Block ranges (multi-row nodes) from the most recent highlight call.
    /// Returns `(start_row, end_row)` pairs in source order.
    pub fn block_ranges(&self) -> Vec<(usize, usize)> {
        self.last_block_ranges.clone()
    }

    /// Highlight a borrowed source string. Returns sqeel-level `HighlightSpan`s
    /// with row/col info and capture-name strings.
    pub fn highlight(&mut self, source: &str, dialect: Dialect) -> Vec<HighlightSpan> {
        if source.is_empty() {
            self.last_errors.clear();
            self.last_block_ranges.clear();
            return vec![];
        }
        self.highlight_shared(&Arc::new(source.to_owned()), dialect)
    }

    /// Highlight a shared source buffer (avoids clone for large buffers).
    ///
    /// Returns an empty `Vec` when the grammar is not yet ready.
    pub fn highlight_shared(
        &mut self,
        source: &Arc<String>,
        dialect: Dialect,
    ) -> Vec<HighlightSpan> {
        if source.is_empty() {
            self.last_errors.clear();
            self.last_block_ranges.clear();
            return vec![];
        }

        let inner = match self.inner.as_mut() {
            Some(i) => i,
            None => {
                self.last_errors.clear();
                self.last_block_ranges.clear();
                return vec![];
            }
        };

        let bytes = source.as_bytes();

        // Legacy full-buffer API: no caller-supplied InputEdits, so the
        // retained tree from a prior call is stale relative to `source`.
        // Reset before parsing so we always do a cold full parse and the
        // span set is consistent with what a fresh `Highlighter` would
        // produce.
        inner.reset();

        // Build newline offsets once and reuse across all per-span lookups.
        let nl_offsets = compute_newline_offsets(source.as_str());

        // Get inner spans (capture-name tagged, byte-range only).
        let mut inner_spans: Vec<InnerSpan> = inner.highlight(bytes);
        hjkl_bonsai::CommentMarkerPass::new().apply(&mut inner_spans, bytes);

        // Enrich with row/col and dialect keyword promotion.
        let mut spans: Vec<HighlightSpan> = inner_spans
            .into_iter()
            .map(|s| {
                let (start_row, start_col) =
                    byte_to_rowcol_with_offsets(&nl_offsets, s.byte_range.start);
                let (end_row, end_col) = byte_to_rowcol_with_offsets(&nl_offsets, s.byte_range.end);
                HighlightSpan {
                    start_byte: s.byte_range.start,
                    end_byte: s.byte_range.end,
                    start_row,
                    start_col,
                    end_row,
                    end_col,
                    capture: s.capture,
                }
            })
            .collect();

        // Post-pass: promote dialect-specific keywords in uncovered regions.
        promote_uncovered_dialect_keywords(source.as_str(), dialect, &mut spans);

        // Harvest parse errors.
        let inner_errors: Vec<InnerError> = inner.parse_errors(bytes);
        self.last_errors = inner_errors
            .into_iter()
            .filter_map(|e| {
                let start_byte = e.byte_range.start;
                let end_byte = e.byte_range.end;
                // Filter out dialect-native statement starts (same logic as before).
                if let Some(slice) = source.get(start_byte..end_byte)
                    && dialect.is_native_statement(slice.trim_start())
                {
                    return None;
                }
                let (start_row, start_col) = byte_to_rowcol_with_offsets(&nl_offsets, start_byte);
                let (end_row, end_col) = byte_to_rowcol_with_offsets(&nl_offsets, end_byte);
                Some(ParseError {
                    start_byte,
                    end_byte,
                    start_row,
                    start_col,
                    end_row,
                    end_col,
                    message: e.message,
                })
            })
            .collect();

        // Collect block ranges from a fresh parse of the same source.
        // Re-borrow inner (can't hold from above because self.last_errors was borrowed).
        if let Some(inner2) = self.inner.as_mut() {
            if let Some(syntax) = inner2.parse(bytes) {
                let mut block_ranges = Vec::new();
                collect_block_ranges(syntax.tree().root_node(), &mut block_ranges);
                block_ranges.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
                block_ranges.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
                self.last_block_ranges = block_ranges;
            } else {
                self.last_block_ranges.clear();
            }
        } else {
            self.last_block_ranges.clear();
        }

        spans
    }
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new_async()
    }
}

/// Recursively collect `(start_row, end_row)` for every node that spans >1 row.
fn collect_block_ranges(node: tree_sitter::Node, out: &mut Vec<(usize, usize)>) {
    let start = node.start_position().row;
    let end = node.end_position().row;
    if end > start {
        out.push((start, end));
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_block_ranges(child, out);
    }
}

/// Collect the byte offset of every `\n` in `source`, sorted ascending.
/// Build this once per highlight call and pass the slice to
/// [`byte_to_rowcol_with_offsets`] to get O(log N) per lookup instead of O(N).
fn compute_newline_offsets(source: &str) -> Vec<usize> {
    source
        .bytes()
        .enumerate()
        .filter_map(|(i, b)| if b == b'\n' { Some(i) } else { None })
        .collect()
}

/// O(log N) row/col lookup using a pre-built sorted list of newline offsets.
/// `newline_offsets` must be the slice returned by [`compute_newline_offsets`]
/// for the same `source`.
fn byte_to_rowcol_with_offsets(newline_offsets: &[usize], byte: usize) -> (usize, usize) {
    // Number of newlines before `byte` == the row index.
    let row = newline_offsets.partition_point(|&nl| nl < byte);
    // Column = distance from the last newline (or start of source) to `byte`.
    let col = match row.checked_sub(1).and_then(|i| newline_offsets.get(i)) {
        Some(&last_nl) => byte.saturating_sub(last_nl + 1),
        None => byte,
    };
    (row, col)
}

/// Thin wrapper that builds newline offsets on the fly. Callers that invoke
/// this in a loop should use [`byte_to_rowcol_with_offsets`] directly with a
/// single pre-built offset table.
#[allow(dead_code)]
fn byte_to_rowcol(source: &str, byte: usize) -> (usize, usize) {
    let offsets = compute_newline_offsets(source);
    byte_to_rowcol_with_offsets(&offsets, byte.min(source.len()))
}

/// Range-scoped variant of [`promote_uncovered_dialect_keywords`]: only
/// scan gaps inside `byte_range`, leaving spans outside it untouched.
fn promote_uncovered_dialect_keywords_in_range(
    source: &str,
    dialect: Dialect,
    byte_range: Range<usize>,
    spans: &mut Vec<HighlightSpan>,
) {
    if matches!(dialect, Dialect::Generic) {
        return;
    }
    let total = source.len();
    if total == 0 || byte_range.start >= byte_range.end {
        return;
    }
    let range_end = byte_range.end.min(total);
    let range_start = byte_range.start.min(range_end);

    let mut covered: Vec<(usize, usize)> = spans
        .iter()
        .filter(|s| s.start_byte < range_end && s.end_byte > range_start)
        .map(|s| (s.start_byte.max(range_start), s.end_byte.min(range_end)))
        .collect();
    covered.sort_by_key(|&(s, _)| s);

    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(covered.len());
    for (s, e) in covered {
        if let Some(last) = merged.last_mut()
            && s <= last.1
        {
            last.1 = last.1.max(e);
        } else {
            merged.push((s, e));
        }
    }

    let bytes = source.as_bytes();
    let nl_offsets = compute_newline_offsets(source);
    let mut cursor = range_start;
    let mut gap_iter = merged.iter().peekable();
    let mut additions: Vec<HighlightSpan> = Vec::new();
    while cursor < range_end {
        match gap_iter.peek().copied() {
            Some(&(gs, ge)) if gs <= cursor => {
                cursor = ge.min(range_end);
                gap_iter.next();
            }
            Some(&(gs, _)) => {
                let stop = gs.min(range_end);
                scan_gap_for_keywords(
                    source,
                    bytes,
                    cursor,
                    stop,
                    dialect,
                    &nl_offsets,
                    &mut additions,
                );
                cursor = stop;
            }
            None => {
                scan_gap_for_keywords(
                    source,
                    bytes,
                    cursor,
                    range_end,
                    dialect,
                    &nl_offsets,
                    &mut additions,
                );
                cursor = range_end;
            }
        }
    }
    spans.extend(additions);
    spans.sort_by_key(|s| s.start_byte);
}

/// Find identifier-shaped words in regions that tree-sitter didn't
/// classify, and emit keyword spans for those that match the active
/// dialect's extra-keyword or native-statement list.
fn promote_uncovered_dialect_keywords(
    source: &str,
    dialect: Dialect,
    spans: &mut Vec<HighlightSpan>,
) {
    if matches!(dialect, Dialect::Generic) {
        return;
    }
    let total = source.len();
    if total == 0 {
        return;
    }

    // Build sorted list of covered byte ranges.
    let mut covered: Vec<(usize, usize)> =
        spans.iter().map(|s| (s.start_byte, s.end_byte)).collect();
    covered.sort_by_key(|&(s, _)| s);

    // Merge overlapping/adjacent ranges.
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(covered.len());
    for (s, e) in covered {
        if let Some(last) = merged.last_mut()
            && s <= last.1
        {
            last.1 = last.1.max(e);
        } else {
            merged.push((s, e));
        }
    }

    let bytes = source.as_bytes();
    let nl_offsets = compute_newline_offsets(source);
    let mut cursor = 0usize;
    let mut gap_iter = merged.iter().peekable();
    let mut additions: Vec<HighlightSpan> = Vec::new();
    while cursor < total {
        match gap_iter.peek().copied() {
            Some(&(gs, ge)) if gs <= cursor => {
                cursor = ge;
                gap_iter.next();
            }
            Some(&(gs, _)) => {
                scan_gap_for_keywords(
                    source,
                    bytes,
                    cursor,
                    gs,
                    dialect,
                    &nl_offsets,
                    &mut additions,
                );
                cursor = gs;
            }
            None => {
                scan_gap_for_keywords(
                    source,
                    bytes,
                    cursor,
                    total,
                    dialect,
                    &nl_offsets,
                    &mut additions,
                );
                cursor = total;
            }
        }
    }
    spans.extend(additions);
    spans.sort_by_key(|s| s.start_byte);
}

fn scan_gap_for_keywords(
    source: &str,
    bytes: &[u8],
    start: usize,
    end: usize,
    dialect: Dialect,
    nl_offsets: &[usize],
    out: &mut Vec<HighlightSpan>,
) {
    let mut i = start;
    while i < end {
        let b = bytes[i];
        if !(b.is_ascii_alphabetic() || b == b'_') {
            i += 1;
            continue;
        }
        let word_start = i;
        while i < end {
            let c = bytes[i];
            if !(c.is_ascii_alphanumeric() || c == b'_') {
                break;
            }
            i += 1;
        }
        let word = &source[word_start..i];
        if dialect.is_extra_keyword(word) {
            let (start_row, start_col) = byte_to_rowcol_with_offsets(nl_offsets, word_start);
            let (end_row, end_col) = byte_to_rowcol_with_offsets(nl_offsets, i);
            out.push(HighlightSpan {
                start_byte: word_start,
                end_byte: i,
                start_row,
                start_col,
                end_row,
                end_col,
                capture: "keyword".to_string(),
            });
        }
    }
}

/// Parse `source` and return the byte ranges of each top-level statement.
pub fn statement_ranges(source: &str) -> Vec<(usize, usize)> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_sequel::LANGUAGE.into())
        .is_err()
    {
        return vec![];
    }
    let Some(tree) = parser.parse(source, None) else {
        return vec![];
    };
    let root = tree.root_node();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let start = child.start_byte();
        let end = child.end_byte();
        if start < end && end <= source.len() {
            ranges.push((start, end));
        }
    }
    if ranges.is_empty() && !source.trim().is_empty() {
        ranges.push((0, source.len()));
    }
    let split: Vec<(usize, usize)> = ranges
        .into_iter()
        .flat_map(|(s, e)| split_top_level_semicolons(source, s, e))
        .collect();
    split
        .into_iter()
        .filter(|&(s, e)| {
            let t = source[s..e].trim();
            !t.is_empty() && t != ";"
        })
        .collect()
}

/// Walk `source[start..end]` and split it at every top-level `;`.
fn split_top_level_semicolons(source: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let end = end.min(bytes.len());
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut stmt_start = start;
    let mut i = start;
    while i < end {
        let c = bytes[i];
        match c {
            b'\'' | b'"' | b'`' => {
                i += 1;
                while i < end && bytes[i] != c {
                    if bytes[i] == b'\\' && i + 1 < end {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if i < end {
                    i += 1;
                }
            }
            b'-' if i + 1 < end && bytes[i + 1] == b'-' => {
                while i < end && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < end && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < end && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                if i + 1 < end {
                    i += 2;
                }
            }
            b';' => {
                out.push((stmt_start, i + 1));
                stmt_start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    if stmt_start < end {
        out.push((stmt_start, end));
    }
    out
}

/// Returns the byte range of the statement containing `byte`.
pub fn statement_at_byte(source: &str, byte: usize) -> Option<(usize, usize)> {
    let ranges = statement_ranges(source);
    if ranges.is_empty() {
        return None;
    }
    for (i, (s, e)) in ranges.iter().enumerate() {
        if byte >= *s && byte < *e {
            return Some((*s, *e));
        }
        if byte < *s {
            return Some(if i == 0 { ranges[0] } else { ranges[i - 1] });
        }
    }
    Some(*ranges.last().unwrap())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub line: usize,
    pub col: usize,
    pub byte: usize,
    pub message: String,
}

/// Parse `source` and return the first syntax error (line/col 1-based).
pub fn first_syntax_error(source: &str) -> Option<SyntaxError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_sequel::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    if !root.has_error() {
        return None;
    }
    let mut cursor = root.walk();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_missing() {
            let p = node.start_position();
            let kind = node.kind();
            let message = if kind.is_empty() {
                "missing token".to_string()
            } else {
                format!("missing `{kind}`")
            };
            return Some(SyntaxError {
                line: p.row + 1,
                col: p.column + 1,
                byte: node.start_byte(),
                message,
            });
        }
        if node.is_error() {
            let p = node.start_position();
            let snippet = source
                .get(node.start_byte()..node.end_byte())
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .trim();
            let message = if snippet.is_empty() {
                "unexpected token".to_string()
            } else {
                let trimmed: String = snippet.chars().take(40).collect();
                format!("unexpected `{trimmed}`")
            };
            return Some(SyntaxError {
                line: p.row + 1,
                col: p.column + 1,
                byte: node.start_byte(),
                message,
            });
        }
        for child in node.children(&mut cursor) {
            if child.has_error() || child.is_error() || child.is_missing() {
                stack.push(child);
            }
        }
    }
    Some(SyntaxError {
        line: 1,
        col: 1,
        byte: 0,
        message: "parse error".to_string(),
    })
}

/// Strip SQL comments (`-- …` and `/* … */`) from `source`.
pub fn strip_sql_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\'' | b'"' | b'`' => {
                out.push(c);
                i += 1;
                while i < bytes.len() {
                    let d = bytes[i];
                    out.push(d);
                    i += 1;
                    if d == c {
                        break;
                    }
                }
            }
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                if i + 1 < bytes.len() {
                    i += 2;
                }
                out.push(b' ');
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| source.to_string())
}

/// True when `query` is a `SHOW CREATE ...` statement.
pub fn is_show_create(query: &str) -> bool {
    let stripped = strip_sql_comments(query);
    let trimmed = stripped.trim_start();
    trimmed.len() >= 11 && trimmed[..11].eq_ignore_ascii_case("show create")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_statement_skips_semicolon_nodes() {
        let src = "select * from foo;\nselect * from bar;";
        let ranges = statement_ranges(src);
        for (s, e) in &ranges {
            let stmt = &src[*s..*e];
            assert!(
                stmt.trim() != ";",
                "statement_ranges should not return semicolon-only ranges"
            );
            assert!(
                !stmt.trim().is_empty(),
                "statement_ranges should not return empty ranges"
            );
        }
        for (s, e) in &ranges {
            let stmt = &src[*s..*e].trim();
            let err = first_syntax_error(stmt);
            assert!(
                err.is_none(),
                "expected no syntax error for {:?}, got: {:?}",
                stmt,
                err
            );
        }
    }

    #[test]
    fn highlights_select_keyword() {
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight("SELECT id FROM users", Dialect::Generic);
        let keywords: Vec<_> = spans
            .iter()
            .filter(|s| is_sql_keyword_capture(&s.capture))
            .collect();
        assert!(
            !keywords.is_empty(),
            "expected keyword spans, got: {spans:#?}"
        );
    }

    #[test]
    fn highlights_identifier() {
        // tree-sitter-sequel maps identifiers to various captures depending
        // on context: @field (column refs), @type (table/schema refs),
        // @variable (aliases), @parameter. Any of these counts as "identifier".
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight("SELECT id FROM users", Dialect::Generic);
        let idents: Vec<_> = spans
            .iter()
            .filter(|s| {
                matches!(
                    s.capture.as_str(),
                    "field" | "type" | "variable" | "parameter"
                ) || s.capture.starts_with("identifier")
                    || s.capture.starts_with("variable")
            })
            .collect();
        assert!(
            !idents.is_empty(),
            "expected identifier spans, got: {spans:#?}"
        );
    }

    #[test]
    fn highlights_string_literal() {
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight("SELECT * FROM users WHERE name = 'alice'", Dialect::Generic);
        let strings: Vec<_> = spans
            .iter()
            .filter(|s| s.capture.starts_with("string"))
            .collect();
        assert!(
            !strings.is_empty(),
            "expected string spans, got: {spans:#?}"
        );
    }

    #[test]
    fn empty_input_no_panic() {
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight("", Dialect::Generic);
        assert!(spans.is_empty());
    }

    #[test]
    fn invalid_sql_no_panic() {
        let mut h = Highlighter::new().unwrap();
        let _spans = h.highlight("??? !!! garbage", Dialect::Generic);
    }

    #[test]
    fn incremental_same_result() {
        // hjkl-bonsai does full re-parse on each call (incremental is
        // Phase 2). Verify the span count is consistent across calls.
        let mut h = Highlighter::new().unwrap();
        let src1 = "SELECT id FROM users";
        let src2 = "SELECT id FROM users WHERE id = 1";
        let spans_full = {
            let mut h2 = Highlighter::new().unwrap();
            h2.highlight(src2, Dialect::Generic)
        };
        h.highlight(src1, Dialect::Generic);
        let spans_incr = h.highlight(src2, Dialect::Generic);
        assert_eq!(spans_full.len(), spans_incr.len());
    }

    #[test]
    fn statement_ranges_splits_consecutive_desc_statements() {
        let src = "desc test;\n\ndesc another;\n";
        let ranges = statement_ranges(src);
        let texts: Vec<&str> = ranges.iter().map(|&(s, e)| src[s..e].trim()).collect();
        assert_eq!(texts, vec!["desc test;", "desc another;"]);
    }

    #[test]
    fn statement_ranges_does_not_split_inside_string_literal() {
        let src = "select '; not end' as x;\n";
        let selects: Vec<String> = statement_ranges(src)
            .into_iter()
            .map(|(s, e)| src[s..e].trim().to_string())
            .filter(|t| t.starts_with("select"))
            .collect();
        assert_eq!(selects.len(), 1, "got: {selects:?}");
        assert!(selects[0].contains("; not end"));
    }

    #[test]
    fn statement_ranges_does_not_split_inside_double_quoted_string() {
        let src = "select \"; still in string\" as x;\n";
        let selects: Vec<String> = statement_ranges(src)
            .into_iter()
            .map(|(s, e)| src[s..e].trim().to_string())
            .filter(|t| t.starts_with("select"))
            .collect();
        assert_eq!(selects.len(), 1, "got: {selects:?}");
    }

    #[test]
    fn statement_ranges_does_not_split_inside_backtick_identifier() {
        let src = "select `;weird col;` from t;\n";
        let selects: Vec<String> = statement_ranges(src)
            .into_iter()
            .map(|(s, e)| src[s..e].trim().to_string())
            .filter(|t| t.starts_with("select"))
            .collect();
        assert_eq!(selects.len(), 1, "got: {selects:?}");
    }

    #[test]
    fn split_top_level_semicolons_respects_line_comment() {
        let src = "a; -- ; still\nb;";
        let parts: Vec<&str> = super::split_top_level_semicolons(src, 0, src.len())
            .into_iter()
            .map(|(s, e)| &src[s..e])
            .collect();
        assert_eq!(parts, vec!["a;", " -- ; still\nb;"]);
    }

    #[test]
    fn split_top_level_semicolons_respects_block_comment() {
        let src = "a; /* ; in comment */ b;";
        let parts: Vec<&str> = super::split_top_level_semicolons(src, 0, src.len())
            .into_iter()
            .map(|(s, e)| &src[s..e])
            .collect();
        assert_eq!(parts, vec!["a;", " /* ; in comment */ b;"]);
    }

    #[test]
    fn split_top_level_semicolons_respects_string_literal() {
        let src = "a; 'x;y'; b;";
        let parts: Vec<&str> = super::split_top_level_semicolons(src, 0, src.len())
            .into_iter()
            .map(|(s, e)| &src[s..e])
            .collect();
        assert_eq!(parts, vec!["a;", " 'x;y';", " b;"]);
    }

    #[test]
    fn statement_at_byte_picks_second_desc_block() {
        let src = "desc test;\n\ndesc another;\n";
        let byte = src.find("another").unwrap();
        let (s, e) = statement_at_byte(src, byte).unwrap();
        let text = src[s..e].trim();
        assert_eq!(text, "desc another;", "got: {text:?}");
    }

    #[test]
    fn statement_at_byte_picks_first_desc_block() {
        let src = "desc test;\n\ndesc another;\n";
        let byte = src.find("test").unwrap();
        let (s, e) = statement_at_byte(src, byte).unwrap();
        let text = src[s..e].trim();
        assert_eq!(text, "desc test;", "got: {text:?}");
    }

    #[test]
    fn parse_error_harvested_for_obvious_junk() {
        let mut h = Highlighter::new().unwrap();
        let src = "this line should error this is a test;\n";
        h.highlight(src, Dialect::MySql);
        let errs = h.last_errors();
        assert!(
            !errs.is_empty(),
            "expected tree-sitter to flag a parse error; got none"
        );
    }

    #[test]
    fn parse_errors_cleared_between_calls() {
        let mut h = Highlighter::new().unwrap();
        h.highlight("this line should error this is a test;\n", Dialect::MySql);
        assert!(!h.last_errors().is_empty());
        h.highlight("SELECT 1;\n", Dialect::MySql);
        assert!(
            h.last_errors().is_empty(),
            "valid SQL should leave no lingering errors"
        );
    }

    #[test]
    fn parse_error_clamped_to_single_row() {
        let mut h = Highlighter::new().unwrap();
        let src = "this line should error this is a test;\nDESC users;\nDESC users;\n";
        h.highlight(src, Dialect::MySql);
        let errs = h.last_errors();
        assert!(!errs.is_empty(), "expected a parse error; got none");
        for e in errs {
            assert_eq!(
                e.start_row, e.end_row,
                "parse error span crosses rows: {e:?}"
            );
        }
    }

    #[test]
    fn parse_error_skipped_for_dialect_native_statement() {
        let mut h = Highlighter::new().unwrap();
        h.highlight("DESC users;\n", Dialect::MySql);
        let flagged_desc = h.last_errors().iter().any(|e| e.message.contains("DESC"));
        assert!(
            !flagged_desc,
            "DESC shouldn't be flagged as error on MySQL; got {:?}",
            h.last_errors()
        );
    }

    #[test]
    fn dialect_from_url_dispatch() {
        assert_eq!(Dialect::from_url("mysql://u:p@h/d"), Dialect::MySql);
        assert_eq!(Dialect::from_url("mariadb://u:p@h/d"), Dialect::MySql);
        assert_eq!(Dialect::from_url("postgres://h/d"), Dialect::Postgres);
        assert_eq!(Dialect::from_url("postgresql://h/d"), Dialect::Postgres);
        assert_eq!(Dialect::from_url("sqlite:///tmp/a.db"), Dialect::Sqlite);
        assert_eq!(Dialect::from_url("sqlite:a.db"), Dialect::Sqlite);
        assert_eq!(Dialect::from_url("other://x"), Dialect::Generic);
    }

    #[test]
    fn mysql_auto_increment_promoted_to_keyword() {
        // tree-sitter-sequel captures AUTO_INCREMENT as @attribute, which
        // is_sql_keyword_capture treats as a keyword for rendering.
        let src = "CREATE TABLE t (id INT AUTO_INCREMENT)";
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight(src, Dialect::MySql);
        let has = spans.iter().any(|s| {
            is_sql_keyword_capture(&s.capture) && &src[s.start_byte..s.end_byte] == "AUTO_INCREMENT"
        });
        assert!(has, "AUTO_INCREMENT should render as keyword on MySQL");
    }

    #[test]
    fn dialect_extra_keyword_tables_are_non_empty() {
        assert!(!Dialect::MySql.extra_keywords().is_empty());
        assert!(!Dialect::Postgres.extra_keywords().is_empty());
        assert!(!Dialect::Sqlite.extra_keywords().is_empty());
        assert!(Dialect::Generic.extra_keywords().is_empty());
    }

    #[test]
    fn is_native_statement_matches_leading_token() {
        assert!(Dialect::MySql.is_native_statement("DESC users"));
        assert!(Dialect::MySql.is_native_statement("desc users"));
        assert!(Dialect::MySql.is_native_statement("DESCRIBE users"));
        assert!(Dialect::MySql.is_native_statement("SHOW TABLES"));
        assert!(Dialect::MySql.is_native_statement("-- lead\nDESC users"));
        assert!(!Dialect::MySql.is_native_statement("SELECT * FROM users"));

        assert!(Dialect::Sqlite.is_native_statement("PRAGMA foreign_keys = ON"));
        assert!(!Dialect::Sqlite.is_native_statement("DESC users"));
    }

    #[test]
    fn is_native_statement_skips_leading_comments_and_whitespace() {
        assert!(Dialect::MySql.is_native_statement("   \n  -- comment line\n  DESC users;\n"));
    }

    #[test]
    fn is_extra_keyword_is_case_insensitive() {
        assert!(Dialect::MySql.is_extra_keyword("auto_increment"));
        assert!(Dialect::MySql.is_extra_keyword("AUTO_INCREMENT"));
        assert!(Dialect::Postgres.is_extra_keyword("ilike"));
        assert!(!Dialect::MySql.is_extra_keyword("ilike"));
    }

    #[test]
    fn desc_lowercase_select_prior_is_keyword() {
        // tree-sitter-sequel may tag DESC as @keyword or @attribute depending on
        // parse context; both count as keyword via is_sql_keyword_capture.
        let src = "select * from users;\n\nDESC users;\n\nDESC users;\n";
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight(src, Dialect::MySql);
        let desc_kw_count = spans
            .iter()
            .filter(|s| {
                is_sql_keyword_capture(&s.capture) && &src[s.start_byte..s.end_byte] == "DESC"
            })
            .count();
        assert_eq!(
            desc_kw_count, 2,
            "expected both DESCs highlighted; spans: {spans:#?}"
        );
    }

    #[test]
    fn generic_dialect_skips_desc_keyword_promotion() {
        // Under Generic dialect our post-pass doesn't promote DESC. Under MySql
        // it does (via promote_uncovered_dialect_keywords). Tree-sitter may also
        // tag DESC via @attribute regardless of dialect — the test just asserts
        // MySql doesn't produce *fewer* keyword DESC spans than Generic, and that
        // at least one DESC is keyword-styled under MySql.
        let src = "select * from users;\n\nDESC users;\n\nDESC users;\n";
        let mut h = Highlighter::new().unwrap();
        let generic_count = h
            .highlight(src, Dialect::Generic)
            .into_iter()
            .filter(|s| {
                is_sql_keyword_capture(&s.capture) && &src[s.start_byte..s.end_byte] == "DESC"
            })
            .count();
        let mut h2 = Highlighter::new().unwrap();
        let mysql_count = h2
            .highlight(src, Dialect::MySql)
            .into_iter()
            .filter(|s| {
                is_sql_keyword_capture(&s.capture) && &src[s.start_byte..s.end_byte] == "DESC"
            })
            .count();
        // Under MySql the post-pass ensures at least as many DESC keyword spans
        // as Generic; both should have at least 1 (tree-sitter tags them @attribute
        // which is treated as keyword).
        assert!(
            mysql_count >= generic_count,
            "MySql must have >= Generic DESC keyword spans (mysql={mysql_count}, generic={generic_count})"
        );
        assert!(
            mysql_count >= 1,
            "expected at least one DESC keyword span under MySql"
        );
    }

    #[test]
    fn debug_dump_with_alter_tail() {
        let header = "select * from ppc_third.searches_182 order by id desc;\n\
                   select * from ppc_third.searches_181 order by id desc;\n\
                   select count(*), status from ppc_third.searches_182 group by status;\n\
                   \n\
                   -- TODO: \n\
                   -- test\n\
                   \n\
                   -- TODO test\n\
                   \n\
                   -- TODO: this is a test\n\
                   -- FIXME: this is a test\n\
                   -- this is a test\n\
                   -- FIX:\n\
                   \n\
                   -- NOTE: another note\n\
                   -- WARN: woah...\n\
                   -- this is a warning\n\
                   -- INFO:  this is \n\
                   \n\
                   select * from users;\n\
                   \n\
                   DESC users;\n\
                   \n\
                   DESC users;\n\
                   \n";
        let alter = "-- ALTER TABLE ppc_third.`searches_182` ADD COLUMN `error` TEXT NULL AFTER `status`;\n";
        let mut src = header.to_string();
        for _ in 0..40 {
            src.push_str(alter);
        }

        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight(&src, Dialect::MySql);
        let desc_count = spans
            .iter()
            .filter(|s| {
                is_sql_keyword_capture(&s.capture) && &src[s.start_byte..s.end_byte] == "DESC"
            })
            .count();
        println!("DESC keyword count = {}", desc_count);
    }

    #[test]
    fn debug_dump_full_buffer_spans() {
        let src = "select * from ppc_third.searches_182 order by id desc;\n\
                   select * from ppc_third.searches_181 order by id desc;\n\
                   select count(*), status from ppc_third.searches_182 group by status;\n\
                   \n\
                   -- TODO: \n\
                   -- test\n\
                   \n\
                   -- TODO test\n\
                   \n\
                   -- TODO: this is a test\n\
                   -- FIXME: this is a test\n\
                   -- this is a test\n\
                   -- FIX:\n\
                   \n\
                   -- NOTE: another note\n\
                   -- WARN: woah...\n\
                   -- this is a warning\n\
                   -- INFO:  this is \n\
                   \n\
                   select * from users;\n\
                   \n\
                   DESC users;\n\
                   \n\
                   DESC users;\n";
        let mut h = Highlighter::new().unwrap();
        let _spans = h.highlight(src, Dialect::MySql);
    }

    #[test]
    fn desc_highlighted_in_full_buffer_repro() {
        let src = "select * from ppc_third.searches_182 order by id desc;\n\
                   select * from ppc_third.searches_181 order by id desc;\n\
                   select count(*), status from ppc_third.searches_182 group by status;\n\
                   \n\
                   -- TODO: \n\
                   -- test\n\
                   \n\
                   -- TODO test\n\
                   \n\
                   -- TODO: this is a test\n\
                   -- FIXME: this is a test\n\
                   -- this is a test\n\
                   -- FIX:\n\
                   \n\
                   -- NOTE: another note\n\
                   -- WARN: woah...\n\
                   -- this is a warning\n\
                   -- INFO:  this is \n\
                   \n\
                   select * from users;\n\
                   \n\
                   DESC users;\n\
                   \n\
                   DESC users;\n";
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight(src, Dialect::MySql);

        let desc_kw_positions: Vec<usize> = spans
            .iter()
            .filter(|s| {
                is_sql_keyword_capture(&s.capture) && &src[s.start_byte..s.end_byte] == "DESC"
            })
            .map(|s| s.start_byte)
            .collect();
        let expected = src
            .match_indices("DESC ")
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        assert_eq!(
            desc_kw_positions, expected,
            "all DESC instances should be keyword spans; got positions {desc_kw_positions:?} for expected {expected:?}; spans: {spans:#?}"
        );
    }

    #[test]
    fn desc_highlighted_after_repeated_incremental_edits() {
        let mut h = Highlighter::new().unwrap();
        let seeds = [
            "select * from users;\n",
            "select * from users;\nD",
            "select * from users;\nDE",
            "select * from users;\nDESC",
            "select * from users;\nDESC ",
            "select * from users;\nDESC users;\n",
            "select * from users;\n\nDESC users;\n",
            "select * from users;\n\nDESC users;\n\nDESC users;\n",
        ];
        for src in seeds {
            h.highlight(src, Dialect::MySql);
        }
        let final_src = "select * from users;\n\nDESC users;\n\nDESC users;\n";
        let spans = h.highlight(final_src, Dialect::MySql);
        let count = spans
            .iter()
            .filter(|s| {
                is_sql_keyword_capture(&s.capture) && &final_src[s.start_byte..s.end_byte] == "DESC"
            })
            .count();
        assert_eq!(count, 2, "expected 2 DESC keyword spans; got: {spans:#?}");
    }

    #[test]
    fn desc_survives_incremental_edit() {
        let mut h = Highlighter::new().unwrap();
        let seed = "SELECT * FROM users;\n";
        h.highlight(seed, Dialect::MySql);

        let edited = "SELECT * FROM users;\nDESC users;\n";
        let spans = h.highlight(edited, Dialect::MySql);
        let has_desc_kw = spans.iter().any(|s| {
            is_sql_keyword_capture(&s.capture) && &edited[s.start_byte..s.end_byte] == "DESC"
        });
        assert!(
            has_desc_kw,
            "DESC should be a keyword after re-parse; spans: {spans:#?}"
        );
    }

    #[test]
    fn incremental_matches_cold_full() {
        let mut source = String::new();
        for i in 0..50 {
            source.push_str(&format!("SELECT id, name FROM users_{i} WHERE id = {i};\n"));
        }

        let mut cold = Highlighter::new().unwrap();
        cold.parse_initial(&source);
        let cold_spans = cold.highlight_range(&source, Dialect::MySql, 0..source.len());

        let mut inc = Highlighter::new().unwrap();
        inc.parse_initial(&source);
        // No-op edit (start == old_end == new_end) keeps the retained tree
        // valid; subsequent parse_incremental returns the same tree.
        let edit = tree_sitter::InputEdit {
            start_byte: 0,
            old_end_byte: 0,
            new_end_byte: 0,
            start_position: tree_sitter::Point { row: 0, column: 0 },
            old_end_position: tree_sitter::Point { row: 0, column: 0 },
            new_end_position: tree_sitter::Point { row: 0, column: 0 },
        };
        inc.edit(&edit);
        assert!(inc.parse_incremental(&source));
        let inc_spans = inc.highlight_range(&source, Dialect::MySql, 0..source.len());

        assert_eq!(
            cold_spans.len(),
            inc_spans.len(),
            "incremental span count drifted from cold"
        );
        for (a, b) in cold_spans.iter().zip(inc_spans.iter()) {
            assert_eq!(a.start_byte, b.start_byte);
            assert_eq!(a.end_byte, b.end_byte);
            assert_eq!(a.capture, b.capture);
        }
    }

    #[test]
    fn desc_after_prior_statement_is_keyword() {
        let src = "SELECT * FROM users;\nDESC users;\n";
        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight(src, Dialect::MySql);
        let has_desc_kw = spans.iter().any(|s| {
            is_sql_keyword_capture(&s.capture) && &src[s.start_byte..s.end_byte] == "DESC"
        });
        assert!(
            has_desc_kw,
            "expected DESC to be a keyword span; spans: {spans:#?}"
        );
    }

    #[test]
    fn native_statement_starts_also_promote_to_keyword() {
        assert!(Dialect::MySql.is_extra_keyword("DESC"));
        assert!(Dialect::MySql.is_extra_keyword("SHOW"));
        assert!(Dialect::Sqlite.is_extra_keyword("PRAGMA"));
        assert!(Dialect::Postgres.is_extra_keyword("LISTEN"));
        assert!(!Dialect::Postgres.is_extra_keyword("DESC"));
    }

    /// `sql_grammar_blocking()` must return a usable `Arc<Grammar>`.
    #[test]
    fn sql_grammar_blocking_returns_grammar() {
        let g = sql_grammar_blocking().expect("sql_grammar_blocking failed");
        assert_eq!(g.name(), "sql");
    }

    /// A `Highlighter` built with `new_async` before the grammar resolves must
    /// return empty spans rather than panicking. After `try_upgrade` (and once
    /// the blocking grammar is cached) it should be ready and highlight normally.
    #[test]
    fn new_async_returns_empty_spans_when_not_ready_then_upgrades() {
        // Build a highlighter that may or may not have the grammar yet.
        let mut h = Highlighter::new_async();
        // Whether ready or not, highlighting must not panic and must return a
        // Vec (empty when grammar is absent, non-empty when present).
        let spans_before = h.highlight("SELECT 1", Dialect::Generic);
        if !h.is_ready() {
            assert!(
                spans_before.is_empty(),
                "expected empty spans when grammar not ready"
            );
        }
        // Force grammar resolution and upgrade.
        sql_grammar_blocking().expect("blocking load failed");
        h.try_upgrade();
        assert!(
            h.is_ready(),
            "Highlighter should be ready after try_upgrade"
        );
        let spans_after = h.highlight("SELECT 1", Dialect::Generic);
        assert!(
            !spans_after.is_empty(),
            "expected keyword spans after grammar loaded; got: {spans_after:#?}"
        );
    }
}
