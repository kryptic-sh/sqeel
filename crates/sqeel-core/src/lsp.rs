//! LSP adapter: thin wrapper over [`hjkl_lsp::LspManager`].
//! Public surface (LspClient, LspWriter, LspEvent, Diagnostic, write_sqls_config) unchanged.

use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

use anyhow::Context;
use hjkl_lsp::{BufferId, LspConfig, LspEvent as HjklLspEvent, LspManager, ServerConfig};
use lsp_types::*;
use serde_json::Value;
use tokio::sync::mpsc;

const BUF: BufferId = 1;
static ID: AtomicI64 = AtomicI64::new(1);
fn next_id() -> i64 {
    ID.fetch_add(1, Ordering::SeqCst)
}

// ── sqeel-specific config writer ─────────────────────────────────────────────

/// Write a `sqls` config file for the given connection URL.
pub fn write_sqls_config(url: &str) -> anyhow::Result<PathBuf> {
    let (driver, dsn) = sqls_driver_and_dsn(url)?;
    let yaml = format!(
        "lowercaseKeywords: false\nconnections:\n  - alias: sqeel\n    driver: {driver}\n    dataSourceName: \"{dsn}\"\n"
    );
    let path = std::env::temp_dir().join(format!("sqeel-sqls-config-{}.yml", std::process::id()));
    std::fs::write(&path, yaml)?;
    // Restrict read access to the owner: the config contains database credentials.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    // On Windows, %TEMP% ACLs already restrict access to the creating user,
    // so no additional permission step is needed there.
    Ok(path)
}

fn sqls_driver_and_dsn(url: &str) -> anyhow::Result<(&'static str, String)> {
    use anyhow::Context as _;
    if let Some(rest) = url
        .strip_prefix("mysql://")
        .or_else(|| url.strip_prefix("mariadb://"))
    {
        let (userpass, after) = rest
            .split_once('@')
            .context("mysql url missing `user@host`")?;
        let (hostport, db_and_rest) = after.split_once('/').unwrap_or((after, ""));
        let db = db_and_rest.split('?').next().unwrap_or("");
        Ok(("mysql", format!("{userpass}@tcp({hostport})/{db}")))
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Ok(("postgresql", url.to_string()))
    } else if url.starts_with("sqlite:") {
        let path = url
            .strip_prefix("sqlite://")
            .or_else(|| url.strip_prefix("sqlite:"))
            .unwrap_or("");
        Ok(("sqlite3", path.to_string()))
    } else {
        anyhow::bail!("unsupported URL scheme for sqls config: {url}")
    }
}

// ── Domain types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub message: String,
    pub severity: DiagnosticSeverity,
}

#[derive(Debug)]
pub enum LspEvent {
    Diagnostics(Vec<Diagnostic>),
    Completion(i64, Vec<String>),
    Hover(i64, String),
    Definition(i64, String, u32, u32),
    SignatureHelp(i64, String),
}

// ── Adapter ───────────────────────────────────────────────────────────────────

struct Inner {
    manager: LspManager,
}

pub struct LspClient {
    inner: Arc<Inner>,
    pub events: mpsc::Receiver<LspEvent>,
}

impl LspClient {
    pub async fn start(
        binary: &str,
        _root_uri: Option<Uri>,
        args: &[String],
    ) -> anyhow::Result<Self> {
        // Probe binary availability early (matches old behaviour).
        std::process::Command::new(binary)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|mut c| {
                let _ = c.kill();
            })
            .with_context(|| format!("failed to spawn LSP: {binary}"))?;

        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "sql".to_string(),
            ServerConfig {
                command: binary.to_string(),
                args: args.to_vec(),
                root_markers: vec![],
                shutdown_idle_after_secs: 0,
            },
        );
        let config = LspConfig {
            enabled: true,
            servers,
        };
        let manager = LspManager::spawn(config);
        let inner = Arc::new(Inner { manager });
        let (evt_tx, evt_rx) = mpsc::channel::<LspEvent>(64);
        let mgr_ref = Arc::clone(&inner);
        std::thread::Builder::new()
            .name("sqeel-lsp-bridge".to_string())
            .spawn(move || bridge_loop(&mgr_ref.manager, evt_tx))
            .context("failed to spawn lsp bridge thread")?;
        Ok(Self {
            inner,
            events: evt_rx,
        })
    }

    pub async fn open_document(&mut self, uri: Uri, text: &str) -> anyhow::Result<()> {
        self.inner
            .manager
            .attach_buffer(BUF, &uri_to_path(&uri), "sql", text);
        Ok(())
    }

    pub async fn change_document(
        &mut self,
        _uri: Uri,
        _version: i32,
        text: &str,
    ) -> anyhow::Result<()> {
        self.inner.manager.notify_change(BUF, text);
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        // LspManager::Drop sends ShutdownAll; explicit shutdown requires ownership.
        // kill_on_drop equivalent: the Inner drop triggers ShutdownAll.
    }

    pub fn writer(&self) -> LspWriter {
        LspWriter {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[derive(Clone)]
pub struct LspWriter {
    inner: Arc<Inner>,
}

impl LspWriter {
    pub async fn change_document(
        &self,
        _uri: Uri,
        _version: i32,
        text: &str,
    ) -> anyhow::Result<()> {
        self.inner.manager.notify_change(BUF, text);
        Ok(())
    }

    pub fn request_completion(&self, uri: Uri, line: u32, col: u32) -> i64 {
        let id = next_id();
        self.inner.manager.send_request(id, BUF, "textDocument/completion", serde_json::json!({
            "textDocument": { "uri": uri.as_str() }, "position": { "line": line, "character": col }
        }));
        id
    }

    pub fn request_definition(&self, uri: Uri, line: u32, col: u32) -> i64 {
        let id = next_id();
        self.inner.manager.send_request(id, BUF, "textDocument/definition", serde_json::json!({
            "textDocument": { "uri": uri.as_str() }, "position": { "line": line, "character": col }
        }));
        id
    }

    pub fn request_hover(&self, uri: Uri, line: u32, col: u32) -> i64 {
        let id = next_id();
        self.inner.manager.send_request(id, BUF, "textDocument/hover", serde_json::json!({
            "textDocument": { "uri": uri.as_str() }, "position": { "line": line, "character": col }
        }));
        id
    }

    pub fn request_signature_help(&self, uri: Uri, line: u32, col: u32) -> i64 {
        let id = next_id();
        self.inner.manager.send_request(
            id,
            BUF,
            "textDocument/signatureHelp",
            serde_json::json!({
                "textDocument": { "uri": uri.as_str() },
                "position": { "line": line, "character": col }
            }),
        );
        id
    }
}

// ── Bridge ────────────────────────────────────────────────────────────────────

fn bridge_loop(manager: &LspManager, tx: mpsc::Sender<LspEvent>) {
    loop {
        match manager.try_recv_event() {
            Some(evt) => {
                if let Some(e) = translate_event(evt)
                    && tx.blocking_send(e).is_err()
                {
                    break;
                }
            }
            // 10 ms: 10× fewer syscalls than 1 ms, still well under a UI frame
            // budget (~16 ms for 60 Hz). The upstream LspManager only exposes
            // try_recv_event(), so polling is the only option here.
            None => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
}

fn translate_event(evt: HjklLspEvent) -> Option<LspEvent> {
    match evt {
        HjklLspEvent::Notification { method, params, .. }
            if method == "textDocument/publishDiagnostics" =>
        {
            let p: lsp_types::PublishDiagnosticsParams = serde_json::from_value(params).ok()?;
            Some(LspEvent::Diagnostics(
                p.diagnostics
                    .into_iter()
                    .map(|d| Diagnostic {
                        line: d.range.start.line,
                        col: d.range.start.character,
                        end_line: d.range.end.line,
                        end_col: d.range.end.character,
                        message: d.message,
                        severity: d.severity.unwrap_or(DiagnosticSeverity::ERROR),
                    })
                    .collect(),
            ))
        }
        HjklLspEvent::Response {
            request_id,
            result: Ok(value),
        } => translate_response(request_id, value),
        _ => None,
    }
}

fn translate_response(id: i64, result: Value) -> Option<LspEvent> {
    let debug = std::env::var("SQEEL_DEBUG_HL_DUMP").ok();
    if let Ok(def) = serde_json::from_value::<GotoDefinitionResponse>(result.clone()) {
        let loc = match def {
            GotoDefinitionResponse::Scalar(l) => Some((l.uri, l.range.start)),
            GotoDefinitionResponse::Array(mut v) => v.pop().map(|l| (l.uri, l.range.start)),
            GotoDefinitionResponse::Link(mut v) => v
                .pop()
                .map(|l| (l.target_uri, l.target_selection_range.start)),
        };
        if let Some((uri, pos)) = loc {
            return Some(LspEvent::Definition(
                id,
                uri.to_string(),
                pos.line,
                pos.character,
            ));
        }
    }
    if let Ok(hover) = serde_json::from_value::<Hover>(result.clone()) {
        if let Some(path) = &debug {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(f, "### lsp hover response id={id}");
            }
        }
        return hover_text_from_contents(&hover.contents).map(|t| LspEvent::Hover(id, t));
    }
    if let Ok(sig) = serde_json::from_value::<SignatureHelp>(result.clone())
        && let Some(text) = signature_help_text(&sig)
    {
        return Some(LspEvent::SignatureHelp(id, text));
    }
    if let Ok(list) = serde_json::from_value::<CompletionResponse>(result) {
        let items: Vec<String> = match list {
            CompletionResponse::Array(v) => v.into_iter().map(|i| i.label).collect(),
            CompletionResponse::List(l) => l.items.into_iter().map(|i| i.label).collect(),
        };
        return Some(LspEvent::Completion(id, items));
    }
    if let Some(path) = &debug {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "### lsp unroutable response id={id}");
        }
    }
    None
}

fn signature_help_text(sig: &SignatureHelp) -> Option<String> {
    if sig.signatures.is_empty() {
        return None;
    }
    let idx = sig.active_signature.unwrap_or(0) as usize;
    let info = sig.signatures.get(idx).or_else(|| sig.signatures.first())?;
    let label = &info.label;
    if label.trim().is_empty() {
        return None;
    }
    // Resolve active parameter index: per-signature wins over top-level.
    let active_param = info
        .active_parameter
        .or(sig.active_parameter)
        .map(|n| n as usize);
    let text = if let Some(param_idx) = active_param
        && let Some(ref params) = info.parameters
        && let Some(param) = params.get(param_idx)
    {
        match &param.label {
            ParameterLabel::Simple(name) => {
                // Wrap the first occurrence of the param substring in `[…]`.
                if let Some(pos) = label.find(name.as_str()) {
                    let before = &label[..pos];
                    let after = &label[pos + name.len()..];
                    format!("{before}[{name}]{after}")
                } else {
                    label.clone()
                }
            }
            ParameterLabel::LabelOffsets([start, end]) => {
                let s = *start as usize;
                let e = *end as usize;
                // Offsets are byte-safe via char boundary checks.
                if s <= label.len() && e <= label.len() && s <= e {
                    let param_slice = &label[s..e];
                    let before = &label[..s];
                    let after = &label[e..];
                    format!("{before}[{param_slice}]{after}")
                } else {
                    label.clone()
                }
            }
        }
    } else {
        label.clone()
    };
    Some(text)
}

fn hover_text_from_contents(contents: &HoverContents) -> Option<String> {
    fn ms(m: &MarkedString) -> String {
        match m {
            MarkedString::String(s) => s.clone(),
            MarkedString::LanguageString(ls) => ls.value.clone(),
        }
    }
    let text = match contents {
        HoverContents::Scalar(s) => ms(s),
        HoverContents::Array(items) => items.iter().map(ms).collect::<Vec<_>>().join("\n"),
        HoverContents::Markup(m) => m.value.clone(),
    };
    let t = text.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn uri_to_path(uri: &Uri) -> PathBuf {
    if let Ok(url) = url::Url::parse(uri.as_str())
        && let Ok(p) = url.to_file_path()
    {
        return p;
    }
    let s = uri.as_str();
    PathBuf::from(s.strip_prefix("file://").unwrap_or(s))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqls_driver_and_dsn_mysql() {
        let (driver, dsn) =
            super::sqls_driver_and_dsn("mysql://root:secret@localhost:3306/mydb").unwrap();
        assert_eq!(driver, "mysql");
        assert_eq!(dsn, "root:secret@tcp(localhost:3306)/mydb");
    }

    #[test]
    fn sqls_driver_and_dsn_mariadb_aliased_to_mysql() {
        let (driver, dsn) = super::sqls_driver_and_dsn("mariadb://u:p@db.host:3307/shop").unwrap();
        assert_eq!(driver, "mysql");
        assert_eq!(dsn, "u:p@tcp(db.host:3307)/shop");
    }

    #[test]
    fn sqls_driver_and_dsn_postgres_passthrough() {
        let (driver, dsn) = super::sqls_driver_and_dsn("postgres://u:p@h:5432/db").unwrap();
        assert_eq!(driver, "postgresql");
        assert_eq!(dsn, "postgres://u:p@h:5432/db");
    }

    #[test]
    fn sqls_driver_and_dsn_sqlite_strips_scheme() {
        let (driver, dsn) = super::sqls_driver_and_dsn("sqlite:///tmp/foo.db").unwrap();
        assert_eq!(driver, "sqlite3");
        assert_eq!(dsn, "/tmp/foo.db");
    }

    #[test]
    fn sqls_driver_and_dsn_rejects_unknown_scheme() {
        assert!(super::sqls_driver_and_dsn("other://x").is_err());
    }

    #[test]
    fn write_sqls_config_writes_file() {
        let path = write_sqls_config("mysql://u:p@host/db").unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("driver: mysql"));
        assert!(body.contains("u:p@tcp(host)/db"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hover_text_scalar_string_extracted() {
        let contents = HoverContents::Scalar(MarkedString::String("hello".into()));
        assert_eq!(
            super::hover_text_from_contents(&contents),
            Some("hello".into())
        );
    }

    #[test]
    fn hover_text_scalar_language_string_extracted() {
        let contents = HoverContents::Scalar(MarkedString::LanguageString(LanguageString {
            language: "sql".into(),
            value: "SELECT 1".into(),
        }));
        assert_eq!(
            super::hover_text_from_contents(&contents),
            Some("SELECT 1".into())
        );
    }

    #[test]
    fn hover_text_array_joins_with_newlines() {
        let contents = HoverContents::Array(vec![
            MarkedString::String("line1".into()),
            MarkedString::String("line2".into()),
        ]);
        assert_eq!(
            super::hover_text_from_contents(&contents),
            Some("line1\nline2".into())
        );
    }

    #[test]
    fn hover_text_markup_extracted() {
        let contents = HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: "## schema.table".into(),
        });
        assert_eq!(
            super::hover_text_from_contents(&contents),
            Some("## schema.table".into())
        );
    }

    #[test]
    fn hover_text_empty_returns_none() {
        let contents = HoverContents::Scalar(MarkedString::String("   ".into()));
        assert_eq!(super::hover_text_from_contents(&contents), None);
    }

    fn make_sig(
        label: &str,
        params: Vec<ParameterLabel>,
        active_sig: Option<u32>,
        active_param: Option<u32>,
    ) -> SignatureHelp {
        SignatureHelp {
            signatures: vec![SignatureInformation {
                label: label.to_string(),
                documentation: None,
                parameters: Some(
                    params
                        .into_iter()
                        .map(|l| ParameterInformation {
                            label: l,
                            documentation: None,
                        })
                        .collect(),
                ),
                active_parameter: None,
            }],
            active_signature: active_sig,
            active_parameter: active_param,
        }
    }

    #[test]
    fn sig_help_label_extracted() {
        let sig = make_sig(
            "date_trunc(field text, source timestamp)",
            vec![],
            None,
            None,
        );
        assert_eq!(
            super::signature_help_text(&sig),
            Some("date_trunc(field text, source timestamp)".into())
        );
    }

    #[test]
    fn sig_help_active_param_simple_wrapped() {
        let sig = make_sig(
            "date_trunc(field text, source timestamp)",
            vec![
                ParameterLabel::Simple("field text".into()),
                ParameterLabel::Simple("source timestamp".into()),
            ],
            None,
            Some(0),
        );
        assert_eq!(
            super::signature_help_text(&sig),
            Some("date_trunc([field text], source timestamp)".into())
        );
    }

    #[test]
    fn sig_help_active_param_second_simple() {
        let sig = make_sig(
            "date_trunc(field text, source timestamp)",
            vec![
                ParameterLabel::Simple("field text".into()),
                ParameterLabel::Simple("source timestamp".into()),
            ],
            None,
            Some(1),
        );
        assert_eq!(
            super::signature_help_text(&sig),
            Some("date_trunc(field text, [source timestamp])".into())
        );
    }

    #[test]
    fn sig_help_active_param_offsets() {
        let label = "fn(a int, b text)";
        // "a int" is at bytes 3..8, "b text" at bytes 10..16
        let sig = make_sig(
            label,
            vec![
                ParameterLabel::LabelOffsets([3, 8]),
                ParameterLabel::LabelOffsets([10, 16]),
            ],
            None,
            Some(1),
        );
        assert_eq!(
            super::signature_help_text(&sig),
            Some("fn(a int, [b text])".into())
        );
    }

    #[test]
    fn sig_help_empty_signatures_returns_none() {
        let sig = SignatureHelp {
            signatures: vec![],
            active_signature: None,
            active_parameter: None,
        };
        assert_eq!(super::signature_help_text(&sig), None);
    }

    #[test]
    fn sig_help_empty_label_returns_none() {
        let sig = SignatureHelp {
            signatures: vec![SignatureInformation {
                label: "   ".to_string(),
                documentation: None,
                parameters: None,
                active_parameter: None,
            }],
            active_signature: None,
            active_parameter: None,
        };
        assert_eq!(super::signature_help_text(&sig), None);
    }
}
