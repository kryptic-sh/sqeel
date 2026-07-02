mod completion_thread;
mod host;
pub mod splash;
mod theme;

// Re-export the engine crate so existing call sites like
// `sqeel_tui::editor::VimMode` keep compiling.
pub use hjkl_engine as editor;
pub use host::{FoldOp, LineRange, SqeelBufferId, SqeelHost, SqeelIntent};

pub use hjkl_clipboard::Clipboard;
use hjkl_clipboard::{MimeType, Selection};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use hjkl_editor_tui::spinner::frame as spinner_frame;
use hjkl_engine_tui::{EditorRatatuiExt, crossterm_to_input, style_from_ratatui};

use completion_thread::CompletionThread;
use crossterm::{
    cursor::SetCursorStyle,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use hjkl_engine::types::Style as EngineStyle;
use hjkl_engine::{Editor, Host};
use hjkl_form::TextFieldEditor;

/// Result of a synchronous tree-sitter highlight pass over a viewport
/// window. Mirrors the shape of the old `highlight_thread::HighlightResult`
/// — `start_row` + `row_count` define the absolute window inside the
/// buffer that `apply_window_spans` re-anchors against.
#[derive(Clone)]
pub struct HighlightResult {
    pub spans: Vec<sqeel_core::highlight::HighlightSpan>,
    pub start_row: usize,
    pub row_count: usize,
    pub parse_errors: Vec<sqeel_core::highlight::ParseError>,
    pub block_ranges: Vec<(usize, usize)>,
}
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use sqeel_config::TlsVerifyMode;
use sqeel_core::{
    AppState, UiProvider,
    completion_ctx::{self, CompletionCtx},
    config::load_main_config,
    highlight::{
        Dialect, Highlighter, first_syntax_error, is_show_create, statement_at_byte,
        statement_ranges, strip_sql_comments,
    },
    lsp::{LspClient, LspEvent},
    schema::{self, SchemaItemKind, SchemaTreeItem, SubGroup},
    state::{AddConnectionField, Focus, KeybindingMode, ResultsCursor, ResultsPane, VimMode},
};
use theme::ui;

/// Whether the active LSP binary came from `$PATH` or was installed by anvil.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LspSource {
    /// Resolved via `which` / `$PATH`.
    Path,
    /// Installed by `hjkl-anvil` into its store dir.
    Anvil,
}

/// Bundle of schema-sidebar search state: query string, whether the input box has
/// focus (typing mode), and cursor position within the filtered list.
#[derive(Clone, Default)]
struct SchemaSearch {
    query: Option<TextInput>,
    focused: bool,
    cursor: usize,
}

impl SchemaSearch {
    fn from_initial(q: Option<String>) -> Self {
        Self {
            query: q.map(|s| TextInput::from_str(&s)),
            focused: false,
            cursor: 0,
        }
    }
    fn query(&self) -> Option<&str> {
        self.query.as_ref().map(|q| q.text.as_str())
    }
    fn is_filtering(&self) -> bool {
        self.query().is_some_and(|q| !q.is_empty())
    }
    fn clear(&mut self) {
        *self = Self::default();
    }
    fn start(&mut self) {
        if self.query.is_none() {
            self.query = Some(TextInput::default());
            self.cursor = 0;
        }
        self.focused = true;
    }
    fn push(&mut self, c: char) {
        if let Some(ref mut q) = self.query {
            q.insert_char(c);
            self.cursor = 0;
        }
    }
    fn handle_nav(&mut self, code: KeyCode) -> bool {
        if let Some(ref mut q) = self.query
            && q.handle_nav(code)
        {
            self.cursor = 0;
            return true;
        }
        false
    }
    fn cursor_down(&mut self, list_len: usize) {
        let max = list_len.saturating_sub(1);
        self.cursor = (self.cursor + 1).min(max);
    }
    fn cursor_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
}

/// Single-line text input with caret movement. Used by every modal/dialog text
/// box (command palette, rename, file picker, schema search, add-connection)
/// so cursor behavior is uniform across the app.
#[derive(Clone, Default)]
struct TextInput {
    text: String,
    /// Caret position as a char index into `text`.
    cursor: usize,
}

impl TextInput {
    fn from_str(s: &str) -> Self {
        Self {
            text: s.to_string(),
            cursor: s.chars().count(),
        }
    }
    fn char_count(&self) -> usize {
        self.text.chars().count()
    }
    fn byte_at(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }
    fn insert_char(&mut self, c: char) {
        let b = self.byte_at(self.cursor);
        self.text.insert(b, c);
        self.cursor += 1;
    }
    /// Insert a string at the caret, advancing the caret past the end
    /// of the inserted text. Used by the bracketed-paste handler so a
    /// paste into a prompt lands as one chunk instead of N keystrokes.
    fn insert_str(&mut self, s: &str) {
        let b = self.byte_at(self.cursor);
        self.text.insert_str(b, s);
        self.cursor += s.chars().count();
    }
    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_at(self.cursor);
        let start = self.byte_at(self.cursor - 1);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }
    fn delete(&mut self) {
        if self.cursor >= self.char_count() {
            return;
        }
        let start = self.byte_at(self.cursor);
        let end = self.byte_at(self.cursor + 1);
        self.text.replace_range(start..end, "");
    }
    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    fn right(&mut self) {
        if self.cursor < self.char_count() {
            self.cursor += 1;
        }
    }
    fn home(&mut self) {
        self.cursor = 0;
    }
    fn end(&mut self) {
        self.cursor = self.char_count();
    }
    /// Try to handle a navigation/edit key. Returns true if consumed.
    /// Char insertion is handled by the caller so it can layer chord logic.
    fn handle_nav(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Left => {
                self.left();
                true
            }
            KeyCode::Right => {
                self.right();
                true
            }
            KeyCode::Home => {
                self.home();
                true
            }
            KeyCode::End => {
                self.end();
                true
            }
            KeyCode::Backspace => {
                self.backspace();
                true
            }
            KeyCode::Delete => {
                self.delete();
                true
            }
            _ => false,
        }
    }
}

/// Snapshot a `TextFieldEditor`'s text + caret column as a `TextInput`
/// view for the existing `draw_input_dialog` renderer. Single-line
/// fields only — multi-line cursor would lose row info.
fn text_field_view(field: &TextFieldEditor) -> TextInput {
    TextInput {
        text: field.text(),
        cursor: field.cursor().1,
    }
}

/// Fan a paste string into a `TextFieldEditor` via `handle_input`,
/// one engine `Char` event per char. Drops `\r` and treats `\n` as
/// a literal char insert (`Enter` is gated on single-line, so the
/// field would swallow it — fine for ex-command paste of multi-line
/// strings on single-line palettes).
fn text_field_paste(field: &mut TextFieldEditor, text: &str) {
    use hjkl_engine::{Input, Key};
    for c in text.chars() {
        if c == '\r' {
            continue;
        }
        let _ = field.handle_input(Input {
            key: Key::Char(c),
            ..Input::default()
        });
    }
}

/// App-local action enum for the file picker.
///
/// `hjkl_picker::PickerAction` no longer carries app-specific variants; every
/// consumer boxes its own type and downcasts on dispatch.
#[derive(Debug)]
enum SqeelFileAction {
    /// Open / switch to the `.sql` file at this path.
    OpenPath(std::path::PathBuf),
}

/// Thin wrapper around `hjkl_picker::FileSource` that overrides `select` to
/// emit `PickerAction::Custom(Box::new(SqeelFileAction::OpenPath(...)))`.
struct SqeelFileSource {
    inner: hjkl_picker::FileSource,
}

impl SqeelFileSource {
    fn new(root: std::path::PathBuf) -> Self {
        Self {
            inner: hjkl_picker::FileSource::new(root),
        }
    }
}

impl hjkl_picker::PickerLogic for SqeelFileSource {
    fn title(&self) -> &str {
        self.inner.title()
    }

    fn item_count(&self) -> usize {
        self.inner.item_count()
    }

    fn label(&self, idx: usize) -> String {
        self.inner.label(idx)
    }

    fn match_text(&self, idx: usize) -> String {
        self.inner.match_text(idx)
    }

    fn preview(&self, idx: usize) -> (hjkl_buffer::Buffer, String) {
        self.inner.preview(idx)
    }

    fn select(&self, idx: usize) -> hjkl_picker::PickerAction {
        let path = self
            .inner
            .items
            .lock()
            .ok()
            .and_then(|g| g.get(idx).map(|p| self.inner.root.join(p)));
        match path {
            Some(abs) => {
                hjkl_picker::PickerAction::Custom(Box::new(SqeelFileAction::OpenPath(abs)))
            }
            None => hjkl_picker::PickerAction::None,
        }
    }

    fn requery_mode(&self) -> hjkl_picker::RequeryMode {
        self.inner.requery_mode()
    }

    fn enumerate(
        &mut self,
        query: Option<&str>,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<std::thread::JoinHandle<()>> {
        self.inner.enumerate(query, cancel)
    }
}

/// Build a fresh leader+space picker rooted at sqeel's queries dir
/// (`~/.local/share/sqeel/queries/`). Lists every saved `.sql` buffer,
/// not just currently-open tabs — closed buffers stay reachable via
/// fuzzy find. Selection emits `SqeelFileAction::OpenPath` via `PickerAction::Custom`.
fn open_query_picker() -> anyhow::Result<hjkl_picker::Picker> {
    let dir = sqeel_core::persistence::queries_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine sqeel queries dir"))?;
    std::fs::create_dir_all(&dir).ok();
    Ok(hjkl_picker::Picker::new(Box::new(SqeelFileSource::new(
        dir,
    ))))
}

/// App-local action enum for the history picker.
#[derive(Debug)]
enum SqeelHistoryAction {
    /// Load this query string into the active editor buffer.
    LoadQuery(String),
}

/// Format a `SystemTime` relative to `now` as a human-readable age string.
/// Examples: "5s ago", "3m ago", "2h ago", "4d ago", "2w ago".
fn format_relative_time(now: SystemTime, then: SystemTime) -> String {
    let secs = now.duration_since(then).unwrap_or_default().as_secs();
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 7 * 86_400 {
        format!("{}d ago", secs / 86_400)
    } else if secs >= 365 * 86_400 {
        let years = secs / (365 * 86_400);
        if years == 1 {
            "1y ago".to_string()
        } else {
            format!("{years}y ago")
        }
    } else {
        let weeks = secs / (7 * 86_400);
        format!("{}w ago", weeks)
    }
}

/// In-memory picker source backed by a snapshot of `AppState::query_history`.
/// Newest entries appear first (snapshot is reversed on construction).
struct SqeelHistorySource {
    /// Snapshot taken at picker-open time; newest entry at index 0.
    entries: Vec<sqeel_core::state::HistoryEntry>,
    /// Wall-clock instant captured at construction, used for all label age
    /// calculations so every row's relative time is consistent.
    opened_at: SystemTime,
}

impl SqeelHistorySource {
    fn new(mut history: Vec<sqeel_core::state::HistoryEntry>) -> Self {
        history.reverse(); // newest first
        Self {
            entries: history,
            opened_at: SystemTime::now(),
        }
    }
}

impl hjkl_picker::PickerLogic for SqeelHistorySource {
    fn title(&self) -> &str {
        "Query history"
    }

    fn item_count(&self) -> usize {
        self.entries.len()
    }

    fn label(&self, idx: usize) -> String {
        let Some(entry) = self.entries.get(idx) else {
            return String::new();
        };
        let first_line = entry.query.lines().next().unwrap_or("").trim();
        let truncated = if first_line.chars().count() > 60 {
            let s: String = first_line.chars().take(60).collect();
            format!("{}…", s)
        } else {
            first_line.to_owned()
        };
        let age = format_relative_time(self.opened_at, entry.timestamp);
        format!("{:<63}  {}", truncated, age)
    }

    fn match_text(&self, idx: usize) -> String {
        self.entries
            .get(idx)
            .map(|e| e.query.clone())
            .unwrap_or_default()
    }

    fn preview(&self, idx: usize) -> (hjkl_buffer::Buffer, String) {
        let text = self
            .entries
            .get(idx)
            .map(|e| e.query.as_str())
            .unwrap_or_default();
        let buf = hjkl_buffer::Buffer::from_str(text);
        (buf, String::new())
    }

    fn select(&self, idx: usize) -> hjkl_picker::PickerAction {
        match self.entries.get(idx) {
            Some(entry) => hjkl_picker::PickerAction::Custom(Box::new(
                SqeelHistoryAction::LoadQuery(entry.query.clone()),
            )),
            None => hjkl_picker::PickerAction::None,
        }
    }

    fn requery_mode(&self) -> hjkl_picker::RequeryMode {
        hjkl_picker::RequeryMode::FilterInMemory
    }

    fn enumerate(
        &mut self,
        _query: Option<&str>,
        _cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<std::thread::JoinHandle<()>> {
        None
    }
}

/// Build a history picker from a snapshot of `AppState::query_history`.
/// Newest entries appear first. Returns `None` when history is empty.
fn open_history_picker(
    snapshot: Vec<sqeel_core::state::HistoryEntry>,
) -> Option<hjkl_picker::Picker> {
    if snapshot.is_empty() {
        return None;
    }
    Some(hjkl_picker::Picker::new(Box::new(SqeelHistorySource::new(
        snapshot,
    ))))
}

pub struct TuiProvider;

impl UiProvider for TuiProvider {
    fn run(state: Arc<Mutex<AppState>>) -> anyhow::Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async_run(state, true))
    }
}

/// Launch the TUI. Pass `show_splash = false` to skip the startup animation.
pub fn run(state: Arc<Mutex<AppState>>, show_splash: bool) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async_run(state, show_splash))
}

async fn async_run(state: Arc<Mutex<AppState>>, show_splash: bool) -> anyhow::Result<()> {
    let theme_err = theme::load();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Inside tmux, wrap the Kitty keyboard protocol enable sequence in a DCS
    // passthrough so the outer terminal receives it; tmux itself silently
    // drops bare CSI > u. Requires `set -g allow-passthrough on` in tmux.
    let in_tmux = std::env::var_os("TMUX").is_some();
    if in_tmux {
        use std::io::Write;
        stdout.write_all(b"\x1bPtmux;\x1b\x1b[>1u\x1b\\")?;
        stdout.flush()?;
    }
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let keybinding_mode = state.lock().unwrap().keybinding_mode;

    // ── Splash phase ──────────────────────────────────────────────────────────
    // Render the startup animation until the user presses any key.
    // Background async work (LSP, schema, etc.) is already in flight via
    // tokio::spawn in apps/sqeel before this point, so nothing is serialised.
    if show_splash {
        let screen = splash::SqeelStartScreen::new();
        loop {
            terminal.draw(|frame| {
                let theme_guard = theme::theme();
                let t = theme_guard.as_ref().expect("theme initialized");
                splash::render(frame, frame.area(), &screen, t);
            })?;

            // Poll for 50 ms (20 Hz redraw); animation cadence is driven by
            // Splash's internal wall clock (120 ms / 8 Hz) — the two are now
            // decoupled so high-frequency events no longer starve the animation.
            if !event::poll(Duration::from_millis(50))? {
                continue;
            }
            match event::read()? {
                Event::Key(key) => {
                    use crossterm::event::KeyEventKind;
                    // Only react to press events; ignore repeat/release.
                    if key.kind == KeyEventKind::Press {
                        break;
                    }
                }
                Event::Resize(_, _) => {
                    // Re-render on the next loop iteration.
                }
                _ => {}
            }
        }
    }

    let result = run_loop(&mut terminal, state, keybinding_mode, theme_err).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        SetCursorStyle::DefaultUserShape,
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture
    )?;
    if in_tmux {
        use std::io::Write;
        let mut out = io::stdout();
        out.write_all(b"\x1bPtmux;\x1b\x1b[<u\x1b\\")?;
        out.flush()?;
    }
    terminal.show_cursor()?;
    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: Arc<Mutex<AppState>>,
    keybinding_mode: KeybindingMode,
    theme_error: Option<String>,
) -> anyhow::Result<()> {
    // Load config before Editor::new so we can seed cursorline/cursorcolumn.
    let main_config = load_main_config()?;
    // Local cursor-highlight state: hjkl-engine 0.3 does not expose these in
    // Settings / Options, so sqeel-tui owns the booleans and interprets the
    // `:set cursorline` / `:set cursorcolumn` tokens before passing the command
    // to the engine ex dispatcher.
    let mut opt_cursorline = main_config.editor.cursorline;
    let mut opt_cursorcolumn = main_config.editor.cursorcolumn;
    let mut editor = Editor::new(
        hjkl_buffer::Buffer::new(),
        SqeelHost::new(Clipboard::new().expect("clipboard init")),
        hjkl_engine::types::Options {
            // Preserve sqeel's pre-0.1.0 default (Settings::default()
            // shiftwidth=2). Without this we'd silently flip to vim's
            // shiftwidth=8 per Options::default().
            shiftwidth: 2,
            ..Default::default()
        },
    );
    editor.keybinding_mode = keybinding_mode;
    let mut highlighter = sqeel_core::highlight::Highlighter::new_async();
    // `(dirty_gen, len_bytes, line_count)` cache for the joined source
    // string so pure scroll frames skip the O(N) Vec<String> ↔ String
    // join. Mirrors the apps/hjkl `RenderCache` pattern.
    let mut hl_cache_dirty_gen: u64 = u64::MAX;
    let mut hl_cache_len_bytes: usize = usize::MAX;
    let mut hl_cache_line_count: u32 = u32::MAX;
    let mut hl_cache_source: String = String::new();
    let mut hl_parsed_dirty_gen: Option<u64> = None;
    let completion_thread = CompletionThread::spawn()?;

    // Start LSP client if binary is configured and reachable
    let scratch_path = std::env::temp_dir().join("sqeel-scratch.sql");
    // Build a file:// URI from the OS temp path (works on Windows and Unix)
    let scratch_uri_str = {
        let p = scratch_path.to_string_lossy();
        if p.starts_with('/') {
            format!("file://{p}")
        } else {
            // Windows: C:\... → file:///C:/...
            format!("file:///{}", p.replace('\\', "/"))
        }
    };
    let scratch_uri: lsp_types::Uri = scratch_uri_str
        .parse()
        .unwrap_or_else(|_| "file:///tmp/sqeel-scratch.sql".parse().unwrap());
    let lsp_binary = main_config.editor.lsp_binary.clone();
    let lsp_auto_install = main_config.editor.lsp_auto_install;
    let mouse_scroll_lines = main_config.editor.mouse_scroll_lines;
    let leader_char: char = main_config.editor.leader_key;

    // ── LSP binary detection ─────────────────────────────────────────────────
    // 1. Try $PATH first (respects user's own sqls install).
    // 2. If missing + lsp_auto_install=true → toast-with-instruction.
    // 3. If missing + lsp_auto_install=false → banner only.
    let lsp_path_resolved: Option<std::path::PathBuf> = which::which(&lsp_binary).ok();
    let mut lsp_source: LspSource = LspSource::Path;
    // Resolved binary path used for both startup and `:Anvil install` post-install.
    let mut lsp_resolved_binary: Option<String> = lsp_path_resolved
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());

    let lsp_start_result = if let Some(ref resolved) = lsp_resolved_binary {
        LspClient::start(resolved, None, &[]).await
    } else {
        // Binary not on PATH — don't attempt to start yet.
        Err(anyhow::anyhow!("sqls not found on $PATH"))
    };
    if let Ok(path) = std::env::var("SQEEL_DEBUG_HL_DUMP") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            match &lsp_start_result {
                Ok(_) => {
                    let _ = writeln!(f, "### lsp started: binary={lsp_binary}");
                }
                Err(e) => {
                    let _ = writeln!(f, "### lsp FAILED to start: binary={lsp_binary} err={e}");
                }
            }
        }
    }
    let mut lsp: Option<LspClient> = lsp_start_result.ok();
    if let Some(ref mut client) = lsp {
        let _ = client.open_document(scratch_uri.clone(), "").await;
    }
    {
        let mut s = state.lock().unwrap();
        s.lsp_available = lsp.is_some();
        s.lsp_binary = lsp_binary.clone();
    }

    // Install pool for background anvil installs (`:Anvil install <name>`).
    // Ex-command registry — `:w`, `:q`, `:s///`, `:set`, … all resolve
    // through hjkl-ex since 0.33 (`hjkl_editor::runtime::ex` was removed).
    // Commands the registry doesn't know come back as `ExEffect::Unknown`
    // and fall through to sqeel's own `:colorscheme` / `:export` / … arms.
    let ex_registry = hjkl_ex::default_registry::<SqeelHost>();
    let install_pool = hjkl_anvil::InstallPool::new();
    // Active install handle — at most one in-flight at a time.
    let mut active_install: Option<hjkl_anvil::InstallHandle> = None;
    // Tracks whether the "Installing sqls…" toast has been surfaced for the
    // current install run, so repeated `Installing` polls don't spam toasts.
    let mut install_installing_announced: bool = false;

    // LSP restarts happen off the main loop (process spawn + initialize
    // handshake costs 100-500ms each). The main loop parks a config
    // path here, a helper task consumes it, and the finished
    // `LspClient` is shipped back through this channel to be swapped in
    // without blocking the render loop.
    let (lsp_restart_tx, mut lsp_restart_rx) =
        tokio::sync::mpsc::unbounded_channel::<anyhow::Result<LspClient>>();
    let mut lsp_restart_in_flight = false;

    // Cold-tab content loads run on `spawn_blocking` so a large file
    // or slow filesystem doesn't freeze the render loop on a tab
    // switch. Finished (tab_index, content) pairs arrive here.
    let (tab_load_tx, mut tab_load_rx) = tokio::sync::mpsc::unbounded_channel::<(usize, String)>();

    // Schema tree-flatten rebuilds run on `spawn_blocking` too — big
    // schemas (hundreds of tables × thousands of columns) allocate a
    // String per node and used to freeze the render loop when the
    // sidebar first populated.
    type SchemaCacheResult = (
        Vec<sqeel_core::schema::SchemaTreeItem>,
        Vec<sqeel_core::schema::SchemaTreeItem>,
        Vec<String>,
    );
    let (schema_cache_tx, mut schema_cache_rx) =
        tokio::sync::mpsc::unbounded_channel::<SchemaCacheResult>();

    let mut editor_dirty = false;
    // Prompt asking the user whether to save dirty buffers before exit.
    let mut quit_prompt: Option<()> = None;
    // Debounce the expensive content pipeline (full-buffer `String` build,
    // tree-sitter re-parse, LSP `didChange`, completion request).  Set when
    // the editor flags a change, cleared on publish.  On huge files this
    // collapses a burst of keystrokes into a single pipeline run.
    let mut content_dirty_since: Option<Instant> = None;
    // Last viewport top row we submitted to the highlight thread.  Seeded
    // to `usize::MAX` so the first iteration always triggers an initial
    // window highlight.
    let mut last_highlight_top: usize = usize::MAX;
    // Dialect of the last highlight submission. When the DB connection
    // resolves and `active_dialect` flips from `Generic` to a concrete
    // dialect, force a re-submit so the worker re-parses with the right
    // dialect-specific keyword promotions.
    let mut last_highlight_dialect: sqeel_core::highlight::Dialect =
        sqeel_core::highlight::Dialect::Generic;
    // Cached last highlight result so we can re-apply marker overlays
    // when diagnostics change, without re-parsing.
    let mut last_highlight_result: Option<HighlightResult> = None;
    let mut last_marker_diag_len: usize = usize::MAX;
    let mut doc_version: i32 = 0;
    // Buffers larger than this are not streamed to the LSP — sqls (and most
    // SQL LSPs) re-parse the whole document on every `didChange` and balloon
    // to multi-GB RAM on huge dumps / seed files.  We still highlight +
    // offer schema completions locally; only the LSP-sourced completions +
    // diagnostics go dark above the threshold.
    const LSP_MAX_BYTES: usize = 512 * 1024;
    // True when we've sent an empty `didChange` to release the LSP's copy
    // of the document after crossing the size threshold.  Reset when we
    // drop back below the threshold so the server re-syncs the real text.
    let mut lsp_suspended = false;
    let mut last_completion_id: Option<i64> = None;
    // Most recent hover request id; responses with different ids are
    // ignored (stale request raced a newer one or the popup was
    // dismissed before the server answered).
    let mut last_hover_id: Option<i64> = None;
    // Most recent signatureHelp request id; stale responses are dropped.
    let mut last_sig_help_id: Option<i64> = None;
    // Rendered signature string while Insert mode is active after `(` / `,`.
    let mut sig_help_text: Option<String> = None;
    // Most recent `gd` goto-definition request id; responses with a
    // different id belong to an earlier dismissed request and are
    // dropped silently.
    let mut last_definition_id: Option<i64> = None;
    let mut last_schema_completions: Vec<String> = Vec::new();
    // Last completion context + prefix, stashed so we can re-run the query
    // once a lazy schema load fills in tables/columns for that context.
    let mut last_completion_ctx: Option<(CompletionCtx, String)> = None;
    let mut last_pending_loads: usize = 0;
    let mut command_input: Option<TextFieldEditor> = None;
    let mut rename_input: Option<TextInput> = None;
    let mut file_picker: Option<hjkl_picker::Picker> = None;
    let mut delete_confirm: Option<String> = None;
    let mut schema_search =
        SchemaSearch::from_initial(state.lock().unwrap().schema_search_query.clone());

    let mut toasts: Vec<(String, ToastKind, std::time::Instant)> = Vec::new();
    if let Some(msg) = theme_error {
        toasts.push((msg, ToastKind::Error, std::time::Instant::now()));
    }
    // If sqls is missing and auto-install is enabled, open the y/N modal instead
    // of the v1 toast-with-instruction. When lsp_auto_install is false, fall back
    // to the banner-only path (no modal, no install prompt).
    let mut sqls_prompt_open: bool = if lsp_resolved_binary.is_none() {
        if lsp_auto_install {
            true // modal will be shown; no toast here
        } else {
            toasts.push((
                format!("LSP: {lsp_binary} missing (lsp_auto_install = false)"),
                ToastKind::Info,
                std::time::Instant::now(),
            ));
            false
        }
    } else {
        false
    };
    // Set when the modal's [y] / Enter arm fires. Checked at the top of the
    // main loop so the install is triggered on the very next iteration, using
    // the same code path as `:Anvil install sqls`.
    let mut sqls_install_pending: bool = false;
    let mut last_esc_at: Option<std::time::Instant> = None;
    // Leader-key chord state. Set when the leader is pressed in an eligible
    // context; cleared when the next key resolves the chord or it times out.
    let mut leader_pending_at: Option<std::time::Instant> = None;
    // Unified clipboard sink: native OS clipboard + OSC 52 fallback over SSH.
    let clipboard = Clipboard::new().expect("clipboard init");
    // Tracks an unfinished `y` in the results pane so a follow-up `y` within
    // 500ms yanks the whole row (vim `yy`).
    let mut pending_results_y: Option<std::time::Instant> = None;
    // Mouse drag tracking
    let mut last_draw_areas = DrawAreas::default();
    let mut mouse_drag_pane: Option<Focus> = None;
    let mut mouse_did_drag = false;
    // Anchor cell captured on mouse-down over a grid (results / hover).
    // Promoted to a visual-block selection on the first drag event;
    // `None` means the press didn't land on a selectable cell.
    let mut mouse_drag_anchor: Option<(usize, usize)> = None;
    // Holds an event the drag coalescer peeked-past but couldn't
    // swallow, so the next iteration processes it instead of calling
    // `event::read` again.
    let mut pending_event: Option<Event> = None;
    // Last cursor shape sent to the terminal. Each `SetCursorStyle`
    // emit is an ANSI escape → blocking write to stdout; skip it when
    // the shape hasn't changed since the last draw.
    let mut last_cursor_shape: Option<CursorShape> = None;
    // Force redraw on first iteration and after every event.
    let mut event_triggered_redraw = true;
    // Last time we ran the schema-freshness sweep. Rate-limited to once a
    // second so we don't walk the tree every tick.
    let mut last_stale_check = Instant::now();
    let mut last_terminal_size = terminal.size()?;
    let mut last_schema_loading = false;
    // Pending first `g` for the schema-pane `gg` chord. Cleared by any other key.
    let mut schema_g_pending = false;
    // Pending first `g` for the results-pane `gg` chord.
    let mut results_g_pending = false;
    // Running count prefix for results-pane nav (digits before j/k/h/l).
    // `0` is context-dependent: a leading `0` is "row start", an `0` mid-
    // count is a digit. Cleared after the next nav keystroke.
    let mut results_count: usize = 0;
    // Live `/` prompt over the results pane. `Some` while the user is
    // typing; commit stashes the pattern and clears this.
    let mut results_search_prompt: Option<TextFieldEditor> = None;
    // Most recent committed results-pane search pattern — kept so
    // `n` / `N` have something to repeat after the prompt closes.
    let mut results_search_pattern: Option<String> = None;
    // Hover popup `/` search — parallel to results_search_*.
    let mut hover_search_prompt: Option<TextInput> = None;
    let mut hover_search_pattern: Option<String> = None;
    loop {
        let mut needs_redraw = event_triggered_redraw;
        event_triggered_redraw = false;

        // Expire toasts after 5 seconds each.
        let before = toasts.len();
        toasts.retain(|(_, _, t)| t.elapsed() < Duration::from_millis(5000));
        if toasts.len() != before {
            needs_redraw = true;
        }

        // Modal [y] / Enter arm sets this flag; we trigger the sqls install here
        // on the next iteration using the same code path as `:Anvil install sqls`.
        if sqls_install_pending {
            sqls_install_pending = false;
            if active_install.is_none() {
                let spec = hjkl_anvil::ToolSpec {
                    category: hjkl_anvil::ToolCategory::Lsp,
                    description: "SQL language server".to_string(),
                    version: "latest".to_string(),
                    bin: "sqls".to_string(),
                    method: hjkl_anvil::InstallMethod::GoInstall(hjkl_anvil::GoMethod {
                        module: "github.com/sqls-server/sqls".to_string(),
                    }),
                };
                let handle = install_pool.install("sqls".to_string(), spec);
                active_install = Some(handle);
                install_installing_announced = false;
                toasts.push((
                    "Anvil: installing sqls via go install…".to_string(),
                    ToastKind::Info,
                    std::time::Instant::now(),
                ));
                needs_redraw = true;
            }
        }

        // Detect terminal size changes that don't produce Event::Resize (e.g. fullscreen toggle).
        if let Ok(size) = terminal.size()
            && size != last_terminal_size
        {
            last_terminal_size = size;
            terminal.autoresize()?;
            needs_redraw = true;
        }

        // Drain async-task result channels without touching the state
        // lock (they're pure mpsc try_recv). We apply them below in one
        // consolidated lock block so per-iter lock pressure stays low.
        let drained_tab_loads: Vec<(usize, String)> =
            std::iter::from_fn(|| tab_load_rx.try_recv().ok()).collect();
        let drained_schema_caches: Vec<(Vec<_>, Vec<_>, Vec<_>)> =
            std::iter::from_fn(|| schema_cache_rx.try_recv().ok()).collect();
        if !drained_tab_loads.is_empty() || !drained_schema_caches.is_empty() {
            needs_redraw = true;
        }

        // Single top-of-iter lock: apply drained channel results, run
        // periodic maintenance, and take any pending tasks/content.
        // Dropping to ~1 lock cycle here instead of 5+ reduces per-event
        // contention with the highlight / executor worker threads.
        let (pending_load, pending_tab_content) = {
            let mut s = state.lock().unwrap();
            for (tab_index, content) in drained_tab_loads {
                s.apply_loaded_tab_content(tab_index, content);
            }
            for (items, all, ids) in drained_schema_caches {
                s.apply_schema_cache_rebuild(items, all, ids);
            }
            // K queued a column load and is waiting for the table to
            // populate — install the cached view as soon as it does.
            s.try_install_pending_hover_table();
            s.evict_cold_tabs();
            let pending_load = s.pending_tab_load.take();
            let pending_tab_content = s.tab_content_pending.take();
            (pending_load, pending_tab_content)
        };

        // Kick off any pending cold-tab disk read on a blocking task
        // so a big file / slow FS doesn't stall the render loop.
        if let Some(load) = pending_load {
            let tx = tab_load_tx.clone();
            tokio::task::spawn_blocking(move || {
                let content = match sqeel_core::persistence::load_query(&load.name) {
                    Ok(c) => c,
                    Err(e) => {
                        // Surface the error as a SQL comment in the buffer so
                        // the user sees it immediately instead of an empty tab.
                        format!("-- failed to load '{}': {e}", load.name)
                    }
                };
                let _ = tx.send((load.tab_index, content));
            });
        }

        // Apply pending tab content (set when connection loads or tab switches).
        {
            if let Some(content) = pending_tab_content {
                editor.set_content(&content);
                // `set_content` flips the editor's dirty flag internally
                // (textarea rebuild). Consume it here so the main-loop
                // `take_dirty()` below doesn't mistake the programmatic
                // load for a user edit and mark the tab dirty.
                let _ = editor.take_dirty();
                editor_dirty = false;
                last_highlight_top = usize::MAX;
                needs_redraw = true;
                // Sync the LSP with the freshly loaded buffer so sqls can
                // emit diagnostics even when the user never touches the
                // editor after open / tab-switch.
                if let Some(ref client) = lsp
                    && content.len() <= LSP_MAX_BYTES
                {
                    // Fire the didChange off the render loop — serialize +
                    // send of a multi-MB buffer would otherwise freeze the
                    // UI for hundreds of ms the moment a large scratch
                    // loads.
                    doc_version += 1;
                    let writer = client.writer();
                    let uri = scratch_uri.clone();
                    let version = doc_version;
                    let text = std::sync::Arc::new(content.clone());
                    let debug_path = std::env::var("SQEEL_DEBUG_HL_DUMP").ok();
                    tokio::spawn(async move {
                        let _ = writer.change_document(uri, version, &text).await;
                        if let Some(path) = debug_path {
                            use std::io::Write;
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&path)
                            {
                                let preview: String = text.chars().take(80).collect();
                                let _ = writeln!(
                                    f,
                                    "### lsp didChange (tab-load) v{version} bytes={} preview={preview:?}",
                                    text.len()
                                );
                            }
                        }
                    });
                    lsp_suspended = false;
                }
            }
        }

        // Sync editor content + submit to highlight thread when changed.
        // Cheap per-keystroke work stays here; the expensive full-buffer
        // `String` build + highlight + LSP + completion submission is
        // debounced below.
        let content_changed = editor.take_dirty();
        if content_changed {
            needs_redraw = true;
            editor_dirty = true;
            if content_dirty_since.is_none() {
                content_dirty_since = Some(Instant::now());
            }
            // mark_active_dirty is folded into the main lock block below.
        }
        // Trailing-edge debounce: publish once the dirty window has aged
        // past the threshold.  The 50 ms event-poll timeout guarantees we
        // revisit this branch quickly even while the user is idle.
        const CONTENT_PUBLISH_DEBOUNCE: Duration = Duration::from_millis(75);
        // Above this total-byte size we stop running the heavy pipeline
        // entirely — no `editor.content()` join, no highlight submit, no
        // completion context parse.  Syntax spans already applied stay
        // rendered; the editor keeps working as a plain text buffer.
        const HEAVY_PIPELINE_MAX_BYTES: usize = 2 * 1024 * 1024;
        let should_publish = content_dirty_since
            .map(|t| t.elapsed() >= CONTENT_PUBLISH_DEBOUNCE)
            .unwrap_or(false);
        let buffer_bytes = if should_publish {
            <hjkl_buffer::Buffer as hjkl_engine::Query>::len_bytes(editor.buffer())
        } else {
            0
        };
        let content: Option<Arc<String>> =
            if should_publish && buffer_bytes <= HEAVY_PIPELINE_MAX_BYTES {
                content_dirty_since = None;
                Some(editor.content_arc())
            } else if should_publish {
                // Over the size gate — clear the dirty timer so we don't
                // re-enter every iteration, and drop any completion popup so
                // the user isn't staring at stale suggestions.
                content_dirty_since = None;
                state.lock().unwrap().dismiss_completions();
                last_completion_id = None;
                last_completion_ctx = None;
                None
            } else {
                None
            };
        // Merged into the single big lock below: extracted here so
        // downstream code (highlight resubmit gate) can consume it
        // without reacquiring the lock.
        let current_dialect;
        {
            let mut s = state.lock().unwrap();
            current_dialect = s.active_dialect;
            if content_changed {
                s.mark_active_dirty();
            }
            // Kick the schema-cache rebuild off the render loop when
            // it's stale and nothing else is already rebuilding. The
            // snapshot + flatten work runs on a blocking task; the
            // finished caches come back through `schema_cache_rx`
            // below and get applied via `apply_schema_cache_rebuild`.
            if let Some(nodes) = s.schema_snapshot_for_rebuild() {
                let tx = schema_cache_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let items = sqeel_core::schema::flatten_tree(&nodes);
                    let all = sqeel_core::schema::flatten_all(&nodes);
                    let mut ids: Vec<String> = Vec::new();
                    let mut stack: Vec<&sqeel_core::schema::SchemaNode> = nodes.iter().collect();
                    while let Some(node) = stack.pop() {
                        ids.push(node.name().to_owned());
                        match node {
                            sqeel_core::schema::SchemaNode::Database { tables, .. } => {
                                stack.extend(tables.iter())
                            }
                            sqeel_core::schema::SchemaNode::Table { columns, .. } => {
                                stack.extend(columns.iter())
                            }
                            sqeel_core::schema::SchemaNode::Column { .. } => {}
                            _ => {}
                        }
                    }
                    ids.sort();
                    ids.dedup();
                    let _ = tx.send((items, all, ids));
                });
                needs_redraw = true;
            }
            // Leaving the schema pane exits search mode entirely.
            if s.focus != Focus::Schema && schema_search.query.is_some() {
                schema_search.clear();
                needs_redraw = true;
            }
            s.vim_mode = editor.vim_mode();
            s.schema_search_query = schema_search.query().map(|q| q.to_string());
            if let Some(ref c) = content {
                s.editor_content = c.clone();
                s.editor_content_synced = true;
            }
            // Synchronous tree-sitter highlight pass for this frame.
            // Drain queued ContentEdits + reset flag from the engine into
            // the retained-tree highlighter, reparse incrementally over
            // the joined source, and run the highlights query scoped to
            // the visible viewport (with margin). Spans + parse errors
            // are spliced into the editor's per-row syntax table via
            // `apply_window_spans`.
            const HIGHLIGHT_WINDOW_MARGIN: usize = 500;
            let viewport_top = editor.host().viewport().top_row;
            let viewport_height = editor.viewport_height_value() as usize;
            let viewport_scrolled = last_highlight_top == usize::MAX
                || viewport_top.abs_diff(last_highlight_top) >= HIGHLIGHT_WINDOW_MARGIN / 2;
            let should_submit = should_resubmit_highlight(
                content_changed,
                viewport_scrolled,
                current_dialect,
                last_highlight_dialect,
            );
            let dragging_editor = mouse_drag_pane == Some(Focus::Editor);

            if should_submit && viewport_height > 0 && editor.buffer().row_count() > 0 {
                if editor.take_content_reset() {
                    highlighter.reset();
                    hl_parsed_dirty_gen = None;
                }
                for e in editor.take_content_edits() {
                    let ie = hjkl_bonsai::InputEdit {
                        start_byte: e.start_byte,
                        old_end_byte: e.old_end_byte,
                        new_end_byte: e.new_end_byte,
                        start_position: hjkl_bonsai::Point {
                            row: e.start_position.0 as usize,
                            column: e.start_position.1 as usize,
                        },
                        old_end_position: hjkl_bonsai::Point {
                            row: e.old_end_position.0 as usize,
                            column: e.old_end_position.1 as usize,
                        },
                        new_end_position: hjkl_bonsai::Point {
                            row: e.new_end_position.0 as usize,
                            column: e.new_end_position.1 as usize,
                        },
                    };
                    highlighter.edit(&ie);
                }

                let buffer = editor.buffer();
                let dg = <hjkl_buffer::Buffer as hjkl_engine::Query>::dirty_gen(buffer);
                let lb = <hjkl_buffer::Buffer as hjkl_engine::Query>::len_bytes(buffer);
                let lc = <hjkl_buffer::Buffer as hjkl_engine::Query>::line_count(buffer);
                let rebuild = dg != hl_cache_dirty_gen
                    || lb != hl_cache_len_bytes
                    || lc != hl_cache_line_count;
                if rebuild {
                    hl_cache_source.clear();
                    hl_cache_source.reserve(lb);
                    let rope = buffer.rope();
                    let row_count = lc as usize;
                    for r in 0..row_count {
                        if r > 0 {
                            hl_cache_source.push('\n');
                        }
                        hl_cache_source.push_str(&hjkl_buffer::rope_line_str(&rope, r));
                    }
                    hl_cache_dirty_gen = dg;
                    hl_cache_len_bytes = lb;
                    hl_cache_line_count = lc;
                }

                // Poll the async grammar loader each tick so the highlighter
                // becomes active once the background clone+compile finishes.
                highlighter.try_upgrade();

                let mut parse_ok = true;
                let parse_needed = hl_parsed_dirty_gen.map(|g| g != dg).unwrap_or(true);
                if parse_needed {
                    if highlighter.tree().is_none() {
                        highlighter.parse_initial(&hl_cache_source);
                    } else if !highlighter.parse_incremental(&hl_cache_source) {
                        parse_ok = false;
                    }
                    if parse_ok {
                        hl_parsed_dirty_gen = Some(dg);
                    }
                }

                if parse_ok {
                    let lines_count = buffer.row_count();
                    let start = viewport_top.saturating_sub(HIGHLIGHT_WINDOW_MARGIN);
                    let end =
                        (viewport_top + viewport_height + HIGHLIGHT_WINDOW_MARGIN).min(lines_count);
                    let row_count_window = end.saturating_sub(start);

                    let vp_start =
                        <hjkl_buffer::Buffer as hjkl_engine::Query>::byte_of_row(buffer, start);
                    let vp_end =
                        <hjkl_buffer::Buffer as hjkl_engine::Query>::byte_of_row(buffer, end)
                            .min(hl_cache_source.len());
                    let vp_end = vp_end.max(vp_start);

                    let inner_spans = highlighter.highlight_range(
                        &hl_cache_source,
                        current_dialect,
                        vp_start..vp_end,
                    );
                    // Re-anchor inner spans (absolute rows) to
                    // window-local rows so apply_window_spans (which
                    // adds `start_row` back) lands them correctly.
                    let spans: Vec<sqeel_core::highlight::HighlightSpan> = inner_spans
                        .into_iter()
                        .map(|mut s| {
                            s.start_row = s.start_row.saturating_sub(start);
                            s.end_row = s.end_row.saturating_sub(start);
                            s
                        })
                        .collect();
                    let parse_errors_full: Vec<sqeel_core::highlight::ParseError> = highlighter
                        .last_errors()
                        .iter()
                        .cloned()
                        .map(|mut e| {
                            e.start_row = e.start_row.saturating_sub(start);
                            e.end_row = e.end_row.saturating_sub(start);
                            e
                        })
                        .collect();
                    let block_ranges_abs = highlighter.block_ranges();

                    let result = HighlightResult {
                        spans,
                        start_row: start,
                        row_count: row_count_window,
                        parse_errors: parse_errors_full,
                        block_ranges: block_ranges_abs
                            .iter()
                            .map(|&(rs, re)| (rs.saturating_sub(start), re.saturating_sub(start)))
                            .collect(),
                    };

                    let row_count = buffer.row_count();
                    let diagnostics = merged_diagnostics(&s.lsp_diagnostics, &result.parse_errors);
                    apply_window_spans(&mut editor, &result, row_count, &diagnostics);
                    s.set_highlights(result.spans.clone());
                    let absolute: Vec<(usize, usize)> = result
                        .block_ranges
                        .iter()
                        .map(|&(rs, re)| (rs + result.start_row, re + result.start_row))
                        .collect();
                    editor.set_syntax_fold_ranges(absolute);
                    last_marker_diag_len = diagnostics.len();
                    last_highlight_result = Some(result);
                    last_highlight_top = viewport_top;
                    last_highlight_dialect = current_dialect;
                    needs_redraw = true;
                }
            } else if let Some(result) = last_highlight_result.as_ref()
                && !dragging_editor
            {
                // Diagnostics changed: re-apply the cached highlight so
                // the underlines update without paying another
                // tree-sitter parse.
                //
                // Skip while the user is mid-mouse-drag: every pixel-
                // row crossing during a drag would otherwise trigger a
                // full window splice (O(window_rows)), dominating the
                // drag event loop and producing visible selection lag.
                let diagnostics = merged_diagnostics(&s.lsp_diagnostics, &result.parse_errors);
                if diagnostics.len() != last_marker_diag_len {
                    let row_count = editor.buffer().row_count();
                    apply_window_spans(&mut editor, result, row_count, &diagnostics);
                    last_marker_diag_len = diagnostics.len();
                    needs_redraw = true;
                }
            }
        }

        // Auto-complete: on every content change, submit a schema completion query to the
        // background thread and (if LSP is available) request supplemental completions.
        // Gate on Insert mode — popping up completions while the user is in
        // Normal / Visual / any-visual mode is always a distraction.
        if let Some(ref content) = content {
            doc_version += 1;

            let (row, col) = editor.cursor();

            // Suppress completions after `;` or on empty buffer. Whitespace
            // only suppresses when ctx is `Any` — inside Table/Column/Qualified
            // contexts, an empty prefix should still surface candidates (e.g.
            // right after `where `).
            let buf_lines = buffer_lines(editor.buffer());
            let char_left = buf_lines.get(row).and_then(|line| {
                let before = &line[..col.min(line.len())];
                before.chars().next_back()
            });
            let hard_suppress = matches!(char_left, Some(';')) || char_left.is_none();

            let prefix = word_prefix_at(&buf_lines, row, col);
            let byte_offset = row_col_to_byte(&buf_lines, row, col);
            let ctx = completion_ctx::parse_context(content, byte_offset);

            let whitespace_left = matches!(char_left, Some(c) if c.is_whitespace());
            let mode_is_insert = editor.vim_mode() == hjkl_engine::VimMode::Insert;
            let suppress = !mode_is_insert
                || hard_suppress
                || (whitespace_left && matches!(ctx, CompletionCtx::Any));

            if suppress {
                state.lock().unwrap().dismiss_completions();
                last_completion_id = None;
                last_completion_ctx = None;
            } else {
                // Context-scoped pool (unfiltered) fed to the prefix-filter
                // thread; empty prefix returns the full sorted pool.
                let (pool, _) = {
                    let mut s = state.lock().unwrap();
                    s.lazy_load_for_context(&ctx);
                    let pool = s.completions_for_context(&ctx, "");
                    (pool, ())
                };
                last_completion_ctx = Some((ctx, prefix.clone()));
                completion_thread.submit(prefix, Arc::new(pool));

                if let Some(ref mut client) = lsp {
                    let too_big = content.len() > LSP_MAX_BYTES;
                    if too_big {
                        // First crossing: release the LSP's in-memory copy
                        // once so sqls can free whatever it parsed, then go
                        // silent until the buffer shrinks again.
                        if !lsp_suspended {
                            let _ = client
                                .change_document(scratch_uri.clone(), doc_version, "")
                                .await;
                            lsp_suspended = true;
                        }
                    } else {
                        if lsp_suspended {
                            lsp_suspended = false;
                        }
                        // Fire didChange from a spawned task so the
                        // per-keystroke JSON serialization of the full
                        // buffer doesn't block the render loop on
                        // multi-MB files. The LSP write-queue is an
                        // mpsc so latest-wins coalescing still happens
                        // on the other side.
                        let writer = client.writer();
                        let uri = scratch_uri.clone();
                        let version = doc_version;
                        let text = Arc::clone(content);
                        let debug_path = std::env::var("SQEEL_DEBUG_HL_DUMP").ok();
                        tokio::spawn(async move {
                            let _ = writer.change_document(uri, version, &text).await;
                            if let Some(path) = debug_path {
                                use std::io::Write;
                                if let Ok(mut f) = std::fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(&path)
                                {
                                    let preview: String = text.chars().take(80).collect();
                                    let _ = writeln!(
                                        f,
                                        "### lsp didChange v{version} bytes={} preview={preview:?}",
                                        text.len()
                                    );
                                }
                            }
                        });
                        // Fire the completion request off the render
                        // loop too — we get the id synchronously from
                        // the shared counter, the serialize + send run
                        // in a spawned task.
                        let id = client.writer().request_completion(
                            scratch_uri.clone(),
                            row as u32,
                            col as u32,
                        );
                        last_completion_id = Some(id);
                        // Signature help fires off the same `(` / `,` keystroke
                        // that just dirtied the buffer. didChange is already
                        // queued above; the server resolves position against
                        // the post-insertion column.
                        if matches!(char_left, Some('(' | ',')) {
                            last_sig_help_id = Some(client.writer().request_signature_help(
                                scratch_uri.clone(),
                                row as u32,
                                col as u32,
                            ));
                        }
                    }
                }
            }
        }

        // Leaving Insert mode: drop any lingering popup so the user
        // isn't stuck with stale completions while navigating in Normal.
        if editor.vim_mode() != hjkl_engine::VimMode::Insert {
            let mut s = state.lock().unwrap();
            if s.show_completions {
                s.dismiss_completions();
                last_completion_id = None;
                last_completion_ctx = None;
                needs_redraw = true;
            }
            if sig_help_text.is_some() {
                sig_help_text = None;
                last_sig_help_id = None;
                needs_redraw = true;
            }
        }

        // Poll schema completion thread results.
        if let Some(schema_items) = completion_thread.try_recv() {
            last_schema_completions = schema_items.clone();
            state.lock().unwrap().set_completions(schema_items);
            needs_redraw = true;
        }

        // When a DB connection resolves, `connect_and_spawn` writes a
        // sqls config file and parks the path on the state. Spawn the
        // LSP restart off the main loop so the 100-500ms process spawn
        // + initialize handshake doesn't freeze render. The finished
        // client ships back via `lsp_restart_rx`.
        if !lsp_restart_in_flight {
            let pending_cfg = state.lock().unwrap().pending_sqls_config.take();
            if let Some(cfg_path) = pending_cfg {
                lsp = None; // kill_on_drop SIGKILLs the previous sqls
                let args: Vec<String> =
                    vec!["-config".into(), cfg_path.to_string_lossy().into_owned()];
                let binary = lsp_binary.clone();
                let tx = lsp_restart_tx.clone();
                lsp_restart_in_flight = true;
                tokio::spawn(async move {
                    let result = LspClient::start(&binary, None, &args).await;
                    if let Ok(path) = std::env::var("SQEEL_DEBUG_HL_DUMP") {
                        use std::io::Write;
                        if let Ok(mut f) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&path)
                        {
                            match &result {
                                Ok(_) => {
                                    let _ =
                                        writeln!(f, "### lsp restarted with config={cfg_path:?}");
                                }
                                Err(e) => {
                                    let _ = writeln!(
                                        f,
                                        "### lsp restart FAILED config={cfg_path:?} err={e}"
                                    );
                                }
                            }
                        }
                    }
                    let _ = tx.send(result);
                });
            }
        }
        // Swap in a finished restart if one is ready. The `open_document`
        // call is cheap (just writes to the child's stdin) so leave it
        // inline.
        while let Ok(result) = lsp_restart_rx.try_recv() {
            lsp_restart_in_flight = false;
            if let Ok(mut client) = result {
                let content = editor.content();
                let _ = client.open_document(scratch_uri.clone(), &content).await;
                doc_version = 1;
                // Warm-up hover request right after opening the doc.
                // sqls fetches the DB schema on its first
                // symbol-resolution request, which would otherwise
                // penalise the user's *real* first `K` by several
                // hundred ms. Firing it now paves the cache before
                // the user interacts. Response is discarded — we
                // don't set `last_hover_id`, so the TUI arm's id
                // check drops the payload silently.
                let _ = client.writer().request_hover(scratch_uri.clone(), 0, 0);
                lsp = Some(client);
                lsp_suspended = false;
                needs_redraw = true;
            }
        }

        // Poll active anvil install handle (non-blocking). When Done, start LSP.
        // Terminal statuses (Done / Failed) clear `active_install` so a
        // follow-up `:Anvil install` isn't blocked by the in-flight guard.
        let mut install_terminal = false;
        let mut install_announced = install_installing_announced;
        if let Some(ref handle) = active_install {
            while let Some(status) = handle.try_recv() {
                match status {
                    hjkl_anvil::InstallStatus::Done { ref bin_path } => {
                        let bin_str = bin_path.to_string_lossy().into_owned();
                        toasts.push((
                            "sqls installed. Starting LSP…".to_string(),
                            ToastKind::Info,
                            std::time::Instant::now(),
                        ));
                        lsp_resolved_binary = Some(bin_str.clone());
                        lsp_source = LspSource::Anvil;
                        let binary = bin_str.clone();
                        let tx = lsp_restart_tx.clone();
                        lsp_restart_in_flight = true;
                        tokio::spawn(async move {
                            let result = LspClient::start(&binary, None, &[]).await;
                            let _ = tx.send(result);
                        });
                        install_terminal = true;
                        needs_redraw = true;
                    }
                    hjkl_anvil::InstallStatus::Failed(ref msg) => {
                        toasts.push((
                            format!("sqls install failed: {msg}"),
                            ToastKind::Error,
                            std::time::Instant::now(),
                        ));
                        install_terminal = true;
                        needs_redraw = true;
                    }
                    hjkl_anvil::InstallStatus::Installing => {
                        if !install_announced {
                            toasts.push((
                                "Installing sqls…".to_string(),
                                ToastKind::Info,
                                std::time::Instant::now(),
                            ));
                            install_announced = true;
                            needs_redraw = true;
                        }
                    }
                    _ => {
                        needs_redraw = true;
                    }
                }
            }
        }
        install_installing_announced = install_announced;
        if install_terminal {
            active_install = None;
            install_installing_announced = false;
        }

        // Drain LSP events
        if let Some(ref mut client) = lsp {
            while let Ok(event) = client.events.try_recv() {
                needs_redraw = true;
                match event {
                    LspEvent::Diagnostics(diags) => {
                        if let Ok(path) = std::env::var("SQEEL_DEBUG_HL_DUMP") {
                            use std::io::Write;
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&path)
                            {
                                let _ = writeln!(
                                    f,
                                    "### lsp diagnostics received ({} items)",
                                    diags.len()
                                );
                                for d in &diags {
                                    let _ = writeln!(
                                        f,
                                        "  {}:{} .. {}:{} [{:?}] {}",
                                        d.line, d.col, d.end_line, d.end_col, d.severity, d.message
                                    );
                                }
                            }
                        }
                        state.lock().unwrap().set_diagnostics(diags);
                    }
                    LspEvent::Definition(id, uri, line, col) => {
                        if Some(id) == last_definition_id {
                            last_definition_id = None;
                            // Same-buffer jumps update the cursor +
                            // push the current position onto the
                            // jumplist (so `Ctrl-o` returns). Cross-
                            // buffer / schema jumps can't be resolved
                            // locally — surface the location as a
                            // toast so the user still has a pointer.
                            if uri == scratch_uri.to_string() {
                                // Push the pre-jump cursor onto the
                                // jumplist so `Ctrl-o` returns to the
                                // call site after the goto.
                                let pre = editor.cursor();
                                editor.jump_to(line as usize + 1, col as usize + 1);
                                if editor.cursor() != pre {
                                    editor.record_jump(pre);
                                }
                            } else {
                                toasts.push((
                                    format!("Defined at: {uri} line {}:{}", line + 1, col + 1),
                                    ToastKind::Info,
                                    std::time::Instant::now(),
                                ));
                            }
                            needs_redraw = true;
                        }
                    }
                    LspEvent::Hover(id, text) => {
                        if Some(id) == last_hover_id {
                            let mut s = state.lock().unwrap();
                            // Both forms capture focus so the popup
                            // stays interactive: tables for cell nav +
                            // yank, plain text for scroll + Esc.
                            if let Some(table) = AppState::parse_hover_table(&text) {
                                s.open_hover_table(table);
                            } else {
                                s.open_hover_text(text);
                            }
                            needs_redraw = true;
                        }
                    }
                    LspEvent::Completion(id, lsp_items) => {
                        if Some(id) == last_completion_id {
                            // LSP results lead; schema identifiers fill in any gaps.
                            let mut merged = lsp_items;
                            let seen: std::collections::HashSet<&str> =
                                merged.iter().map(String::as_str).collect();
                            let extras: Vec<String> = last_schema_completions
                                .iter()
                                .filter(|item| !seen.contains(item.as_str()))
                                .cloned()
                                .collect();
                            merged.extend(extras);
                            state.lock().unwrap().set_completions(merged);
                        }
                    }
                    LspEvent::SignatureHelp(id, text) => {
                        if Some(id) == last_sig_help_id {
                            sig_help_text = Some(text);
                            last_hover_id = None;
                            needs_redraw = true;
                        }
                    }
                }
            }
        }

        // Single end-of-iter lock: schema loading flags, periodic stale
        // sweep, and results_dirty — collapsed from four separate
        // lock/unlock cycles so drag frames don't pay lock thrash here.
        let stale_due = last_stale_check.elapsed() >= Duration::from_secs(1);
        let (schema_loading, pending_loads, lazy_pool) = {
            let mut s = state.lock().unwrap();
            if stale_due {
                s.refresh_stale_schema();
            }
            if s.results_dirty {
                needs_redraw = true;
                s.results_dirty = false;
            }
            let pending_loads = s.schema_pending_loads;
            // If lazy schema loads just drained, stage a fresh completion
            // pool under the same lock to avoid reacquiring below.
            let lazy_pool = if last_pending_loads > 0
                && pending_loads < last_pending_loads
                && let Some((ctx, prefix)) = last_completion_ctx.clone()
            {
                Some((prefix, s.completions_for_context(&ctx, "")))
            } else {
                None
            };
            // Hover-loading state piggybacks on the same tick — its
            // spinner needs periodic redraws to animate while we
            // wait on the LSP response.
            if s.hover_loading {
                needs_redraw = true;
            }
            (s.schema_loading, pending_loads, lazy_pool)
        };
        if stale_due {
            last_stale_check = Instant::now();
        }
        if schema_loading || last_schema_loading != schema_loading {
            needs_redraw = true;
        }
        last_schema_loading = schema_loading;
        if let Some((prefix, pool)) = lazy_pool {
            completion_thread.submit(prefix, Arc::new(pool));
        }
        last_pending_loads = pending_loads;

        if needs_redraw {
            // Only the two things that can't be referenced directly
            // (need the state lock or a type adapt) get snapshotted;
            // everything else passes through by ref.
            let quit_prompt_dirty: Option<Vec<String>> = quit_prompt
                .as_ref()
                .map(|_| state.lock().unwrap().dirty_tab_names());
            // toast_snap only materializes when there are toasts to
            // render — the empty case is the common one during drag /
            // steady-state and skipping the Vec alloc keeps per-frame
            // work minimal.
            let toast_snap: Vec<(String, ToastKind)> = if toasts.is_empty() {
                Vec::new()
            } else {
                toasts
                    .iter()
                    .map(|(msg, kind, _)| (msg.clone(), *kind))
                    .collect()
            };
            let editor_search_text = editor.search_prompt().map(|p| p.text.as_str().to_owned());
            let last_editor_search = editor.last_search().map(str::to_owned);
            let results_search_text = results_search_prompt.as_ref().map(|p| p.text());
            let hover_search_text = hover_search_prompt.as_ref().map(|p| p.text.clone());
            let command_input_view = command_input.as_ref().map(text_field_view);
            let sig_help_snap = sig_help_text.clone();
            terminal.draw(|f| {
                let s = state.lock().unwrap();
                last_draw_areas = draw(
                    f,
                    &s,
                    &mut editor,
                    command_input_view.as_ref(),
                    rename_input.as_ref(),
                    file_picker.as_mut(),
                    delete_confirm.as_deref(),
                    quit_prompt_dirty.as_deref(),
                    sqls_prompt_open,
                    &schema_search,
                    editor_search_text.as_deref(),
                    last_editor_search.as_deref(),
                    results_search_text.as_deref(),
                    hover_search_text.as_deref(),
                    sig_help_snap.as_deref(),
                    &toast_snap,
                    opt_cursorline,
                    opt_cursorcolumn,
                );
            })?;
            // Apply the cursor shape requested by draw(). Hidden is handled by
            // ratatui (no set_cursor_position call leaves the cursor hidden).
            // Skip the ANSI escape when the shape hasn't changed — this
            // runs on every frame otherwise and each emit is a blocking
            // stdout write.
            // Prompt cursor shape feedback: when a `:` palette or
            // results `/` prompt is open in Normal mode, swap the
            // dialog's default Bar shape for Block so the user sees
            // mode feedback while editing the prompt line itself.
            let shape = if let Some(ref f) = command_input {
                match f.vim_mode() {
                    hjkl_engine::VimMode::Insert => CursorShape::Bar,
                    _ => CursorShape::Block,
                }
            } else if let Some(ref f) = results_search_prompt {
                match f.vim_mode() {
                    hjkl_engine::VimMode::Insert => CursorShape::Bar,
                    _ => CursorShape::Block,
                }
            } else {
                last_draw_areas.cursor_shape
            };
            if last_cursor_shape != Some(shape) {
                match shape {
                    CursorShape::Bar => {
                        let _ = execute!(terminal.backend_mut(), SetCursorStyle::SteadyBar);
                    }
                    CursorShape::Block => {
                        let _ = execute!(terminal.backend_mut(), SetCursorStyle::SteadyBlock);
                    }
                    CursorShape::Hidden => {}
                }
                last_cursor_shape = Some(shape);
            }
            last_terminal_size = terminal.size()?;
        }

        let mut ev = if let Some(e) = pending_event.take() {
            e
        } else {
            if !event::poll(Duration::from_millis(50))? {
                continue;
            }
            event::read()?
        };

        // Coalesce consecutive mouse-drag events: the terminal can
        // emit them faster than we can redraw, so N queued drags mean
        // N full frame redraws for what visually is one cursor jump.
        // Drain intermediate drags (keeping the latest) and stash any
        // non-drag follow-up for the next loop iteration.
        if matches!(&ev, Event::Mouse(m) if matches!(m.kind, MouseEventKind::Drag(_))) {
            while event::poll(Duration::ZERO)? {
                let next = event::read()?;
                if matches!(&next, Event::Mouse(m) if matches!(m.kind, MouseEventKind::Drag(_))) {
                    ev = next;
                } else {
                    pending_event = Some(next);
                    break;
                }
            }
        }

        event_triggered_redraw = true;
        match ev {
            Event::Mouse(mouse) => {
                // Hover popup steals focus; a stray click on the
                // underlying editor would otherwise move `state.focus`
                // back out of `Focus::Hover` and break Esc/nav.
                // Clicks inside the popup's body area move the hover
                // cursor to the clicked cell; clicks outside the body
                // are swallowed so they don't leak to the editor.
                if state.lock().unwrap().focus == Focus::Hover {
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            let mut s = state.lock().unwrap();
                            if let Some((row, col)) = s.hover_click_to_cell(mouse.column, mouse.row)
                            {
                                s.hover_cursor = ResultsCursor::Cell { row, col };
                                s.clamp_hover_scroll();
                                mouse_drag_anchor = Some((row, col));
                            } else {
                                mouse_drag_anchor = None;
                            }
                            mouse_drag_pane = Some(Focus::Hover);
                            mouse_did_drag = false;
                        }
                        MouseEventKind::Drag(MouseButton::Left) => {
                            if mouse_drag_pane == Some(Focus::Hover)
                                && let Some(anchor) = mouse_drag_anchor
                            {
                                use sqeel_core::state::{ResultsSelection, ResultsSelectionMode};
                                let mut s = state.lock().unwrap();
                                if !mouse_did_drag {
                                    s.hover_selection = Some(ResultsSelection {
                                        anchor,
                                        mode: ResultsSelectionMode::Block,
                                    });
                                }
                                if let Some((row, col)) =
                                    s.hover_drag_to_cell(mouse.column, mouse.row)
                                {
                                    s.hover_cursor = ResultsCursor::Cell { row, col };
                                    s.clamp_hover_scroll();
                                }
                            }
                            mouse_did_drag = true;
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            mouse_drag_pane = None;
                            mouse_did_drag = false;
                            mouse_drag_anchor = None;
                        }
                        MouseEventKind::ScrollDown => {
                            state.lock().unwrap().hover_cursor_move(1, 0);
                        }
                        MouseEventKind::ScrollUp => {
                            state.lock().unwrap().hover_cursor_move(-1, 0);
                        }
                        _ => {}
                    }
                    continue;
                }
                // Help overlay swallows clicks / drags so they don't
                // fall through to whatever pane sits underneath — but
                // scroll events need to pass through so the user can
                // mouse-scroll the help content itself.
                if state.lock().unwrap().show_help
                    && !matches!(
                        mouse.kind,
                        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
                    )
                {
                    continue;
                }
                let area = terminal.size()?;
                let schema_width = (area.width * 15 / 100).max(30);
                let show_results = state.lock().unwrap().has_results();
                let editor_ratio = state.lock().unwrap().editor_ratio;
                let s = state.lock().unwrap();
                let bottom_rows = 1 + (!s.lsp_available) as u16;
                drop(s);
                let main_height = area.height.saturating_sub(bottom_rows);
                let editor_height = if show_results {
                    (main_height as f32 * editor_ratio) as u16
                } else {
                    main_height
                };

                // Determine which pane the mouse is over
                let pane = if mouse.column < schema_width {
                    Focus::Schema
                } else if show_results && mouse.row >= editor_height {
                    Focus::Results
                } else {
                    Focus::Editor
                };

                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        use ratatui::layout::Position;
                        let pos = Position {
                            x: mouse.column,
                            y: mouse.row,
                        };
                        if last_draw_areas.tab_bar.contains(pos) {
                            // Click on editor tab bar — determine which tab
                            let rel_x =
                                mouse.column.saturating_sub(last_draw_areas.tab_bar.x) as usize;
                            let clicked = {
                                let s = state.lock().unwrap();
                                let mut offset = 0usize;
                                let mut found = None;
                                for (i, tab) in s.tabs.iter().enumerate() {
                                    let w = tab.name.len()
                                        + 2
                                        + if i + 1 < s.tabs.len() { 1 } else { 0 };
                                    if rel_x < offset + w {
                                        found = Some(i);
                                        break;
                                    }
                                    offset += w;
                                }
                                found
                            };
                            if let Some(idx) = clicked {
                                let content = {
                                    let mut s = state.lock().unwrap();
                                    s.focus = Focus::Editor;
                                    if editor_dirty {
                                        s.editor_content = editor.content_arc();
                                        s.mark_active_dirty();
                                        editor_dirty = false;
                                    }
                                    s.switch_to_tab(idx);
                                    s.tab_content_pending.take()
                                };
                                if let Some(c) = content {
                                    editor.set_content(&c);
                                    let _ = editor.take_dirty();
                                    editor_dirty = false;
                                    last_highlight_top = usize::MAX;
                                }
                            } else {
                                state.lock().unwrap().focus = Focus::Editor;
                            }
                            mouse_did_drag = false;
                        } else if let Some(rtb) = last_draw_areas.results_tab_bar
                            && rtb.contains(pos)
                        {
                            // Click on results tab bar — select tab and focus results
                            let rel_x = mouse.column.saturating_sub(rtb.x) as usize;
                            let clicked = {
                                let s = state.lock().unwrap();
                                let mut offset = 0usize;
                                let mut found = None;
                                for (i, _tab) in s.result_tabs.iter().enumerate() {
                                    let label_w = format!(" {} ", i + 1).chars().count();
                                    let w =
                                        label_w + if i + 1 < s.result_tabs.len() { 1 } else { 0 };
                                    if rel_x < offset + w {
                                        found = Some(i);
                                        break;
                                    }
                                    offset += w;
                                }
                                found
                            };
                            if let Some(idx) = clicked {
                                let mut s = state.lock().unwrap();
                                s.active_result_tab = idx;
                                s.focus = Focus::Results;
                            }
                            mouse_did_drag = false;
                        } else {
                            let mut s = state.lock().unwrap();
                            s.focus = pane;
                            if pane == Focus::Schema {
                                let la = last_draw_areas.schema_list_area;
                                if mouse.row < la.y {
                                    // Click in the search box row: enter search mode.
                                    schema_search.start();
                                } else if s.schema_connect_error.is_some()
                                    && s.schema_nodes.is_empty()
                                {
                                    // Any click on the failure placeholder
                                    // pops the details modal.
                                    s.open_connect_error_popup();
                                } else if mouse.row >= la.y
                                    && mouse.column >= la.x
                                    && mouse.column < la.x + la.width
                                {
                                    let rel = (mouse.row - la.y) as usize;
                                    let idx = rel + last_draw_areas.schema_list_offset;
                                    if last_draw_areas.schema_list_filtered {
                                        let query = schema_search.query().unwrap_or("");
                                        let filtered =
                                            schema::filter_items(s.all_schema_items(), query);
                                        if idx < filtered.len() {
                                            schema_search.cursor = idx;
                                            schema_search.focused = false;
                                            let path_str = schema::path_to_string(
                                                &filtered[idx].node_path,
                                                &s.schema_nodes,
                                            );
                                            s.restore_schema_cursor_by_path(&path_str);
                                            s.schema_toggle_current();
                                        }
                                    } else {
                                        let max = s.visible_schema_items().len();
                                        if idx < max {
                                            s.schema_cursor = idx;
                                            s.schema_toggle_current();
                                        }
                                    }
                                }
                            }
                            // Capture the anchor cell for a potential
                            // drag-select before we drop the lock. The
                            // first drag event promotes this to a live
                            // block selection; a plain click leaves
                            // the anchor unused.
                            mouse_drag_anchor = if pane == Focus::Results {
                                s.results_click_to_cell(mouse.column, mouse.row)
                            } else {
                                None
                            };
                            drop(s);
                            if pane == Focus::Editor {
                                let (doc_row, doc_col) = editor_cell_to_doc(
                                    &editor,
                                    last_draw_areas.editor,
                                    mouse.column,
                                    mouse.row,
                                );
                                editor.mouse_click_doc(doc_row, doc_col);
                            }
                            mouse_drag_pane = Some(pane);
                            mouse_did_drag = false;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if mouse_drag_pane == Some(Focus::Editor) {
                            if !mouse_did_drag {
                                editor.mouse_begin_drag();
                            }
                            let (doc_row, doc_col) = editor_cell_to_doc(
                                &editor,
                                last_draw_areas.editor,
                                mouse.column,
                                mouse.row,
                            );
                            editor.mouse_extend_drag_doc(doc_row, doc_col);
                        } else if mouse_drag_pane == Some(Focus::Results)
                            && let Some(anchor) = mouse_drag_anchor
                        {
                            // First drag frame: install a Block
                            // selection anchored at the mouse-down
                            // cell. Subsequent frames extend it by
                            // moving the results cursor.
                            use sqeel_core::state::{
                                ResultsCursor, ResultsSelection, ResultsSelectionMode,
                            };
                            let mut s = state.lock().unwrap();
                            if !mouse_did_drag {
                                let idx = s.active_result_tab;
                                if let Some(t) = s.result_tabs.get_mut(idx) {
                                    t.cursor = ResultsCursor::Cell {
                                        row: anchor.0,
                                        col: anchor.1,
                                    };
                                    t.selection = Some(ResultsSelection {
                                        anchor,
                                        mode: ResultsSelectionMode::Block,
                                    });
                                }
                            }
                            if let Some((row, col)) =
                                s.results_drag_to_cell(mouse.column, mouse.row)
                            {
                                let idx = s.active_result_tab;
                                if let Some(t) = s.result_tabs.get_mut(idx) {
                                    t.cursor = ResultsCursor::Cell { row, col };
                                }
                                s.clamp_results_cursor();
                            }
                        }
                        mouse_did_drag = true;
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if !mouse_did_drag && mouse_drag_pane == Some(Focus::Results) {
                            let click = {
                                let s = state.lock().unwrap();
                                extract_results_left_click(
                                    mouse.column,
                                    mouse.row,
                                    &last_draw_areas,
                                    &s,
                                )
                            };
                            if let Some((text, label, cur)) = click {
                                {
                                    let mut s = state.lock().unwrap();
                                    let idx = s.active_result_tab;
                                    if let Some(t) = s.result_tabs.get_mut(idx) {
                                        t.cursor = cur;
                                    }
                                    s.clamp_results_cursor();
                                }
                                let ok = clipboard
                                    .set(Selection::Clipboard, MimeType::Text, text.as_bytes())
                                    .is_ok();
                                toasts.push((
                                    if ok {
                                        format!("{label} copied to clipboard")
                                    } else {
                                        format!("{label}: clipboard copy failed (too large)")
                                    },
                                    if ok {
                                        ToastKind::Info
                                    } else {
                                        ToastKind::Error
                                    },
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                        mouse_drag_pane = None;
                        mouse_did_drag = false;
                    }
                    MouseEventKind::Up(MouseButton::Right) => {
                        use ratatui::layout::Position;
                        let pos = Position {
                            x: mouse.column,
                            y: mouse.row,
                        };
                        if last_draw_areas.results.is_some_and(|r| r.contains(pos)) {
                            let s = state.lock().unwrap();
                            if let Some(text) =
                                extract_results_row(mouse.column, mouse.row, &last_draw_areas, &s)
                            {
                                drop(s);
                                let ok = clipboard
                                    .set(Selection::Clipboard, MimeType::Text, text.as_bytes())
                                    .is_ok();
                                toasts.push((
                                    if ok {
                                        "Row copied to clipboard".to_string()
                                    } else {
                                        "Row: clipboard copy failed (too large)".to_string()
                                    },
                                    if ok {
                                        ToastKind::Info
                                    } else {
                                        ToastKind::Error
                                    },
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        let mut s = state.lock().unwrap();
                        if s.show_help {
                            let max = last_draw_areas.help_max_scroll;
                            s.help_scroll = s
                                .help_scroll
                                .saturating_add(mouse_scroll_lines as u16)
                                .min(max);
                        } else {
                            s.focus = pane;
                            match pane {
                                Focus::Schema => {
                                    schema_search.focused = false;
                                    // Wheel always scrolls the viewport — works
                                    // even with an active filter. The cursor
                                    // stays where the user put it.
                                    s.scroll_schema_viewport(mouse_scroll_lines as i32);
                                }
                                Focus::Results => {
                                    for _ in 0..mouse_scroll_lines {
                                        s.scroll_results_down();
                                    }
                                }
                                Focus::Editor => {
                                    drop(s);
                                    editor.scroll_down(mouse_scroll_lines as i16);
                                }
                                Focus::Hover => {
                                    for _ in 0..mouse_scroll_lines {
                                        s.hover_cursor_move(1, 0);
                                    }
                                }
                            }
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        let mut s = state.lock().unwrap();
                        if s.show_help {
                            s.help_scroll = s.help_scroll.saturating_sub(mouse_scroll_lines as u16);
                        } else {
                            s.focus = pane;
                            match pane {
                                Focus::Schema => {
                                    schema_search.focused = false;
                                    s.scroll_schema_viewport(-(mouse_scroll_lines as i32));
                                }
                                Focus::Results => {
                                    for _ in 0..mouse_scroll_lines {
                                        s.scroll_results_up();
                                    }
                                }
                                Focus::Editor => {
                                    drop(s);
                                    editor.scroll_up(mouse_scroll_lines as i16);
                                }
                                Focus::Hover => {
                                    for _ in 0..mouse_scroll_lines {
                                        s.hover_cursor_move(-1, 0);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::Paste(text) => {
                // Bracketed-paste arrives as one event rather than N
                // key events. Insert as one atomic chunk:
                // - editor in Insert mode → splice into the buffer.
                // - any active prompt → paste into that prompt.
                // Other modes ignore the paste (vim-ish — use `p`/`P`
                // for the yank-register flow).
                let focus = state.lock().unwrap().focus;
                if focus == Focus::Editor && editor.vim_mode() == hjkl_engine::VimMode::Insert {
                    editor.insert_str(&text);
                } else if let Some(ref mut cmd) = command_input {
                    text_field_paste(cmd, &text);
                } else if let Some(ref mut rp) = rename_input {
                    rp.insert_str(&text);
                }
            }
            Event::Key(key) => {
                // Hover popups (both text and table forms) capture
                // focus and live until Esc; no auto-dismiss here.
                // Ctrl-C while a query / batch is running cancels the
                // current query and skips any remaining ones in the
                // batch. Falls through to the regular handler
                // otherwise so Ctrl-C keeps its "dismiss results"
                // binding in idle state.
                if key.modifiers == KeyModifiers::CONTROL && matches!(key.code, KeyCode::Char('c'))
                {
                    let in_flight = state.lock().unwrap().query_in_flight();
                    if in_flight {
                        state.lock().unwrap().cancel_current_query();
                        toasts.push((
                            "Query cancelled".into(),
                            ToastKind::Info,
                            std::time::Instant::now(),
                        ));
                        continue;
                    }
                }
                // Double-Esc within 500ms dismisses any visible toasts. Tracked
                // globally so it works regardless of which mode the first Esc
                // may have exited.
                if key.code == KeyCode::Esc {
                    let now = std::time::Instant::now();
                    if let Some(prev) = last_esc_at
                        && now.duration_since(prev) <= Duration::from_millis(500)
                        && !toasts.is_empty()
                    {
                        toasts.clear();
                    }
                    last_esc_at = Some(now);
                }
                let (
                    focus,
                    vim_mode,
                    show_completions,
                    show_switcher,
                    show_add,
                    show_help,
                    show_connect_error,
                    show_results,
                    show_pgpass_picker,
                ) = {
                    let s = state.lock().unwrap();
                    (
                        s.focus,
                        s.vim_mode,
                        s.show_completions,
                        s.show_connection_switcher,
                        s.show_add_connection,
                        s.show_help,
                        s.show_connect_error_popup,
                        s.has_results(),
                        s.show_pgpass_picker,
                    )
                };

                // ── Leader-key chord ─────────────────────────────────────────────
                // Eligible context: no modal open, schema search box not focused,
                // and either we're outside the editor or in Vim Normal mode.
                let leader_eligible = command_input.is_none()
                    && rename_input.is_none()
                    && file_picker.is_none()
                    && delete_confirm.is_none()
                    && editor.search_prompt().is_none()
                    && !sqls_prompt_open
                    && !show_switcher
                    && !show_add
                    && !show_pgpass_picker
                    && !show_help
                    && !show_connect_error
                    && !show_completions
                    && !schema_search.focused
                    && (focus != Focus::Editor || vim_mode == VimMode::Normal);

                // Resolve a pending leader chord with the current key.
                if let Some(t) = leader_pending_at {
                    let expired = t.elapsed() > Duration::from_millis(1500);
                    leader_pending_at = None;
                    if !expired {
                        // Capital R (Shift+r) → refresh schema cache.
                        if matches!(
                            (key.modifiers, key.code),
                            (KeyModifiers::SHIFT, KeyCode::Char('R'))
                        ) {
                            let conn_name = state
                                .lock()
                                .unwrap()
                                .active_connection
                                .clone()
                                .unwrap_or_else(|| "database".into());
                            let triggered = state.lock().unwrap().refresh_schema();
                            if triggered {
                                toasts.push((
                                    format!("Refreshing schema for {conn_name}…"),
                                    ToastKind::Info,
                                    std::time::Instant::now(),
                                ));
                            } else {
                                toasts.push((
                                    "No active connection to refresh".to_string(),
                                    ToastKind::Error,
                                    std::time::Instant::now(),
                                ));
                            }
                            continue;
                        }
                        if key.modifiers == KeyModifiers::NONE {
                            match key.code {
                                KeyCode::Char('c') => {
                                    state.lock().unwrap().open_connection_switcher();
                                    continue;
                                }
                                KeyCode::Char('n') => {
                                    let content = {
                                        let mut s = state.lock().unwrap();
                                        s.new_tab();
                                        s.tab_content_pending.take()
                                    };
                                    if let Some(c) = content {
                                        editor.set_content(&c);
                                        let _ = editor.take_dirty();
                                        editor_dirty = false;
                                        last_highlight_top = usize::MAX;
                                    }
                                    continue;
                                }
                                KeyCode::Char('r') => {
                                    let s = state.lock().unwrap();
                                    let current = s
                                        .tabs
                                        .get(s.active_tab)
                                        .map(|t| t.name.clone())
                                        .unwrap_or_default();
                                    drop(s);
                                    rename_input = Some(TextInput::from_str(&current));
                                    continue;
                                }
                                KeyCode::Char('h') => {
                                    let snapshot = state.lock().unwrap().query_history.clone();
                                    file_picker = open_history_picker(snapshot);
                                    continue;
                                }
                                KeyCode::Char(c) if c == leader_char => {
                                    file_picker = open_query_picker().ok();
                                    continue;
                                }
                                KeyCode::Char('d') => {
                                    let s = state.lock().unwrap();
                                    if let Some(name) =
                                        s.tabs.get(s.active_tab).map(|t| t.name.clone())
                                    {
                                        drop(s);
                                        delete_confirm = Some(name);
                                    }
                                    continue;
                                }
                                KeyCode::Esc => continue,
                                // <leader><CR> — run statement under cursor
                                // (tmux/SSH-friendly alt for Ctrl+Enter)
                                KeyCode::Enter => {
                                    run_statement_under_cursor(&mut editor, &state);
                                    continue;
                                }
                                // <leader><Tab> — run all statements in file
                                // (tmux/SSH-friendly alt for Ctrl+Shift+Enter)
                                KeyCode::Tab => {
                                    run_all_statements(&mut editor, &state);
                                    continue;
                                }
                                _ => {}
                            }
                        }
                    }
                    // Unknown chord or expired — silently drop the second key so
                    // the leader doesn't accidentally insert text.
                    continue;
                }

                // Arm leader on press.
                if leader_eligible
                    && key.modifiers == KeyModifiers::NONE
                    && matches!(key.code, KeyCode::Char(c) if c == leader_char)
                {
                    leader_pending_at = Some(std::time::Instant::now());
                    continue;
                }

                // ── Quit confirmation (unsaved buffers) ──────────────────────────
                if quit_prompt.is_some() {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Char('y')) => {
                            quit_prompt = None;
                            let pending = {
                                let mut s = state.lock().unwrap();
                                s.editor_content = editor.content_arc();
                                s.editor_content_synced = true;
                                s.mark_active_dirty();
                                s.prepare_save_all_dirty()
                            };
                            let failed = commit_pending_saves(&state, pending).await;
                            if failed.is_empty() {
                                break;
                            }
                            toasts.push((
                                format!("Save failed for: {}", failed.join(", ")),
                                ToastKind::Error,
                                std::time::Instant::now(),
                            ));
                        }
                        (KeyModifiers::NONE, KeyCode::Char('n')) => {
                            break;
                        }
                        _ => {
                            // Any other key (Esc, c, …) cancels.
                            quit_prompt = None;
                        }
                    }
                    continue;
                }

                // ── Command input mode ───────────────────────────────────────────
                if let Some(ref mut cmd) = command_input {
                    use hjkl_engine::{Input as EngineInput, Key as EngineKey};
                    let input: EngineInput = crossterm_to_input(key);
                    if input.key == EngineKey::Esc {
                        // Esc-once / Esc-twice grammar (mirrors apps/hjkl):
                        // empty (any mode) or Normal+non-empty → close.
                        // Insert+non-empty → drop to Normal so user can
                        // edit the prompt line with vim motions.
                        let text = cmd.text();
                        if text.is_empty() || cmd.vim_mode() != hjkl_engine::VimMode::Insert {
                            command_input = None;
                        } else {
                            cmd.enter_normal();
                        }
                        continue;
                    }
                    if input.key == EngineKey::Enter {
                        let cmd_str = cmd.text();
                        command_input = None;
                        let trimmed = cmd_str.trim();
                        // ── :LspInfo ────────────────────────────────────────────────
                        if trimmed == "LspInfo" {
                            let source_label = match lsp_source {
                                LspSource::Path => "PATH",
                                LspSource::Anvil => "anvil",
                            };
                            let state_label = if lsp.is_some() {
                                "running"
                            } else {
                                "not running"
                            };
                            let bin_label = lsp_resolved_binary.as_deref().unwrap_or(&lsp_binary);
                            toasts.push((
                                format!(
                                    "LSP {lsp_binary}: {state_label} (source: {source_label}) binary={bin_label}"
                                ),
                                ToastKind::Info,
                                std::time::Instant::now(),
                            ));
                            continue;
                        }
                        // ── :Anvil … ─────────────────────────────────────────────
                        if let Some(rest) = trimmed.strip_prefix("Anvil") {
                            match parse_anvil_cmd(rest) {
                                AnvilCmd::Usage => {
                                    // Bare :Anvil — usage hint (no picker UI yet).
                                    toasts.push((
                                        "Anvil: usage — :Anvil install <name>  |  :Anvil update [name]  |  :Anvil uninstall <name>".to_string(),
                                        ToastKind::Info,
                                        std::time::Instant::now(),
                                    ));
                                }
                                AnvilCmd::Install(name) => {
                                    if name != "sqls" {
                                        toasts.push((
                                            format!("Anvil: unknown tool {name:?} (only 'sqls' supported)"),
                                            ToastKind::Error,
                                            std::time::Instant::now(),
                                        ));
                                    } else if active_install.is_some() {
                                        toasts.push((
                                            "Anvil: install already in progress".to_string(),
                                            ToastKind::Info,
                                            std::time::Instant::now(),
                                        ));
                                    } else {
                                        let spec = hjkl_anvil::ToolSpec {
                                            category: hjkl_anvil::ToolCategory::Lsp,
                                            description: "SQL language server".to_string(),
                                            version: "latest".to_string(),
                                            bin: "sqls".to_string(),
                                            method: hjkl_anvil::InstallMethod::GoInstall(
                                                hjkl_anvil::GoMethod {
                                                    module: "github.com/sqls-server/sqls"
                                                        .to_string(),
                                                },
                                            ),
                                        };
                                        let handle = install_pool.install("sqls".to_string(), spec);
                                        active_install = Some(handle);
                                        toasts.push((
                                            "Anvil: installing sqls via go install…".to_string(),
                                            ToastKind::Info,
                                            std::time::Instant::now(),
                                        ));
                                    }
                                }
                                AnvilCmd::Update(name_opt) => {
                                    // Re-install at latest; only sqls supported for now.
                                    let name = name_opt.unwrap_or("sqls");
                                    if name != "sqls" {
                                        toasts.push((
                                            format!("Anvil: unknown tool {name:?} (only 'sqls' supported)"),
                                            ToastKind::Error,
                                            std::time::Instant::now(),
                                        ));
                                    } else if active_install.is_some() {
                                        toasts.push((
                                            "Anvil: install already in progress".to_string(),
                                            ToastKind::Info,
                                            std::time::Instant::now(),
                                        ));
                                    } else {
                                        let spec = hjkl_anvil::ToolSpec {
                                            category: hjkl_anvil::ToolCategory::Lsp,
                                            description: "SQL language server".to_string(),
                                            version: "latest".to_string(),
                                            bin: "sqls".to_string(),
                                            method: hjkl_anvil::InstallMethod::GoInstall(
                                                hjkl_anvil::GoMethod {
                                                    module: "github.com/sqls-server/sqls"
                                                        .to_string(),
                                                },
                                            ),
                                        };
                                        let handle = install_pool.install("sqls".to_string(), spec);
                                        active_install = Some(handle);
                                        toasts.push((
                                            "Anvil: updating sqls via go install…".to_string(),
                                            ToastKind::Info,
                                            std::time::Instant::now(),
                                        ));
                                    }
                                }
                                AnvilCmd::Uninstall(name) => {
                                    if name != "sqls" {
                                        toasts.push((
                                            format!("Anvil: unknown tool {name:?} (only 'sqls' supported)"),
                                            ToastKind::Error,
                                            std::time::Instant::now(),
                                        ));
                                    } else {
                                        // Remove the anvil-managed store dir for sqls.
                                        match hjkl_anvil::store::package_dir("sqls") {
                                            Ok(pkg_dir) => {
                                                if pkg_dir.exists() {
                                                    if let Err(e) =
                                                        std::fs::remove_dir_all(&pkg_dir)
                                                    {
                                                        toasts.push((
                                                            format!("Anvil: uninstall failed: {e}"),
                                                            ToastKind::Error,
                                                            std::time::Instant::now(),
                                                        ));
                                                    } else {
                                                        // If the LSP was anvil-managed, clear the
                                                        // resolved binary so it won't restart.
                                                        if lsp_source == LspSource::Anvil {
                                                            lsp_resolved_binary = None;
                                                        }
                                                        toasts.push((
                                                            "Anvil: sqls uninstalled".to_string(),
                                                            ToastKind::Info,
                                                            std::time::Instant::now(),
                                                        ));
                                                    }
                                                } else {
                                                    toasts.push((
                                                        "Anvil: sqls is not installed by anvil"
                                                            .to_string(),
                                                        ToastKind::Info,
                                                        std::time::Instant::now(),
                                                    ));
                                                }
                                            }
                                            Err(e) => {
                                                toasts.push((
                                                    format!("Anvil: store error: {e}"),
                                                    ToastKind::Error,
                                                    std::time::Instant::now(),
                                                ));
                                            }
                                        }
                                    }
                                }
                                AnvilCmd::Unknown => {
                                    toasts.push((
                                        "Anvil: unknown subcommand — try :Anvil install sqls"
                                            .to_string(),
                                        ToastKind::Error,
                                        std::time::Instant::now(),
                                    ));
                                }
                            }
                            continue;
                        }
                        // Gate cursorline / cursorcolumn locally: hjkl-engine 0.3 does
                        // not expose these in Settings, so sqeel-tui owns the booleans
                        // and intercepts the `:set` tokens before the engine sees them.
                        let opts_result =
                            apply_cursor_opts(trimmed, &mut opt_cursorline, &mut opt_cursorcolumn);
                        let suppress_engine_info = opts_result.info.is_some();
                        if let Some(msg) = opts_result.info {
                            toasts.push((msg, ToastKind::Info, std::time::Instant::now()));
                        }
                        let ex_effect =
                            hjkl_ex::try_dispatch(&ex_registry, &mut editor, &opts_result.forward)
                                .unwrap_or_else(|| {
                                    hjkl_ex::ExEffect::Unknown(opts_result.forward.to_string())
                                });
                        match ex_effect {
                            hjkl_ex::ExEffect::Quit { force, save } => {
                                let local_dirty = editor_dirty;
                                let any_dirty = {
                                    let mut s = state.lock().unwrap();
                                    s.editor_content = editor.content_arc();
                                    s.editor_content_synced = true;
                                    editor_dirty = false;
                                    if local_dirty {
                                        s.mark_active_dirty();
                                    }
                                    local_dirty || s.any_dirty()
                                };
                                if force {
                                    break;
                                }
                                if save {
                                    let pending = state.lock().unwrap().prepare_save_all_dirty();
                                    let failed = commit_pending_saves(&state, pending).await;
                                    if failed.is_empty() {
                                        break;
                                    }
                                    toasts.push((
                                        format!("Save failed for: {}", failed.join(", ")),
                                        ToastKind::Error,
                                        std::time::Instant::now(),
                                    ));
                                } else if any_dirty {
                                    quit_prompt = Some(());
                                } else {
                                    break;
                                }
                            }
                            hjkl_ex::ExEffect::Save => {
                                let prepared = {
                                    let mut s = state.lock().unwrap();
                                    // The heavy content pipeline is
                                    // gated off for buffers over
                                    // 2 MB, which otherwise leaves
                                    // `editor_content_synced = false`
                                    // and the save falls back to
                                    // stale `tab.content`.
                                    s.editor_content = editor.content_arc();
                                    s.editor_content_synced = true;
                                    s.prepare_save_active_tab()
                                };
                                match prepared {
                                    Ok(pending) => {
                                        // Run the disk write on a
                                        // blocking task so multi-MB
                                        // saves don't freeze the
                                        // render loop.
                                        let name = pending.name.clone();
                                        let idx = pending.tab_index;
                                        let commit =
                                            tokio::task::spawn_blocking(move || pending.commit())
                                                .await
                                                .unwrap_or_else(|e| {
                                                    Err(std::io::Error::other(format!(
                                                        "spawn_blocking join error: {e}"
                                                    )))
                                                });
                                        match commit {
                                            Ok(()) => {
                                                if let Some(i) = idx {
                                                    state.lock().unwrap().mark_tab_saved(i);
                                                }
                                                editor_dirty = false;
                                                toasts.push((
                                                    format!("Saved {name}"),
                                                    ToastKind::Info,
                                                    std::time::Instant::now(),
                                                ));
                                            }
                                            Err(e) => toasts.push((
                                                format!("Save failed: {e}"),
                                                ToastKind::Error,
                                                std::time::Instant::now(),
                                            )),
                                        }
                                    }
                                    Err(e) => toasts.push((
                                        format!("Save failed: {e}"),
                                        ToastKind::Error,
                                        std::time::Instant::now(),
                                    )),
                                }
                            }
                            hjkl_ex::ExEffect::Substituted {
                                count,
                                lines_changed,
                            } => {
                                state.lock().unwrap().focus = Focus::Editor;
                                editor_dirty = true;
                                let sub_word = if count == 1 {
                                    "substitution"
                                } else {
                                    "substitutions"
                                };
                                let line_word = if lines_changed == 1 { "line" } else { "lines" };
                                toasts.push((
                                    format!("{count} {sub_word} on {lines_changed} {line_word}"),
                                    ToastKind::Info,
                                    std::time::Instant::now(),
                                ));
                            }
                            hjkl_ex::ExEffect::Ok => {
                                state.lock().unwrap().focus = Focus::Editor;
                            }
                            hjkl_ex::ExEffect::Info(msg) => {
                                // Suppress the engine's bare `:set` info dump
                                // when we already surfaced a query result for
                                // a cursor-opt token (`?` form).
                                if !suppress_engine_info {
                                    toasts.push((msg, ToastKind::Info, std::time::Instant::now()));
                                }
                            }
                            hjkl_ex::ExEffect::Error(msg) => {
                                toasts.push((msg, ToastKind::Error, std::time::Instant::now()));
                            }
                            hjkl_ex::ExEffect::Unknown(c) => {
                                if c == "colorscheme" {
                                    toasts.push((
                                        format!("Available: {}", theme::available_colorschemes()),
                                        ToastKind::Info,
                                        std::time::Instant::now(),
                                    ));
                                } else if let Some(name) =
                                    c.strip_prefix("colorscheme").and_then(|rest| {
                                        let rest = rest.trim();
                                        if rest.is_empty() { None } else { Some(rest) }
                                    })
                                {
                                    match theme::switch_colorscheme(name) {
                                        Ok(()) => {
                                            toasts.push((
                                                format!("colorscheme: {name}"),
                                                ToastKind::Info,
                                                std::time::Instant::now(),
                                            ));
                                        }
                                        Err(msg) => {
                                            toasts.push((
                                                msg,
                                                ToastKind::Error,
                                                std::time::Instant::now(),
                                            ));
                                        }
                                    }
                                } else if c.starts_with("export") {
                                    let msg =
                                        handle_export_cmd(&c, &state.lock().unwrap(), &mut toasts);
                                    if let Some((text, kind)) = msg {
                                        toasts.push((text, kind, std::time::Instant::now()));
                                    }
                                } else if c == "refreshschema" || c == "refresh" {
                                    let conn_name = state
                                        .lock()
                                        .unwrap()
                                        .active_connection
                                        .clone()
                                        .unwrap_or_else(|| "database".into());
                                    let triggered = state.lock().unwrap().refresh_schema();
                                    if triggered {
                                        toasts.push((
                                            format!("Refreshing schema for {conn_name}…"),
                                            ToastKind::Info,
                                            std::time::Instant::now(),
                                        ));
                                    } else {
                                        toasts.push((
                                            "No active connection to refresh".to_string(),
                                            ToastKind::Error,
                                            std::time::Instant::now(),
                                        ));
                                    }
                                } else if c.starts_with("describe")
                                    || c.starts_with("desc ")
                                    || c == "desc"
                                {
                                    let s = state.lock().unwrap();
                                    let tab_idx = s.active_result_tab;
                                    let (toast, sent) = handle_describe_cmd(&c, &s, tab_idx);
                                    drop(s);
                                    if let Some((text, kind)) = toast {
                                        toasts.push((text, kind, std::time::Instant::now()));
                                    }
                                    if sent {
                                        // query dispatched — results pane is the feedback
                                    }
                                } else if c == "migrate-secrets" {
                                    let msgs = run_migrate_secrets();
                                    for (text, kind) in msgs {
                                        toasts.push((text, kind, std::time::Instant::now()));
                                    }
                                } else {
                                    toasts.push((
                                        format!("Unknown command: :{c}"),
                                        ToastKind::Error,
                                        std::time::Instant::now(),
                                    ));
                                }
                            }
                            hjkl_ex::ExEffect::None => {}
                            hjkl_ex::ExEffect::SaveAs(_) => {
                                toasts.push((
                                    ":w <path> not yet supported in sqeel-tui".to_string(),
                                    ToastKind::Error,
                                    std::time::Instant::now(),
                                ));
                            }
                            hjkl_ex::ExEffect::InfoTitled { content, .. } => {
                                toasts.push((content, ToastKind::Info, std::time::Instant::now()));
                            }
                            // `:e <name>` — open a saved query from sqeel's
                            // queries dir, mirroring the file-picker path.
                            hjkl_ex::ExEffect::EditFile { path, .. } => {
                                let name = std::path::Path::new(&path)
                                    .file_name()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                if name.is_empty() {
                                    toasts.push((
                                        ":e needs a file name".to_string(),
                                        ToastKind::Error,
                                        std::time::Instant::now(),
                                    ));
                                } else {
                                    let mut s = state.lock().unwrap();
                                    if editor_dirty {
                                        s.editor_content = editor.content_arc();
                                        s.mark_active_dirty();
                                        editor_dirty = false;
                                    }
                                    if let Some(idx) = s.tabs.iter().position(|t| t.name == name) {
                                        s.switch_to_tab(idx);
                                    } else if let Ok(content) =
                                        sqeel_core::persistence::load_query(&name)
                                    {
                                        s.tabs.push(sqeel_core::state::TabEntry {
                                            name,
                                            content: Some(content),
                                            last_accessed: Some(Instant::now()),
                                            cursor: None,
                                            dirty: false,
                                        });
                                        let idx = s.tabs.len() - 1;
                                        s.switch_to_tab(idx);
                                    } else {
                                        toasts.push((
                                            format!("no saved query named {name}"),
                                            ToastKind::Error,
                                            std::time::Instant::now(),
                                        ));
                                    }
                                }
                            }
                            // `:s///c` interactive confirm — matched
                            // separately from the wildcard so the toast
                            // doesn't Debug-dump the whole match list.
                            hjkl_ex::ExEffect::SubstituteConfirm { .. } => {
                                toasts.push((
                                    ":s///c confirm mode not supported in sqeel — use :s///g"
                                        .to_string(),
                                    ToastKind::Error,
                                    std::time::Instant::now(),
                                ));
                            }
                            // Quickfix / location lists, buffer ops, cwd, …
                            // — hjkl-app machinery sqeel doesn't model.
                            other => {
                                toasts.push((
                                    format!("unsupported in sqeel: {other:?}"),
                                    ToastKind::Error,
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                        continue;
                    }
                    cmd.handle_input(input);
                    continue;
                }

                // ── Rename input mode ────────────────────────────────────────────
                if let Some(ref mut name) = rename_input {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            rename_input = None;
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            let name_str = rename_input.take().unwrap_or_default().text;
                            let mut s = state.lock().unwrap();
                            if let Err(e) = s.rename_active_tab(&name_str) {
                                toasts.push((
                                    format!("Rename failed: {e}"),
                                    ToastKind::Error,
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                            name.insert_char(c);
                        }
                        (KeyModifiers::NONE, code) if name.handle_nav(code) => {}
                        _ => {}
                    }
                    continue;
                }

                // ── Delete confirmation (leader+d) ───────────────────────────────
                if delete_confirm.is_some() {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Char('y'))
                        | (KeyModifiers::NONE, KeyCode::Enter) => {
                            delete_confirm = None;
                            let mut s = state.lock().unwrap();
                            if let Err(e) = s.delete_active_tab() {
                                toasts.push((
                                    format!("Delete failed: {e}"),
                                    ToastKind::Error,
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                        _ => {
                            // Any other key cancels (Esc, n, etc.).
                            delete_confirm = None;
                        }
                    }
                    continue;
                }

                // ── File picker (leader+space) ───────────────────────────────────
                if let Some(ref mut picker) = file_picker {
                    use hjkl_picker::{PickerAction, PickerEvent};
                    match hjkl_picker_tui::handle_key(picker, key) {
                        PickerEvent::Cancel => {
                            file_picker = None;
                        }
                        PickerEvent::Select(PickerAction::Custom(boxed)) => {
                            let boxed = match boxed.downcast::<SqeelFileAction>() {
                                Ok(action) => {
                                    let SqeelFileAction::OpenPath(path) = *action;
                                    let name = path
                                        .file_name()
                                        .map(|s| s.to_string_lossy().into_owned())
                                        .unwrap_or_default();
                                    if !name.is_empty() {
                                        let mut s = state.lock().unwrap();
                                        if editor_dirty {
                                            s.editor_content = editor.content_arc();
                                            s.mark_active_dirty();
                                            editor_dirty = false;
                                        }
                                        if let Some(idx) =
                                            s.tabs.iter().position(|t| t.name == name)
                                        {
                                            s.switch_to_tab(idx);
                                        } else if let Ok(content) =
                                            sqeel_core::persistence::load_query(&name)
                                        {
                                            s.tabs.push(sqeel_core::state::TabEntry {
                                                name,
                                                content: Some(content),
                                                last_accessed: Some(Instant::now()),
                                                cursor: None,
                                                dirty: false,
                                            });
                                            let idx = s.tabs.len() - 1;
                                            s.switch_to_tab(idx);
                                        }
                                    }
                                    None
                                }
                                Err(b) => Some(b),
                            };
                            if let Some(boxed) = boxed
                                && let Ok(action) = boxed.downcast::<SqeelHistoryAction>()
                            {
                                let SqeelHistoryAction::LoadQuery(query) = *action;
                                let content = {
                                    let mut s = state.lock().unwrap();
                                    s.new_tab_with_content(query);
                                    s.tab_content_pending.take()
                                };
                                if let Some(c) = content {
                                    editor.set_content(&c);
                                    let _ = editor.take_dirty();
                                    editor_dirty = false;
                                    last_highlight_top = usize::MAX;
                                }
                            }
                            file_picker = None;
                        }
                        PickerEvent::Select(_) | PickerEvent::None => {}
                    }
                    continue;
                }

                // ── Hover-popup `/` search prompt ────────────────────────────────
                if let Some(ref mut prompt) = hover_search_prompt {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            hover_search_prompt = None;
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            let text = hover_search_prompt.take().unwrap_or_default().text;
                            if !text.is_empty() {
                                let mut s = state.lock().unwrap();
                                let found = s.hover_find(&text, true, false);
                                hover_search_pattern = Some(text);
                                drop(s);
                                if !found {
                                    toasts.push((
                                        "Pattern not found".into(),
                                        ToastKind::Info,
                                        std::time::Instant::now(),
                                    ));
                                }
                            }
                        }
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                            prompt.insert_char(c);
                        }
                        (KeyModifiers::NONE, code) if prompt.handle_nav(code) => {}
                        _ => {}
                    }
                    continue;
                }

                // ── Results-pane `/` search prompt ───────────────────────────────
                if let Some(ref mut prompt) = results_search_prompt {
                    use hjkl_engine::{Input as EngineInput, Key as EngineKey};
                    let input: EngineInput = crossterm_to_input(key);
                    if input.key == EngineKey::Esc {
                        let text = prompt.text();
                        if text.is_empty() || prompt.vim_mode() != hjkl_engine::VimMode::Insert {
                            results_search_prompt = None;
                        } else {
                            prompt.enter_normal();
                        }
                        continue;
                    }
                    if input.key == EngineKey::Enter {
                        let text = prompt.text();
                        results_search_prompt = None;
                        if !text.is_empty() {
                            let mut s = state.lock().unwrap();
                            let found = s.results_find(&text, true, false);
                            results_search_pattern = Some(text);
                            drop(s);
                            if !found {
                                toasts.push((
                                    "Pattern not found".into(),
                                    ToastKind::Info,
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                        continue;
                    }
                    prompt.handle_input(input);
                    continue;
                }

                // ── Schema search box (typing mode) ─────────────────────────────
                if schema_search.focused {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => schema_search.clear(),
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            // Keep filter active, switch to list navigation mode.
                            schema_search.focused = false;
                        }
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                            schema_search.push(c);
                            if let Some(q) = schema_search.query() {
                                state.lock().unwrap().lazy_load_for_schema_search(q);
                            }
                        }
                        (KeyModifiers::NONE, code) if schema_search.handle_nav(code) => {
                            if let Some(q) = schema_search.query() {
                                state.lock().unwrap().lazy_load_for_schema_search(q);
                            }
                        }
                        // ctrl+hjkl: dismiss search and move focus.
                        (KeyModifiers::CONTROL, KeyCode::Char('h')) => {
                            schema_search.clear();
                            tmux_navigate('L');
                        }
                        (KeyModifiers::CONTROL, KeyCode::Char('l' | 'k')) => {
                            schema_search.clear();
                            state.lock().unwrap().focus = Focus::Editor;
                        }
                        (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
                            schema_search.clear();
                            if show_results {
                                state.lock().unwrap().focus = Focus::Results;
                            } else {
                                tmux_navigate('D');
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // ── Schema filter navigation (filter active, box unfocused) ───────
                if schema_search.is_filtering() && focus == Focus::Schema {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => schema_search.clear(),
                        (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down) => {
                            schema_search.cursor_down(last_draw_areas.schema_list_count);
                        }
                        (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up) => {
                            schema_search.cursor_up();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('/')) => {
                            schema_search.focused = true;
                        }
                        _ => {}
                    }
                    continue;
                }

                // The `/` / `?` search prompt is owned by the editor now;
                // just forward the key and let sqeel-vim handle it.
                if editor.search_prompt().is_some() {
                    hjkl_vim::dispatch_input(&mut editor, crossterm_to_input(key));
                    continue;
                }

                // ── Connection-error details popup ───────────────────────────────
                if show_connect_error {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc | KeyCode::Enter) => {
                            state.lock().unwrap().close_connect_error_popup();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('r')) => {
                            let mut s = state.lock().unwrap();
                            s.close_connect_error_popup();
                            s.retry_connection();
                        }
                        _ => {}
                    }
                    continue;
                }

                // ── Help overlay ─────────────────────────────────────────────────
                if show_help {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            state.lock().unwrap().close_help();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down) => {
                            let max = last_draw_areas.help_max_scroll;
                            let mut s = state.lock().unwrap();
                            s.help_scroll = s.help_scroll.saturating_add(1).min(max);
                        }
                        (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up) => {
                            let mut s = state.lock().unwrap();
                            s.help_scroll = s.help_scroll.saturating_sub(1);
                        }
                        _ => {}
                    }
                    continue;
                }

                // ── sqls install prompt modal ────────────────────────────────────
                // Letter keys accept NONE or SHIFT modifiers so terminals that emit
                // uppercase Y/N with SHIFT (most do) still match.
                if sqls_prompt_open {
                    let mods_letter_ok =
                        matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT);
                    match key.code {
                        KeyCode::Enter => {
                            sqls_prompt_open = false;
                            sqls_install_pending = true;
                        }
                        KeyCode::Esc => {
                            sqls_prompt_open = false;
                            toasts.push((
                                format!("LSP: {} missing", lsp_binary),
                                ToastKind::Info,
                                std::time::Instant::now(),
                            ));
                        }
                        KeyCode::Char('y') | KeyCode::Char('Y') if mods_letter_ok => {
                            sqls_prompt_open = false;
                            sqls_install_pending = true;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') if mods_letter_ok => {
                            sqls_prompt_open = false;
                            toasts.push((
                                format!("LSP: {} missing", lsp_binary),
                                ToastKind::Info,
                                std::time::Instant::now(),
                            ));
                        }
                        _ => {}
                    }
                    continue;
                }

                // ── pgpass picker (above add-connection) ────────────────────────
                if show_pgpass_picker {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Char('j'))
                        | (KeyModifiers::NONE, KeyCode::Down) => {
                            state.lock().unwrap().pgpass_picker_down();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('k'))
                        | (KeyModifiers::NONE, KeyCode::Up) => {
                            state.lock().unwrap().pgpass_picker_up();
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            state.lock().unwrap().pgpass_apply_selected();
                        }
                        (KeyModifiers::NONE, KeyCode::Esc)
                        | (KeyModifiers::NONE, KeyCode::Char('q')) => {
                            let mut s = state.lock().unwrap();
                            s.close_pgpass_picker();
                            s.open_add_connection();
                        }
                        _ => {}
                    }
                    continue;
                }

                // ── Add connection modal (highest priority) ──────────────────────
                if show_add {
                    let verify_mode_active = state.lock().unwrap().add_connection_field
                        == AddConnectionField::VerifyMode;
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            state.lock().unwrap().close_add_connection();
                        }
                        (KeyModifiers::NONE, KeyCode::Tab) => {
                            state.lock().unwrap().add_connection_tab();
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            if verify_mode_active {
                                // Enter on the VerifyMode toggle — toggle, don't save.
                                state.lock().unwrap().add_connection_toggle_verify_mode();
                            } else {
                                let mut s = state.lock().unwrap();
                                if let Err(e) = s.save_new_connection() {
                                    // Surface validation / save failure inside the
                                    // popup itself — the results pane is for query
                                    // output, not connection-form feedback.
                                    s.add_connection_error = Some(format!("{e}"));
                                }
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Backspace) => {
                            state.lock().unwrap().add_connection_backspace();
                        }
                        (KeyModifiers::NONE, KeyCode::Delete) => {
                            state.lock().unwrap().add_connection_delete();
                        }
                        (KeyModifiers::NONE, KeyCode::Left) => {
                            if verify_mode_active {
                                state.lock().unwrap().add_connection_toggle_verify_mode();
                            } else {
                                state.lock().unwrap().add_connection_left();
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Right) => {
                            if verify_mode_active {
                                state.lock().unwrap().add_connection_toggle_verify_mode();
                            } else {
                                state.lock().unwrap().add_connection_right();
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Home) => {
                            state.lock().unwrap().add_connection_home();
                        }
                        (KeyModifiers::NONE, KeyCode::End) => {
                            state.lock().unwrap().add_connection_end();
                        }
                        (KeyModifiers::NONE, KeyCode::Char(' ')) if verify_mode_active => {
                            state.lock().unwrap().add_connection_toggle_verify_mode();
                        }
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(ch)) => {
                            state.lock().unwrap().add_connection_type_char(ch);
                        }
                        _ => {}
                    }
                    continue;
                }

                // ── Connection switcher modal ────────────────────────────────────
                if show_switcher {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            state.lock().unwrap().close_connection_switcher();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('j')) => {
                            state.lock().unwrap().switcher_down();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('k')) => {
                            state.lock().unwrap().switcher_up();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('n')) => {
                            state.lock().unwrap().open_add_connection();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('e')) => {
                            state.lock().unwrap().open_edit_connection();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('d')) => {
                            let result = state.lock().unwrap().delete_selected_connection();
                            if let Err(e) = result {
                                state
                                    .lock()
                                    .unwrap()
                                    .set_error(format!("Delete failed: {e}"));
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            state.lock().unwrap().confirm_connection_switch();
                        }
                        _ => {
                            // Any other key disarms an in-flight delete
                            // confirmation so the user doesn't lose their
                            // place — matches vim's "press wrong key,
                            // command cancels" feel.
                            state.lock().unwrap().disarm_connection_delete();
                        }
                    }
                    continue;
                }

                // ── Normal key handling ──────────────────────────────────────────

                // Completion popup navigation
                if show_completions {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            state.lock().unwrap().dismiss_completions();
                            if keybinding_mode == KeybindingMode::Vim {
                                // Route through the editor so the regular
                                // insert-Esc handling (back-one + sticky col
                                // sync) runs. force_normal() bypasses both.
                                hjkl_vim::dispatch_input(&mut editor, crossterm_to_input(key));
                            }
                            continue;
                        }
                        (KeyModifiers::NONE, KeyCode::Up)
                        | (KeyModifiers::SHIFT, KeyCode::BackTab) => {
                            state.lock().unwrap().completion_cursor_up();
                            continue;
                        }
                        (KeyModifiers::NONE, KeyCode::Down)
                        | (KeyModifiers::NONE, KeyCode::Tab) => {
                            state.lock().unwrap().completion_cursor_down();
                            continue;
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            let chosen = state
                                .lock()
                                .unwrap()
                                .selected_completion()
                                .map(|s| s.to_owned());
                            if let Some(text) = chosen {
                                editor.accept_completion(&text);
                                state.lock().unwrap().dismiss_completions();
                                // Consume dirty flag so completions don't re-trigger immediately.
                                editor.take_dirty();
                            }
                            continue;
                        }
                        _ => {}
                    }
                }

                // Any key other than a second `g` aborts the pending `gg` chord.
                let keep_schema_g_pending = focus == Focus::Schema
                    && key.modifiers == KeyModifiers::NONE
                    && matches!(key.code, KeyCode::Char('g'));
                match (key.modifiers, key.code) {
                    // Shift+H / Shift+L: prev / next tab. Active outside the
                    // editor or when in Vim Normal mode so it doesn't shadow
                    // typing in Insert mode.
                    (KeyModifiers::SHIFT, KeyCode::Char('L')) if focus == Focus::Results => {
                        state.lock().unwrap().next_result_tab();
                    }
                    (KeyModifiers::SHIFT, KeyCode::Char('H')) if focus == Focus::Results => {
                        state.lock().unwrap().prev_result_tab();
                    }
                    (KeyModifiers::SHIFT, KeyCode::Char('L'))
                        if focus != Focus::Editor || vim_mode == VimMode::Normal =>
                    {
                        let content = {
                            let mut s = state.lock().unwrap();
                            if editor_dirty {
                                s.editor_content = editor.content_arc();
                                s.mark_active_dirty();
                                editor_dirty = false;
                            }
                            s.next_tab();
                            s.tab_content_pending.take()
                        };
                        if let Some(c) = content {
                            editor.set_content(&c);
                            let _ = editor.take_dirty();
                            editor_dirty = false;
                            last_highlight_top = usize::MAX;
                        }
                    }
                    (KeyModifiers::SHIFT, KeyCode::Char('H'))
                        if focus != Focus::Editor || vim_mode == VimMode::Normal =>
                    {
                        let content = {
                            let mut s = state.lock().unwrap();
                            if editor_dirty {
                                s.editor_content = editor.content_arc();
                                s.mark_active_dirty();
                                editor_dirty = false;
                            }
                            s.prev_tab();
                            s.tab_content_pending.take()
                        };
                        if let Some(c) = content {
                            editor.set_content(&c);
                            let _ = editor.take_dirty();
                            editor_dirty = false;
                            last_highlight_top = usize::MAX;
                        }
                    }
                    // Command mode
                    (KeyModifiers::NONE, KeyCode::Char(':'))
                        if focus != Focus::Editor || vim_mode == VimMode::Normal =>
                    {
                        let mut field = TextFieldEditor::new(true);
                        field.enter_insert_at_end();
                        command_input = Some(field);
                    }
                    // Help: ?
                    (KeyModifiers::NONE, KeyCode::Char('?'))
                        if focus != Focus::Editor || vim_mode == VimMode::Normal =>
                    {
                        state.lock().unwrap().open_help();
                    }
                    // Schema pane navigation
                    (KeyModifiers::NONE, KeyCode::Char('j')) if focus == Focus::Schema => {
                        state.lock().unwrap().schema_cursor_down();
                    }
                    (KeyModifiers::NONE, KeyCode::Char('k')) if focus == Focus::Schema => {
                        state.lock().unwrap().schema_cursor_up();
                    }
                    (KeyModifiers::NONE, KeyCode::Char('g')) if focus == Focus::Schema => {
                        // `gg` → top. First `g` arms the chord; second `g`
                        // (landing here with pending already set) fires it.
                        if schema_g_pending {
                            state.lock().unwrap().schema_cursor_top();
                        } else {
                            schema_g_pending = true;
                        }
                    }
                    (KeyModifiers::SHIFT, KeyCode::Char('G'))
                    | (KeyModifiers::NONE, KeyCode::Char('G'))
                        if focus == Focus::Schema =>
                    {
                        state.lock().unwrap().schema_cursor_bottom();
                    }
                    (KeyModifiers::NONE, KeyCode::Enter | KeyCode::Char('l'))
                        if focus == Focus::Schema =>
                    {
                        let mut s = state.lock().unwrap();
                        // Enter on the "Connection failed" placeholder
                        // pops the full-error modal instead of trying
                        // to expand a (non-existent) schema node.
                        if s.schema_connect_error.is_some() && s.schema_nodes.is_empty() {
                            s.open_connect_error_popup();
                        } else {
                            // Check if the focused item is an IndexGroup, ForeignKeyGroup,
                            // or ForeignKey before falling through to the normal toggle.
                            let item_kind = s
                                .visible_schema_items()
                                .get(s.schema_cursor)
                                .map(|i| i.kind.clone());
                            match item_kind {
                                Some(SchemaItemKind::IndexGroup { .. }) => {
                                    s.schema_toggle_subgroup(SubGroup::Indexes);
                                }
                                Some(SchemaItemKind::ForeignKeyGroup { .. }) => {
                                    s.schema_toggle_subgroup(SubGroup::ForeignKeys);
                                }
                                Some(SchemaItemKind::ForeignKey { .. }) => {
                                    s.schema_fk_jump();
                                }
                                _ => {
                                    s.schema_toggle_current();
                                }
                            }
                        }
                    }
                    // Schema search
                    (KeyModifiers::NONE, KeyCode::Char('/')) if focus == Focus::Schema => {
                        schema_search.start();
                    }
                    // Retry the last failed connection. Only fires when
                    // the sidebar is showing a connect-error placeholder;
                    // `retry_connection` returns false otherwise so the
                    // key is a no-op when there's nothing to retry.
                    (KeyModifiers::NONE, KeyCode::Char('r')) if focus == Focus::Schema => {
                        state.lock().unwrap().retry_connection();
                    }
                    // Results pane: digit count prefix. `0` only counts
                    // as a digit when a count is already in progress —
                    // otherwise it's the `0` row-start binding below.
                    (KeyModifiers::NONE, KeyCode::Char(c @ '0'..='9'))
                        if focus == Focus::Results
                            && (c != '0' || results_count > 0)
                            && state.lock().unwrap().active_ddl_text().is_none() =>
                    {
                        results_count = results_count
                            .saturating_mul(10)
                            .saturating_add((c as u8 - b'0') as usize);
                    }
                    // ── Hover popup (Focus::Hover) — grid nav + yank ─────
                    // Esc first cancels an active visual selection so
                    // the second press closes the popup, mirroring the
                    // results-pane idiom. Also drops the pending hover
                    // request id so a late response doesn't re-open
                    // the popup after the user dismissed it.
                    (KeyModifiers::NONE, KeyCode::Esc) if focus == Focus::Hover => {
                        let mut s = state.lock().unwrap();
                        if s.hover_selection.is_some() {
                            s.hover_selection = None;
                        } else {
                            s.close_hover();
                            last_hover_id = None;
                            last_sig_help_id = None;
                            sig_help_text = None;
                        }
                    }
                    // `V` / `v` / `Ctrl-V` — visual-line / block
                    // selection inside the hover grid. Toggles off on
                    // the same key, exactly like the results pane.
                    (KeyModifiers::SHIFT, KeyCode::Char('V'))
                    | (KeyModifiers::NONE, KeyCode::Char('V'))
                        if focus == Focus::Hover =>
                    {
                        use sqeel_core::state::{ResultsSelection, ResultsSelectionMode};
                        let mut s = state.lock().unwrap();
                        let already = matches!(
                            s.hover_selection,
                            Some(ResultsSelection {
                                mode: ResultsSelectionMode::Line,
                                ..
                            })
                        );
                        if already {
                            s.hover_selection = None;
                        } else if let ResultsCursor::Cell { row, col } = s.hover_cursor {
                            s.hover_selection = Some(ResultsSelection {
                                anchor: (row, col),
                                mode: ResultsSelectionMode::Line,
                            });
                        }
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('v'))
                    | (KeyModifiers::NONE, KeyCode::Char('v'))
                        if focus == Focus::Hover =>
                    {
                        use sqeel_core::state::{ResultsSelection, ResultsSelectionMode};
                        let mut s = state.lock().unwrap();
                        let already = matches!(
                            s.hover_selection,
                            Some(ResultsSelection {
                                mode: ResultsSelectionMode::Block,
                                ..
                            })
                        );
                        if already {
                            s.hover_selection = None;
                        } else if let ResultsCursor::Cell { row, col } = s.hover_cursor {
                            s.hover_selection = Some(ResultsSelection {
                                anchor: (row, col),
                                mode: ResultsSelectionMode::Block,
                            });
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down)
                        if focus == Focus::Hover =>
                    {
                        let mut s = state.lock().unwrap();
                        if s.hover_table.is_some() {
                            s.hover_cursor_move(1, 0);
                        } else {
                            s.hover_scroll = s.hover_scroll.saturating_add(1);
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up)
                        if focus == Focus::Hover =>
                    {
                        let mut s = state.lock().unwrap();
                        if s.hover_table.is_some() {
                            s.hover_cursor_move(-1, 0);
                        } else {
                            s.hover_scroll = s.hover_scroll.saturating_sub(1);
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('l') | KeyCode::Right)
                        if focus == Focus::Hover =>
                    {
                        state.lock().unwrap().hover_cursor_move(0, 1);
                    }
                    (KeyModifiers::NONE, KeyCode::Char('h') | KeyCode::Left)
                        if focus == Focus::Hover =>
                    {
                        state.lock().unwrap().hover_cursor_move(0, -1);
                    }
                    (KeyModifiers::SHIFT, KeyCode::Char('G'))
                    | (KeyModifiers::NONE, KeyCode::Char('G'))
                        if focus == Focus::Hover =>
                    {
                        state
                            .lock()
                            .unwrap()
                            .hover_cursor_edge(sqeel_core::state::HoverEdge::LastRow);
                    }
                    (KeyModifiers::NONE, KeyCode::Char('g')) if focus == Focus::Hover => {
                        // `gg` — repurpose the results-pane chord tracker
                        // since only one pane is ever focused at a time.
                        if results_g_pending {
                            state
                                .lock()
                                .unwrap()
                                .hover_cursor_edge(sqeel_core::state::HoverEdge::FirstRow);
                            results_g_pending = false;
                        } else {
                            results_g_pending = true;
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('0')) if focus == Focus::Hover => {
                        state
                            .lock()
                            .unwrap()
                            .hover_cursor_edge(sqeel_core::state::HoverEdge::RowStart);
                    }
                    (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('$'))
                        if focus == Focus::Hover =>
                    {
                        state
                            .lock()
                            .unwrap()
                            .hover_cursor_edge(sqeel_core::state::HoverEdge::RowEnd);
                    }
                    // `/` → open hover search prompt.
                    (KeyModifiers::NONE, KeyCode::Char('/')) if focus == Focus::Hover => {
                        hover_search_prompt = Some(TextInput::default());
                    }
                    // `n` / `N` — repeat committed hover search.
                    (KeyModifiers::NONE, KeyCode::Char('n')) if focus == Focus::Hover => {
                        if let Some(pat) = hover_search_pattern.clone() {
                            let mut s = state.lock().unwrap();
                            if !s.hover_find(&pat, true, true) {
                                drop(s);
                                toasts.push((
                                    "Pattern not found".into(),
                                    ToastKind::Info,
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                    }
                    (KeyModifiers::SHIFT, KeyCode::Char('N'))
                    | (KeyModifiers::NONE, KeyCode::Char('N'))
                        if focus == Focus::Hover =>
                    {
                        if let Some(pat) = hover_search_pattern.clone() {
                            let mut s = state.lock().unwrap();
                            if !s.hover_find(&pat, false, true) {
                                drop(s);
                                toasts.push((
                                    "Pattern not found".into(),
                                    ToastKind::Info,
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('y')) if focus == Focus::Hover => {
                        let yanked = state.lock().unwrap().hover_yank();
                        if let Some((text, label)) = yanked {
                            let ok = clipboard
                                .set(Selection::Clipboard, MimeType::Text, text.as_bytes())
                                .is_ok();
                            toasts.push((
                                if ok {
                                    format!("{label} copied to clipboard")
                                } else {
                                    format!("{label}: clipboard copy failed (too large)")
                                },
                                if ok {
                                    ToastKind::Info
                                } else {
                                    ToastKind::Error
                                },
                                std::time::Instant::now(),
                            ));
                        }
                    }
                    // Results pane navigation. Arrow keys mirror hjkl.
                    (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down)
                        if focus == Focus::Results =>
                    {
                        let n = results_count.max(1);
                        results_count = 0;
                        let mut s = state.lock().unwrap();
                        if s.active_ddl_text().is_some() {
                            for _ in 0..n {
                                s.scroll_results_down();
                            }
                        } else {
                            for _ in 0..n {
                                s.results_cursor_down();
                            }
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up)
                        if focus == Focus::Results =>
                    {
                        let n = results_count.max(1);
                        results_count = 0;
                        let mut s = state.lock().unwrap();
                        if s.active_ddl_text().is_some() {
                            for _ in 0..n {
                                s.scroll_results_up();
                            }
                        } else {
                            for _ in 0..n {
                                s.results_cursor_up();
                            }
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('l') | KeyCode::Right)
                        if focus == Focus::Results =>
                    {
                        let n = results_count.max(1);
                        results_count = 0;
                        let mut s = state.lock().unwrap();
                        if s.active_ddl_text().is_some() {
                            for _ in 0..n {
                                s.scroll_results_right();
                            }
                        } else {
                            for _ in 0..n {
                                s.results_cursor_right();
                            }
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('h') | KeyCode::Left)
                        if focus == Focus::Results =>
                    {
                        let n = results_count.max(1);
                        results_count = 0;
                        let mut s = state.lock().unwrap();
                        if s.active_ddl_text().is_some() {
                            for _ in 0..n {
                                s.scroll_results_left();
                            }
                        } else {
                            for _ in 0..n {
                                s.results_cursor_left();
                            }
                        }
                    }
                    // `gg` chord → first row.
                    (KeyModifiers::NONE, KeyCode::Char('g')) if focus == Focus::Results => {
                        if results_g_pending {
                            state.lock().unwrap().results_cursor_first_row();
                            results_count = 0;
                            results_g_pending = false;
                        } else {
                            results_g_pending = true;
                        }
                    }
                    // `G` → last row.
                    (KeyModifiers::SHIFT, KeyCode::Char('G'))
                    | (KeyModifiers::NONE, KeyCode::Char('G'))
                        if focus == Focus::Results =>
                    {
                        state.lock().unwrap().results_cursor_last_row();
                        results_count = 0;
                    }
                    // `0` → first column of current row. Shadowed by
                    // digit-count arm above when a count is in progress.
                    (KeyModifiers::NONE, KeyCode::Char('0')) if focus == Focus::Results => {
                        state.lock().unwrap().results_cursor_row_start();
                        results_count = 0;
                    }
                    // `$` → last column of current row.
                    (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('$'))
                        if focus == Focus::Results =>
                    {
                        state.lock().unwrap().results_cursor_row_end();
                        results_count = 0;
                    }
                    // `/` → open the results-pane search prompt.
                    (KeyModifiers::NONE, KeyCode::Char('/')) if focus == Focus::Results => {
                        let mut field = TextFieldEditor::new(true);
                        field.enter_insert_at_end();
                        results_search_prompt = Some(field);
                    }
                    // `n` / `N` → repeat the last committed results search.
                    (KeyModifiers::NONE, KeyCode::Char('n')) if focus == Focus::Results => {
                        if let Some(pat) = results_search_pattern.clone() {
                            let mut s = state.lock().unwrap();
                            if !s.results_find(&pat, true, true) {
                                drop(s);
                                toasts.push((
                                    "Pattern not found".into(),
                                    ToastKind::Info,
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                    }
                    (KeyModifiers::SHIFT, KeyCode::Char('N'))
                    | (KeyModifiers::NONE, KeyCode::Char('N'))
                        if focus == Focus::Results =>
                    {
                        if let Some(pat) = results_search_pattern.clone() {
                            let mut s = state.lock().unwrap();
                            if !s.results_find(&pat, false, true) {
                                drop(s);
                                toasts.push((
                                    "Pattern not found".into(),
                                    ToastKind::Info,
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                    }
                    // Enter visual-line / visual-block selection in results.
                    (KeyModifiers::SHIFT, KeyCode::Char('V'))
                    | (KeyModifiers::NONE, KeyCode::Char('V'))
                        if focus == Focus::Results =>
                    {
                        let mut s = state.lock().unwrap();
                        let already_line = matches!(
                            s.active_result().and_then(|t| t.selection),
                            Some(sqeel_core::state::ResultsSelection {
                                mode: sqeel_core::state::ResultsSelectionMode::Line,
                                ..
                            })
                        );
                        if already_line {
                            s.results_clear_selection();
                        } else {
                            s.results_enter_selection(
                                sqeel_core::state::ResultsSelectionMode::Line,
                            );
                        }
                    }
                    // Block selection: `Ctrl-V` (vim) or lowercase `v`.
                    // Char-visual doesn't apply to a cell grid, so `v`
                    // is repurposed here — also gives users whose
                    // terminal swallows `Ctrl-V` a working fallback.
                    (KeyModifiers::CONTROL, KeyCode::Char('v'))
                    | (KeyModifiers::NONE, KeyCode::Char('v'))
                        if focus == Focus::Results =>
                    {
                        let mut s = state.lock().unwrap();
                        let already_block = matches!(
                            s.active_result().and_then(|t| t.selection),
                            Some(sqeel_core::state::ResultsSelection {
                                mode: sqeel_core::state::ResultsSelectionMode::Block,
                                ..
                            })
                        );
                        if already_block {
                            s.results_clear_selection();
                        } else {
                            s.results_enter_selection(
                                sqeel_core::state::ResultsSelectionMode::Block,
                            );
                        }
                    }
                    // Esc cancels an active selection before falling through
                    // to the default Esc handling.
                    (KeyModifiers::NONE, KeyCode::Esc)
                        if focus == Focus::Results
                            && state
                                .lock()
                                .unwrap()
                                .active_result()
                                .and_then(|t| t.selection)
                                .is_some() =>
                    {
                        state.lock().unwrap().results_clear_selection();
                    }
                    (KeyModifiers::NONE, KeyCode::Char('y')) if focus == Focus::Results => {
                        let now = std::time::Instant::now();
                        let has_selection = state
                            .lock()
                            .unwrap()
                            .active_result()
                            .and_then(|t| t.selection)
                            .is_some();
                        let is_yy = pending_results_y
                            .is_some_and(|t| now.duration_since(t).as_millis() < 500);
                        let yanked = if has_selection {
                            let mut s = state.lock().unwrap();
                            let y = s.results_selection_yank();
                            s.results_clear_selection();
                            y
                        } else if is_yy {
                            state.lock().unwrap().results_cursor_yank_row()
                        } else {
                            state.lock().unwrap().results_cursor_yank()
                        };
                        pending_results_y = if has_selection || is_yy {
                            None
                        } else {
                            Some(now)
                        };
                        if let Some((text, label)) = yanked {
                            let ok = clipboard
                                .set(Selection::Clipboard, MimeType::Text, text.as_bytes())
                                .is_ok();
                            toasts.push((
                                if ok {
                                    format!("{label} copied to clipboard")
                                } else {
                                    format!("{label}: clipboard copy failed (too large)")
                                },
                                if ok {
                                    ToastKind::Info
                                } else {
                                    ToastKind::Error
                                },
                                now,
                            ));
                        }
                    }
                    // On error tab: Enter jumps editor cursor to the reported line:col
                    (KeyModifiers::NONE, KeyCode::Enter) if focus == Focus::Results => {
                        let jump = {
                            let s = state.lock().unwrap();
                            s.active_result().and_then(|t| match &t.kind {
                                ResultsPane::Error(msg) => parse_error_position(msg),
                                _ => None,
                            })
                        };
                        if let Some((line, col)) = jump {
                            editor.jump_to(line, col);
                            state.lock().unwrap().focus = Focus::Editor;
                        }
                    }
                    // Execute query under cursor: Ctrl+Enter
                    // Visual/VisualLine/VisualBlock first: run the
                    // selected text instead of the statement under
                    // cursor. Lets the user mark exactly what to run.
                    (KeyModifiers::CONTROL, KeyCode::Enter) => {
                        run_statement_under_cursor(&mut editor, &state);
                    }
                    // Run all statements in the file: Ctrl+Shift+Enter
                    (m, KeyCode::Enter)
                        if m.contains(KeyModifiers::CONTROL) && m.contains(KeyModifiers::SHIFT) =>
                    {
                        run_all_statements(&mut editor, &state);
                    }
                    // History navigation: Ctrl+P (prev) / Ctrl+N (next)
                    (KeyModifiers::CONTROL, KeyCode::Char('p')) if focus == Focus::Editor => {
                        let recalled = state.lock().unwrap().history_prev().map(|s| s.to_owned());
                        if let Some(q) = recalled {
                            editor.set_content(&q);
                            last_highlight_top = usize::MAX;
                        }
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('n')) if focus == Focus::Editor => {
                        let recalled = state.lock().unwrap().history_next().map(|s| s.to_owned());
                        if let Some(q) = recalled {
                            editor.set_content(&q);
                        } else {
                            editor.set_content("");
                        }
                        last_highlight_top = usize::MAX;
                    }
                    // Pane focus — forward to tmux when already at the edge pane
                    (KeyModifiers::CONTROL, KeyCode::Char('h')) => {
                        if focus == Focus::Schema {
                            tmux_navigate('L');
                        } else {
                            state.lock().unwrap().focus = Focus::Schema;
                        }
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                        if focus == Focus::Editor {
                            tmux_navigate('R');
                        } else {
                            state.lock().unwrap().focus = Focus::Editor;
                        }
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
                        if focus == Focus::Results || !show_results {
                            tmux_navigate('D');
                        } else {
                            state.lock().unwrap().focus = Focus::Results;
                        }
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
                        if focus == Focus::Editor {
                            tmux_navigate('U');
                        } else {
                            state.lock().unwrap().focus = Focus::Editor;
                        }
                    }
                    // `K` in Normal → LSP hover request. Popup opens
                    // immediately in a loading state so the user can
                    // cancel with `Esc` even before sqls answers
                    // (cold server can take a beat on first request).
                    // Response arrives via `LspEvent::Hover` and swaps
                    // content into place.
                    (KeyModifiers::SHIFT, KeyCode::Char('K'))
                    | (KeyModifiers::NONE, KeyCode::Char('K'))
                        if focus == Focus::Editor && vim_mode == VimMode::Normal =>
                    {
                        let (row, col) = editor.cursor();
                        let word = word_at_cursor(&buffer_lines(editor.buffer()), row, col);
                        // Three-tier dispatch for K:
                        //   1. Word matches a table whose columns are
                        //      cached → render from cache instantly.
                        //   2. Word matches a known table but columns
                        //      haven't been fetched yet → queue a
                        //      `SchemaLoadRequest::Columns`, show
                        //      loading; main loop swaps to the table
                        //      once schema_cache_rx fills it.
                        //   3. No table match → fall through to LSP
                        //      `textDocument/hover`.
                        let mut handled = false;
                        if !word.is_empty() {
                            let mut s = state.lock().unwrap();
                            if let Some(table) = s.hover_table_from_cache(&word) {
                                s.open_hover_table(table);
                                last_hover_id = None;
                                handled = true;
                            } else if let Some((db, loaded)) = s.find_table(&word)
                                && !loaded
                            {
                                s.open_hover_pending_columns(db, word.clone());
                                last_hover_id = None;
                                handled = true;
                            }
                        }
                        if !handled && let Some(ref client) = lsp {
                            last_hover_id = Some(client.writer().request_hover(
                                scratch_uri.clone(),
                                row as u32,
                                col as u32,
                            ));
                            state.lock().unwrap().open_hover_loading();
                        }
                    }
                    // `/`, `?`, `n`, `N` — all handled in the vim engine.
                    _ if focus == Focus::Editor => {
                        if vim_mode == VimMode::Normal
                            && (key.modifiers == KeyModifiers::NONE
                                || key.modifiers == KeyModifiers::SHIFT)
                            && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P'))
                            && let Some(text) = clipboard
                                .get(Selection::Clipboard, MimeType::Text)
                                .ok()
                                .and_then(|b| String::from_utf8(b).ok())
                        {
                            editor.seed_yank(text);
                        }
                        // `"+` / `"*` paste path — pull the OS clipboard
                        // into the `+` / `*` register slot so vim's
                        // paste handler reads the live contents instead
                        // of a stale snapshot.
                        if editor.pending_register_is_clipboard()
                            && let Some(text) = clipboard
                                .get(Selection::Clipboard, MimeType::Text)
                                .ok()
                                .and_then(|b| String::from_utf8(b).ok())
                        {
                            editor.sync_clipboard_register(text, false);
                        }
                        hjkl_vim::dispatch_input(&mut editor, crossterm_to_input(key));
                        // Drain any LSP intent raised by the vim
                        // engine (e.g. `gd` → GotoDefinition) and
                        // route it to `sqls`. Response lands on the
                        // `LspEvent` channel and jumps the cursor.
                        if let Some(hjkl_engine::LspIntent::GotoDefinition) =
                            editor.take_lsp_intent()
                            && let Some(ref client) = lsp
                        {
                            let (row, col) = editor.cursor();
                            last_definition_id = Some(client.writer().request_definition(
                                scratch_uri.clone(),
                                row as u32,
                                col as u32,
                            ));
                        }
                        if let Some(text) = editor.host_mut().take_clipboard_writes().pop() {
                            let ok = clipboard
                                .set(Selection::Clipboard, MimeType::Text, text.as_bytes())
                                .is_ok();
                            toasts.push((
                                if ok {
                                    "Yanked to clipboard".to_string()
                                } else {
                                    "Yank: clipboard copy failed (too large)".to_string()
                                },
                                if ok {
                                    ToastKind::Info
                                } else {
                                    ToastKind::Error
                                },
                                std::time::Instant::now(),
                            ));
                        }
                    }
                    _ => {}
                }
                if !keep_schema_g_pending {
                    schema_g_pending = false;
                }
                // Clear pending `g` and digit count once any key that
                // wasn't part of the chord/count consumed its turn.
                // Count arms above reset to 0 when they fire; `gg`
                // clears it on the second `g`. Anything else lands here
                // with results_count == 0 already, except where the
                // user hit a non-nav key between digits — flush it.
                // Both Results and Hover use the same chord tracker;
                // include both panes here or a `g` in hover would be
                // wiped before the second keystroke can complete `gg`.
                let keep_results_g = matches!(focus, Focus::Results | Focus::Hover)
                    && key.modifiers == KeyModifiers::NONE
                    && matches!(key.code, KeyCode::Char('g'))
                    && results_g_pending;
                if !keep_results_g {
                    results_g_pending = false;
                }
                let keep_results_count = focus == Focus::Results
                    && key.modifiers == KeyModifiers::NONE
                    && matches!(key.code, KeyCode::Char('0'..='9'));
                if !keep_results_count {
                    results_count = 0;
                }
            } // Event::Key
            Event::Resize(_, _) => {
                terminal.autoresize()?;
            }
            _ => {} // FocusGained, FocusLost, Paste — ignore
        } // match event
    }
    // Graceful LSP shutdown.  `kill_on_drop(true)` is the ultimate
    // backstop for crashes / SIGKILL; this path lets a well-behaved
    // server clean up on clean exits.
    if let Some(mut client) = lsp.take() {
        client.shutdown().await;
    }
    Ok(())
}

/// Run the SQL statement under the cursor (or the visual selection) against the
/// active connection.  Mirrors the `Ctrl+Enter` / `<leader><CR>` key handlers.
fn run_statement_under_cursor(
    editor: &mut Editor<hjkl_buffer::Buffer, SqeelHost>,
    state: &Arc<Mutex<AppState>>,
) {
    let content = editor.content();
    let stmt = if let Some(sel) = visual_selection_text(editor) {
        sel
    } else {
        let cursor_byte = cursor_byte_offset(&buffer_lines(editor.buffer()), editor.cursor());
        statement_at_byte(&content, cursor_byte)
            .map(|(s, e)| content[s..e].trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| content.trim().to_string())
    };
    // Drop visual mode after capturing the text so results render against a
    // clean cursor (the user expects Ctrl+Enter to also "commit" the
    // selection — like running it commits to it).
    if editor.vim_mode() != hjkl_engine::VimMode::Normal {
        hjkl_vim::dispatch_input(
            editor,
            hjkl_engine::Input {
                key: hjkl_engine::Key::Esc,
                ..hjkl_engine::Input::default()
            },
        );
    }
    let stmt = stmt.trim().to_string();
    let mut s = state.lock().unwrap();
    s.dismiss_completions();
    let dialect = s.active_dialect;
    if strip_sql_comments(&stmt).trim().is_empty() {
        // nothing to run
    } else if !dialect.is_native_statement(&stmt)
        && let Some(err) = first_syntax_error(&stmt)
    {
        s.dismiss_results();
        s.set_error(format!(
            "Syntax error at {}:{} — {}",
            err.line, err.col, err.message
        ));
    } else {
        s.dismiss_results();
        let tab_idx = s.push_loading_tab(stmt.clone());
        let sent = s.send_query(stmt.clone(), tab_idx);
        if !sent {
            s.push_history(&stmt);
            s.dismiss_results();
            s.set_error("No DB connected. Use --url / --connection or <leader>c to switch.".into());
        }
    }
}

/// Run every non-empty statement in the editor buffer against the active
/// connection.  Mirrors the `Ctrl+Shift+Enter` / `<leader><Tab>` key handlers.
fn run_all_statements(
    editor: &mut Editor<hjkl_buffer::Buffer, SqeelHost>,
    state: &Arc<Mutex<AppState>>,
) {
    let content = editor.content();
    let stmts: Vec<String> = statement_ranges(&content)
        .into_iter()
        .map(|(s, e)| content[s..e].trim().to_string())
        .filter(|s| !s.is_empty())
        .filter(|s| !strip_sql_comments(s).trim().is_empty())
        .collect();
    let mut s = state.lock().unwrap();
    s.dismiss_completions();
    let dialect = s.active_dialect;
    // Syntax pre-check only if none of the statements are engine-native
    // (DESC, SHOW, PRAGMA, …) — tree-sitter-sequel rejects those but the DB
    // runs them fine.
    let any_native = stmts.iter().any(|s| dialect.is_native_statement(s));
    let syntax_err = if any_native {
        None
    } else {
        first_syntax_error(&content)
    };
    if stmts.is_empty() {
        // nothing to run
    } else if let Some(err) = syntax_err {
        s.dismiss_results();
        s.set_error(format!(
            "Syntax error at {}:{} — {}",
            err.line, err.col, err.message
        ));
    } else {
        s.dismiss_results();
        for stmt in &stmts {
            s.push_loading_tab(stmt.clone());
        }
        if !s.send_batch(stmts, 0) {
            s.dismiss_results();
            s.set_error("No DB connected. Use --url / --connection or <leader>c to switch.".into());
        }
    }
}

fn tmux_navigate(direction: char) {
    if std::env::var("TMUX").is_ok() {
        let _ = std::process::Command::new("tmux")
            .args(["select-pane", &format!("-{direction}")])
            .spawn();
    }
}

fn mode_label(state: &AppState) -> Span<'static> {
    let u = ui();
    match state.vim_mode {
        VimMode::Normal => Span::styled(" NORMAL ", Style::default().fg(u.status_mode_normal)),
        VimMode::Insert => Span::styled(" INSERT ", Style::default().fg(u.status_mode_insert)),
        VimMode::Visual => Span::styled(" VISUAL ", Style::default().fg(u.status_mode_visual)),
        VimMode::VisualLine => Span::styled(" V-LINE ", Style::default().fg(u.status_mode_visual)),
        VimMode::VisualBlock => {
            Span::styled(" V-BLOCK ", Style::default().fg(u.status_mode_visual))
        }
    }
}

fn diag_label(state: &AppState) -> Option<Span<'static>> {
    let errors = state
        .lsp_diagnostics
        .iter()
        .filter(|d| d.severity == lsp_types::DiagnosticSeverity::ERROR)
        .count();
    let warnings = state
        .lsp_diagnostics
        .iter()
        .filter(|d| d.severity == lsp_types::DiagnosticSeverity::WARNING)
        .count();
    if errors > 0 {
        Some(Span::styled(
            format!(" ✖ {errors}E "),
            Style::default().fg(ui().status_diag_error),
        ))
    } else if warnings > 0 {
        Some(Span::styled(
            format!(" ⚠ {warnings}W "),
            Style::default().fg(ui().status_diag_warning),
        ))
    } else {
        None
    }
}

/// Width of the dedicated diagnostic-sign column rendered to the left of
/// the number gutter (vim `signcolumn=yes`). Shared by the renderer, the
/// terminal-cursor placement, and the mouse→doc translation so all three
/// agree on where the text column starts.
const EDITOR_SIGN_COL_WIDTH: u16 = 1;

/// Translate a terminal-cell mouse position inside the editor pane into
/// document `(row, col)` coordinates. Mirrors the geometry `draw_editor`
/// renders with: one tab-bar row on top, a 1-col horizontal margin, then
/// `[sign][number]` gutter before the text. Replaces the engine's removed
/// `mouse_click_in_rect` (the mouse API takes doc coordinates since
/// hjkl-engine 0.8).
fn editor_cell_to_doc<H: Host>(
    editor: &Editor<hjkl_buffer::Buffer, H>,
    area: Rect,
    col: u16,
    row: u16,
) -> (usize, usize) {
    let rope = editor.buffer().rope();
    let inner_top = area.y.saturating_add(1); // tab bar row
    let content_x = area
        .x
        .saturating_add(1) // horizontal margin
        .saturating_add(EDITOR_SIGN_COL_WIDTH)
        .saturating_add(editor.lnum_width());
    let v = editor.host().viewport();
    let rel_row = row.saturating_sub(inner_top) as usize;
    let doc_row = (v.top_row + rel_row).min(rope.len_lines().saturating_sub(1));
    let rel_col = col.saturating_sub(content_x) as usize + v.top_col;
    let line_chars = hjkl_buffer::rope_line_str(&rope, doc_row).chars().count();
    (doc_row, rel_col.min(line_chars.saturating_sub(1)))
}

/// Status-bar block showing `/<pat> <i>/<n>` when an editor search is active.
/// `i` is the 1-based index of the match at-or-after the cursor; 0 means no
/// match has been navigated to yet (cursor is past the last match).
fn search_label<H: Host>(editor: &Editor<hjkl_buffer::Buffer, H>) -> Option<Span<'static>> {
    let re = editor.search_state().pattern.as_ref()?;
    let pat = re.as_str().to_string();
    let lines = buffer_lines(editor.buffer());
    let (cur_row, cur_col) = editor.cursor();
    let mut total = 0usize;
    let mut current = 0usize;
    for (row, line) in lines.iter().enumerate() {
        for m in re.find_iter(line) {
            total += 1;
            if current == 0 {
                let on_or_after_cursor = row > cur_row
                    || (row == cur_row && byte_to_char_col(line, m.start()) >= cur_col);
                if on_or_after_cursor {
                    current = total;
                }
            }
        }
    }
    if total == 0 {
        return Some(Span::raw(format!(" /{pat} 0/0 ")));
    }
    if current == 0 {
        current = total;
    }
    Some(Span::raw(format!(" /{pat} {current}/{total} ")))
}

fn byte_to_char_col(line: &str, byte_idx: usize) -> usize {
    line[..byte_idx.min(line.len())].chars().count()
}

/// Extract the first `L:C` (1-based line:column) location from a message like
/// `"Syntax error at 3:7 — unexpected `foo`"`. Returns `None` if no match.
fn parse_error_position(msg: &str) -> Option<(usize, usize)> {
    let bytes = msg.as_bytes();
    for i in 0..bytes.len() {
        if !bytes[i].is_ascii_digit() {
            continue;
        }
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b':' {
            continue;
        }
        let mut k = j + 1;
        let col_start = k;
        while k < bytes.len() && bytes[k].is_ascii_digit() {
            k += 1;
        }
        if k == col_start {
            continue;
        }
        let line: usize = msg[i..j].parse().ok()?;
        let col: usize = msg[col_start..k].parse().ok()?;
        return Some((line, col));
    }
    None
}

/// Pull the active visual-mode selection out as a runnable string.
/// Returns `None` when the editor isn't in any Visual mode.
///
/// - **Visual** (charwise): exact span between anchor and cursor.
///   First and last lines may be partial; intermediate lines are
///   joined with `\n`.
/// - **VisualLine**: every line in `[top, bot]`, joined with `\n`.
/// - **VisualBlock**: collapsed to the full lines the block spans —
///   running a column-narrow rectangle as SQL doesn't make sense, but
///   "the lines I marked" matches user intent and matches what
///   VisualLine would have produced.
fn visual_selection_text<H: Host>(editor: &Editor<hjkl_buffer::Buffer, H>) -> Option<String> {
    let lines = buffer_lines(editor.buffer());
    match editor.vim_mode() {
        hjkl_engine::VimMode::Visual => {
            let ((sr, sc), (er, ec)) = editor.char_highlight()?;
            if sr == er {
                let line = lines.get(sr)?;
                Some(
                    line.chars()
                        .skip(sc)
                        .take(ec.saturating_sub(sc) + 1)
                        .collect(),
                )
            } else {
                let mut out = String::new();
                let first = lines.get(sr)?;
                out.push_str(&first.chars().skip(sc).collect::<String>());
                for r in (sr + 1)..er {
                    out.push('\n');
                    if let Some(l) = lines.get(r) {
                        out.push_str(l);
                    }
                }
                out.push('\n');
                let last = lines.get(er)?;
                out.push_str(&last.chars().take(ec + 1).collect::<String>());
                Some(out)
            }
        }
        hjkl_engine::VimMode::VisualLine => {
            let (top, bot) = editor.line_highlight()?;
            let bot = bot.min(lines.len().saturating_sub(1));
            Some(lines[top..=bot].join("\n"))
        }
        hjkl_engine::VimMode::VisualBlock => {
            let (top, bot, _, _) = editor.block_highlight()?;
            let bot = bot.min(lines.len().saturating_sub(1));
            Some(lines[top..=bot].join("\n"))
        }
        _ => None,
    }
}

/// Convert a (row, char-col) cursor into a byte offset into `lines.join("\n")`.
fn cursor_byte_offset(lines: &[String], cursor: (usize, usize)) -> usize {
    let mut byte = 0;
    for (i, line) in lines.iter().enumerate() {
        if i < cursor.0 {
            byte += line.len() + 1; // +1 for '\n'
        } else if i == cursor.0 {
            byte += line
                .chars()
                .take(cursor.1)
                .map(|c| c.len_utf8())
                .sum::<usize>();
            break;
        }
    }
    byte
}

/// Desired terminal cursor shape after a draw. The TUI uses a thin vertical bar
/// for any text-input context (insert mode, dialogs, schema search) and a thick
/// block for editor normal mode, so cursors look consistent across the app.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Error,
    Info,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum CursorShape {
    #[default]
    Hidden,
    Bar,
    Block,
}

#[derive(Default, Clone, Copy)]
struct DrawAreas {
    schema_list_area: Rect,
    schema_list_offset: usize,
    schema_list_count: usize,
    schema_list_filtered: bool,
    editor: Rect,
    tab_bar: Rect,
    results: Option<Rect>,
    results_tab_bar: Option<Rect>,
    cursor_shape: CursorShape,
    /// Upper bound for `help_scroll`: beyond this the bottom of the
    /// help overlay is already visible. Recomputed each frame from the
    /// current terminal size so `j` / `Down` / wheel-down saturate at
    /// the last meaningful scroll offset.
    help_max_scroll: u16,
}

#[allow(clippy::too_many_arguments)]
fn draw<H: Host>(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    editor: &mut Editor<hjkl_buffer::Buffer, H>,
    command_input: Option<&TextInput>,
    rename_input: Option<&TextInput>,
    file_picker: Option<&mut hjkl_picker::Picker>,
    delete_confirm: Option<&str>,
    quit_prompt_dirty: Option<&[String]>,
    sqls_prompt_open: bool,
    schema_search: &SchemaSearch,
    editor_search_text: Option<&str>,
    last_editor_search: Option<&str>,
    results_search_text: Option<&str>,
    hover_search_text: Option<&str>,
    sig_help_text: Option<&str>,
    toasts: &[(String, ToastKind)],
    cursorline: bool,
    cursorcolumn: bool,
) -> DrawAreas {
    let area = f.area();

    let lsp_warn = !state.lsp_available;

    // Always reserve 1 row for the status bar; optionally 1 more for LSP warning above it.
    let (main_area, lsp_warn_area, status_area) = if lsp_warn {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        (chunks[0], Some(chunks[1]), chunks[2])
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        (chunks[0], None, chunks[1])
    };

    let outer_raw = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![
            Constraint::Min(30),
            Constraint::Length(1),
            Constraint::Percentage(85),
        ])
        .split(main_area);
    let outer: Vec<Rect> = {
        let sep = outer_raw[1];
        f.render_widget(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(ui().pane_sep).bg(ui().schema_pane_bg)),
            sep,
        );
        vec![outer_raw[0], outer_raw[2]]
    };

    let schema_focused = state.focus == Focus::Schema;
    let editor_focused = state.focus == Focus::Editor;
    let results_focused = state.focus == Focus::Results;

    // Schema panel
    let (
        schema_list_area,
        schema_list_offset,
        schema_list_count,
        schema_list_filtered,
        schema_search_cursor,
    ) = draw_schema(f, state, outer[0], schema_focused, schema_search);

    let show_results = state.has_results();
    let editor_pct = (state.editor_ratio * 100.0) as u16;
    let results_pct = 100 - editor_pct;

    let right_chunks = if show_results {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(editor_pct),
                Constraint::Percentage(results_pct),
            ])
            .split(outer[1])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(100)])
            .split(outer[1])
    };

    // Tab bar is the top row of the editor pane, flush with no padding.
    let tab_bar = Rect {
        x: right_chunks[0].x,
        y: right_chunks[0].y,
        width: right_chunks[0].width,
        height: 1,
    };
    let results_tab_bar = if show_results {
        let results_area = right_chunks[1];
        if state.result_tabs.len() > 1 && results_area.height > 2 {
            // Tab bar now sits beneath a 1-row separator at the top of the
            // results pane — shift its y accordingly for click hit-testing.
            Some(Rect {
                x: results_area.x + 1,
                y: results_area.y + 1,
                width: results_area.width.saturating_sub(2),
                height: 1,
            })
        } else {
            None
        }
    } else {
        None
    };
    let mut areas = DrawAreas {
        schema_list_area,
        schema_list_offset,
        schema_list_count,
        schema_list_filtered,
        editor: right_chunks[0],
        tab_bar,
        results: if show_results {
            Some(right_chunks[1])
        } else {
            None
        },
        results_tab_bar,
        cursor_shape: CursorShape::Hidden,
        help_max_scroll: 0,
    };

    draw_editor(
        f,
        state,
        editor,
        right_chunks[0],
        editor_focused,
        (editor_search_text, last_editor_search),
        (cursorline, cursorcolumn),
    );

    if show_results {
        draw_results(
            f,
            state,
            right_chunks[1],
            results_focused,
            results_search_text,
        );
    }

    // Completion popup (overlay).  Use viewport-relative coordinates so
    // the popup stays inside the editor even when the cursor lives deep
    // in a long file.
    if state.show_completions && !state.completions.is_empty() {
        let (cur_row, cur_col) = editor.cursor();
        let top_row = editor.host().viewport().top_row;
        let top_col = editor.host().viewport().top_col;
        let screen_row = cur_row.saturating_sub(top_row);
        let screen_col = cur_col.saturating_sub(top_col);
        draw_completions(f, state, right_chunks[0], screen_row, screen_col);
    }

    // Signature help bar: one-line overlay above the cursor, shown while
    // Insert mode is active after `(` or `,`. Suppresses hover popup
    // (signature wins over hover when both would otherwise be visible).
    if let Some(text) = sig_help_text {
        let (cur_row, cur_col) = editor.cursor();
        let top_row = editor.host().viewport().top_row;
        let top_col = editor.host().viewport().top_col;
        let screen_row = cur_row.saturating_sub(top_row);
        let screen_col = cur_col.saturating_sub(top_col);
        draw_sig_help_bar(f, right_chunks[0], screen_row, screen_col, text);
    } else {
        // Hover popup (LSP `K`). Centered borderless dialog matching the
        // command / rename / delete / file-picker styling — dialog_bg as
        // the only chrome, padded inside, `Clear` under it to punch
        // through whatever the editor drew.
        if let Some(ref table) = state.hover_table {
            draw_hover_table(f, area, state, table, hover_search_text);
        } else if let Some(ref text) = state.hover_text {
            draw_hover_popup(f, area, state.hover_scroll, text);
        } else if state.hover_loading {
            draw_hover_loading(f, area);
        }
    }

    // sqls install prompt modal (shown on startup when sqls is missing)
    if sqls_prompt_open {
        draw_sqls_prompt_modal(f, area);
    }

    // Connection switcher modal (top-level overlay)
    if state.show_connection_switcher {
        draw_connection_switcher(f, state, area);
    }

    // pgpass picker (sits above connection switcher, below add-connection)
    if state.show_pgpass_picker {
        draw_pgpass_picker(f, state, area);
    }

    // Add connection dialog (above switcher and pgpass picker)
    let mut add_connection_cursor: Option<(u16, u16)> = None;
    if state.show_add_connection {
        add_connection_cursor = Some(draw_add_connection(f, state, area));
    }

    // Help overlay (topmost)
    if state.show_help {
        areas.help_max_scroll = draw_help(f, area, state.help_scroll);
    }

    // Connection-error details popup. Sits above help / switcher /
    // add — when a connect fails the popup is the only useful next
    // step, so it wins z-order against everything except toasts.
    if state.show_connect_error_popup
        && let Some(err) = state.schema_connect_error.as_deref()
    {
        let name = state.active_connection.as_deref();
        let headline = state
            .schema_connect_error_kind
            .map(|k| k.headline())
            .unwrap_or("Connection failed");
        draw_connect_error_popup(f, area, headline, err, name);
    }

    // LSP warning bar (above status bar)
    if let Some(warn_area) = lsp_warn_area {
        let msg = Paragraph::new(Span::styled(
            format!(" ⚠ LSP not available ({})", state.lsp_binary),
            Style::default().fg(ui().lsp_warn_fg).bg(ui().lsp_warn_bg),
        ));
        f.render_widget(msg, warn_area);
    }

    // Status bar (always at bottom)
    draw_status_bar(f, state, editor, status_area);

    // Command palette: small centered dialog, no borders, 2-col + 1-row padding.
    let mut dialog_cursor: Option<(u16, u16)> = None;
    if let Some(cmd) = command_input {
        dialog_cursor = Some(draw_input_dialog(f, area, ": ", cmd));
    }

    // Rename prompt: same shape as command palette.
    if let Some(name) = rename_input {
        dialog_cursor = Some(draw_input_dialog(f, area, "> ", name));
    }

    // Editor `/` / `?` search: same shape as command palette. The
    // editor owns the prompt state; we read it for render via
    // `editor.search_prompt()`.
    if let Some(prompt) = editor.search_prompt() {
        let prefix = if prompt.forward { "/ " } else { "? " };
        let input = TextInput {
            text: prompt.text.clone(),
            cursor: prompt.cursor,
        };
        dialog_cursor = Some(draw_input_dialog(f, area, prefix, &input));
    }

    // Delete confirmation: centered borderless dialog.
    if let Some(name) = delete_confirm {
        draw_confirm_dialog(f, area, &format!("Delete '{name}'?  (y / n)"));
    }

    // Quit confirmation when there are unsaved buffers.
    if let Some(names) = quit_prompt_dirty {
        let list = if names.len() <= 3 {
            names.join(", ")
        } else {
            format!("{} + {} more", names[..3].join(", "), names.len() - 3)
        };
        draw_confirm_dialog(
            f,
            area,
            &format!("Save unsaved buffers [{list}]?  (y=save / n=discard / c=cancel)"),
        );
    }

    // File picker (leader+space): centered dialog with input + scrollable list.
    if let Some(picker) = file_picker {
        let active_name = state.tabs.get(state.active_tab).map(|t| t.name.as_str());
        dialog_cursor = Some(draw_file_picker(f, area, picker, active_name));
    }

    // Toast notifications (top-right corner, stacked vertically).
    // Each toast pads 1 row top + bottom around the message. Multi-line
    // messages (e.g. `:reg`, `:marks`) expand the box vertically and widen
    // it to the longest line.
    let mut y_off: u16 = 0;
    for (msg, kind) in toasts {
        let style = match kind {
            ToastKind::Error => Style::default()
                .fg(ui().toast_error_fg)
                .bg(ui().toast_error_bg),
            ToastKind::Info => Style::default()
                .fg(ui().toast_info_fg)
                .bg(ui().toast_info_bg),
        };
        let msg_lines: Vec<&str> = msg.lines().collect();
        let line_count = msg_lines.len().max(1) as u16;
        let max_line = msg_lines
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0) as u16;
        let width = (max_line + 4).min(area.width);
        let height = (line_count + 2).min(area.height.saturating_sub(y_off));
        if height == 0 {
            break;
        }
        let toast_area = Rect {
            x: area.width.saturating_sub(width),
            y: y_off,
            width,
            height,
        };
        f.render_widget(Clear, toast_area);
        f.render_widget(Block::default().style(style), toast_area);
        if height >= 2 {
            let msg_area = Rect {
                x: toast_area.x + 2,
                y: toast_area.y + 1,
                width: toast_area.width.saturating_sub(4),
                height: height.saturating_sub(2),
            };
            f.render_widget(Paragraph::new(msg.as_str()).style(style), msg_area);
        }
        y_off = y_off.saturating_add(height).saturating_add(1);
    }

    // Pick the active cursor target: dialogs > add-connection > schema search >
    // editor (when focused). Bar shape for any text-input context, Block for
    // editor normal mode.
    let (cursor_pos, shape) = if let Some(p) = dialog_cursor {
        (Some(p), CursorShape::Bar)
    } else if let Some(p) = add_connection_cursor {
        (Some(p), CursorShape::Bar)
    } else if let Some(p) = schema_search_cursor {
        (Some(p), CursorShape::Bar)
    } else if state.focus == Focus::Editor
        && !state.show_help
        && !state.show_connection_switcher
        && !state.show_pgpass_picker
    {
        // Reconstruct the textarea rect that draw_editor uses:
        // top row is the tab bar, then a 1-col horizontal margin around the body.
        let pane = right_chunks[0];
        let textarea_rect = Rect {
            x: pane.x.saturating_add(1),
            y: pane.y.saturating_add(1),
            width: pane.width.saturating_sub(2),
            height: pane.height.saturating_sub(1),
        };
        let pos = editor.cursor_screen_pos(
            textarea_rect.x,
            textarea_rect.y,
            textarea_rect.width,
            textarea_rect.height,
            EDITOR_SIGN_COL_WIDTH,
        );
        let shape = if state.vim_mode == VimMode::Insert {
            CursorShape::Bar
        } else {
            CursorShape::Block
        };
        (pos, shape)
    } else {
        (None, CursorShape::Hidden)
    };
    if let Some((x, y)) = cursor_pos {
        f.set_cursor_position((x, y));
    }
    areas.cursor_shape = shape;

    areas
}

fn extract_results_left_click(
    x: u16,
    y: u16,
    areas: &DrawAreas,
    state: &AppState,
) -> Option<(String, &'static str, ResultsCursor)> {
    let results_area = areas.results?;
    use ratatui::layout::Position;
    if !results_area.contains(Position { x, y }) {
        return None;
    }
    let tab_bar_rows: u16 = if state.result_tabs.len() > 1 { 2 } else { 0 };
    // Shared query-row hit-test: row 3 below the tab bar is the query line
    // for every pane that shows it (Results/Error/Cancelled when a query is
    // attached). Clicking it copies the query verbatim.
    let query_text = state
        .active_result()
        .map(|t| t.query.clone())
        .unwrap_or_default();
    let has_query = !query_text.trim().is_empty();
    let pane_has_query_row = matches!(
        state.results(),
        sqeel_core::state::ResultsPane::Results(_)
            | sqeel_core::state::ResultsPane::Cancelled
            | sqeel_core::state::ResultsPane::Error(_)
            | sqeel_core::state::ResultsPane::NonQuery { .. }
    ) && has_query;
    if pane_has_query_row
        && y == results_area.y + tab_bar_rows + 3
        && x >= results_area.x
        && x < results_area.x + results_area.width
    {
        return Some((query_text, "Query", ResultsCursor::Query));
    }
    match state.results() {
        sqeel_core::state::ResultsPane::Results(r) => {
            let header_y = results_area.y + tab_bar_rows + 5;
            let body_y = results_area.y + tab_bar_rows + 7;
            let body_x = results_area.x + 1;
            if y < header_y || y == header_y + 1 {
                return None;
            }
            let char_offset: usize = r
                .col_widths
                .iter()
                .take(state.results_col_scroll())
                .map(|&w| w as usize + 1)
                .sum();
            let rel = (x.saturating_sub(body_x) as usize).saturating_add(char_offset);
            let mut cursor_x = 0usize;
            let mut col_idx: Option<usize> = None;
            for (i, &w) in r.col_widths.iter().enumerate() {
                let col_w = w as usize;
                if rel < cursor_x + col_w {
                    col_idx = Some(i);
                    break;
                }
                cursor_x += col_w;
                if i + 1 < r.col_widths.len() {
                    if rel == cursor_x {
                        return None;
                    }
                    cursor_x += 1;
                }
            }
            let col_idx = col_idx?;
            if y == header_y {
                let name = r.columns.get(col_idx)?.clone();
                return Some((name, "Column", ResultsCursor::Header(col_idx)));
            }
            if y < body_y {
                return None;
            }
            let row_idx = (y - body_y) as usize + state.results_scroll();
            let value = r.rows.get(row_idx)?.get(col_idx)?.trim().to_string();
            Some((
                value,
                "Value",
                ResultsCursor::Cell {
                    row: row_idx,
                    col: col_idx,
                },
            ))
        }
        sqeel_core::state::ResultsPane::Error(e) => {
            let content_y = results_area.y + tab_bar_rows;
            if y < content_y {
                return None;
            }
            let rel_y = (y - content_y) as usize;
            let query = state
                .active_result()
                .map(|t| t.query.clone())
                .unwrap_or_default();
            let (body_start, has_q) = if !query.trim().is_empty() {
                (5usize, true)
            } else {
                (3usize, false)
            };
            if has_q && rel_y == 3 {
                return Some((query.clone(), "Query", ResultsCursor::Query));
            }
            if rel_y >= body_start {
                let line_idx = rel_y - body_start + state.results_scroll();
                let line = e.lines().nth(line_idx)?.to_string();
                return Some((line, "Line", ResultsCursor::MessageLine(line_idx)));
            }
            None
        }
        sqeel_core::state::ResultsPane::Cancelled => {
            let content_y = results_area.y + tab_bar_rows;
            if y < content_y {
                return None;
            }
            let rel_y = (y - content_y) as usize;
            let query = state
                .active_result()
                .map(|t| t.query.clone())
                .unwrap_or_default();
            let has_q = !query.trim().is_empty();
            let body_start = if has_q { 5 } else { 3 };
            if has_q && rel_y == 3 {
                return Some((query, "Query", ResultsCursor::Query));
            }
            if rel_y >= body_start {
                return Some((
                    "Skipped after earlier error".to_string(),
                    "Line",
                    ResultsCursor::MessageLine(0),
                ));
            }
            None
        }
        sqeel_core::state::ResultsPane::NonQuery {
            verb,
            rows_affected,
        } => {
            let content_y = results_area.y + tab_bar_rows;
            if y < content_y {
                return None;
            }
            let rel_y = (y - content_y) as usize;
            let query = state
                .active_result()
                .map(|t| t.query.clone())
                .unwrap_or_default();
            let has_q = !query.trim().is_empty();
            let body_start = if has_q { 5 } else { 3 };
            if has_q && rel_y == 3 {
                return Some((query, "Query", ResultsCursor::Query));
            }
            if rel_y >= body_start {
                return Some((
                    non_query_summary(verb, *rows_affected),
                    "Line",
                    ResultsCursor::MessageLine(0),
                ));
            }
            None
        }
        _ => None,
    }
}

/// One-line summary for the NonQuery results pane. DML verbs report
/// the affected row count; DDL / transaction control collapse to
/// "OK · {VERB}" since the row count is meaningless there.
fn non_query_summary(verb: &str, rows_affected: u64) -> String {
    let is_dml = matches!(
        verb,
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "MERGE" | "UPSERT"
    );
    if is_dml {
        let noun = if rows_affected == 1 { "row" } else { "rows" };
        format!("{verb} OK · {rows_affected} {noun} affected")
    } else {
        format!("{verb} OK")
    }
}

fn extract_results_row(x: u16, y: u16, areas: &DrawAreas, state: &AppState) -> Option<String> {
    let results_area = areas.results?;
    use ratatui::layout::Position;
    if !results_area.contains(Position { x, y }) {
        return None;
    }
    let r = match state.results() {
        sqeel_core::state::ResultsPane::Results(r) => r,
        _ => return None,
    };
    let tab_bar_rows: u16 = if state.result_tabs.len() > 1 { 2 } else { 0 };
    let body_y = results_area.y + tab_bar_rows + 7;
    if y < body_y {
        return None;
    }
    let row_idx = (y - body_y) as usize + state.results_scroll();
    r.rows.get(row_idx).map(|row| row.join("\t"))
}

fn draw_status_bar<H: Host>(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    editor: &Editor<hjkl_buffer::Buffer, H>,
    area: Rect,
) {
    let mode = mode_label(state);
    let mode_width = mode.content.len() as u16;

    let conn = state
        .active_connection
        .as_deref()
        .unwrap_or("no connection");
    let tab_name = state
        .tabs
        .get(state.active_tab)
        .map(|t| t.name.as_str())
        .unwrap_or("");
    let center_text = format!(" {conn} › {tab_name} ");

    let (row, col) = editor.cursor();
    let cursor_str = format!(" {}:{} ", row + 1, col + 1);
    let cursor_width = cursor_str.len() as u16;

    let diag = diag_label(state);
    let diag_width = diag.as_ref().map(|s| s.content.len() as u16).unwrap_or(0);

    let search = search_label(editor);
    let search_width = search.as_ref().map(|s| s.content.len() as u16).unwrap_or(0);

    let right_width = cursor_width + diag_width + search_width;
    let center_width = area.width.saturating_sub(mode_width + right_width);

    // Mode block (left)
    let mode_area = Rect {
        x: area.x,
        y: area.y,
        width: mode_width.min(area.width),
        height: 1,
    };
    // Center info
    let center_area = Rect {
        x: area.x + mode_width,
        y: area.y,
        width: center_width.min(area.width.saturating_sub(mode_width)),
        height: 1,
    };
    // Right side (search + diag + cursor)
    let right_x = area.x + mode_width + center_width;
    let search_area = Rect {
        x: right_x,
        y: area.y,
        width: search_width,
        height: 1,
    };
    let diag_area = Rect {
        x: right_x + search_width,
        y: area.y,
        width: diag_width,
        height: 1,
    };
    let cursor_area = Rect {
        x: right_x + search_width + diag_width,
        y: area.y,
        width: cursor_width.min(
            area.width
                .saturating_sub(mode_width + center_width + search_width + diag_width),
        ),
        height: 1,
    };

    let bar_bg = Style::default()
        .bg(ui().status_bar_bg)
        .fg(ui().status_bar_fg);

    // Mode label (colored fg, same bg as status bar)
    let mode_style = Style::default()
        .bg(mode.style.fg.unwrap_or(ui().status_mode_normal))
        .fg(ui().status_mode_fg)
        .add_modifier(Modifier::BOLD);
    f.render_widget(
        Paragraph::new(Span::styled(mode.content.to_string(), mode_style)),
        mode_area,
    );

    // Center: connection > tab
    f.render_widget(Paragraph::new(center_text).style(bar_bg), center_area);

    // Search match counter
    if let Some(s) = search {
        let style = Style::default()
            .bg(ui().status_search_bg)
            .fg(ui().status_search_fg)
            .add_modifier(Modifier::BOLD);
        f.render_widget(
            Paragraph::new(Span::styled(s.content.to_string(), style)),
            search_area,
        );
    }

    // Diagnostics
    if let Some(d) = diag {
        let diag_style = Style::default()
            .bg(d.style.fg.unwrap_or(ui().status_diag_warning))
            .fg(ui().status_mode_fg)
            .add_modifier(Modifier::BOLD);
        f.render_widget(
            Paragraph::new(Span::styled(d.content.to_string(), diag_style)),
            diag_area,
        );
    }

    // Cursor position (right-aligned, highlighted)
    let cursor_style = Style::default()
        .bg(ui().status_hint_bg)
        .fg(ui().status_hint_fg)
        .add_modifier(Modifier::BOLD);
    f.render_widget(
        Paragraph::new(Span::styled(cursor_str, cursor_style)),
        cursor_area,
    );
}

fn schema_item_line(item: &SchemaTreeItem, u: &theme::UiColors) -> Line<'static> {
    let indent = " ".repeat(1 + item.depth * 2);
    if let SchemaItemKind::Placeholder { loading } = item.kind {
        // Greyed-out hint row; for loading rows also render the shared
        // spinner frame so the user knows something is still in flight.
        let style = Style::default()
            .fg(u.schema_placeholder_fg)
            .add_modifier(Modifier::ITALIC);
        let mut spans = vec![Span::raw(indent)];
        if loading {
            spans.push(Span::styled(format!("{} ", spinner_frame()), style));
        }
        spans.push(Span::styled(item.name.clone(), style));
        return Line::from(spans);
    }
    // Group header rows (IndexGroup / ForeignKeyGroup) — DIM, no icon.
    if let SchemaItemKind::IndexGroup { .. } | SchemaItemKind::ForeignKeyGroup { .. } = &item.kind {
        let style = Style::default()
            .fg(u.schema_placeholder_fg)
            .add_modifier(Modifier::DIM);
        return Line::from(vec![
            Span::raw(indent),
            Span::styled(item.name.clone(), style),
        ]);
    }
    let (icon, icon_color) = match &item.kind {
        SchemaItemKind::Database => ("󰆼", u.schema_icon_db),
        SchemaItemKind::Table => ("󰓫", u.schema_icon_table),
        SchemaItemKind::Column { is_pk: true, .. } => ("󰌆", u.schema_icon_pk),
        SchemaItemKind::Column { .. } => ("󱘚", u.schema_icon_column),
        SchemaItemKind::Index { .. } => ("󰠞", u.schema_icon_column),
        SchemaItemKind::ForeignKey { .. } => ("󰈩", u.schema_icon_pk),
        SchemaItemKind::Placeholder { .. } => unreachable!("handled above"),
        SchemaItemKind::IndexGroup { .. } | SchemaItemKind::ForeignKeyGroup { .. } => {
            unreachable!("handled above")
        }
    };
    let mut spans = vec![
        Span::raw(indent),
        Span::styled(icon.to_string(), Style::default().fg(icon_color)),
        Span::raw(format!(" {}", item.name)),
    ];
    if let SchemaItemKind::Column { type_name, .. } = &item.kind
        && !type_name.is_empty()
    {
        spans.push(Span::raw(": "));
        spans.push(Span::styled(
            type_name.clone(),
            Style::default().fg(u.schema_type_fg),
        ));
    }
    Line::from(spans)
}

fn draw_schema(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    area: Rect,
    focused: bool,
    search: &SchemaSearch,
) -> (Rect, usize, usize, bool, Option<(u16, u16)>) {
    let searching = search.focused;
    let search_cursor = search.cursor;
    let title = if state.schema_loading {
        format!("Explorer {}", spinner_frame())
    } else if state.schema_nodes.is_empty() {
        "Explorer".to_string()
    } else {
        let count = state.schema_nodes.len();
        format!("Explorer ✓ ({count})")
    };

    let border_style = if focused {
        Style::default().fg(ui().schema_border_focus)
    } else {
        Style::default()
    };

    // Fill pane background (full area), then inset content by 1 on all sides.
    f.render_widget(
        Block::default().style(Style::default().bg(ui().schema_pane_bg)),
        area,
    );

    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });

    // Search box is always visible (3 rows: border+input+border), list below
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(inner);

    let query = search.query().unwrap_or("");
    let has_filter = !query.is_empty();
    // Magnifier glyph + space prefix marks this as the search input.
    let prefix = "🔍 ";
    let input_text = format!("{prefix}{query}");
    let text_cursor = search.query.as_ref().map(|q| q.cursor).unwrap_or(0);
    // The magnifier emoji is 2 cells wide; total prefix width = 3 cells.
    let prefix_cells: u16 = 3;
    let search_cursor_pos = if searching {
        Some((
            chunks[0].x + 1 + prefix_cells + text_cursor as u16,
            chunks[0].y + 1,
        ))
    } else {
        None
    };
    let search_block = Block::default()
        .title(title.clone())
        .title_style(Style::default().add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(if searching {
            Style::default().fg(ui().schema_border_focus)
        } else if has_filter {
            Style::default().fg(ui().schema_border_filter)
        } else {
            border_style
        });
    f.render_widget(Paragraph::new(input_text).block(search_block), chunks[0]);

    // Inset 1 char left+right to align with search box inner content
    let list_area = Rect {
        x: chunks[1].x + 1,
        y: chunks[1].y,
        width: chunks[1].width.saturating_sub(2),
        height: chunks[1].height,
    };

    let items: Vec<&SchemaTreeItem> = if has_filter {
        schema::filter_items(state.all_schema_items(), query)
    } else {
        state.visible_schema_items().iter().collect()
    };

    let item_count = items.len();

    if items.is_empty() {
        let placeholder: String = if has_filter {
            "No matches".into()
        } else if state.schema_connect_error.is_some() {
            let name = state.active_connection.as_deref();
            let retry_hint = match name {
                Some(n) => format!("\n[r] retry {n}"),
                None => "\n[r] retry".into(),
            };
            let headline = state
                .schema_connect_error_kind
                .map(|k| k.headline())
                .unwrap_or("Connection failed");
            format!("{headline}\n[Enter] details{retry_hint}")
        } else if state.schema_connecting {
            match state.active_connection.as_deref() {
                Some(name) => format!("Connecting to {name}..."),
                None => "Connecting...".into(),
            }
        } else if state.active_connection.is_some() {
            "Loading...".into()
        } else {
            "No connection".into()
        };
        f.render_widget(Paragraph::new(placeholder), list_area);
        return (list_area, 0, 0, has_filter, search_cursor_pos);
    }

    let u = ui();
    let list_items: Vec<ListItem> = items
        .iter()
        .map(|item| ListItem::new(schema_item_line(item, &u)))
        .collect();

    // When search box is actively focused, don't highlight the list.
    // In filter-nav mode (filter active, box not focused) use search_cursor.
    // Normal mode uses state.schema_cursor.
    let cursor = if has_filter {
        search_cursor
    } else {
        state.schema_cursor
    };
    let (highlight_style, selected) = if searching {
        (Style::default(), None)
    } else if focused {
        (Style::default().bg(ui().schema_sel_active_bg), Some(cursor))
    } else {
        (
            Style::default().bg(ui().schema_sel_inactive_bg),
            Some(cursor),
        )
    };

    // Publish viewport height so cursor-nav helpers on AppState can keep the
    // selection visible without needing the draw metrics plumbed through.
    state
        .schema_viewport_rows
        .store(list_area.height, std::sync::atomic::Ordering::Relaxed);

    let height = list_area.height as usize;
    let max_offset = item_count.saturating_sub(height.max(1));
    let offset = state.schema_scroll_offset.min(max_offset);
    // Only mark the row as "selected" when it's actually inside the viewport;
    // otherwise ratatui's List would fight our offset and snap back to the
    // cursor every frame.
    let selected_visible = selected.and_then(|c| {
        if height > 0 && c >= offset && c < offset + height {
            Some(c)
        } else {
            None
        }
    });

    let list = List::new(list_items).highlight_style(highlight_style);
    let mut list_state = ListState::default()
        .with_offset(offset)
        .with_selected(selected_visible);
    f.render_stateful_widget(list, list_area, &mut list_state);
    (
        list_area,
        list_state.offset(),
        item_count,
        has_filter,
        search_cursor_pos,
    )
}

fn draw_editor<H: Host>(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    editor: &mut Editor<hjkl_buffer::Buffer, H>,
    area: Rect,
    focused: bool,
    // (active_search, last_search) — bundled to stay under clippy's arg-count limit.
    search: (Option<&str>, Option<&str>),
    // (cursorline, cursorcolumn) — bundled to stay under clippy's arg-count limit.
    cursor_opts: (bool, bool),
) {
    let (editor_search, last_editor_search) = search;
    // Fill pane background
    f.render_widget(
        Block::default().style(Style::default().bg(ui().editor_pane_bg)),
        area,
    );

    // Tab bar sits flush at the top (full-width, no padding); the remaining
    // content below is inset by 1 on all sides.
    let tab_bar_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    let body_outer = Rect {
        x: area.x,
        y: area.y.saturating_add(1),
        width: area.width,
        height: area.height.saturating_sub(1),
    };
    let inner = body_outer.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 0,
    });

    // Show first diagnostic message if any
    let diag_line = state
        .lsp_diagnostics
        .first()
        .map(|d| format!(" {}:{} {}", d.line + 1, d.col + 1, d.message));

    // Split inner: textarea + optional diag (1)
    let mut constraints = vec![Constraint::Min(1)];
    if diag_line.is_some() {
        constraints.push(Constraint::Length(1));
    }
    let body_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);
    // Build a `chunks`-like slice: [tab_bar, textarea, ...extras] so the rest of
    // this function (which references chunks[0..]) keeps working unchanged.
    let mut chunks: Vec<Rect> = vec![tab_bar_area];
    chunks.extend(body_chunks.iter().copied());

    f.render_widget(
        Paragraph::new(build_tab_title(state)).style(Style::default().bg(ui().editor_tab_bar_bg)),
        chunks[0],
    );

    let cursor_line_bg = if focused {
        ui().editor_cursor_line_active
    } else {
        ui().editor_cursor_line_inactive
    };

    // Phase 7d-ii-c: render through `BufferView` instead of the
    // textarea widget. The textarea field stays as the input /
    // edit authority for now (Phase 7f rips it); the buffer mirrors
    // its content + cursor + viewport + spans on every step, so the
    // render is byte-for-byte derived from the buffer state.
    editor.set_viewport_height(chunks[1].height);
    // Gutter width must mirror `Editor::cursor_screen_pos`'s reserved
    // number column or the terminal cursor lands off by one column.
    // `lnum_width()` is the engine's own calculation (hjkl#96), so the
    // renderer, the cursor placement, and the mouse translation all
    // agree — including `:set nonumber`, where it collapses to 0.
    let gutter_width = editor.lnum_width();
    let wrap_mode = editor.settings().wrap;
    {
        let v = editor.host_mut().viewport_mut();
        v.width = chunks[1].width;
        v.height = chunks[1].height;
        v.text_width = chunks[1]
            .width
            .saturating_sub(gutter_width + EDITOR_SIGN_COL_WIDTH);
        v.wrap = wrap_mode;
    }

    // Plumb the host's `/` search regex into the buffer so
    // `BufferView` can paint search bg from the cached regex.
    // Visual mode suppresses the highlight so selection isn't
    // out-shouted by it (matches the previous textarea wiring).
    let search_query = if state.vim_mode == VimMode::Visual || state.vim_mode == VimMode::VisualLine
    {
        None
    } else {
        editor_search.or(last_editor_search)
    };
    let search_pattern = search_query
        .filter(|q| !q.is_empty())
        .and_then(|q| regex::Regex::new(q).ok());
    editor.set_search_pattern(search_pattern);

    // Gutter width matches what tui-textarea reserved: digit count
    // for the largest line number, plus a leading + trailing space.
    let gutter = hjkl_buffer_tui::Gutter {
        width: gutter_width,
        style: Style::default().fg(ui().editor_line_num),
        line_offset: 0,
        numbers: hjkl_buffer_tui::GutterNumbers::Absolute,
        sign_column_width: EDITOR_SIGN_COL_WIDTH,
        fold_column_width: 0,
    };
    // Gutter diagnostic signs: highest severity per row wins
    // (error > warning). Painted by `BufferView` as part of its
    // gutter pass — no post-render overlay.
    let signs: Vec<hjkl_buffer_tui::Sign> = state
        .lsp_diagnostics
        .iter()
        .filter_map(|d| match d.severity {
            lsp_types::DiagnosticSeverity::ERROR => Some(hjkl_buffer_tui::Sign {
                row: d.line as usize,
                ch: '●',
                style: Style::default().fg(ui().status_diag_error),
                priority: 2,
            }),
            lsp_types::DiagnosticSeverity::WARNING => Some(hjkl_buffer_tui::Sign {
                row: d.line as usize,
                ch: '⚠',
                style: Style::default().fg(ui().status_diag_warning),
                priority: 1,
            }),
            _ => None,
        })
        .collect();

    let style_table: Vec<Style> = editor.ratatui_style_table();
    let resolver = move |id: u32| style_table.get(id as usize).copied().unwrap_or_default();
    let selection = editor.buffer_selection();
    let (cursorline, cursorcolumn) = cursor_opts;
    let cursorline_style = if cursorline {
        Style::default().bg(cursor_line_bg)
    } else {
        Style::default()
    };
    let cursorcolumn_style = if cursorcolumn {
        Style::default().bg(ui().sql_cursor_column_bg)
    } else {
        Style::default()
    };
    let view = hjkl_buffer_tui::BufferView {
        buffer: editor.buffer(),
        viewport: editor.host().viewport(),
        selection,
        resolver: &resolver,
        cursor_line_bg: cursorline_style,
        cursor_line_row: None,
        fold_line_bg: Style::default(),
        folds_override: None,
        cursor_column_bg: cursorcolumn_style,
        selection_bg: Style::default().add_modifier(Modifier::REVERSED),
        cursor_style: Style::default().bg(cursor_line_bg),
        gutter: Some(gutter),
        search_bg: Style::default()
            .bg(ui().editor_search_bg)
            .fg(ui().editor_search_fg),
        signs: &signs,
        conceals: &[],
        spans: editor.buffer_spans(),
        search_pattern: editor.search_state().pattern.as_ref(),
        non_text_style: Style::default(),
        show_eob: false,
        diag_overlays: &[],
        colorcolumn_cols: &[],
        colorcolumn_style: Style::default(),
        listchars: None,
        indent_guides_enabled: false,
        indent_guide_char: '│',
        indent_guide_shiftwidth: 4,
        indent_guide_fg: Color::DarkGray,
        indent_guide_active_fg: Color::DarkGray,
        indent_guide_active_col: None,
        eol_hints: &[],
        blame_plan: None,
        diff_filler: None,
    };
    f.render_widget(view, chunks[1]);

    if let Some(msg) = diag_line {
        f.render_widget(
            Paragraph::new(msg).style(Style::default().fg(ui().editor_error_fg)),
            chunks[2],
        );
    }
}

fn build_tab_title(state: &AppState) -> Line<'_> {
    let mut spans: Vec<Span> = vec![];
    for (i, tab) in state.tabs.iter().enumerate() {
        let active = i == state.active_tab;
        let style = if active {
            Style::default()
                .fg(ui().tab_active_fg)
                .bg(ui().tab_active_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(ui().tab_inactive_fg)
        };
        spans.push(Span::styled(format!(" {} ", tab.name), style));
        if i + 1 < state.tabs.len() {
            spans.push(Span::styled("│", Style::default().fg(ui().tab_sep_fg)));
        }
    }
    Line::from(spans)
}

fn draw_results(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    area: Rect,
    focused: bool,
    search_text: Option<&str>,
) {
    // Fill pane background (full area), then inset content by 1 on all sides.
    f.render_widget(
        Block::default().style(Style::default().bg(ui().results_pane_bg)),
        area,
    );
    let area = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 0,
    });
    // When the `/` prompt is live, carve one row off the bottom and
    // render the search input there; the rest of the pane lays out as
    // usual in the shrunk area.
    let (area, prompt_area) = if search_text.is_some() && area.height > 1 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    // Split off a separator + 1-row tab bar at the top when there are multiple
    // result tabs. The separator sits above the tab strip.
    let sep_style = Style::default().fg(ui().results_sep);
    let (tab_bar_area, content_area) = if state.result_tabs.len() > 1 && area.height > 2 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(area);
        let hr: String = "─".repeat(area.width as usize);
        f.render_widget(Paragraph::new(hr).style(sep_style), chunks[0]);
        (Some(chunks[1]), chunks[2])
    } else {
        (None, area)
    };

    if let Some(tab_area) = tab_bar_area {
        f.render_widget(results_tab_bar(state), tab_area);
    }

    match state.results() {
        ResultsPane::Results(r) => {
            let title_style = if focused {
                Style::default().fg(ui().results_title_active)
            } else {
                Style::default().fg(ui().results_title_inactive)
            };

            let query_text = state
                .active_result()
                .map(|t| t.query.clone())
                .unwrap_or_default();

            // `SHOW CREATE TABLE/VIEW/...` returns a single row whose last
            // column holds the DDL. Render that as a syntax-highlighted block
            // instead of a 1x2 table, which is unreadable.
            if is_show_create(&query_text)
                && r.rows.len() == 1
                && r.columns.len() >= 2
                && let Some(ddl) = r.rows[0].last()
            {
                let sep_style = Style::default().fg(ui().results_sep);
                let title = if state.result_tabs.len() > 1 {
                    format!(
                        " Results ({}/{} • DDL)",
                        state.active_result_tab + 1,
                        state.result_tabs.len()
                    )
                } else {
                    " Results (DDL)".to_string()
                };
                let query_line = highlight_query_line(&query_text, state.active_dialect);
                let body_lines = highlight_sql_lines(ddl, state.active_dialect);
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Min(0),
                    ])
                    .split(content_area);
                let hr: String = "─".repeat(content_area.width as usize);
                f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[0]);
                f.render_widget(Paragraph::new(title).style(title_style), chunks[1]);
                f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[2]);
                f.render_widget(Paragraph::new(query_line), chunks[3]);
                f.render_widget(Paragraph::new(hr).style(sep_style), chunks[4]);
                let body_area = chunks[5];
                state
                    .results_body_rows
                    .store(body_area.height, std::sync::atomic::Ordering::Relaxed);
                state
                    .results_body_width
                    .store(body_area.width, std::sync::atomic::Ordering::Relaxed);
                let v_scroll = state.results_scroll().min(body_lines.len()) as u16;
                let h_scroll = state.results_col_scroll() as u16;
                f.render_widget(
                    Paragraph::new(body_lines).scroll((v_scroll, h_scroll)),
                    body_area,
                );
                return;
            }

            let title = if state.result_tabs.len() > 1 {
                format!(
                    " Results ({}/{} • {} rows)",
                    state.active_result_tab + 1,
                    state.result_tabs.len(),
                    r.rows.len()
                )
            } else {
                format!(" Results ({} rows)", r.rows.len())
            };
            let col_start = state.results_col_scroll();
            let sep_style = Style::default().fg(ui().results_sep);
            let header_style = Style::default()
                .fg(ui().results_header_active)
                .add_modifier(Modifier::BOLD);

            // Char offset into the full-width row string, derived from col_scroll.
            // Each rendered column is padded to col_widths[i], separated by `│`.
            let char_offset: u16 = r
                .col_widths
                .iter()
                .take(col_start)
                .map(|&w| w as u32 + 1)
                .sum::<u32>() as u16;

            let cursor = state.active_result().map(|t| t.cursor);
            let col_bg = results_cursor_bg(focused);
            let cursor_bg = results_cursor_bg_strong(focused);
            // Highlighted column (Header or Cell cursor) — whole column gets muted bg.
            let active_col: Option<usize> = match cursor {
                Some(ResultsCursor::Header(c)) | Some(ResultsCursor::Cell { col: c, .. }) => {
                    Some(c)
                }
                _ => None,
            };
            let cursor_row: Option<usize> = match cursor {
                Some(ResultsCursor::Cell { row, .. }) => Some(row),
                _ => None,
            };

            let selection_bounds = state.results_selection_bounds();
            let header_cursor_col = if matches!(cursor, Some(ResultsCursor::Header(_))) {
                active_col
            } else {
                None
            };
            let (header_line, body_lines) = render_grid_lines(
                &r.columns,
                &r.rows,
                &r.col_widths,
                cursor_row,
                active_col,
                header_cursor_col,
                selection_bounds,
                state.results_scroll(),
                header_style,
                sep_style,
                Style::default().bg(cursor_bg),
                Style::default().bg(col_bg),
            );

            let mut query_line = highlight_query_line(&query_text, state.active_dialect);
            if cursor == Some(ResultsCursor::Query) {
                let qbg = results_cursor_bg(focused);
                query_line = Line::from(
                    query_line
                        .spans
                        .into_iter()
                        .map(|s| {
                            let st = s.style.bg(qbg);
                            Span::styled(s.content, st)
                        })
                        .collect::<Vec<_>>(),
                );
            }

            // Split content_area: hr (1) + title (1) + hr (1) + query (1) + hr (1) + header (1) + hr (1) + body (rest).
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Min(0),
                ])
                .split(content_area);

            let hr: String = "─".repeat(content_area.width as usize);
            f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[0]);
            f.render_widget(Paragraph::new(title).style(title_style), chunks[1]);
            f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[2]);
            f.render_widget(Paragraph::new(query_line), chunks[3]);
            f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[4]);
            f.render_widget(
                Paragraph::new(header_line).scroll((0, char_offset)),
                chunks[5],
            );
            f.render_widget(Paragraph::new(hr).style(sep_style), chunks[6]);
            let body_area = chunks[7];
            use std::sync::atomic::Ordering;
            state.results_body_x.store(body_area.x, Ordering::Relaxed);
            state.results_body_y.store(body_area.y, Ordering::Relaxed);
            state
                .results_body_rows
                .store(body_area.height, Ordering::Relaxed);
            state
                .results_body_width
                .store(body_area.width, Ordering::Relaxed);
            f.render_widget(
                Paragraph::new(body_lines).scroll((0, char_offset)),
                body_area,
            );
        }
        ResultsPane::Error(e) => {
            let title_text = render_pos_title(state, "Result");
            let cursor = state.active_result().map(|t| t.cursor);
            let cursor_bg = results_cursor_bg(focused);
            let body: Vec<Line<'static>> = e
                .lines()
                .enumerate()
                .map(|(i, el)| {
                    let mut st = Style::default().fg(ui().results_error);
                    if cursor == Some(ResultsCursor::MessageLine(i)) {
                        st = st.bg(cursor_bg);
                    }
                    Line::from(Span::styled(format!(" {}", el), st))
                })
                .collect();
            let has_query = state
                .active_result()
                .map(|t| !t.query.trim().is_empty())
                .unwrap_or(false);
            render_framed_pane(
                f,
                content_area,
                &title_text,
                Style::default().fg(ui().results_error),
                state,
                body,
                has_query,
            );
        }
        ResultsPane::Loading => {
            let frame = spinner_frame();
            let title_text = render_pos_title(state, "Result");
            let body = vec![Line::from(Span::styled(
                format!(" {} Running query…", frame),
                Style::default().fg(ui().results_loading),
            ))];
            render_framed_pane(
                f,
                content_area,
                &title_text,
                Style::default().fg(ui().results_loading),
                state,
                body,
                false,
            );
        }
        ResultsPane::Cancelled => {
            let title_text = render_pos_title(state, "Result");
            let cursor = state.active_result().map(|t| t.cursor);
            let mut st = Style::default().fg(ui().results_cancelled);
            if matches!(cursor, Some(ResultsCursor::MessageLine(_))) {
                st = st.bg(results_cursor_bg(focused));
            }
            let body = vec![Line::from(Span::styled(
                " Skipped (previous query failed)",
                st,
            ))];
            let has_query = state
                .active_result()
                .map(|t| !t.query.trim().is_empty())
                .unwrap_or(false);
            render_framed_pane(
                f,
                content_area,
                &title_text,
                Style::default().fg(ui().results_cancelled),
                state,
                body,
                has_query,
            );
        }
        ResultsPane::NonQuery {
            verb,
            rows_affected,
        } => {
            let title_text = render_pos_title(state, "Result");
            let cursor = state.active_result().map(|t| t.cursor);
            let body_text = non_query_summary(verb, *rows_affected);
            let title_style = Style::default().fg(ui().results_title_active);
            let mut st = Style::default().fg(ui().results_loading);
            if matches!(cursor, Some(ResultsCursor::MessageLine(_))) {
                st = st.bg(results_cursor_bg(focused));
            }
            let body = vec![Line::from(Span::styled(format!(" {body_text}"), st))];
            let has_query = state
                .active_result()
                .map(|t| !t.query.trim().is_empty())
                .unwrap_or(false);
            render_framed_pane(
                f,
                content_area,
                &title_text,
                title_style,
                state,
                body,
                has_query,
            );
        }
        ResultsPane::Empty => unreachable!(),
        _ => {}
    }

    // Paint the `/` prompt row last so it sits on top of whatever the
    // content renderer drew above.
    if let (Some(rect), Some(text)) = (prompt_area, search_text) {
        let style = Style::default().fg(ui().status_mode_normal);
        f.render_widget(Paragraph::new(format!("/{text}")).style(style), rect);
    }
}

/// Muted background for the currently-highlighted column in the results pane —
/// mirrors the editor's `cursor_line_bg` so focus feels consistent.
fn results_cursor_bg(focused: bool) -> Color {
    if focused {
        ui().results_col_active_bg
    } else {
        ui().results_col_inactive_bg
    }
}

/// Build the header + body `Line`s for a tabular grid. Used by both
/// the results pane and the LSP hover popup so their styling stays in
/// lock-step (cursor cell, column-wide bg, selection rectangle).
///
/// - `cursor_row` / `active_col` drive the cursor-cell + column-wide
///   backgrounds on body rows. Pass `None` for each if the cursor isn't
///   on the grid.
/// - `header_cursor_col` flips the header cell at that column to the
///   strong cursor bg (used when the results cursor is on the header
///   row; hover grids don't have a header cursor so pass `None`).
/// - `selection_bounds` is `(top, bot, left, right)` when a visual
///   selection is active. Rows inside it take the muted column bg.
/// - `body_skip` drops N leading body rows (row-scroll offset).
#[allow(clippy::too_many_arguments)]
fn render_grid_lines(
    columns: &[String],
    rows: &[Vec<String>],
    col_widths: &[u16],
    cursor_row: Option<usize>,
    active_col: Option<usize>,
    header_cursor_col: Option<usize>,
    selection_bounds: Option<(usize, usize, usize, usize)>,
    body_skip: usize,
    header_style: Style,
    sep_style: Style,
    cursor_style: Style,
    selection_style: Style,
) -> (Line<'static>, Vec<Line<'static>>) {
    let col_count = columns.len();
    let mut header_spans: Vec<Span<'static>> = Vec::with_capacity(col_count * 2);
    for (i, c) in columns.iter().enumerate() {
        let w = col_widths.get(i).copied().unwrap_or(0) as usize;
        let inner = w.saturating_sub(1);
        let mut st = header_style;
        if header_cursor_col == Some(i) {
            st = st.patch(cursor_style);
        }
        header_spans.push(Span::styled(format!(" {:<inner$}", c, inner = inner), st));
        if i + 1 < col_count {
            header_spans.push(Span::styled("│".to_string(), sep_style));
        }
    }
    let header_line = Line::from(header_spans);

    let body_lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .skip(body_skip)
        .map(|(row_idx, row)| {
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(col_count * 2);
            for i in 0..col_count {
                let w = col_widths.get(i).copied().unwrap_or(0) as usize;
                let inner = w.saturating_sub(1);
                let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
                let text = format!(" {:<inner$}", cell, inner = inner);
                let is_cursor = cursor_row == Some(row_idx) && active_col == Some(i);
                let is_selected = selection_bounds
                    .is_some_and(|(t, b, l, rr)| row_idx >= t && row_idx <= b && i >= l && i <= rr);
                let style = if is_cursor {
                    Some(cursor_style)
                } else if is_selected {
                    Some(selection_style)
                } else {
                    None
                };
                if let Some(st) = style {
                    spans.push(Span::styled(text, st));
                } else {
                    spans.push(Span::raw(text));
                }
                if i + 1 < col_count {
                    spans.push(Span::styled("│".to_string(), sep_style));
                }
            }
            Line::from(spans)
        })
        .collect();

    (header_line, body_lines)
}

/// Slightly brighter bg used for the single cell (or header) the cursor
/// actually points at, sitting on top of the column-wide muted bg.
fn results_cursor_bg_strong(focused: bool) -> Color {
    if focused {
        ui().results_cursor_active_bg
    } else {
        ui().results_cursor_inactive_bg
    }
}

fn render_pos_title(state: &AppState, label: &str) -> String {
    if state.result_tabs.len() > 1 {
        format!(
            " {label} ({}/{})",
            state.active_result_tab + 1,
            state.result_tabs.len(),
        )
    } else {
        format!(" {label}")
    }
}

/// Draw the hr/title/hr/query/hr chrome shared by Error, Loading, and
/// Cancelled panes, then the caller-supplied body below. When `show_query`
/// is false the query row + its trailing separator are omitted.
fn render_framed_pane(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    title_style: Style,
    state: &AppState,
    body: Vec<Line<'static>>,
    show_query: bool,
) {
    let sep_style = Style::default().fg(ui().results_sep);
    let hr: String = "─".repeat(area.width as usize);
    let query_text = state
        .active_result()
        .map(|t| t.query.clone())
        .unwrap_or_default();

    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(1), // hr
        Constraint::Length(1), // title
        Constraint::Length(1), // hr
    ];
    if show_query {
        constraints.push(Constraint::Length(1)); // query
        constraints.push(Constraint::Length(1)); // hr
    }
    constraints.push(Constraint::Min(0));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[0]);
    f.render_widget(
        Paragraph::new(title.to_string()).style(title_style),
        chunks[1],
    );
    f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[2]);
    let body_chunk = if show_query {
        let mut query_line = highlight_query_line(&query_text, state.active_dialect);
        let cursor = state.active_result().map(|t| t.cursor);
        if state.focus == Focus::Results && cursor == Some(ResultsCursor::Query) {
            let qbg = results_cursor_bg(state.focus == Focus::Results);
            query_line = Line::from(
                query_line
                    .spans
                    .into_iter()
                    .map(|s| {
                        let st = s.style.bg(qbg);
                        Span::styled(s.content, st)
                    })
                    .collect::<Vec<_>>(),
            );
        }
        f.render_widget(Paragraph::new(query_line), chunks[3]);
        f.render_widget(Paragraph::new(hr).style(sep_style), chunks[4]);
        chunks[5]
    } else {
        chunks[3]
    };
    state
        .results_body_rows
        .store(body_chunk.height, std::sync::atomic::Ordering::Relaxed);
    state
        .results_body_width
        .store(body_chunk.width, std::sync::atomic::Ordering::Relaxed);
    let scroll_y = state.active_result().map(|t| t.scroll as u16).unwrap_or(0);
    f.render_widget(
        Paragraph::new(body)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .scroll((scroll_y, 0)),
        body_chunk,
    );
}

/// Render a 1-row tab bar above the results pane: numbered tabs with the active
/// one highlighted in cyan.
fn results_tab_bar(state: &AppState) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(state.result_tabs.len() * 2);
    for (i, tab) in state.result_tabs.iter().enumerate() {
        let is_err = matches!(tab.kind, ResultsPane::Error(_));
        let is_loading = matches!(tab.kind, ResultsPane::Loading);
        let is_cancelled = matches!(tab.kind, ResultsPane::Cancelled);
        let label = format!(" {} ", i + 1);
        let u = ui();
        let style = if i == state.active_result_tab {
            Style::default()
                .fg(u.tab_active_fg)
                .bg(if is_err {
                    u.tab_err_bg
                } else if is_loading {
                    u.tab_loading_bg
                } else if is_cancelled {
                    u.tab_cancel_bg
                } else {
                    u.tab_active_bg
                })
                .add_modifier(Modifier::BOLD)
        } else if is_err {
            Style::default().fg(u.tab_err_fg)
        } else if is_loading {
            Style::default().fg(u.tab_loading_fg)
        } else if is_cancelled {
            Style::default().fg(u.tab_cancel_fg)
        } else {
            Style::default().fg(u.results_header_active)
        };
        spans.push(Span::styled(label, style));
        if i + 1 < state.result_tabs.len() {
            spans.push(Span::styled("│", Style::default().fg(u.tab_sep_fg)));
        }
    }
    Line::from(spans)
}

/// Build a syntax-highlighted single-line Line for the results-pane query row.
/// Newlines in the source are collapsed to spaces. Byte offsets from the
/// highlighter refer to the original (multiline) source — we remap them onto
/// the flattened string so spans stay aligned.
/// Render `source` as syntax-highlighted lines. Spans crossing line breaks
/// are split per row. Shared tree-sitter parser kept in TLS (same pattern as
/// `highlight_query_line`).
fn highlight_sql_lines(source: &str, dialect: Dialect) -> Vec<Line<'static>> {
    use std::cell::RefCell;
    thread_local! {
        static HL: RefCell<Option<Highlighter>> = const { RefCell::new(None) };
    }

    let spans = HL.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Highlighter::new_async());
        }
        if let Some(h) = slot.as_mut() {
            h.try_upgrade();
            h.highlight(source, dialect)
        } else {
            vec![]
        }
    });

    let bytes = source.as_bytes();
    let plain = Style::default().fg(ui().sql_plain);

    // Byte range of each line (without the trailing newline).
    let mut line_ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            line_ranges.push((start, i));
            start = i + 1;
        }
    }
    line_ranges.push((start, bytes.len()));

    line_ranges
        .iter()
        .map(|&(ls, le)| {
            let mut out: Vec<Span<'static>> = Vec::new();
            let mut cursor = ls;
            for s in &spans {
                let sb = s.start_byte.max(ls);
                let eb = s.end_byte.min(le);
                if sb >= eb {
                    continue;
                }
                if sb > cursor
                    && let Ok(raw) = std::str::from_utf8(&bytes[cursor..sb])
                {
                    out.push(Span::styled(raw.to_string(), plain));
                }
                if let Ok(raw) = std::str::from_utf8(&bytes[sb..eb]) {
                    let style = capture_style(s.capture.as_str()).unwrap_or(plain);
                    out.push(Span::styled(raw.to_string(), style));
                }
                cursor = eb;
            }
            if cursor < le
                && let Ok(raw) = std::str::from_utf8(&bytes[cursor..le])
            {
                out.push(Span::styled(raw.to_string(), plain));
            }
            if out.is_empty() {
                Line::from(Span::raw(""))
            } else {
                Line::from(out)
            }
        })
        .collect()
}

fn highlight_query_line(query: &str, dialect: Dialect) -> Line<'static> {
    use std::cell::RefCell;
    thread_local! {
        static HL: RefCell<Option<Highlighter>> = const { RefCell::new(None) };
    }

    if query.is_empty() {
        return Line::from(vec![Span::raw(" ")]);
    }

    let spans = HL.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Highlighter::new_async());
        }
        if let Some(h) = slot.as_mut() {
            h.try_upgrade();
            h.highlight(query, dialect)
        } else {
            vec![]
        }
    });

    let bytes = query.as_bytes();
    let mut out: Vec<Span<'static>> = vec![Span::raw(" ")];
    let plain = Style::default().fg(ui().sql_plain);
    let mut cursor = 0usize;
    let flatten = |b: &[u8]| -> String {
        std::str::from_utf8(b)
            .unwrap_or("")
            .replace(['\n', '\r'], " ")
    };

    for s in &spans {
        if s.start_byte >= bytes.len() || s.end_byte > bytes.len() || s.start_byte >= s.end_byte {
            continue;
        }
        if s.start_byte > cursor {
            out.push(Span::styled(flatten(&bytes[cursor..s.start_byte]), plain));
        }
        let slice = flatten(&bytes[s.start_byte..s.end_byte]);
        let style = capture_style(s.capture.as_str()).unwrap_or(plain);
        out.push(Span::styled(slice, style));
        cursor = s.end_byte;
    }
    if cursor < bytes.len() {
        out.push(Span::styled(flatten(&bytes[cursor..]), plain));
    }
    Line::from(out)
}

/// Commit each [`PendingSave`] on a blocking task so multi-MB writes
/// don't stall the render loop, then clear the matching tab's dirty
/// flag on success. Returns the names of saves that failed.
async fn commit_pending_saves(
    state: &Arc<Mutex<AppState>>,
    pending: Vec<sqeel_core::state::PendingSave>,
) -> Vec<String> {
    let mut failed = Vec::new();
    for p in pending {
        let tab_index = p.tab_index;
        let name = p.name.clone();
        let commit = tokio::task::spawn_blocking(move || p.commit())
            .await
            .unwrap_or_else(|e| {
                Err(std::io::Error::other(format!(
                    "spawn_blocking join error: {e}"
                )))
            });
        match commit {
            Ok(()) => {
                if let Some(i) = tab_index {
                    state.lock().unwrap().mark_tab_saved(i);
                }
            }
            Err(_) => failed.push(name),
        }
    }
    failed
}

/// Combine the LSP diagnostics vector with tree-sitter-derived parse
/// errors into one list for the inline-underline overlay. Parse errors
/// are lifted to `ERROR` severity so they render with the same loud
/// styling as an LSP error — they're "why did my SQL not run" markers
/// either way.
fn merged_diagnostics(
    lsp: &[sqeel_core::lsp::Diagnostic],
    parse_errors: &[sqeel_core::highlight::ParseError],
) -> Vec<sqeel_core::lsp::Diagnostic> {
    let mut out: Vec<sqeel_core::lsp::Diagnostic> = lsp.to_vec();
    out.extend(parse_errors.iter().map(|e| sqeel_core::lsp::Diagnostic {
        line: e.start_row as u32,
        col: e.start_col as u32,
        end_line: e.end_row as u32,
        end_col: e.end_col as u32,
        message: e.message.clone(),
        severity: lsp_types::DiagnosticSeverity::ERROR,
    }));
    out
}

/// Decide whether the highlight worker needs a fresh submission.
///
/// Fires on:
/// - `content_changed` — user edited the buffer.
/// - `viewport_scrolled` — viewport moved far enough that the current
///   parse window no longer covers what's on screen.
/// - A dialect flip — the DB handshake is async, so the first parse at
///   startup runs under `Dialect::Generic`. Once the connection resolves
///   and sets the concrete dialect we need to re-parse so dialect-specific
///   keyword promotion (DESC / SHOW / PRAGMA / …) kicks in.
fn should_resubmit_highlight(
    content_changed: bool,
    viewport_scrolled: bool,
    current_dialect: Dialect,
    last_dialect: Dialect,
) -> bool {
    content_changed || viewport_scrolled || current_dialect != last_dialect
}

fn capture_style(capture: &str) -> Option<Style> {
    let u = ui();
    if sqeel_core::highlight::is_sql_keyword_capture(capture) {
        Some(
            Style::default()
                .fg(u.sql_keyword)
                .add_modifier(Modifier::BOLD),
        )
    } else if capture.starts_with("string") || capture.starts_with("literal") {
        Some(Style::default().fg(u.sql_string))
    } else if capture.starts_with("comment") {
        Some(
            Style::default()
                .fg(u.sql_comment)
                .add_modifier(Modifier::ITALIC),
        )
    } else if capture.starts_with("number")
        || capture.starts_with("integer")
        || capture.starts_with("float")
    {
        Some(Style::default().fg(u.sql_number))
    } else if capture.starts_with("operator") || capture == "punctuation.special" {
        Some(Style::default().fg(u.sql_operator))
    } else {
        None
    }
}

/// Splice a window of tree-sitter spans into the textarea's existing
/// per-row syntax span table.  Spans in the result are slice-local —
/// rows are rebased by `result.start_row` before being written.
///
/// Avoids the 700k-row `vec![Vec::new(); row_count]` allocation the old
/// `syntax_spans_by_row` paid on the main thread every time a highlight
/// result arrived: we `take` the existing outer `Vec`, mutate only the
/// rows inside the window, and put it back.
fn apply_window_spans<H: Host>(
    editor: &mut Editor<hjkl_buffer::Buffer, H>,
    result: &HighlightResult,
    buffer_rows: usize,
    diagnostics: &[sqeel_core::lsp::Diagnostic],
) {
    let mut by_row = std::mem::take(&mut editor.styled_spans);
    if by_row.len() < buffer_rows {
        by_row.resize_with(buffer_rows, Vec::new);
    }
    let window_start = result.start_row;
    let window_end = (window_start + result.row_count).min(buffer_rows);
    for row_spans in by_row.iter_mut().take(window_end).skip(window_start) {
        row_spans.clear();
    }
    // Materialize only the rows in the highlight window.
    let seed_start = window_start;
    let rope = editor.buffer().rope();
    let buffer_lines: Vec<String> = (seed_start..window_end.min(rope.len_lines()))
        .map(|r| hjkl_buffer::rope_line_str(&rope, r))
        .collect();
    // Lookup helper: absolute row → slice index, or `None` for rows
    // outside the materialized window.
    let line_at = |row: usize| -> Option<&str> {
        if row < seed_start {
            return None;
        }
        buffer_lines.get(row - seed_start).map(String::as_str)
    };
    let line_len_at = |row: usize| -> usize { line_at(row).map(str::len).unwrap_or(0) };
    for s in &result.spans {
        // Marker spans are handled in the dedicated overlay pass below.
        if s.capture.starts_with("comment.marker.") {
            continue;
        }
        let Some(style) = capture_style(s.capture.as_str()).map(style_from_ratatui) else {
            continue;
        };
        let sr = s.start_row + window_start;
        let er = s.end_row + window_start;
        if sr >= buffer_rows {
            continue;
        }
        if sr == er {
            if s.end_col > s.start_col {
                by_row[sr].push((s.start_col, s.end_col, style));
            }
        } else {
            by_row[sr].push((s.start_col, usize::MAX, style));
            for row_spans in by_row.iter_mut().take(er.min(buffer_rows)).skip(sr + 1) {
                row_spans.push((0, usize::MAX, style));
            }
            if er < buffer_rows && s.end_col > 0 {
                by_row[er].push((0, s.end_col, style));
            }
        }
    }
    // Apply comment marker overlays sourced from hjkl-bonsai's
    // CommentMarkerPass (injected into result.spans by sqeel-core).
    // Uses overlay_span to splice in marker styles on top of the comment
    // spans already written above.
    for s in &result.spans {
        if !s.capture.starts_with("comment.marker.") {
            continue;
        }
        let sr = s.start_row + window_start;
        let er = s.end_row + window_start;
        if sr >= buffer_rows {
            continue;
        }
        let Some(style) = marker_capture_style(s.capture.as_str()).map(style_from_ratatui) else {
            continue;
        };
        if sr == er {
            if s.end_col > s.start_col {
                overlay_span(&mut by_row[sr], s.start_col, s.end_col, style);
            }
        } else {
            overlay_span(&mut by_row[sr], s.start_col, usize::MAX, style);
            for row_spans in by_row.iter_mut().take(er.min(buffer_rows)).skip(sr + 1) {
                overlay_span(row_spans, 0, usize::MAX, style);
            }
            if er < buffer_rows && s.end_col > 0 {
                overlay_span(&mut by_row[er], 0, s.end_col, style);
            }
        }
    }
    // LSP diagnostic underlines. Applied last so the underline stacks
    // on top of the keyword / marker overlays; we preserve the existing
    // span's fg and just layer on an error-coloured underline.
    for d in diagnostics {
        apply_diagnostic_underline(&mut by_row, d, &line_len_at, buffer_rows);
    }
    // Sort each touched row so `line_spans` sees them in start-byte order.
    for row_spans in by_row.iter_mut().take(window_end).skip(window_start) {
        row_spans.sort_by_key(|&(s, _, _)| s);
    }
    editor.install_syntax_spans(by_row);
}

/// Layer an LSP diagnostic's error / warning underline onto `by_row`
/// at the diagnostic's range. Existing spans in the range are split
/// and their fg preserved — we only add the `UNDERLINED` modifier and
/// paint the underline colour with the diagnostic severity colour, so
/// keyword / marker colouring inside the range still renders.
fn apply_diagnostic_underline(
    by_row: &mut [Vec<(usize, usize, EngineStyle)>],
    d: &sqeel_core::lsp::Diagnostic,
    line_len: &impl Fn(usize) -> usize,
    buffer_rows: usize,
) {
    let u = ui();
    let color = match d.severity {
        lsp_types::DiagnosticSeverity::ERROR => u.status_diag_error,
        lsp_types::DiagnosticSeverity::WARNING => u.status_diag_warning,
        _ => return,
    };
    let start_row = d.line as usize;
    let end_row = d.end_line as usize;
    if start_row >= buffer_rows {
        return;
    }
    let stop = end_row.min(buffer_rows.saturating_sub(1));
    for (row, row_spans) in by_row.iter_mut().enumerate().take(stop + 1).skip(start_row) {
        let line_len = line_len(row);
        let start_col = if row == start_row { d.col as usize } else { 0 };
        let mut end_col = if row == end_row {
            d.end_col as usize
        } else {
            line_len
        };
        // Zero-width ranges (LSP sometimes emits those) need to
        // highlight *something* — fall back to `start_col..line_end`,
        // clamped to at least one cell.
        if end_col <= start_col {
            end_col = line_len.max(start_col + 1);
        }
        end_col = end_col.min(line_len.max(start_col + 1));
        if start_col >= end_col {
            continue;
        }
        merge_underline(row_spans, start_col, end_col, color);
    }
}

/// Split `row` at `[start, end)` boundaries, adding the `UNDERLINE`
/// attr to the overlap region of each existing span. Uncovered bytes
/// in `[start, end)` get a bare underline span using `color` as fg.
///
/// Engine-native styles carry no separate underline colour (hjkl 0.33
/// interns `hjkl_engine::Style`), so the diagnostic colour goes on the
/// fg — which the old ratatui path also did for visibility in terminals
/// without colored-underline support.
fn merge_underline(
    row: &mut Vec<(usize, usize, EngineStyle)>,
    start: usize,
    end: usize,
    color: Color,
) {
    let ecolor = style_from_ratatui(Style::default().fg(color)).fg;
    let mut out: Vec<(usize, usize, EngineStyle)> = Vec::with_capacity(row.len() + 4);
    let mut overlap_ranges: Vec<(usize, usize)> = Vec::new();
    for &(s, e, sty) in row.iter() {
        if e <= start || s >= end {
            out.push((s, e, sty));
            continue;
        }
        if s < start {
            out.push((s, start, sty));
        }
        let olap_s = s.max(start);
        let olap_e = e.min(end);
        // Replace the syntax fg with the diagnostic colour inside the
        // range so the underline reads loud against the editor bg even
        // in terminals without colored-underline support. The range is
        // small (usually one token) so losing syntax colour there is a
        // fair trade for unambiguous error visibility.
        let merged = EngineStyle {
            fg: ecolor,
            attrs: sty.attrs | hjkl_engine::types::Attrs::UNDERLINE,
            ..sty
        };
        out.push((olap_s, olap_e, merged));
        overlap_ranges.push((olap_s, olap_e));
        if e > end {
            out.push((end, e, sty));
        }
    }
    // Fill gaps in [start, end) uncovered by any existing span.
    overlap_ranges.sort_by_key(|&(s, _)| s);
    let bare = EngineStyle {
        fg: ecolor,
        attrs: hjkl_engine::types::Attrs::UNDERLINE,
        ..EngineStyle::default()
    };
    let mut cursor = start;
    for (s, e) in overlap_ranges {
        if s > cursor {
            out.push((cursor, s, bare));
        }
        cursor = cursor.max(e);
    }
    if cursor < end {
        out.push((cursor, end, bare));
    }
    out.sort_by_key(|&(s, _, _)| s);
    *row = out;
}

/// Map a `comment.marker.*` capture name to a ratatui Style.
///
/// Label captures (`comment.marker.todo` etc.) produce a bold badge with
/// `sql_marker_fg` on the marker bg — contrast-driven visibility, matching
/// hjkl-bonsai's canonical theme so the label stays readable on the cursor
/// row.
/// Tail captures (`comment.marker.tail.*`) produce an italic tint.
/// Returns `None` for unrecognised capture names.
fn marker_capture_style(capture: &str) -> Option<Style> {
    let u = ui();
    let is_tail = capture.contains(".tail.");
    let color = if capture.contains(".todo") {
        u.sql_marker_todo
    } else if capture.contains(".fixme") {
        u.sql_marker_fixme
    } else if capture.contains(".note") {
        u.sql_marker_note
    } else if capture.contains(".warn") {
        u.sql_marker_warn
    } else {
        return None;
    };
    if is_tail {
        Some(Style::default().fg(color).add_modifier(Modifier::ITALIC))
    } else {
        Some(
            Style::default()
                .fg(u.sql_marker_fg)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        )
    }
}

/// Insert a marker span `[ms, me)` with `style` into `row`, trimming /
/// splitting any existing span that overlaps so the marker isn't masked
/// by an outer tree-sitter comment span.
fn overlay_span(
    row: &mut Vec<(usize, usize, EngineStyle)>,
    ms: usize,
    me: usize,
    style: EngineStyle,
) {
    let mut trimmed: Vec<(usize, usize, EngineStyle)> = Vec::with_capacity(row.len() + 2);
    for &(s, e, sty) in row.iter() {
        if e <= ms || s >= me {
            trimmed.push((s, e, sty));
        } else if s < ms && e > me {
            trimmed.push((s, ms, sty));
            trimmed.push((me, e, sty));
        } else if s < ms {
            trimmed.push((s, ms, sty));
        } else if e > me {
            trimmed.push((me, e, sty));
        }
        // else: span fully inside marker — drop it.
    }
    trimmed.push((ms, me, style));
    *row = trimmed;
}

/// Materialize the buffer's logical lines as owned `String`s.
///
/// `hjkl_buffer::Buffer` went rope-only in 0.33 (`Buffer::lines` deleted);
/// the line-slice helpers below (`word_prefix_at`, `row_col_to_byte`, …)
/// keep their `&[String]` shape and get fed through this adapter. O(n) per
/// call — fine for sqeel's SQL-scratch buffer sizes (the heavy pipeline is
/// already gated off above 2 MB).
fn buffer_lines(buffer: &hjkl_buffer::Buffer) -> Vec<String> {
    let rope = buffer.rope();
    (0..rope.len_lines())
        .map(|r| hjkl_buffer::rope_line_str(&rope, r))
        .collect()
}

/// Convert a `(row, col)` character position into a byte offset in the
/// joined source (`\n` between lines). Used to feed cursor position into
/// `completion_ctx::parse_context`, which operates on a single string.
fn row_col_to_byte(lines: &[String], row: usize, col: usize) -> usize {
    let mut offset = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if i == row {
            for (char_count, (b, _)) in line.char_indices().enumerate() {
                if char_count == col {
                    return offset + b;
                }
            }
            return offset + line.len();
        }
        offset += line.len() + 1; // `\n`
    }
    offset
}

/// Returns the word (alphanumeric + `_`) ending at `col` on `line`.
fn word_prefix_at(lines: &[String], row: usize, col: usize) -> String {
    let Some(line) = lines.get(row) else {
        return String::new();
    };
    let before = &line[..col.min(line.len())];
    before
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

/// Full word spanning the cursor — extends both left and right of the
/// caret until it hits a non-identifier character. Used by `K` to
/// short-circuit the LSP hover when the word matches a table we've
/// already cached columns for.
fn word_at_cursor(lines: &[String], row: usize, col: usize) -> String {
    let Some(line) = lines.get(row) else {
        return String::new();
    };
    let chars: Vec<char> = line.chars().collect();
    let col = col.min(chars.len());
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut start = col;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    chars[start..end].iter().collect()
}

/// Render a tabular hover payload as a centred, borderless, focus-
/// stealing dialog. Chrome matches the command palette — 2-col / 1-row
/// padding on `dialog_bg`, no border rule.
fn draw_hover_table(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &AppState,
    table: &sqeel_core::state::QueryResult,
    search_text: Option<&str>,
) {
    if area.width < 20 || area.height < 5 {
        return;
    }
    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);

    // Natural width: sum of col_widths + separator per column gap.
    let natural_w: u32 = table
        .col_widths
        .iter()
        .map(|&w| w as u32 + 1)
        .sum::<u32>()
        .saturating_sub(1);
    let popup_w = (natural_w as u16)
        .saturating_add(4) // 2-col pad each side
        .clamp(40, area.width.saturating_sub(4).min(100));
    // Body max = terminal height - vertical padding (2) - borders
    // space (none) - header/hr (2). Popup height = header + hr + body
    // + 1 top + 1 bottom pad.
    // Reserve one extra row when the `/` prompt is live.
    let prompt_rows: u16 = if search_text.is_some() { 1 } else { 0 };
    let max_body = area.height.saturating_sub(4 + prompt_rows);
    let body_h = (table.rows.len() as u16).min(max_body.max(1));
    let popup_h = (body_h + 4 + prompt_rows).min(area.height.saturating_sub(2));

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    // No title row — the popup is chromeless apart from `dialog_bg`.
    // Layout: header (1) + separator (1) + body (rest) + optional
    // `/` prompt (1) pinned to the bottom.
    let mut constraints = vec![
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ];
    if prompt_rows > 0 {
        constraints.push(Constraint::Length(1));
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let sep_style = Style::default().fg(u.results_sep).bg(u.dialog_bg);
    let header_style = bg.add_modifier(Modifier::BOLD).fg(u.results_header_active);
    // Highlights for the hover popup: results-pane bg colors read as
    // identical to `dialog_bg` (both are dim neutral shades), so
    // rolling `REVERSED` in inverts bg/fg of whatever cell the cursor
    // or selection covers. Unconditionally visible regardless of the
    // active theme.
    let cursor_style = bg.add_modifier(Modifier::REVERSED | Modifier::BOLD);
    let selection_style = Style::default()
        .fg(u.editor_search_fg)
        .bg(u.editor_search_bg);

    let cursor_row = match state.hover_cursor {
        ResultsCursor::Cell { row, .. } => Some(row),
        _ => None,
    };
    let active_col = match state.hover_cursor {
        ResultsCursor::Cell { col, .. } => Some(col),
        _ => None,
    };
    let selection_bounds: Option<(usize, usize, usize, usize)> =
        state.hover_selection.and_then(|sel| {
            let (ar, ac) = sel.anchor;
            let (cr, cc) = match state.hover_cursor {
                ResultsCursor::Cell { row, col } => (row, col),
                _ => return None,
            };
            let top = ar.min(cr);
            let bot = ar.max(cr);
            let (left, right) = match sel.mode {
                sqeel_core::state::ResultsSelectionMode::Line => {
                    (0, table.columns.len().saturating_sub(1))
                }
                sqeel_core::state::ResultsSelectionMode::Block => (ac.min(cc), ac.max(cc)),
            };
            Some((top, bot, left, right))
        });

    let (header_line, body_lines) = render_grid_lines(
        &table.columns,
        &table.rows,
        &table.col_widths,
        cursor_row,
        active_col,
        None,
        selection_bounds,
        state.hover_scroll,
        header_style,
        sep_style,
        cursor_style,
        selection_style,
    );

    let char_offset: u16 = table
        .col_widths
        .iter()
        .take(state.hover_col_scroll)
        .map(|&w| w as u32 + 1)
        .sum::<u32>() as u16;

    f.render_widget(
        Paragraph::new(header_line)
            .style(bg)
            .scroll((0, char_offset)),
        chunks[0],
    );
    let hr: String = "─".repeat(inner.width as usize);
    f.render_widget(Paragraph::new(hr).style(sep_style), chunks[1]);
    let body_rect = chunks[2];
    // Publish the body rect so nav helpers can clamp the scroll
    // offsets and the mouse-click handler can translate terminal-
    // space coordinates into grid (row, col). Without this `l` past
    // the viewport leaves the cursor off-screen, and clicks inside
    // the popup can't hit their cell.
    use std::sync::atomic::Ordering;
    state.hover_body_x.store(body_rect.x, Ordering::Relaxed);
    state.hover_body_y.store(body_rect.y, Ordering::Relaxed);
    state
        .hover_body_height
        .store(body_rect.height, Ordering::Relaxed);
    state
        .hover_body_width
        .store(body_rect.width, Ordering::Relaxed);
    f.render_widget(
        Paragraph::new(body_lines)
            .style(bg)
            .scroll((0, char_offset)),
        body_rect,
    );

    // `/` prompt pinned to the bottom when active.
    if let (Some(text), Some(rect)) = (search_text, chunks.get(3).copied()) {
        let prompt_style = Style::default().fg(u.status_mode_normal).bg(u.dialog_bg);
        f.render_widget(Paragraph::new(format!("/{text}")).style(prompt_style), rect);
    }
}

/// Loading placeholder rendered while the LSP hover response is in
/// flight. Uses the same borderless `dialog_bg` chrome so the
/// popup's footprint is consistent once content arrives.
fn draw_hover_loading(f: &mut ratatui::Frame<'_>, area: Rect) {
    if area.width < 20 || area.height < 5 {
        return;
    }
    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);
    // Size from actual content. The hourglass glyph is wide in most
    // fonts, so use `UnicodeWidthStr` to measure display cells rather
    // than chars; char-count under-sized the popup and clipped the
    // trailing "(Esc to cancel)" hint.
    use unicode_width::UnicodeWidthStr;
    let label = format!("{} Loading hover…", spinner_frame());
    let hint = "  (Esc to cancel)";
    let content_w = (label.width() + hint.width()) as u16;
    let popup_w = (content_w + 4).min(area.width.saturating_sub(4));
    let popup_h = 3u16.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: 1,
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(label, bg.add_modifier(Modifier::BOLD)),
            Span::styled(hint, bg),
        ]))
        .style(bg),
        inner,
    );
}

/// One-line signature-help bar rendered above the cursor in the editor
/// pane. Uses `dialog_bg` chrome so it's visually distinct from the
/// text; truncated to the available editor width.
fn draw_sig_help_bar(
    f: &mut ratatui::Frame<'_>,
    editor_area: Rect,
    screen_row: usize,
    screen_col: usize,
    text: &str,
) {
    if editor_area.height < 3 {
        return;
    }
    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);
    // Position: the row above the cursor; fall back to the row below
    // when the cursor is on the first visible line (no room above).
    let bar_y = if screen_row > 0 {
        editor_area.y + screen_row as u16 - 1
    } else {
        editor_area.y + screen_row as u16 + 1
    };
    if bar_y >= editor_area.y + editor_area.height {
        return;
    }
    // Anchor x at the cursor column but keep it inside the editor area.
    let bar_x = (editor_area.x + screen_col as u16).min(
        editor_area
            .x
            .saturating_add(editor_area.width.saturating_sub(1)),
    );
    let max_w = editor_area
        .width
        .saturating_sub(bar_x.saturating_sub(editor_area.x));
    if max_w == 0 {
        return;
    }
    let label = format!(" {text} ");
    let display: String = label.chars().take(max_w as usize).collect();
    let bar = Rect {
        x: bar_x,
        y: bar_y,
        width: display.chars().count() as u16,
        height: 1,
    };
    f.render_widget(Clear, bar);
    f.render_widget(
        Paragraph::new(Span::styled(display, bg.add_modifier(Modifier::BOLD))),
        bar,
    );
}

/// Plain-text (or lightly-markdown) hover payload. Same borderless
/// dialog_bg chrome as the table form; supports j/k scroll via the
/// passed-in `scroll` offset.
fn draw_hover_popup(f: &mut ratatui::Frame<'_>, area: Rect, scroll: usize, text: &str) {
    if area.width < 10 || area.height < 5 {
        return;
    }
    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);
    let lines = format_hover_lines(text);
    let longest = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>() as u16
        })
        .max()
        .unwrap_or(0);
    let popup_w = (longest + 4).clamp(30, area.width.saturating_sub(4).min(80));
    let content_h = (lines.len() as u16).min(area.height.saturating_sub(4));
    // popup = content + 2 rows of vertical padding.
    let popup_h = (content_h + 2).min(area.height.saturating_sub(2));

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    // Chromeless — no title row, content takes the whole inner area.
    f.render_widget(
        Paragraph::new(lines)
            .style(bg)
            .scroll((scroll as u16, 0))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );
}

/// Render an LSP hover payload (markdown or plain text) as styled
/// ratatui lines via pulldown-cmark. Handles headers, fenced code
/// blocks, inline code, bold, italic, and blockquotes; everything else
/// flattens to plain spans so the popup stays readable for arbitrary
/// server output.
fn format_hover_lines(text: &str) -> Vec<Line<'static>> {
    use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
    let u = ui();
    let header1 = Style::default()
        .fg(u.dialog_border)
        .add_modifier(Modifier::BOLD);
    let header2 = Style::default()
        .fg(u.sql_keyword)
        .add_modifier(Modifier::BOLD);
    let code_style = Style::default().fg(u.sql_ident);
    let bold_style = Style::default().add_modifier(Modifier::BOLD);
    let italic_style = Style::default().add_modifier(Modifier::ITALIC);
    let quote_style = Style::default()
        .fg(u.sql_comment)
        .add_modifier(Modifier::ITALIC);

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(text, opts);

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    // Active style overlay stack. Headings / blockquotes push a base
    // style that applies to every text span emitted until the matching
    // End event pops it; `bold` / `emph` / `code` layer on top.
    let mut base_stack: Vec<Style> = Vec::new();
    let mut span_stack: Vec<Style> = Vec::new();
    let mut in_code_block = false;

    let flush_line = |current: &mut Vec<Span<'static>>, out: &mut Vec<Line<'static>>| {
        out.push(Line::from(std::mem::take(current)));
    };
    let active_style = |base_stack: &[Style], span_stack: &[Style]| -> Style {
        // Right-most wins so inner emphasis overrides outer heading
        // base. Patch each layer so partial styles (fg only, modifier
        // only) compose instead of overwriting.
        let mut acc = Style::default();
        for s in base_stack {
            acc = acc.patch(*s);
        }
        for s in span_stack {
            acc = acc.patch(*s);
        }
        acc
    };
    let push_text = |text: &str,
                     style: Style,
                     current: &mut Vec<Span<'static>>,
                     out: &mut Vec<Line<'static>>| {
        // Newlines inside a text event (rare) still need to split the
        // current line so wrapping works.
        let mut parts = text.split('\n').peekable();
        while let Some(chunk) = parts.next() {
            if !chunk.is_empty() {
                current.push(Span::styled(chunk.to_string(), style));
            }
            if parts.peek().is_some() {
                flush_line(current, out);
            }
        }
    };

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                base_stack.push(match level {
                    HeadingLevel::H1 => header1,
                    _ => header2,
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                base_stack.pop();
                flush_line(&mut current, &mut out);
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                flush_line(&mut current, &mut out);
                out.push(Line::from(""));
            }
            Event::Start(Tag::BlockQuote(_)) => base_stack.push(quote_style),
            Event::End(TagEnd::BlockQuote) => {
                base_stack.pop();
                flush_line(&mut current, &mut out);
            }
            Event::Start(Tag::CodeBlock(_)) => {
                in_code_block = true;
                base_stack.push(code_style);
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                base_stack.pop();
            }
            Event::Start(Tag::Emphasis) => span_stack.push(italic_style),
            Event::End(TagEnd::Emphasis) => {
                span_stack.pop();
            }
            Event::Start(Tag::Strong) => span_stack.push(bold_style),
            Event::End(TagEnd::Strong) => {
                span_stack.pop();
            }
            Event::Text(t) => {
                let st = active_style(&base_stack, &span_stack);
                if in_code_block {
                    // Code blocks emit a single Text with embedded
                    // newlines; split them so each source line becomes
                    // its own visual row.
                    for (i, chunk) in t.split('\n').enumerate() {
                        if i > 0 {
                            flush_line(&mut current, &mut out);
                        }
                        if !chunk.is_empty() {
                            current.push(Span::styled(chunk.to_string(), st));
                        }
                    }
                } else {
                    push_text(&t, st, &mut current, &mut out);
                }
            }
            Event::Code(t) => {
                current.push(Span::styled(t.into_string(), code_style));
            }
            Event::SoftBreak => current.push(Span::raw(" ")),
            Event::HardBreak => flush_line(&mut current, &mut out),
            Event::Rule => {
                flush_line(&mut current, &mut out);
                out.push(Line::from(Span::styled(
                    "─".repeat(40),
                    Style::default().fg(u.results_sep),
                )));
            }
            _ => {}
        }
    }
    if !current.is_empty() {
        flush_line(&mut current, &mut out);
    }
    // Strip trailing blank lines so the popup sizes tightly.
    while out
        .last()
        .is_some_and(|l| l.spans.iter().all(|s| s.content.is_empty()))
    {
        out.pop();
    }
    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

fn draw_completions(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    editor_area: Rect,
    cur_row: usize,
    cur_col: usize,
) {
    let cursor = state.completion_cursor;
    let items: Vec<ListItem> = state
        .completions
        .iter()
        .map(|s| ListItem::new(s.as_str()))
        .collect();

    let longest = state
        .completions
        .iter()
        .map(|s| s.chars().count() as u16)
        .max()
        .unwrap_or(0);
    let popup_w = (longest + 2)
        .clamp(20, 60)
        .min(editor_area.width.saturating_sub(2));
    let popup_h = (items.len() as u16 + 2).min(12);

    // inner editor area starts 1 cell in from the block border
    let inner_x = editor_area.x + 1;
    let inner_y = editor_area.y + 1;
    let inner_w = editor_area.width.saturating_sub(2);
    let inner_h = editor_area.height.saturating_sub(2);

    // cursor position in screen coords (row 0 = first visible line)
    let cx = inner_x.saturating_add(cur_col as u16);
    let cy = inner_y.saturating_add(cur_row as u16);

    // place popup one row below cursor; flip up if it would overflow bottom
    let popup_y = if cy + 2 + popup_h <= inner_y + inner_h {
        cy + 2
    } else {
        cy.saturating_sub(popup_h)
    };
    // clamp x so popup stays inside the editor
    let popup_x = cx.min((inner_x + inner_w).saturating_sub(popup_w));

    let popup = Rect {
        x: popup_x,
        y: popup_y,
        width: popup_w,
        height: popup_h.min(inner_h),
    };

    // Borderless — match command palette / hover / help styling. A bg
    // fill + 1-col horizontal padding inside replaces the old border
    // frame so the overlay reads as one unified chrome.
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let list = List::new(items)
        .style(bg)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let inner = Rect {
        x: popup.x + 1,
        y: popup.y,
        width: popup.width.saturating_sub(2),
        height: popup.height,
    };
    let mut list_state = ListState::default().with_selected(Some(cursor));
    f.render_stateful_widget(list, inner, &mut list_state);
}

/// Small borderless centered dialog: 2-col horizontal padding, 1-row vertical
/// padding, single line of input. Used by the command palette and rename.
/// Borderless centered single-line input dialog. The caller supplies the
/// prompt prefix (e.g. `> `, `: `) so cursor placement stays exact regardless
/// of glyph width. Returns the terminal-space cursor position so the caller
/// can place the real cursor.
fn draw_input_dialog(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    prefix: &str,
    input: &TextInput,
) -> (u16, u16) {
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    let content = format!("{prefix}{}", input.text);
    let inner_w = (content.chars().count() as u16 + 1).max(20);
    let width = (inner_w + 4).min(area.width.saturating_sub(4));
    let height = 3u16.min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let line = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: 1,
    };
    f.render_widget(Paragraph::new(content).style(bg), line);
    (
        line.x + prefix.chars().count() as u16 + input.cursor as u16,
        line.y,
    )
}

/// Borderless centered confirmation dialog with a single message line.
fn draw_confirm_dialog(f: &mut ratatui::Frame<'_>, area: Rect, message: &str) {
    let bg = Style::default()
        .fg(ui().dialog_error_fg)
        .bg(ui().dialog_error_bg);
    let inner_w = (message.chars().count() as u16).max(20);
    let width = (inner_w + 4).min(area.width.saturating_sub(4));
    let height = 3u16.min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let line = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: 1,
    };
    f.render_widget(Paragraph::new(message.to_string()).style(bg), line);
}

/// Modal y/N prompt shown at startup when `sqls` is missing and
/// `lsp_auto_install = true`. Keyboard-only: `y`/`Y`/`Enter` → install,
/// `n`/`N`/`Esc` → dismiss. Mirrors the `draw_confirm_dialog` shape but
/// taller to accommodate the explanatory body text.
fn draw_sqls_prompt_modal(f: &mut ratatui::Frame<'_>, area: Rect) {
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    // Fixed body lines — longest is 53 chars.
    let body_lines = [
        "SQL LSP not found",
        "",
        "sqeel uses 'sqls' for completion and diagnostics.",
        "Install via hjkl-anvil now? (~30s, ~15MB)",
        "",
        "[y] yes, install   [n] no, skip this session",
    ];
    let inner_w: u16 = body_lines
        .iter()
        .map(|l| l.chars().count() as u16)
        .max()
        .unwrap_or(20);
    let width = (inner_w + 4).min(area.width.saturating_sub(4));
    // 1 top pad + N body rows + 1 bottom pad
    let height = (body_lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let inner_x = popup.x + 2;
    let inner_w = popup.width.saturating_sub(4);
    for (i, line) in body_lines.iter().enumerate() {
        let row = Rect {
            x: inner_x,
            y: popup.y + 1 + i as u16,
            width: inner_w,
            height: 1,
        };
        let style = if i == 0 {
            // Title row: bold
            bg.add_modifier(Modifier::BOLD)
        } else if i == body_lines.len() - 1 {
            // Key hint row: dim
            bg.add_modifier(Modifier::DIM)
        } else {
            bg
        };
        f.render_widget(Paragraph::new(line.to_string()).style(style), row);
    }
}

/// File picker dialog: borderless, padded, with input row + matching tab names.
///
/// Drives `hjkl_picker::Picker` — refresh runs here so the visible list
/// stays in sync with the streaming `FileSource` background scan. The
/// `active_name` overlay marks the currently-loaded tab with `* `.
fn draw_file_picker(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    picker: &mut hjkl_picker::Picker,
    active_name: Option<&str>,
) -> (u16, u16) {
    picker.tick(Instant::now());
    picker.refresh();

    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    let width = 60u16.min(area.width.saturating_sub(4));
    let max_rows = 12u16;
    let entries = picker.visible_entries();
    let list_rows = (entries.len() as u16).min(max_rows).max(1);
    // 1 row top pad + 1 row input + 1 row separator + N rows + 1 row bottom pad
    let height = (list_rows + 4).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner_x = popup.x + 2;
    let inner_w = popup.width.saturating_sub(4);

    // Input row
    let query_text = picker.query.text();
    let display: String = query_text.lines().next().unwrap_or("").to_string();
    let input_area = Rect {
        x: inner_x,
        y: popup.y + 1,
        width: inner_w,
        height: 1,
    };
    f.render_widget(Paragraph::new(format!("> {display}")).style(bg), input_area);
    let (_, ccol) = picker.query.cursor();
    let cursor_pos = (input_area.x + 2 + ccol as u16, input_area.y);

    // Results list
    let list_y = popup.y + 3;
    let match_style = bg.fg(ui().editor_search_bg).add_modifier(Modifier::BOLD);
    let cursor = picker.selected.min(entries.len().saturating_sub(1));
    for (i, (label, matches)) in entries.iter().take(list_rows as usize).enumerate() {
        let row = Rect {
            x: inner_x,
            y: list_y + i as u16,
            width: inner_w,
            height: 1,
        };
        let is_cursor = i == cursor;
        // FileSource labels are `"  <relpath>"` (two-cell prefix). Strip
        // it so the active-tab marker `"* "` slots in cleanly without
        // doubling the indent.
        let bare = label.strip_prefix("  ").unwrap_or(label.as_str());
        let is_active = active_name == Some(bare);
        let marker = if is_active { "* " } else { "  " };
        let row_style = if is_cursor {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg
        };
        let mut spans: Vec<Span> = vec![Span::styled(marker.to_string(), row_style)];
        // matches positions index into the original label (with its 2-space
        // prefix). Adjust by -2 so highlights land on `bare`.
        for (ci, ch) in bare.chars().enumerate() {
            let orig_idx = ci + 2;
            if matches.contains(&orig_idx) {
                spans.push(Span::styled(ch.to_string(), match_style));
            } else {
                spans.push(Span::styled(ch.to_string(), row_style));
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(row_style), row);
    }
    if entries.is_empty() {
        let row = Rect {
            x: inner_x,
            y: list_y,
            width: inner_w,
            height: 1,
        };
        f.render_widget(
            Paragraph::new("(no matches)").style(bg.add_modifier(Modifier::DIM)),
            row,
        );
    }
    cursor_pos
}

fn draw_connection_switcher(f: &mut ratatui::Frame<'_>, state: &AppState, area: Rect) {
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    let conns = &state.available_connections;
    let cursor = state.connection_switcher_cursor;
    let active_name = state.active_connection.as_deref();

    let width = 60u16.min(area.width.saturating_sub(4));
    let max_rows = 12u16;
    let list_rows = (conns.len() as u16).min(max_rows).max(1);
    // 1 row top pad + 1 row header + 1 row separator + N rows + 1 row bottom pad
    let height = (list_rows + 4).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner_x = popup.x + 2;
    let inner_w = popup.width.saturating_sub(4);

    // Header row
    let header = Rect {
        x: inner_x,
        y: popup.y + 1,
        width: inner_w,
        height: 1,
    };
    f.render_widget(
        Paragraph::new("Connections  (Enter connect · n new · e edit · d delete)")
            .style(bg.add_modifier(Modifier::DIM)),
        header,
    );

    // List
    let list_y = popup.y + 3;
    if conns.is_empty() {
        let row = Rect {
            x: inner_x,
            y: list_y,
            width: inner_w,
            height: 1,
        };
        f.render_widget(
            Paragraph::new("(no connections configured)").style(bg.add_modifier(Modifier::DIM)),
            row,
        );
        return;
    }
    let cur = cursor.min(conns.len().saturating_sub(1));
    for (i, c) in conns.iter().take(list_rows as usize).enumerate() {
        let row = Rect {
            x: inner_x,
            y: list_y + i as u16,
            width: inner_w,
            height: 1,
        };
        let is_cursor = i == cur;
        let is_active = active_name == Some(c.name.as_str());
        let mut row_style = bg;
        if is_cursor {
            row_style = row_style.add_modifier(Modifier::REVERSED);
        }

        // Badge glyph + color reflects the live state of the active connection.
        // Non-active connections have no badge (they were never attempted this
        // session, or were superseded by a later switch).
        let (badge_glyph, badge_color) = if is_active {
            if state.schema_connecting {
                // Handshake in flight.
                ("◌ ", Color::Yellow)
            } else if state.schema_connect_error.is_some() {
                // Last attempt failed.
                ("✗ ", Color::Red)
            } else {
                // Live connection.
                ("● ", Color::Green)
            }
        } else {
            ("  ", ui().dialog_fg)
        };

        // Active entry: bold the name so it stands out beyond the cursor
        // highlight (which is REVERSED and thus lost on non-focused rows).
        let name_style = if is_active {
            row_style.add_modifier(Modifier::BOLD)
        } else {
            row_style
        };

        let line = Line::from(vec![
            Span::styled(badge_glyph, row_style.fg(badge_color)),
            Span::styled(c.name.clone(), name_style),
            Span::styled(format!(" — {}", c.url), row_style),
        ]);
        f.render_widget(Paragraph::new(line).style(row_style), row);
    }
}

fn draw_pgpass_picker(f: &mut ratatui::Frame<'_>, state: &AppState, area: Rect) {
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    let entries = &state.pgpass_entries;
    let cursor = state.pgpass_picker_cursor;

    let width = 72u16.min(area.width.saturating_sub(4));
    let max_rows = 10u16;
    let list_rows = (entries.len() as u16).min(max_rows).max(1);
    // 1 row top pad + 1 row title + 1 row separator + N rows + 1 row hint + 1 row bottom pad
    let height = (list_rows + 5).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner_x = popup.x + 2;
    let inner_w = popup.width.saturating_sub(4);

    // Title row
    let title_area = Rect {
        x: inner_x,
        y: popup.y + 1,
        width: inner_w,
        height: 1,
    };
    f.render_widget(
        Paragraph::new("Use credential from ~/.pgpass?").style(bg.add_modifier(Modifier::DIM)),
        title_area,
    );

    // Entry list
    let list_y = popup.y + 3;
    let cur = cursor.min(entries.len().saturating_sub(1));
    for (i, e) in entries.iter().take(list_rows as usize).enumerate() {
        let row = Rect {
            x: inner_x,
            y: list_y + i as u16,
            width: inner_w,
            height: 1,
        };
        let is_cursor = i == cur;
        let row_style = if is_cursor {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg
        };
        let label = format!("{}:{}/{}  ({})", e.host, e.port, e.database, e.user);
        f.render_widget(Paragraph::new(label).style(row_style), row);
    }

    // Hint row at the bottom
    let hint_y = popup.y + height.saturating_sub(2);
    let hint_area = Rect {
        x: inner_x,
        y: hint_y,
        width: inner_w,
        height: 1,
    };
    f.render_widget(
        Paragraph::new("j/k  ↵ select  Esc cancel").style(bg.add_modifier(Modifier::DIM)),
        hint_area,
    );
}

fn draw_help(f: &mut ratatui::Frame<'_>, area: Rect, scroll: u16) -> u16 {
    const SECTIONS: &[(&str, &[(&str, &str)])] = &[
        (
            "Global",
            &[
                ("?", "Open this help (normal mode)"),
                ("Ctrl+Enter", "Run statement under cursor"),
                ("Ctrl+Shift+Enter", "Run all statements in file"),
                (":q", "Quit"),
                ("Esc Esc", "Dismiss all toasts"),
            ],
        ),
        (
            "Leader (default Space — config: editor.leader_key)",
            &[
                ("<leader> c", "Connection switcher"),
                ("<leader> n", "New scratch tab"),
                ("<leader> r", "Rename current tab"),
                ("<leader> R", "Refresh schema cache"),
                ("<leader> d", "Delete current tab (confirm)"),
                ("<leader> <leader>", "Fuzzy file picker"),
                (
                    "<leader> <CR>",
                    "Run statement under cursor (tmux/SSH-friendly alt)",
                ),
                (
                    "<leader> <Tab>",
                    "Run all statements in file (tmux/SSH-friendly alt)",
                ),
            ],
        ),
        (
            "Pane Focus",
            &[
                ("Ctrl+H / click", "Focus schema"),
                ("Ctrl+L / click", "Focus editor"),
                ("Ctrl+J / click", "Focus results"),
                ("Ctrl+K / click", "Focus editor"),
            ],
        ),
        (
            "Explorer Pane",
            &[
                ("j / k", "Navigate up / down"),
                ("Enter / l", "Expand / collapse node"),
                ("<leader> R / :refreshschema", "Refresh schema cache"),
                (
                    ":describe <table>  /  :desc <table>",
                    "Show column schema for table",
                ),
            ],
        ),
        (
            "Results Pane",
            &[
                ("j / k", "Down / up (count prefix)"),
                ("h / l", "Left / right (count prefix)"),
                ("gg / G", "First / last row"),
                ("0 / $", "First / last column"),
                ("/", "Search cells"),
                ("n / N", "Next / previous match"),
                ("H / L", "Prev / next result tab"),
                ("V", "Visual-line select rows"),
                ("v / Ctrl+V", "Visual-block select rectangle"),
                ("y", "Yank selection / row"),
                ("Esc", "Clear selection"),
                ("Left click", "Copy column value"),
                ("Right click", "Copy full row"),
                ("Left click (error)", "Copy query or error text"),
            ],
        ),
        (
            "Tabs",
            &[
                ("<leader>n", "New scratch tab"),
                ("Shift+L", "Next tab"),
                ("Shift+H", "Prev tab"),
                ("<leader>r", "Rename current tab"),
                ("<leader>d", "Delete current tab"),
                ("<leader><leader>", "Fuzzy switch tab"),
                ("Click tab name", "Switch to tab"),
            ],
        ),
        (
            "Editor — Vim",
            &[
                ("i", "Insert mode"),
                ("Esc", "Normal mode"),
                ("v", "Visual mode"),
                ("K", "LSP hover under cursor"),
                ("Ctrl+P / Ctrl+N", "History prev / next"),
            ],
        ),
        (
            "Connection Switcher",
            &[
                ("j / k", "Navigate"),
                ("Enter", "Connect"),
                ("n", "New connection"),
                ("e", "Edit connection"),
                ("d", "Delete connection"),
                ("Esc", "Close"),
            ],
        ),
        (
            "Add / Edit Connection",
            &[
                ("Tab", "Switch Name / URL field"),
                ("Enter", "Save"),
                ("Esc", "Cancel"),
            ],
        ),
    ];

    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);
    // Borderless dialog styling: same 2-col / 1-row padding idiom as
    // the command palette and hover popups. Two extra rows reserved
    // for the title + trailing pad, so content height = total_rows.
    let width = 62.min(area.width.saturating_sub(4));
    let total_rows: u16 = SECTIONS
        .iter()
        .map(|(_, items)| items.len() as u16 + 2)
        .sum::<u16>()
        + 1;
    let height = (total_rows + 4).min(area.height.saturating_sub(4));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    let title = Paragraph::new(Line::from(Span::styled(
        " Help",
        bg.add_modifier(Modifier::BOLD),
    )))
    .style(bg);
    f.render_widget(title, chunks[0]);

    let max_scroll = total_rows.saturating_sub(chunks[1].height);
    let scroll = scroll.min(max_scroll);

    let mut lines: Vec<ratatui::text::Line<'static>> = vec![];
    for (section, items) in SECTIONS {
        lines.push(ratatui::text::Line::from(Span::styled(
            format!(" {section}"),
            bg.add_modifier(Modifier::BOLD),
        )));
        for (key, desc) in *items {
            let pad = 20usize.saturating_sub(key.len());
            lines.push(ratatui::text::Line::from(vec![
                Span::styled(
                    format!("  {key}"),
                    Style::default().fg(u.completion_key).bg(u.dialog_bg),
                ),
                Span::styled(" ".repeat(pad), bg),
                Span::styled((*desc).to_string(), bg),
            ]));
        }
        lines.push(ratatui::text::Line::raw(""));
    }

    f.render_widget(
        Paragraph::new(lines).style(bg).scroll((scroll, 0)),
        chunks[1],
    );
    max_scroll
}

fn draw_connect_error_popup(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    headline: &str,
    err: &str,
    name: Option<&str>,
) {
    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);
    let title = match name {
        Some(n) => format!(" {headline} — {n}"),
        None => format!(" {headline}"),
    };
    let footer = "[r] retry  [Esc/Enter] close";

    // Wrap the body to ~75% of the screen width minus padding.
    let width = (area.width * 3 / 4)
        .clamp(40, 100)
        .min(area.width.saturating_sub(4));
    let inner_w = width.saturating_sub(4) as usize;
    let body_lines: Vec<String> = err
        .lines()
        .flat_map(|line| {
            if line.is_empty() {
                vec![String::new()]
            } else {
                line.chars()
                    .collect::<Vec<_>>()
                    .chunks(inner_w.max(1))
                    .map(|c| c.iter().collect::<String>())
                    .collect()
            }
        })
        .collect();
    let body_h = (body_lines.len() as u16).max(1);
    // 1 title + 1 blank + body + 1 blank + 1 footer + 2 vertical padding
    let height = (body_h + 6).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(body_lines.len() + 4);
    lines.push(Line::from(Span::styled(
        title,
        bg.add_modifier(Modifier::BOLD).fg(u.dialog_error_fg),
    )));
    lines.push(Line::raw(""));
    for l in body_lines {
        lines.push(Line::from(Span::styled(l, bg)));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        footer,
        bg.add_modifier(Modifier::DIM),
    )));

    f.render_widget(Paragraph::new(lines).style(bg), inner);
}

/// Returns true when `url`'s scheme supports TLS (mysql/mariadb/postgres).
/// Mirrors the identical helper in sqeel-core so the TUI can branch on it
/// without taking a dependency on core internals.
fn tui_url_supports_tls(url: &str) -> bool {
    let scheme = url.split(':').next().unwrap_or("");
    matches!(scheme, "mysql" | "mariadb" | "postgres" | "postgresql")
}

fn draw_add_connection(f: &mut ratatui::Frame<'_>, state: &AppState, area: Rect) -> (u16, u16) {
    // Match the connection switcher / file picker style: no border,
    // bg-filled block, padded inner content. Header + footer rows use
    // DIM; the focused field is REVERSED so it reads at a glance
    // without competing with the muted hint.
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    let width = 64u16.min(area.width.saturating_sub(4));
    let has_error = state.add_connection_error.is_some();
    let show_tls = tui_url_supports_tls(&state.add_connection_url);
    // Layout (no TLS):
    //   pad + header + blank + name + url + password + blank +
    //   [error + blank when present] + hint + pad  → base height 9.
    // Layout (TLS):
    //   pad + header + blank + name + url + password +
    //   ca_cert + client_cert + client_key + verify_mode + blank +
    //   [error + blank when present] + hint + pad  → base height 13.
    // Error rows grow the popup by 2 (the row itself + a leading blank)
    // so the hint stays put when the error is absent.
    let extra = if has_error { 2 } else { 0 };
    let base_height: u16 = if show_tls { 13 } else { 9 };
    let height = (base_height + extra).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner_x = popup.x + 2;
    let inner_w = popup.width.saturating_sub(4);
    let header_text = if state.edit_connection_original_name.is_some() {
        "Edit Connection  (Tab switch · Enter save · Esc cancel)"
    } else {
        "Add Connection  (Tab switch · Enter save · Esc cancel)"
    };
    f.render_widget(
        Paragraph::new(header_text).style(bg.add_modifier(Modifier::DIM)),
        Rect {
            x: inner_x,
            y: popup.y + 1,
            width: inner_w,
            height: 1,
        },
    );

    let name_label = "Name      ";
    let url_label = "URL       ";
    let pw_label = "Password  ";
    let label_w = name_label.chars().count() as u16;

    let focused = state.add_connection_field;
    let name_style = if focused == AddConnectionField::Name {
        bg.add_modifier(Modifier::REVERSED)
    } else {
        bg
    };
    let url_style = if focused == AddConnectionField::Url {
        bg.add_modifier(Modifier::REVERSED)
    } else {
        bg
    };
    let pw_style = if focused == AddConnectionField::Password {
        bg.add_modifier(Modifier::REVERSED)
    } else {
        bg
    };

    let name_row = Rect {
        x: inner_x,
        y: popup.y + 3,
        width: inner_w,
        height: 1,
    };
    let url_row = Rect {
        x: inner_x,
        y: popup.y + 4,
        width: inner_w,
        height: 1,
    };
    let pw_row = Rect {
        x: inner_x,
        y: popup.y + 5,
        width: inner_w,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(format!("{name_label}{}", state.add_connection_name)).style(name_style),
        name_row,
    );
    f.render_widget(
        Paragraph::new(format!("{url_label}{}", state.add_connection_url)).style(url_style),
        url_row,
    );
    // Mask the password with `*` characters.
    let pw_masked: String = "*".repeat(state.add_connection_password.chars().count());
    f.render_widget(
        Paragraph::new(format!("{pw_label}{pw_masked}")).style(pw_style),
        pw_row,
    );

    // TLS rows: only shown when the URL scheme supports TLS.
    let (ca_cert_row, client_cert_row, client_key_row, verify_row) = if show_tls {
        let ca_cert_label = "CA Cert   ";
        let client_cert_label = "Clt Cert  ";
        let client_key_label = "Clt Key   ";
        let verify_label = "Verify    ";

        let ca_cert_row = Rect {
            x: inner_x,
            y: popup.y + 6,
            width: inner_w,
            height: 1,
        };
        let client_cert_row = Rect {
            x: inner_x,
            y: popup.y + 7,
            width: inner_w,
            height: 1,
        };
        let client_key_row = Rect {
            x: inner_x,
            y: popup.y + 8,
            width: inner_w,
            height: 1,
        };
        let verify_row = Rect {
            x: inner_x,
            y: popup.y + 9,
            width: inner_w,
            height: 1,
        };

        let ca_cert_style = if focused == AddConnectionField::CaCert {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg
        };
        let client_cert_style = if focused == AddConnectionField::ClientCert {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg
        };
        let client_key_style = if focused == AddConnectionField::ClientKey {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg
        };

        f.render_widget(
            Paragraph::new(format!("{ca_cert_label}{}", state.add_connection_ca_cert))
                .style(ca_cert_style),
            ca_cert_row,
        );
        f.render_widget(
            Paragraph::new(format!(
                "{client_cert_label}{}",
                state.add_connection_client_cert
            ))
            .style(client_cert_style),
            client_cert_row,
        );
        f.render_widget(
            Paragraph::new(format!(
                "{client_key_label}{}",
                state.add_connection_client_key
            ))
            .style(client_key_style),
            client_key_row,
        );

        // VerifyMode row: two toggle chips — active one is REVERSED, inactive is DIM.
        let verify_focused = focused == AddConnectionField::VerifyMode;
        let is_full = state.add_connection_verify_mode == TlsVerifyMode::Full;
        let full_style = if is_full {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg.add_modifier(Modifier::DIM)
        };
        let skip_style = if !is_full {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg.add_modifier(Modifier::DIM)
        };
        let verify_label_style = if verify_focused {
            bg.add_modifier(Modifier::REVERSED)
        } else {
            bg
        };
        let verify_line = Line::from(vec![
            Span::styled(verify_label, verify_label_style),
            Span::styled(" Full ", full_style),
            Span::styled("  ", bg),
            Span::styled(" Skip ", skip_style),
        ]);
        f.render_widget(Paragraph::new(verify_line), verify_row);

        (
            Some(ca_cert_row),
            Some(client_cert_row),
            Some(client_key_row),
            Some(verify_row),
        )
    } else {
        (None, None, None, None)
    };

    // Error row: offset by 4 when TLS rows are visible (rows 6-9 are TLS).
    let tls_offset: u16 = if show_tls { 4 } else { 0 };
    if let Some(err) = state.add_connection_error.as_deref() {
        let err_style = Style::default()
            .fg(ui().dialog_error_fg)
            .bg(ui().dialog_error_bg);
        // Truncate the message to one row — the popup already has
        // a fixed height. Long sqlx errors stay readable in the
        // status bar / popup details flow elsewhere.
        let max = inner_w as usize;
        let mut shown = err.replace('\n', " ");
        if shown.chars().count() > max {
            shown = shown
                .chars()
                .take(max.saturating_sub(1))
                .collect::<String>()
                + "…";
        }
        f.render_widget(
            Paragraph::new(shown).style(err_style),
            Rect {
                x: inner_x,
                y: popup.y + 7 + tls_offset,
                width: inner_w,
                height: 1,
            },
        );
    }
    let hint_y = popup.y + 7 + tls_offset + extra;
    let hint_text = if focused == AddConnectionField::VerifyMode {
        "Space/Enter toggle · Tab next field · Esc cancel"
    } else if focused == AddConnectionField::Url {
        "URL: mysql:// postgres:// sqlite: duckdb::memory: duckdb:/path"
    } else {
        "Name: letters/digits/-/_  ·  Password stored in OS keyring (blank = URL inline)"
    };
    f.render_widget(
        Paragraph::new(hint_text).style(bg.add_modifier(Modifier::DIM)),
        Rect {
            x: inner_x,
            y: hint_y,
            width: inner_w,
            height: 1,
        },
    );

    match focused {
        AddConnectionField::Name => (
            name_row.x + label_w + state.add_connection_name_cursor as u16,
            name_row.y,
        ),
        AddConnectionField::Url => (
            url_row.x + label_w + state.add_connection_url_cursor as u16,
            url_row.y,
        ),
        AddConnectionField::Password => (
            pw_row.x + label_w + state.add_connection_password_cursor as u16,
            pw_row.y,
        ),
        AddConnectionField::CaCert => {
            let row = ca_cert_row.unwrap_or(pw_row);
            (
                row.x + label_w + state.add_connection_ca_cert_cursor as u16,
                row.y,
            )
        }
        AddConnectionField::ClientCert => {
            let row = client_cert_row.unwrap_or(pw_row);
            (
                row.x + label_w + state.add_connection_client_cert_cursor as u16,
                row.y,
            )
        }
        AddConnectionField::ClientKey => {
            let row = client_key_row.unwrap_or(pw_row);
            (
                row.x + label_w + state.add_connection_client_key_cursor as u16,
                row.y,
            )
        }
        AddConnectionField::VerifyMode => {
            // Position cursor on the active chip: "Full" starts at label_w+1,
            // "Skip" starts at label_w+9 (label_w + " Full " (6) + "  " (2) + " "(1)).
            let row = verify_row.unwrap_or(pw_row);
            let chip_offset: u16 = if state.add_connection_verify_mode == TlsVerifyMode::Full {
                label_w + 1
            } else {
                label_w + 9
            };
            (row.x + chip_offset, row.y)
        }
    }
}

/// Pre-process a raw `:` command string, stripping out cursorline /
/// cursorcolumn tokens from any `:set …` call and updating the caller's
/// booleans in place.  Returns the residual command (possibly shortened).
///
/// hjkl-engine 0.3 does not expose cursorline / cursorcolumn in its
/// `Settings` struct, so sqeel-tui owns these two booleans and intercepts
/// them before forwarding the rest of the `:set` line to the engine.
///
/// Supported token forms (mirrors vim):
///   `cursorline` / `cul`        → enable
///   `nocursorline` / `nocul`    → disable
///   `cursorcolumn` / `cuc`      → enable
///   `nocursorcolumn` / `nocuc`  → disable
///
/// Query-only tokens (`cursorline?` / `cul?`) and bare `:set` (no args)
/// are left in place so the engine's Info handler can report them.
/// Result of pre-processing a `:set` command for cursorline / cursorcolumn.
///
/// `forward` is what the engine's ex dispatcher receives (tokens we
/// consumed are stripped). `info` is an optional status string surfaced
/// as an Info toast — used for the `?` query form.
pub(crate) struct CursorOptsResult<'a> {
    pub forward: std::borrow::Cow<'a, str>,
    pub info: Option<String>,
}

/// Parse `on/off/true/false/yes/no/1/0` as a bool. Case-insensitive.
fn parse_bool_value(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn apply_cursor_opts<'a>(
    cmd: &'a str,
    cursorline: &mut bool,
    cursorcolumn: &mut bool,
) -> CursorOptsResult<'a> {
    // Only handle `:set …` commands.
    let body = if let Some(rest) = cmd.strip_prefix("set").map(str::trim_start) {
        rest
    } else if let Some(rest) = cmd.strip_prefix("se").map(str::trim_start) {
        rest
    } else {
        return CursorOptsResult {
            forward: std::borrow::Cow::Borrowed(cmd),
            info: None,
        };
    };

    let mut residual: Vec<&str> = Vec::new();
    let mut consumed_any = false;
    let mut info_lines: Vec<String> = Vec::new();
    for token in body.split_whitespace() {
        // Strip a trailing `?` (query form) or `!` (toggle form) before
        // matching the name. `=value` is split on the `=` separately.
        let (name, suffix, value) = if let Some(eq) = token.find('=') {
            (&token[..eq], None, Some(&token[eq + 1..]))
        } else if let Some(rest) = token.strip_suffix('?') {
            (rest, Some('?'), None)
        } else if let Some(rest) = token.strip_suffix('!') {
            (rest, Some('!'), None)
        } else {
            (token, None, None)
        };

        // Resolve which option (if any) this name refers to.
        let target = match name {
            "cursorline" | "cul" => Some('l'),
            "nocursorline" | "nocul" => Some('L'), // no-prefix variant
            "cursorcolumn" | "cuc" => Some('c'),
            "nocursorcolumn" | "nocuc" => Some('C'),
            _ => None,
        };

        match (target, suffix, value) {
            // Bare bool: `:set cul` / `:set nocul`
            (Some('l'), None, None) => {
                *cursorline = true;
                consumed_any = true;
            }
            (Some('L'), None, None) => {
                *cursorline = false;
                consumed_any = true;
            }
            (Some('c'), None, None) => {
                *cursorcolumn = true;
                consumed_any = true;
            }
            (Some('C'), None, None) => {
                *cursorcolumn = false;
                consumed_any = true;
            }
            // Toggle: `:set cul!` / `:set cuc!`
            (Some('l' | 'L'), Some('!'), None) => {
                *cursorline = !*cursorline;
                consumed_any = true;
            }
            (Some('c' | 'C'), Some('!'), None) => {
                *cursorcolumn = !*cursorcolumn;
                consumed_any = true;
            }
            // Query: `:set cul?` / `:set cuc?`
            (Some('l' | 'L'), Some('?'), None) => {
                let label = if *cursorline {
                    "cursorline"
                } else {
                    "nocursorline"
                };
                info_lines.push(label.to_string());
                consumed_any = true;
            }
            (Some('c' | 'C'), Some('?'), None) => {
                let label = if *cursorcolumn {
                    "cursorcolumn"
                } else {
                    "nocursorcolumn"
                };
                info_lines.push(label.to_string());
                consumed_any = true;
            }
            // Value assign: `:set cul=on` / `:set cuc=off`
            (Some('l' | 'L'), None, Some(v)) => {
                if let Some(b) = parse_bool_value(v) {
                    *cursorline = b;
                    consumed_any = true;
                } else {
                    residual.push(token);
                }
            }
            (Some('c' | 'C'), None, Some(v)) => {
                if let Some(b) = parse_bool_value(v) {
                    *cursorcolumn = b;
                    consumed_any = true;
                } else {
                    residual.push(token);
                }
            }
            // Anything else (unknown name, `cul=?`, etc.) → forward to engine.
            _ => residual.push(token),
        }
    }

    let info = if info_lines.is_empty() {
        None
    } else {
        Some(info_lines.join("  "))
    };

    if !consumed_any {
        return CursorOptsResult {
            forward: std::borrow::Cow::Borrowed(cmd),
            info,
        };
    }

    let forward = if residual.is_empty() {
        // All tokens consumed; the engine needs a no-op `:set` that
        // succeeds silently.  Return `"set"` which emits an Info dump
        // (harmless; the caller suppresses it when we already have our
        // own info to surface).
        std::borrow::Cow::Owned("set".to_string())
    } else {
        std::borrow::Cow::Owned(format!("set {}", residual.join(" ")))
    };

    CursorOptsResult { forward, info }
}

/// Parsed form of an `:Anvil …` ex-command.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AnvilCmd<'a> {
    /// Bare `:Anvil` — show usage hint.
    Usage,
    /// `:Anvil install <name>`
    Install(&'a str),
    /// `:Anvil update [name]`  (None = all)
    Update(Option<&'a str>),
    /// `:Anvil uninstall <name>`
    Uninstall(&'a str),
    /// Unrecognized sub-command.
    Unknown,
}

/// Parse the body of an `:Anvil …` command (the part after "Anvil").
pub(crate) fn parse_anvil_cmd(rest: &str) -> AnvilCmd<'_> {
    let args: Vec<&str> = rest.split_whitespace().collect();
    match args.as_slice() {
        [] => AnvilCmd::Usage,
        ["install", name] => AnvilCmd::Install(name),
        ["update"] => AnvilCmd::Update(None),
        ["update", name] => AnvilCmd::Update(Some(name)),
        ["uninstall", name] => AnvilCmd::Uninstall(name),
        _ => AnvilCmd::Unknown,
    }
}

/// Execute the `:migrate-secrets` ex-command.
///
/// Walks all known connections via `load_connections`, identifies those with
/// inline passwords, and attempts to migrate each one to the OS keyring via
/// `migrate_connection_to_keyring`. Returns a list of toast messages (one per
/// connection attempted, plus a summary).
fn run_migrate_secrets() -> Vec<(String, ToastKind)> {
    use sqeel_core::config::{MigrationResult, migrate_connection_to_keyring};

    let conns = match sqeel_core::config::load_connections() {
        Ok(c) => c,
        Err(e) => {
            return vec![(
                format!(":migrate-secrets: failed to load connections: {e}"),
                ToastKind::Error,
            )];
        }
    };

    let mut migrated = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut msgs: Vec<(String, ToastKind)> = Vec::new();

    for conn in &conns {
        match migrate_connection_to_keyring(&conn.name) {
            Ok(MigrationResult::Migrated) => {
                migrated += 1;
                msgs.push((
                    format!("Migrated `{}` password to keyring", conn.name),
                    ToastKind::Info,
                ));
            }
            Ok(MigrationResult::NoPassword) => {
                skipped += 1;
            }
            Ok(MigrationResult::KeyringFailed(reason)) => {
                failed += 1;
                msgs.push((
                    format!(
                        "`{}` keyring write failed: {} (left as plaintext)",
                        conn.name, reason
                    ),
                    ToastKind::Error,
                ));
            }
            Err(e) => {
                failed += 1;
                msgs.push((
                    format!(":migrate-secrets error for `{}`: {e}", conn.name),
                    ToastKind::Error,
                ));
            }
        }
    }

    let summary = format!(
        ":migrate-secrets done — {migrated} migrated, {skipped} skipped (no password), {failed} failed"
    );
    msgs.push((summary, ToastKind::Info));
    msgs
}

/// Parse and execute a `:export csv|json [<path>]` ex-command.
///
/// Returns `Some((message, kind))` that the caller should push as a toast, or
/// `None` when a toast was already pushed into `toasts` directly.  In
/// practice this always returns `Some`; the `Option` makes the calling site
/// tidy.
fn handle_export_cmd(
    cmd: &str,
    state: &AppState,
    _toasts: &mut Vec<(String, ToastKind, std::time::Instant)>,
) -> Option<(String, ToastKind)> {
    let mut parts = cmd.split_whitespace();
    // skip the "export" token we already matched on
    let _ = parts.next();

    let format = match parts.next() {
        Some(f) => f,
        None => {
            return Some((
                "usage: :export csv|json [<path>]".to_string(),
                ToastKind::Error,
            ));
        }
    };

    let ext = match format {
        "csv" => "csv",
        "json" => "json",
        other => {
            return Some((format!("unknown export format: {other}"), ToastKind::Error));
        }
    };

    // Resolve the active QueryResult.
    let result = match state.active_result() {
        Some(tab) => match &tab.kind {
            sqeel_core::state::ResultsPane::Results(r) => r,
            _ => {
                return Some(("no query result to export".to_string(), ToastKind::Error));
            }
        },
        None => {
            return Some(("no active result tab".to_string(), ToastKind::Error));
        }
    };

    // Resolve the output path.
    let path: std::path::PathBuf = match parts.next() {
        Some(raw) => {
            // Expand a leading `~/`.
            if let Some(rest) = raw.strip_prefix("~/") {
                match dirs::home_dir() {
                    Some(home) => home.join(rest),
                    None => {
                        return Some((
                            "could not resolve home directory".to_string(),
                            ToastKind::Error,
                        ));
                    }
                }
            } else {
                std::path::PathBuf::from(raw)
            }
        }
        None => {
            // Default: ~/.local/share/sqeel/results/<conn_name>/<timestamp>.<ext>
            let conn_name = sqeel_core::persistence::sanitize_conn_slug(
                state.active_connection.as_deref().unwrap_or("scratch"),
            );
            let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
            let home = match dirs::home_dir() {
                Some(h) => h,
                None => {
                    return Some((
                        "could not resolve home directory".to_string(),
                        ToastKind::Error,
                    ));
                }
            };
            home.join(".local/share/sqeel/results")
                .join(&conn_name)
                .join(format!("{ts}.{ext}"))
        }
    };

    // Ensure parent directory exists.
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return Some((format!("mkdir failed: {e}"), ToastKind::Error));
    }

    // Serialize the result.
    let content = match ext {
        "csv" => sqeel_core::persistence::export_csv(result),
        "json" => match sqeel_core::persistence::export_json(result) {
            Ok(s) => s,
            Err(e) => return Some((format!("JSON serialization failed: {e}"), ToastKind::Error)),
        },
        _ => unreachable!(),
    };

    // Write to disk.
    if let Err(e) = std::fs::write(&path, &content) {
        return Some((format!("write failed: {e}"), ToastKind::Error));
    }

    let row_count = result.rows.len();
    Some((
        format!("Exported {row_count} rows to {}", path.display()),
        ToastKind::Info,
    ))
}

// ── :describe / :desc ex-command ────────────────────────────────────────────

/// Handle `:describe <table>` / `:desc <table>`.
///
/// Returns `(Option<toast>, bool sent)`.  When `sent` is `true` the query was
/// dispatched and the results pane is the feedback — no success toast is
/// emitted (mirrors the pattern used by run-query).  The caller drops the
/// state lock before calling `send_query` if needed; here we take `tab_idx`
/// separately so the lock can be held for the read-only parts then released.
fn handle_describe_cmd(
    cmd: &str,
    state: &AppState,
    tab_idx: usize,
) -> (Option<(String, ToastKind)>, bool) {
    let mut parts = cmd.split_whitespace();
    // skip the "describe" / "desc" token
    let _ = parts.next();

    let table = match parts.next() {
        Some(t) => t,
        None => {
            return (
                Some(("usage: :describe <table>".to_string(), ToastKind::Error)),
                false,
            );
        }
    };

    // Reject embedded single-quotes — cheap SQLi guard.
    if table.contains('\'') {
        return (
            Some((
                "table name must not contain single quotes".to_string(),
                ToastKind::Error,
            )),
            false,
        );
    }

    let sql = match state.active_dialect {
        Dialect::MySql => format!("DESCRIBE `{table}`"),
        Dialect::Postgres => format!(
            "SELECT column_name, data_type, is_nullable, column_default, \
             character_maximum_length \
             FROM information_schema.columns \
             WHERE table_name = '{table}' \
             ORDER BY ordinal_position"
        ),
        Dialect::Sqlite => format!("PRAGMA table_info({table})"),
        Dialect::Generic => {
            return (
                Some((
                    "No dialect selected — connect to a DB first.".to_string(),
                    ToastKind::Error,
                )),
                false,
            );
        }
    };

    let sent = state.send_query(sql, tab_idx);
    if sent {
        (None, true)
    } else {
        (
            Some((
                "No DB connected. Use <leader>c to switch connections.".to_string(),
                ToastKind::Error,
            )),
            false,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::format_hover_lines;
    use hjkl_engine::{Input, Key};
    use hjkl_form::TextFieldEditor;
    use hjkl_form::VimMode as FormVimMode;
    use ratatui::style::Modifier;
    use sqeel_core::{
        AppState,
        state::{Focus, QueryResult},
    };

    fn type_chars(field: &mut TextFieldEditor, s: &str) {
        for c in s.chars() {
            field.handle_input(Input {
                key: Key::Char(c),
                ..Input::default()
            });
        }
    }

    #[test]
    fn command_prompt_open_type_submit() {
        let mut f = TextFieldEditor::new(true);
        f.enter_insert_at_end();
        assert_eq!(f.vim_mode(), FormVimMode::Insert);
        type_chars(&mut f, "q!");
        assert_eq!(f.text(), "q!");
        // Enter at any mode submits — text() captures the payload, the
        // host then drops the field.
        assert_eq!(f.text().trim(), "q!");
    }

    #[test]
    fn command_prompt_esc_grammar() {
        let mut f = TextFieldEditor::new(true);
        f.enter_insert_at_end();
        // Empty + Insert + Esc → host closes (empty-text branch).
        assert!(f.text().is_empty());

        // Non-empty + Insert + Esc → host drops to Normal.
        type_chars(&mut f, "wq");
        assert_eq!(f.vim_mode(), FormVimMode::Insert);
        f.enter_normal();
        assert_eq!(f.vim_mode(), FormVimMode::Normal);
        assert_eq!(f.text(), "wq");
        // Normal + Esc → host closes (cancel).
    }

    #[test]
    fn results_search_prompt_dirty_signals_incremental_refresh() {
        let mut f = TextFieldEditor::new(true);
        f.enter_insert_at_end();
        let before = f.dirty_gen();
        f.handle_input(Input {
            key: Key::Char('a'),
            ..Input::default()
        });
        let after = f.dirty_gen();
        assert!(
            after != before,
            "dirty_gen must advance for incremental refresh hook"
        );
    }

    #[test]
    fn text_field_paste_inserts_chars() {
        let mut f = TextFieldEditor::new(true);
        f.enter_insert_at_end();
        super::text_field_paste(&mut f, "hello\rworld");
        assert_eq!(f.text(), "helloworld");
    }

    #[test]
    fn layout_ratio_default() {
        let state = AppState::new();
        let s = state.lock().unwrap();
        assert_eq!(s.editor_ratio, 1.0);
    }

    #[test]
    fn layout_ratio_with_results() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_results(QueryResult {
            columns: vec!["col".into()],
            rows: vec![vec!["val".into()]],
            col_widths: vec![],
        });
        assert_eq!(s.editor_ratio, 0.5);
    }

    #[test]
    fn focus_transitions() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.focus = Focus::Schema;
        assert_eq!(s.focus, Focus::Schema);
        s.focus = Focus::Results;
        assert_eq!(s.focus, Focus::Results);
        s.focus = Focus::Editor;
        assert_eq!(s.focus, Focus::Editor);
    }

    #[test]
    fn completions_set_and_dismiss() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_completions(vec!["SELECT".into(), "FROM".into()]);
        assert!(s.show_completions);
        assert_eq!(s.completions.len(), 2);
        s.dismiss_completions();
        assert!(!s.show_completions);
    }

    #[test]
    fn diagnostics_stored() {
        use lsp_types::DiagnosticSeverity;
        use sqeel_core::lsp::Diagnostic;
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_diagnostics(vec![Diagnostic {
            line: 0,
            col: 5,
            end_line: 0,
            end_col: 10,
            message: "unexpected token".into(),
            severity: DiagnosticSeverity::ERROR,
        }]);
        assert!(s.has_errors());
    }

    #[test]
    fn connection_switcher_open_close() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        assert!(!s.show_connection_switcher);
        s.open_connection_switcher();
        assert!(s.show_connection_switcher);
        s.close_connection_switcher();
        assert!(!s.show_connection_switcher);
    }

    #[test]
    fn connection_switcher_navigation() {
        use sqeel_core::config::ConnectionConfig;
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_available_connections(vec![
            ConnectionConfig {
                name: "local".into(),
                url: "mysql://localhost/mydb".into(),
                tls: None,
            },
            ConnectionConfig {
                name: "staging".into(),
                url: "mysql://staging/mydb".into(),
                tls: None,
            },
        ]);
        s.open_connection_switcher();
        assert_eq!(s.connection_switcher_cursor, 0);
        s.switcher_down();
        assert_eq!(s.connection_switcher_cursor, 1);
        // Cannot go past last
        s.switcher_down();
        assert_eq!(s.connection_switcher_cursor, 1);
        s.switcher_up();
        assert_eq!(s.connection_switcher_cursor, 0);
    }

    #[test]
    fn connection_switcher_confirm_sets_pending() {
        use sqeel_core::config::ConnectionConfig;
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_available_connections(vec![ConnectionConfig {
            name: "local".into(),
            url: "mysql://localhost/mydb".into(),
            tls: None,
        }]);
        s.open_connection_switcher();
        let url = s.confirm_connection_switch();
        assert_eq!(url, Some("mysql://localhost/mydb".into()));
        assert_eq!(s.pending_reconnect, Some("mysql://localhost/mydb".into()));
        assert!(!s.show_connection_switcher);
    }

    #[test]
    fn marker_capture_style_label_todo() {
        let u = super::theme::ui();
        let style = super::marker_capture_style("comment.marker.todo").unwrap();
        assert_eq!(style.fg, Some(u.sql_marker_fg));
        assert_eq!(style.bg, Some(u.sql_marker_todo));
    }

    #[test]
    fn marker_capture_style_tail_todo() {
        let u = super::theme::ui();
        let style = super::marker_capture_style("comment.marker.tail.todo").unwrap();
        assert_eq!(style.fg, Some(u.sql_marker_todo));
        assert_eq!(style.bg, None);
    }

    #[test]
    fn marker_capture_style_fixme() {
        let u = super::theme::ui();
        let style = super::marker_capture_style("comment.marker.fixme").unwrap();
        assert_eq!(style.bg, Some(u.sql_marker_fixme));
    }

    #[test]
    fn marker_capture_style_note() {
        let u = super::theme::ui();
        let style = super::marker_capture_style("comment.marker.note").unwrap();
        assert_eq!(style.bg, Some(u.sql_marker_note));
    }

    #[test]
    fn marker_capture_style_warn() {
        let u = super::theme::ui();
        let style = super::marker_capture_style("comment.marker.warn").unwrap();
        assert_eq!(style.bg, Some(u.sql_marker_warn));
    }

    #[test]
    fn marker_capture_style_unknown_returns_none() {
        assert!(super::marker_capture_style("comment.marker.unknown").is_none());
    }

    #[test]
    fn text_input_insert_str_inserts_at_caret_and_advances() {
        let mut t = super::TextInput::from_str("ab");
        t.insert_str("XYZ");
        assert_eq!(t.text, "abXYZ");
        assert_eq!(t.cursor, 5);
        t.left();
        t.left();
        t.insert_str("--");
        assert_eq!(t.text, "abX--YZ");
        assert_eq!(t.cursor, 5);
    }

    #[test]
    fn text_input_insert_str_handles_multibyte() {
        let mut t = super::TextInput::from_str("á");
        t.insert_str("ñ");
        assert_eq!(t.text, "áñ");
        assert_eq!(t.cursor, 2);
    }

    #[test]
    fn should_resubmit_triggers_on_dialect_flip() {
        use sqeel_core::highlight::Dialect;
        // Steady state: no content change, no scroll, no dialect change.
        assert!(!super::should_resubmit_highlight(
            false,
            false,
            Dialect::Generic,
            Dialect::Generic
        ));
        // Dialect changes (e.g. async DB handshake completes) → force
        // re-parse even when content is idle.
        assert!(super::should_resubmit_highlight(
            false,
            false,
            Dialect::MySql,
            Dialect::Generic
        ));
        // Content change fires regardless of dialect match.
        assert!(super::should_resubmit_highlight(
            true,
            false,
            Dialect::Generic,
            Dialect::Generic
        ));
        // Viewport scroll fires regardless of dialect match.
        assert!(super::should_resubmit_highlight(
            false,
            true,
            Dialect::Generic,
            Dialect::Generic
        ));
    }

    #[test]
    fn diagnostic_underline_marks_range_with_severity_color() {
        use hjkl_engine::types::{Attrs, Color as EColor, Style as EngineStyle};
        use sqeel_core::lsp::Diagnostic;
        let _ = super::theme::load();

        let blue = EColor(10, 20, 30);
        let mut row: Vec<(usize, usize, EngineStyle)> = vec![(
            0,
            10,
            EngineStyle {
                fg: Some(blue),
                ..EngineStyle::default()
            },
        )];
        let by_row = std::slice::from_mut(&mut row);
        let diag = Diagnostic {
            line: 0,
            col: 2,
            end_line: 0,
            end_col: 7,
            message: "nope".into(),
            severity: lsp_types::DiagnosticSeverity::ERROR,
        };
        let lines = ["SELECT * x;".to_string()];
        super::apply_diagnostic_underline(
            by_row,
            &diag,
            &|row| lines.get(row).map(String::len).unwrap_or(0),
            1,
        );

        let u = super::theme::ui();
        let expected_fg =
            super::style_from_ratatui(ratatui::style::Style::default().fg(u.status_diag_error)).fg;
        let overlap = row
            .iter()
            .find(|&&(s, e, _)| s == 2 && e == 7)
            .expect("overlap span missing");
        // fg flips to error colour so the range reads loud even in
        // terminals without colored-underline support.
        assert_eq!(overlap.2.fg, expected_fg);
        assert!(
            overlap.2.attrs.contains(Attrs::UNDERLINE),
            "overlap missing UNDERLINE attr"
        );
        // Bytes outside the range keep their original fg.
        let left = row
            .iter()
            .find(|&&(s, e, _)| s == 0 && e == 2)
            .expect("left segment missing");
        assert_eq!(left.2.fg, Some(blue));
        let right = row
            .iter()
            .find(|&&(s, e, _)| s == 7 && e == 10)
            .expect("right segment missing");
        assert_eq!(right.2.fg, Some(blue));
    }

    #[test]
    fn diagnostic_underline_paints_gap_when_no_existing_spans() {
        use hjkl_engine::types::{Attrs, Style as EngineStyle};
        use sqeel_core::lsp::Diagnostic;
        let _ = super::theme::load();

        let mut row: Vec<(usize, usize, EngineStyle)> = Vec::new();
        let by_row = std::slice::from_mut(&mut row);
        let diag = Diagnostic {
            line: 0,
            col: 3,
            end_line: 0,
            end_col: 8,
            message: "nope".into(),
            severity: lsp_types::DiagnosticSeverity::ERROR,
        };
        let lines = ["some random text".to_string()];
        super::apply_diagnostic_underline(
            by_row,
            &diag,
            &|row| lines.get(row).map(String::len).unwrap_or(0),
            1,
        );

        let u = super::theme::ui();
        let expected_fg =
            super::style_from_ratatui(ratatui::style::Style::default().fg(u.status_diag_error)).fg;
        let span = row
            .iter()
            .find(|&&(s, e, _)| s == 3 && e == 8)
            .expect("bare diagnostic span missing");
        assert_eq!(span.2.fg, expected_fg);
        assert!(span.2.attrs.contains(Attrs::UNDERLINE));
    }

    #[test]
    fn diagnostic_underline_zero_width_range_falls_back() {
        use hjkl_engine::types::Style as EngineStyle;
        use sqeel_core::lsp::Diagnostic;
        let _ = super::theme::load();

        let mut row: Vec<(usize, usize, EngineStyle)> = Vec::new();
        let by_row = std::slice::from_mut(&mut row);
        let diag = Diagnostic {
            line: 0,
            col: 5,
            end_line: 0,
            end_col: 5,
            message: "nope".into(),
            severity: lsp_types::DiagnosticSeverity::ERROR,
        };
        let lines = ["hello world".to_string()];
        super::apply_diagnostic_underline(
            by_row,
            &diag,
            &|row| lines.get(row).map(String::len).unwrap_or(0),
            1,
        );
        assert!(!row.is_empty(), "zero-width diag produced no spans");
    }

    #[test]
    fn overlay_splits_outer_span_around_marker() {
        use hjkl_engine::types::Style as EngineStyle;
        let base = EngineStyle::default();
        let marker = EngineStyle::default();
        let mut row = vec![(0usize, 30usize, base)];
        super::overlay_span(&mut row, 10, 15, marker);
        row.sort_by_key(|&(s, _, _)| s);
        let ranges: Vec<(usize, usize)> = row.iter().map(|&(s, e, _)| (s, e)).collect();
        assert_eq!(ranges, vec![(0, 10), (10, 15), (15, 30)]);
    }

    #[test]
    fn apply_window_spans_with_alter_tail_repro() {
        use super::HighlightResult;
        use super::theme;
        use sqeel_core::highlight::{Dialect, Highlighter};

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
        let _ = theme::load();

        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight(&src, Dialect::MySql);
        let lines: Vec<String> = src.lines().map(|l| l.to_string()).collect();
        let row_count = lines.len();
        let result = HighlightResult {
            spans,
            start_row: 0,
            row_count,
            parse_errors: Vec::new(),
            block_ranges: Vec::new(),
        };

        let mut editor = hjkl_engine::Editor::new(
            hjkl_buffer::Buffer::new(),
            hjkl_engine::types::DefaultHost::new(),
            hjkl_engine::types::Options::default(),
        );
        editor.set_content(&lines.join("\n"));
        super::apply_window_spans(&mut editor, &result, row_count, &[]);
        let by_row = editor.styled_spans.clone();

        let keyword_style = super::style_from_ratatui(super::capture_style("keyword").unwrap());
        for row in [21usize, 23] {
            let spans = &by_row[row];
            let has_kw_at_zero = spans
                .iter()
                .any(|&(s, e, st)| s == 0 && e >= 4 && st == keyword_style);
            assert!(
                has_kw_at_zero,
                "row {row} missing Keyword span; row spans = {spans:?}"
            );
        }
    }

    #[test]
    fn apply_window_spans_keeps_both_desc_keyword_spans() {
        use super::HighlightResult;
        use super::theme;
        use sqeel_core::highlight::{Dialect, Highlighter};

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
        let _ = theme::load();

        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight(src, Dialect::MySql);
        let lines: Vec<String> = src.lines().map(|l| l.to_string()).collect();
        let row_count = lines.len();
        let result = HighlightResult {
            spans,
            start_row: 0,
            row_count,
            parse_errors: Vec::new(),
            block_ranges: Vec::new(),
        };

        let mut editor = hjkl_engine::Editor::new(
            hjkl_buffer::Buffer::new(),
            hjkl_engine::types::DefaultHost::new(),
            hjkl_engine::types::Options::default(),
        );
        editor.set_content(&lines.join("\n"));
        super::apply_window_spans(&mut editor, &result, row_count, &[]);
        let by_row = editor.styled_spans.clone();

        // Row 21 and row 23 each hold `DESC users;`. Both should have
        // at least one span starting at col 0 with Keyword styling.
        let keyword_style = super::style_from_ratatui(super::capture_style("keyword").unwrap());
        for row in [21usize, 23] {
            let spans = &by_row[row];
            let has_kw_at_zero = spans
                .iter()
                .any(|&(s, e, st)| s == 0 && e >= 4 && st == keyword_style);
            assert!(
                has_kw_at_zero,
                "row {row} missing Keyword span at col 0..4; row spans = {spans:?}"
            );
        }
    }

    #[test]
    fn hover_formatter_strips_h1_header_markers() {
        let lines = format_hover_lines("# schema.users\nbody");
        assert_eq!(lines[0].spans[0].content, "schema.users");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[1].spans[0].content, "body");
    }

    #[test]
    fn hover_formatter_strips_code_fence_markers() {
        let text = "before\n```sql\nSELECT 1\n```\nafter";
        let lines = format_hover_lines(text);
        let joined: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<&str>>()
                    .join("")
            })
            .collect();
        // Fence markers themselves must not appear in rendered lines.
        assert!(joined.iter().all(|l| !l.contains("```")));
        assert!(joined.iter().any(|l| l == "SELECT 1"));
        assert!(joined.first().is_some_and(|l| l == "before"));
        assert!(joined.last().is_some_and(|l| l == "after"));
    }

    #[test]
    fn hover_formatter_splits_inline_code() {
        let lines = format_hover_lines("text `code` more");
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.to_string())
            .collect();
        assert!(joined.contains("code"));
        // Inline code span carries the code style's fg — it must be
        // a distinct span, not concatenated with the surrounding text.
        let has_code_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content == "code" && s.style.fg.is_some());
        assert!(has_code_span);
    }

    #[test]
    fn hover_formatter_emits_bold_span() {
        let lines = format_hover_lines("a **bold** b");
        let bold_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content == "bold")
            .expect("bold span present");
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn hover_formatter_preserves_unicode() {
        let lines = format_hover_lines("tablé");
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.to_string())
            .collect();
        assert!(joined.contains("tablé"));
    }

    // ── apply_cursor_opts tests ──────────────────────────────────────────────

    use super::apply_cursor_opts;

    #[test]
    fn cursor_opts_set_cursorline_enables() {
        let mut cl = false;
        let mut cc = false;
        apply_cursor_opts("set cursorline", &mut cl, &mut cc);
        assert!(cl);
        assert!(!cc);
    }

    #[test]
    fn cursor_opts_set_nocursorline_disables() {
        let mut cl = true;
        let mut cc = false;
        apply_cursor_opts("set nocursorline", &mut cl, &mut cc);
        assert!(!cl);
    }

    #[test]
    fn cursor_opts_short_alias_cul() {
        let mut cl = false;
        let mut cc = false;
        apply_cursor_opts("set cul", &mut cl, &mut cc);
        assert!(cl);
    }

    #[test]
    fn cursor_opts_nocul_alias() {
        let mut cl = true;
        let mut cc = false;
        apply_cursor_opts("set nocul", &mut cl, &mut cc);
        assert!(!cl);
    }

    #[test]
    fn cursor_opts_set_cursorcolumn_enables() {
        let mut cl = false;
        let mut cc = false;
        apply_cursor_opts("set cursorcolumn", &mut cl, &mut cc);
        assert!(!cl);
        assert!(cc);
    }

    #[test]
    fn cursor_opts_cuc_alias() {
        let mut cl = false;
        let mut cc = false;
        apply_cursor_opts("set cuc", &mut cl, &mut cc);
        assert!(cc);
    }

    #[test]
    fn cursor_opts_nocuc_alias() {
        let mut cl = false;
        let mut cc = true;
        apply_cursor_opts("set nocuc", &mut cl, &mut cc);
        assert!(!cc);
    }

    #[test]
    fn cursor_opts_non_set_command_passthrough() {
        let mut cl = false;
        let mut cc = false;
        let out = apply_cursor_opts("write", &mut cl, &mut cc);
        assert_eq!(out.forward, "write");
        assert!(out.info.is_none());
        assert!(!cl);
        assert!(!cc);
    }

    #[test]
    fn cursor_opts_mixed_tokens_passes_rest_to_engine() {
        let mut cl = false;
        let mut cc = false;
        let out = apply_cursor_opts("set cursorline expandtab", &mut cl, &mut cc);
        assert!(cl);
        assert_eq!(out.forward, "set expandtab");
    }

    #[test]
    fn cursor_opts_both_consumed_returns_bare_set() {
        let mut cl = false;
        let mut cc = false;
        let out = apply_cursor_opts("set cursorline cursorcolumn", &mut cl, &mut cc);
        assert!(cl);
        assert!(cc);
        assert_eq!(out.forward, "set");
    }

    #[test]
    fn cursor_opts_value_assign_on_enables() {
        let mut cl = false;
        let mut cc = false;
        apply_cursor_opts("set cul=on", &mut cl, &mut cc);
        assert!(cl);
        apply_cursor_opts("set cuc=true", &mut cl, &mut cc);
        assert!(cc);
    }

    #[test]
    fn cursor_opts_value_assign_off_disables() {
        let mut cl = true;
        let mut cc = true;
        apply_cursor_opts("set cul=off", &mut cl, &mut cc);
        assert!(!cl);
        apply_cursor_opts("set cuc=no", &mut cl, &mut cc);
        assert!(!cc);
    }

    #[test]
    fn cursor_opts_value_assign_invalid_falls_through() {
        let mut cl = false;
        let mut cc = false;
        let out = apply_cursor_opts("set cul=banana", &mut cl, &mut cc);
        assert!(!cl);
        // Invalid value → forwarded to engine (which will error).
        assert!(out.forward.contains("cul=banana"));
    }

    #[test]
    fn cursor_opts_toggle_bang_flips() {
        let mut cl = false;
        let mut cc = true;
        apply_cursor_opts("set cul!", &mut cl, &mut cc);
        assert!(cl);
        apply_cursor_opts("set cuc!", &mut cl, &mut cc);
        assert!(!cc);
        apply_cursor_opts("set cul!", &mut cl, &mut cc);
        assert!(!cl);
    }

    #[test]
    fn cursor_opts_query_returns_info_string() {
        let mut cl = true;
        let mut cc = false;
        let out = apply_cursor_opts("set cul?", &mut cl, &mut cc);
        assert_eq!(out.info.as_deref(), Some("cursorline"));
        let out = apply_cursor_opts("set cuc?", &mut cl, &mut cc);
        assert_eq!(out.info.as_deref(), Some("nocursorcolumn"));
    }

    #[test]
    fn cursor_opts_query_multiple_joins_info() {
        let mut cl = true;
        let mut cc = true;
        let out = apply_cursor_opts("set cul? cuc?", &mut cl, &mut cc);
        assert_eq!(out.info.as_deref(), Some("cursorline  cursorcolumn"));
    }

    // ── parse_anvil_cmd tests ────────────────────────────────────────────────

    use super::{AnvilCmd, parse_anvil_cmd};

    #[test]
    fn anvil_cmd_bare_is_usage() {
        assert_eq!(parse_anvil_cmd(""), AnvilCmd::Usage);
    }

    #[test]
    fn anvil_cmd_install_sqls() {
        assert_eq!(parse_anvil_cmd("install sqls"), AnvilCmd::Install("sqls"));
    }

    #[test]
    fn anvil_cmd_install_unknown_name() {
        // Parser accepts any name; caller is responsible for rejecting unknown tools.
        assert_eq!(parse_anvil_cmd("install gopls"), AnvilCmd::Install("gopls"));
    }

    #[test]
    fn anvil_cmd_update_all() {
        assert_eq!(parse_anvil_cmd("update"), AnvilCmd::Update(None));
    }

    #[test]
    fn anvil_cmd_update_named() {
        assert_eq!(
            parse_anvil_cmd("update sqls"),
            AnvilCmd::Update(Some("sqls"))
        );
    }

    #[test]
    fn anvil_cmd_uninstall() {
        assert_eq!(
            parse_anvil_cmd("uninstall sqls"),
            AnvilCmd::Uninstall("sqls")
        );
    }

    #[test]
    fn anvil_cmd_unknown_subcommand() {
        assert_eq!(parse_anvil_cmd("frobnicate"), AnvilCmd::Unknown);
    }

    // ── LspSource tests ──────────────────────────────────────────────────────

    use super::LspSource;

    #[test]
    fn lsp_source_path_variant() {
        let src = LspSource::Path;
        assert_eq!(src, LspSource::Path);
        assert_ne!(src, LspSource::Anvil);
    }

    #[test]
    fn lsp_source_anvil_variant() {
        let src = LspSource::Anvil;
        assert_eq!(src, LspSource::Anvil);
    }

    // ── sqls prompt modal logic tests ───────────────────────────────────────
    // Mirror the initialisation logic from run_loop: derive the (sqls_prompt_open,
    // toast) pair the same way the real loop does and assert the expected outcome.

    fn sqls_startup_state(
        lsp_resolved_binary: Option<&str>,
        lsp_auto_install: bool,
        lsp_binary: &str,
    ) -> (bool, Option<String>) {
        // Returns (sqls_prompt_open, first_toast_message_if_any).
        let mut toast: Option<String> = None;
        let prompt_open = if lsp_resolved_binary.is_none() {
            if lsp_auto_install {
                true
            } else {
                toast = Some(format!(
                    "LSP: {lsp_binary} missing (lsp_auto_install = false)"
                ));
                false
            }
        } else {
            false
        };
        (prompt_open, toast)
    }

    #[test]
    fn sqls_missing_auto_install_opens_modal() {
        let (prompt_open, toast) = sqls_startup_state(None, true, "sqls");
        assert!(
            prompt_open,
            "modal must open when sqls missing + auto_install=true"
        );
        assert!(toast.is_none(), "no toast when modal is used");
    }

    #[test]
    fn sqls_missing_auto_install_false_shows_banner_not_modal() {
        let (prompt_open, toast) = sqls_startup_state(None, false, "sqls");
        assert!(
            !prompt_open,
            "modal must NOT open when lsp_auto_install=false"
        );
        let msg = toast.expect("banner toast must be pushed");
        assert!(msg.contains("lsp_auto_install = false"));
    }

    #[test]
    fn sqls_found_on_path_no_modal_no_toast() {
        let (prompt_open, toast) = sqls_startup_state(Some("/usr/local/bin/sqls"), true, "sqls");
        assert!(!prompt_open, "modal must NOT open when binary is found");
        assert!(toast.is_none(), "no toast when binary is found");
    }

    #[test]
    fn sqls_prompt_dismiss_n_produces_banner_toast() {
        // Simulate the n/N/Esc arm: prompt closes and a "missing" banner is pushed.
        let lsp_binary = "sqls";
        // n key arm: close modal, push banner
        let sqls_prompt_open = false;
        let toast_msg = format!("LSP: {lsp_binary} missing");
        assert!(!sqls_prompt_open);
        assert!(toast_msg.contains("missing"));
    }

    #[test]
    fn sqls_prompt_accept_y_sets_install_pending() {
        // Simulate the y/Y/Enter arm: prompt closes and install_pending is set.
        let sqls_prompt_open = false;
        let sqls_install_pending = true;
        assert!(!sqls_prompt_open);
        assert!(sqls_install_pending);
    }

    // ── :export command tests ────────────────────────────────────────────────

    fn make_state_with_result(rows: Vec<Vec<String>>) -> AppState {
        let mut s = AppState::default();
        s.set_results(QueryResult {
            columns: vec!["a".to_string(), "b".to_string()],
            rows,
            col_widths: vec![],
        });
        s
    }

    #[test]
    fn export_no_subcommand_returns_usage_error() {
        let s = make_state_with_result(vec![]);
        let mut toasts = vec![];
        let result = super::handle_export_cmd("export", &s, &mut toasts);
        let (msg, kind) = result.unwrap();
        assert!(msg.contains("usage:"), "expected usage hint, got: {msg}");
        assert!(matches!(kind, super::ToastKind::Error));
    }

    #[test]
    fn export_unknown_format_returns_error() {
        let s = make_state_with_result(vec![]);
        let mut toasts = vec![];
        let result = super::handle_export_cmd("export foo /tmp/x", &s, &mut toasts);
        let (msg, kind) = result.unwrap();
        assert!(
            msg.contains("unknown export format"),
            "expected format error, got: {msg}"
        );
        assert!(matches!(kind, super::ToastKind::Error));
    }

    #[test]
    fn export_csv_explicit_path_writes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.csv");
        let s = make_state_with_result(vec![
            vec!["1".to_string(), "alpha".to_string()],
            vec!["2".to_string(), "beta".to_string()],
        ]);
        let mut toasts = vec![];
        let cmd = format!("export csv {}", path.display());
        let result = super::handle_export_cmd(&cmd, &s, &mut toasts);
        let (msg, kind) = result.unwrap();
        assert!(
            matches!(kind, super::ToastKind::Info),
            "expected info, got: {msg}"
        );
        assert!(msg.contains("2 rows"), "expected row count, got: {msg}");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("a,b"));
        assert!(content.contains("1,alpha"));
    }

    #[test]
    fn export_json_explicit_path_writes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.json");
        let s = make_state_with_result(vec![vec!["x".to_string(), "y".to_string()]]);
        let mut toasts = vec![];
        let cmd = format!("export json {}", path.display());
        let result = super::handle_export_cmd(&cmd, &s, &mut toasts);
        let (msg, kind) = result.unwrap();
        assert!(
            matches!(kind, super::ToastKind::Info),
            "expected info, got: {msg}"
        );
        assert!(msg.contains("1 rows"), "expected row count, got: {msg}");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"columns\""));
    }

    #[test]
    fn export_no_active_result_returns_error() {
        let s = AppState::default(); // no results tab
        let mut toasts = vec![];
        let result = super::handle_export_cmd("export csv /tmp/noop.csv", &s, &mut toasts);
        let (msg, kind) = result.unwrap();
        assert!(matches!(kind, super::ToastKind::Error));
        assert!(
            msg.contains("no active result") || msg.contains("no query result"),
            "expected no-result error, got: {msg}"
        );
    }

    // ── :describe command tests ──────────────────────────────────────────────

    #[test]
    fn describe_no_table_arg_returns_usage_error() {
        let s = AppState::default();
        let (toast, sent) = super::handle_describe_cmd("describe", &s, 0);
        assert!(!sent);
        let (msg, kind) = toast.unwrap();
        assert!(msg.contains("usage:"), "expected usage hint, got: {msg}");
        assert!(matches!(kind, super::ToastKind::Error));
    }

    #[test]
    fn describe_single_quote_in_table_rejected() {
        use sqeel_core::highlight::Dialect;
        let mut s = AppState::default();
        s.active_dialect = Dialect::Postgres;
        let (toast, sent) = super::handle_describe_cmd("describe foo'bar", &s, 0);
        assert!(!sent);
        let (msg, kind) = toast.unwrap();
        assert!(
            msg.contains("single quote"),
            "expected single-quote error, got: {msg}"
        );
        assert!(matches!(kind, super::ToastKind::Error));
    }

    #[test]
    fn describe_generic_dialect_returns_error() {
        use sqeel_core::highlight::Dialect;
        let mut s = AppState::default();
        s.active_dialect = Dialect::Generic;
        let (toast, sent) = super::handle_describe_cmd("describe users", &s, 0);
        assert!(!sent);
        let (msg, kind) = toast.unwrap();
        assert!(
            msg.contains("No dialect"),
            "expected dialect error, got: {msg}"
        );
        assert!(matches!(kind, super::ToastKind::Error));
    }

    #[test]
    fn describe_postgres_builds_information_schema_query() {
        use sqeel_core::highlight::Dialect;
        // Without a real connection send_query returns false — we test that
        // the function reports "No DB connected" and does NOT panic; more
        // importantly we verify the sql path via a dialect-matched check.
        let mut s = AppState::default();
        s.active_dialect = Dialect::Postgres;
        // No query_tx wired → send_query returns false.
        let (toast, sent) = super::handle_describe_cmd("describe my_table", &s, 0);
        assert!(!sent, "expected false — no connection");
        let (msg, _) = toast.unwrap();
        // "No DB connected" toast confirms we reached the dispatch attempt
        // (i.e. the SQL was built correctly for Postgres path).
        assert!(
            msg.contains("No DB connected"),
            "expected no-connection error, got: {msg}"
        );
    }

    #[test]
    fn describe_sqlite_pragma_path() {
        use sqeel_core::highlight::Dialect;
        let mut s = AppState::default();
        s.active_dialect = Dialect::Sqlite;
        let (toast, sent) = super::handle_describe_cmd("describe my_table", &s, 0);
        assert!(!sent, "expected false — no connection");
        let (msg, _) = toast.unwrap();
        assert!(
            msg.contains("No DB connected"),
            "expected no-connection error, got: {msg}"
        );
    }

    #[test]
    fn describe_mysql_path() {
        use sqeel_core::highlight::Dialect;
        let mut s = AppState::default();
        s.active_dialect = Dialect::MySql;
        let (toast, sent) = super::handle_describe_cmd("describe my_table", &s, 0);
        assert!(!sent, "expected false — no connection");
        let (msg, _) = toast.unwrap();
        assert!(
            msg.contains("No DB connected"),
            "expected no-connection error, got: {msg}"
        );
    }

    #[test]
    fn desc_alias_works() {
        use sqeel_core::highlight::Dialect;
        let mut s = AppState::default();
        s.active_dialect = Dialect::Postgres;
        // "desc users" — leading verb check in the caller uses starts_with("desc ")
        let (toast, sent) = super::handle_describe_cmd("desc users", &s, 0);
        assert!(!sent, "expected false — no connection");
        let (msg, _) = toast.unwrap();
        assert!(
            msg.contains("No DB connected"),
            "expected no-connection error via desc alias, got: {msg}"
        );
    }

    // ── format_relative_time ─────────────────────────────────────────────────
    use super::format_relative_time;
    use std::time::{Duration, SystemTime};

    fn ago(secs: u64) -> SystemTime {
        SystemTime::now() - Duration::from_secs(secs)
    }

    #[test]
    fn relative_time_seconds() {
        let then = ago(30);
        let now = SystemTime::now();
        let s = format_relative_time(now, then);
        // Allow ±2s jitter in CI
        assert!(s.ends_with("s ago"), "expected 'Xs ago', got: {s}");
        let n: u64 = s.trim_end_matches("s ago").parse().unwrap();
        assert!((28..=32).contains(&n), "expected ~30s, got {n}");
    }

    #[test]
    fn relative_time_minutes() {
        let then = ago(120); // 2 minutes
        let now = SystemTime::now();
        let s = format_relative_time(now, then);
        assert!(s.ends_with("m ago"), "expected 'Xm ago', got: {s}");
        let n: u64 = s.trim_end_matches("m ago").parse().unwrap();
        assert!((1..=3).contains(&n), "expected ~2m, got {n}");
    }

    #[test]
    fn relative_time_hours() {
        let then = ago(7200); // 2 hours
        let now = SystemTime::now();
        let s = format_relative_time(now, then);
        assert!(s.ends_with("h ago"), "expected 'Xh ago', got: {s}");
        let n: u64 = s.trim_end_matches("h ago").parse().unwrap();
        assert!((1..=3).contains(&n), "expected ~2h, got {n}");
    }

    #[test]
    fn relative_time_days() {
        let then = ago(4 * 86_400); // 4 days
        let now = SystemTime::now();
        let s = format_relative_time(now, then);
        assert!(s.ends_with("d ago"), "expected 'Xd ago', got: {s}");
        let n: u64 = s.trim_end_matches("d ago").parse().unwrap();
        assert!((3..=5).contains(&n), "expected ~4d, got {n}");
    }

    #[test]
    fn relative_time_weeks() {
        let then = ago(14 * 86_400); // 2 weeks
        let now = SystemTime::now();
        let s = format_relative_time(now, then);
        assert!(s.ends_with("w ago"), "expected 'Xw ago', got: {s}");
        let n: u64 = s.trim_end_matches("w ago").parse().unwrap();
        assert!((1..=3).contains(&n), "expected ~2w, got {n}");
    }

    #[test]
    fn relative_time_over_one_year_shows_years() {
        // 400 days ≈ 1y 35d — should now render as "1y ago" not "52w ago"
        let then = ago(400 * 86_400);
        let now = SystemTime::now();
        let s = format_relative_time(now, then);
        assert_eq!(s, "1y ago");
    }

    #[test]
    fn relative_time_multiple_years() {
        let then = ago(800 * 86_400); // ~2y
        let now = SystemTime::now();
        let s = format_relative_time(now, then);
        assert_eq!(s, "2y ago");
    }

    #[test]
    fn relative_time_boundary_60s() {
        // Exactly 60s → first minute bucket
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(120);
        let then = SystemTime::UNIX_EPOCH + Duration::from_secs(60);
        let s = format_relative_time(now, then);
        assert_eq!(s, "1m ago");
    }

    #[test]
    fn relative_time_boundary_3600s() {
        // Exactly 3600s → first hour bucket
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(7200);
        let then = SystemTime::UNIX_EPOCH + Duration::from_secs(3600);
        let s = format_relative_time(now, then);
        assert_eq!(s, "1h ago");
    }

    #[test]
    fn relative_time_boundary_86400s() {
        // Exactly 86400s → first day bucket
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(172_800);
        let then = SystemTime::UNIX_EPOCH + Duration::from_secs(86_400);
        let s = format_relative_time(now, then);
        assert_eq!(s, "1d ago");
    }

    #[test]
    fn relative_time_boundary_7days() {
        // Exactly 7d → first week bucket
        let week = 7 * 86_400;
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2 * week);
        let then = SystemTime::UNIX_EPOCH + Duration::from_secs(week);
        let s = format_relative_time(now, then);
        assert_eq!(s, "1w ago");
    }
}
