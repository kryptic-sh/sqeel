use std::sync::Arc;
use tree_sitter::Parser;

use hjkl_tree_sitter::{HighlightSpan as InnerSpan, LanguageRegistry, ParseError as InnerError};

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

/// Thin wrapper around `hjkl_tree_sitter::Highlighter` that:
/// - adds dialect-specific keyword promotion for SQL.
/// - enriches spans with row/col from byte offsets.
/// - caches last errors and block ranges.
pub struct Highlighter {
    inner: hjkl_tree_sitter::Highlighter,
    last_errors: Vec<ParseError>,
    last_block_ranges: Vec<(usize, usize)>,
    // Keep an Arc<String> + old tree for incremental parse (block_ranges / errors).
    old_source: Option<Arc<String>>,
}

impl Highlighter {
    pub fn new() -> anyhow::Result<Self> {
        let registry = LanguageRegistry::new();
        let config = registry.by_name("sql").ok_or_else(|| {
            anyhow::anyhow!("sql language not found in hjkl-tree-sitter registry")
        })?;
        let inner = hjkl_tree_sitter::Highlighter::new(config)?;
        Ok(Self {
            inner,
            last_errors: Vec::new(),
            last_block_ranges: Vec::new(),
            old_source: None,
        })
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
            self.old_source = None;
            self.last_errors.clear();
            self.last_block_ranges.clear();
            return vec![];
        }
        self.highlight_shared(&Arc::new(source.to_owned()), dialect)
    }

    /// Highlight a shared source buffer (avoids clone for large buffers).
    pub fn highlight_shared(
        &mut self,
        source: &Arc<String>,
        dialect: Dialect,
    ) -> Vec<HighlightSpan> {
        if source.is_empty() {
            self.old_source = None;
            self.last_errors.clear();
            self.last_block_ranges.clear();
            return vec![];
        }

        let bytes = source.as_bytes();

        // Get inner spans (capture-name tagged, byte-range only).
        let inner_spans: Vec<InnerSpan> = self.inner.highlight(bytes);

        // Enrich with row/col and dialect keyword promotion.
        let mut spans: Vec<HighlightSpan> = inner_spans
            .into_iter()
            .map(|s| {
                let (start_row, start_col) = byte_to_rowcol(source.as_str(), s.byte_range.start);
                let (end_row, end_col) = byte_to_rowcol(source.as_str(), s.byte_range.end);
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
        let inner_errors: Vec<InnerError> = self.inner.parse_errors(bytes);
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
                let (start_row, start_col) = byte_to_rowcol(source.as_str(), start_byte);
                let (end_row, end_col) = byte_to_rowcol(source.as_str(), end_byte);
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
        if let Some(syntax) = self.inner.parse(bytes) {
            let mut block_ranges = Vec::new();
            collect_block_ranges(syntax.tree().root_node(), &mut block_ranges);
            block_ranges.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
            block_ranges.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
            self.last_block_ranges = block_ranges;
        } else {
            self.last_block_ranges.clear();
        }

        self.old_source = Some(Arc::clone(source));
        spans
    }
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new().expect("failed to initialize hjkl-tree-sitter SQL highlighter")
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

fn byte_to_rowcol(source: &str, byte: usize) -> (usize, usize) {
    let prefix = &source[..byte.min(source.len())];
    let row = prefix.bytes().filter(|&b| b == b'\n').count();
    let col = prefix.bytes().rev().take_while(|&b| b != b'\n').count();
    (row, col)
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
                scan_gap_for_keywords(source, bytes, cursor, gs, dialect, &mut additions);
                cursor = gs;
            }
            None => {
                scan_gap_for_keywords(source, bytes, cursor, total, dialect, &mut additions);
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
            let (start_row, start_col) = byte_to_rowcol(source, word_start);
            let (end_row, end_col) = byte_to_rowcol(source, i);
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
        // hjkl-tree-sitter does full re-parse on each call (incremental is
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
}
