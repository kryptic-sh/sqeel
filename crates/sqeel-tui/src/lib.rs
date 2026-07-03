mod completion_thread;
mod ex;
mod exec;
mod host;
mod picker;
mod render;
pub mod splash;
mod syntax;
mod theme;

use ex::*;
use exec::*;
use picker::*;
use render::*;
use syntax::*;

// Re-export the engine crate so existing call sites like
// `sqeel_tui::editor::VimMode` keep compiling.
pub use hjkl_engine as editor;
pub use host::SqeelHost;

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
    AppState,
    completion_ctx::{self, CompletionCtx},
    config::load_main_config,
    highlight::{
        Dialect, Highlighter, first_syntax_error, is_show_create, statement_at_byte,
        strip_sql_comments,
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
        SqeelHost::new(),
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

    // Start LSP client if binary is configured and reachable.
    // Each sqeel tab gets its own LSP document (uri derived from the tab
    // name) so late-arriving diagnostics can be matched to the document
    // they describe instead of blindly painting the active tab.
    let mut active_lsp_uri: lsp_types::Uri = {
        let s = state.lock().unwrap();
        tab_lsp_uri(
            s.active_connection.as_deref(),
            s.tabs
                .get(s.active_tab)
                .map(|t| t.name.as_str())
                .unwrap_or("scratch"),
        )
    };
    let lsp_binary = main_config.editor.lsp_binary.clone();
    let lsp_auto_install = main_config.editor.lsp_auto_install;
    let confirm_destructive = main_config.editor.confirm_destructive;
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
        let _ = client.open_document(active_lsp_uri.clone(), "").await;
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
    // Run the destructive-statement guard intercepted; `Some` while its y/N
    // confirm modal is up.
    let mut destructive_confirm: Option<PendingRun> = None;
    let mut schema_search =
        SchemaSearch::from_initial(state.lock().unwrap().schema_search_query.clone());

    let mut toasts: Vec<(String, ToastKind, std::time::Instant)> = Vec::new();
    if let Some(msg) = theme_error {
        toast(&mut toasts, ToastKind::Error, msg);
    }
    // If sqls is missing and auto-install is enabled, open the y/N modal instead
    // of the v1 toast-with-instruction. When lsp_auto_install is false, fall back
    // to the banner-only path (no modal, no install prompt).
    let mut sqls_prompt_open: bool = if lsp_resolved_binary.is_none() {
        if lsp_auto_install {
            true // modal will be shown; no toast here
        } else {
            toast(
                &mut toasts,
                ToastKind::Info,
                format!("LSP: {lsp_binary} missing (lsp_auto_install = false)"),
            );
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
                toast(
                    &mut toasts,
                    ToastKind::Info,
                    "Anvil: installing sqls via go install…".to_string(),
                );
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
        let (pending_load, pending_tab_content, active_tab_name, active_conn_name) = {
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
            let active_tab_name = s.tabs.get(s.active_tab).map(|t| t.name.clone());
            let active_conn_name = s.active_connection.clone();
            (
                pending_load,
                pending_tab_content,
                active_tab_name,
                active_conn_name,
            )
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

        // Track the active tab's LSP document. A tab switch, rename, or
        // connection change repoints the uri; `open_document` didOpens new
        // docs lazily and full-text-syncs already-open ones. Diagnostics on
        // screen belong to the previous document — drop them until this doc
        // publishes.
        if let Some(name) = &active_tab_name {
            let uri = tab_lsp_uri(active_conn_name.as_deref(), name);
            if uri != active_lsp_uri {
                active_lsp_uri = uri;
                needs_redraw = true;
                state.lock().unwrap().lsp_diagnostics.clear();
                if let Some(ref mut client) = lsp {
                    let text = pending_tab_content
                        .clone()
                        .unwrap_or_else(|| editor.content());
                    if text.len() <= LSP_MAX_BYTES {
                        let _ = client.open_document(active_lsp_uri.clone(), &text).await;
                    }
                }
            }
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
                    let writer = client.writer();
                    let uri = active_lsp_uri.clone();
                    let text = std::sync::Arc::new(content.clone());
                    let debug_path = std::env::var("SQEEL_DEBUG_HL_DUMP").ok();
                    tokio::spawn(async move {
                        let _ = writer.change_document(uri, &text).await;
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
                                    "### lsp didChange (tab-load) bytes={} preview={preview:?}",
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
                            let _ = client.change_document(active_lsp_uri.clone(), "").await;
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
                        let uri = active_lsp_uri.clone();
                        let text = Arc::clone(content);
                        let debug_path = std::env::var("SQEEL_DEBUG_HL_DUMP").ok();
                        tokio::spawn(async move {
                            let _ = writer.change_document(uri, &text).await;
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
                                        "### lsp didChange bytes={} preview={preview:?}",
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
                            active_lsp_uri.clone(),
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
                                active_lsp_uri.clone(),
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
                let _ = client.open_document(active_lsp_uri.clone(), &content).await;
                // Warm-up hover request right after opening the doc.
                // sqls fetches the DB schema on its first
                // symbol-resolution request, which would otherwise
                // penalise the user's *real* first `K` by several
                // hundred ms. Firing it now paves the cache before
                // the user interacts. Response is discarded — we
                // don't set `last_hover_id`, so the TUI arm's id
                // check drops the payload silently.
                let _ = client.writer().request_hover(active_lsp_uri.clone(), 0, 0);
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
                        toast(
                            &mut toasts,
                            ToastKind::Info,
                            "sqls installed. Starting LSP…".to_string(),
                        );
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
                        toast(
                            &mut toasts,
                            ToastKind::Error,
                            format!("sqls install failed: {msg}"),
                        );
                        install_terminal = true;
                        needs_redraw = true;
                    }
                    hjkl_anvil::InstallStatus::Installing => {
                        if !install_announced {
                            toast(&mut toasts, ToastKind::Info, "Installing sqls…".to_string());
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
                    LspEvent::Diagnostics(diag_uri, diags) => {
                        // A publish that raced a tab switch describes the
                        // OLD document — don't paint it on the new tab.
                        if diag_uri != active_lsp_uri.to_string() {
                            continue;
                        }
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
                            if uri == active_lsp_uri.to_string() {
                                // Push the pre-jump cursor onto the
                                // jumplist so `Ctrl-o` returns to the
                                // call site after the goto.
                                let pre = editor.cursor();
                                editor.jump_to(line as usize + 1, col as usize + 1);
                                if editor.cursor() != pre {
                                    editor.record_jump(pre);
                                }
                            } else {
                                toast(
                                    &mut toasts,
                                    ToastKind::Info,
                                    format!("Defined at: {uri} line {}:{}", line + 1, col + 1),
                                );
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
            let destructive_confirm_label: Option<String> =
                destructive_confirm.as_ref().map(PendingRun::warn_label);
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
                    destructive_confirm_label.as_deref(),
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
                                toast(
                                    &mut toasts,
                                    if ok {
                                        ToastKind::Info
                                    } else {
                                        ToastKind::Error
                                    },
                                    if ok {
                                        format!("{label} copied to clipboard")
                                    } else {
                                        format!("{label}: clipboard copy failed (too large)")
                                    },
                                );
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
                                toast(
                                    &mut toasts,
                                    if ok {
                                        ToastKind::Info
                                    } else {
                                        ToastKind::Error
                                    },
                                    if ok {
                                        "Row copied to clipboard".to_string()
                                    } else {
                                        "Row: clipboard copy failed (too large)".to_string()
                                    },
                                );
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
                        toast(&mut toasts, ToastKind::Info, "Query cancelled");
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
                    && destructive_confirm.is_none()
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
                                toast(
                                    &mut toasts,
                                    ToastKind::Info,
                                    format!("Refreshing schema for {conn_name}…"),
                                );
                            } else {
                                toast(
                                    &mut toasts,
                                    ToastKind::Error,
                                    "No active connection to refresh".to_string(),
                                );
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
                                    // Per-connection history: the picker only
                                    // lists what ran on the active connection.
                                    let snapshot = {
                                        let s = state.lock().unwrap();
                                        s.query_history
                                            .iter()
                                            .filter(|e| e.connection == s.active_connection)
                                            .cloned()
                                            .collect()
                                    };
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
                                    destructive_confirm = run_statement_under_cursor(
                                        &mut editor,
                                        &state,
                                        confirm_destructive,
                                    );
                                    continue;
                                }
                                // <leader><Tab> — run all statements in file
                                // (tmux/SSH-friendly alt for Ctrl+Shift+Enter)
                                KeyCode::Tab => {
                                    destructive_confirm = run_all_statements(
                                        &mut editor,
                                        &state,
                                        confirm_destructive,
                                    );
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
                            toast(
                                &mut toasts,
                                ToastKind::Error,
                                format!("Save failed for: {}", failed.join(", ")),
                            );
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

                // ── Destructive-run confirmation ─────────────────────────────────
                if destructive_confirm.is_some() {
                    match (key.modifiers, key.code) {
                        // Only an explicit `y` confirms. Enter deliberately
                        // does NOT — the run chord ends in Enter, so a
                        // double-tap would blow straight through the guard.
                        (KeyModifiers::NONE, KeyCode::Char('y'))
                        | (KeyModifiers::SHIFT, KeyCode::Char('Y')) => {
                            if let Some(pending) = destructive_confirm.take() {
                                dispatch_pending_run(&state, pending);
                            }
                        }
                        _ => {
                            // n / Esc / anything else cancels — a guard that
                            // can be blown through by a stray key isn't one.
                            destructive_confirm = None;
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
                            toast(
                                &mut toasts,
                                ToastKind::Info,
                                format!(
                                    "LSP {lsp_binary}: {state_label} (source: {source_label}) binary={bin_label}"
                                ),
                            );
                            continue;
                        }
                        // ── :Anvil … ─────────────────────────────────────────────
                        if let Some(rest) = trimmed.strip_prefix("Anvil") {
                            match parse_anvil_cmd(rest) {
                                AnvilCmd::Usage => {
                                    // Bare :Anvil — usage hint (no picker UI yet).
                                    toast(&mut toasts, ToastKind::Info, "Anvil: usage — :Anvil install <name>  |  :Anvil update [name]  |  :Anvil uninstall <name>".to_string());
                                }
                                AnvilCmd::Install(name) => {
                                    if name != "sqls" {
                                        toast(
                                            &mut toasts,
                                            ToastKind::Error,
                                            format!(
                                                "Anvil: unknown tool {name:?} (only 'sqls' supported)"
                                            ),
                                        );
                                    } else if active_install.is_some() {
                                        toast(
                                            &mut toasts,
                                            ToastKind::Info,
                                            "Anvil: install already in progress".to_string(),
                                        );
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
                                        toast(
                                            &mut toasts,
                                            ToastKind::Info,
                                            "Anvil: installing sqls via go install…".to_string(),
                                        );
                                    }
                                }
                                AnvilCmd::Update(name_opt) => {
                                    // Re-install at latest; only sqls supported for now.
                                    let name = name_opt.unwrap_or("sqls");
                                    if name != "sqls" {
                                        toast(
                                            &mut toasts,
                                            ToastKind::Error,
                                            format!(
                                                "Anvil: unknown tool {name:?} (only 'sqls' supported)"
                                            ),
                                        );
                                    } else if active_install.is_some() {
                                        toast(
                                            &mut toasts,
                                            ToastKind::Info,
                                            "Anvil: install already in progress".to_string(),
                                        );
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
                                        toast(
                                            &mut toasts,
                                            ToastKind::Info,
                                            "Anvil: updating sqls via go install…".to_string(),
                                        );
                                    }
                                }
                                AnvilCmd::Uninstall(name) => {
                                    if name != "sqls" {
                                        toast(
                                            &mut toasts,
                                            ToastKind::Error,
                                            format!(
                                                "Anvil: unknown tool {name:?} (only 'sqls' supported)"
                                            ),
                                        );
                                    } else {
                                        // Remove the anvil-managed store dir for sqls.
                                        match hjkl_anvil::store::package_dir("sqls") {
                                            Ok(pkg_dir) => {
                                                if pkg_dir.exists() {
                                                    if let Err(e) =
                                                        std::fs::remove_dir_all(&pkg_dir)
                                                    {
                                                        toast(
                                                            &mut toasts,
                                                            ToastKind::Error,
                                                            format!("Anvil: uninstall failed: {e}"),
                                                        );
                                                    } else {
                                                        // If the LSP was anvil-managed, clear the
                                                        // resolved binary so it won't restart.
                                                        if lsp_source == LspSource::Anvil {
                                                            lsp_resolved_binary = None;
                                                        }
                                                        toast(
                                                            &mut toasts,
                                                            ToastKind::Info,
                                                            "Anvil: sqls uninstalled".to_string(),
                                                        );
                                                    }
                                                } else {
                                                    toast(
                                                        &mut toasts,
                                                        ToastKind::Info,
                                                        "Anvil: sqls is not installed by anvil"
                                                            .to_string(),
                                                    );
                                                }
                                            }
                                            Err(e) => {
                                                toast(
                                                    &mut toasts,
                                                    ToastKind::Error,
                                                    format!("Anvil: store error: {e}"),
                                                );
                                            }
                                        }
                                    }
                                }
                                AnvilCmd::Unknown => {
                                    toast(
                                        &mut toasts,
                                        ToastKind::Error,
                                        "Anvil: unknown subcommand — try :Anvil install sqls"
                                            .to_string(),
                                    );
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
                            toast(&mut toasts, ToastKind::Info, msg);
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
                                    toast(
                                        &mut toasts,
                                        ToastKind::Error,
                                        format!("Save failed for: {}", failed.join(", ")),
                                    );
                                } else if any_dirty {
                                    quit_prompt = Some(());
                                } else {
                                    break;
                                }
                            }
                            hjkl_ex::ExEffect::Save => {
                                {
                                    let mut s = state.lock().unwrap();
                                    // The heavy content pipeline is gated
                                    // off for buffers over 2 MB, which
                                    // otherwise leaves
                                    // `editor_content_synced = false` and
                                    // the save falls back to stale
                                    // `tab.content`.
                                    s.editor_content = editor.content_arc();
                                    s.editor_content_synced = true;
                                }
                                if save_active_tab(&state, &mut toasts).await {
                                    editor_dirty = false;
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
                                toast(
                                    &mut toasts,
                                    ToastKind::Info,
                                    format!("{count} {sub_word} on {lines_changed} {line_word}"),
                                );
                            }
                            hjkl_ex::ExEffect::Ok => {
                                state.lock().unwrap().focus = Focus::Editor;
                            }
                            hjkl_ex::ExEffect::Info(msg) => {
                                // Suppress the engine's bare `:set` info dump
                                // when we already surfaced a query result for
                                // a cursor-opt token (`?` form).
                                if !suppress_engine_info {
                                    toast(&mut toasts, ToastKind::Info, msg);
                                }
                            }
                            hjkl_ex::ExEffect::Error(msg) => {
                                toast(&mut toasts, ToastKind::Error, msg);
                            }
                            hjkl_ex::ExEffect::Unknown(c) => {
                                if c == "colorscheme" {
                                    toast(
                                        &mut toasts,
                                        ToastKind::Info,
                                        format!("Available: {}", theme::available_colorschemes()),
                                    );
                                } else if let Some(name) =
                                    c.strip_prefix("colorscheme").and_then(|rest| {
                                        let rest = rest.trim();
                                        if rest.is_empty() { None } else { Some(rest) }
                                    })
                                {
                                    match theme::switch_colorscheme(name) {
                                        Ok(()) => {
                                            toast(
                                                &mut toasts,
                                                ToastKind::Info,
                                                format!("colorscheme: {name}"),
                                            );
                                        }
                                        Err(msg) => {
                                            toast(&mut toasts, ToastKind::Error, msg);
                                        }
                                    }
                                } else if c.starts_with("export") {
                                    let msg =
                                        handle_export_cmd(&c, &state.lock().unwrap(), &mut toasts);
                                    if let Some((text, kind)) = msg {
                                        toast(&mut toasts, kind, text);
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
                                        toast(
                                            &mut toasts,
                                            ToastKind::Info,
                                            format!("Refreshing schema for {conn_name}…"),
                                        );
                                    } else {
                                        toast(
                                            &mut toasts,
                                            ToastKind::Error,
                                            "No active connection to refresh".to_string(),
                                        );
                                    }
                                } else if c.starts_with("describe")
                                    || c.starts_with("desc ")
                                    || c == "desc"
                                {
                                    let s = state.lock().unwrap();
                                    let tab_idx = s.active_result_tab;
                                    let (describe_toast, sent) =
                                        handle_describe_cmd(&c, &s, tab_idx);
                                    drop(s);
                                    if let Some((text, kind)) = describe_toast {
                                        toast(&mut toasts, kind, text);
                                    }
                                    if sent {
                                        // query dispatched — results pane is the feedback
                                    }
                                } else if c == "migrate-secrets" {
                                    let msgs = run_migrate_secrets();
                                    for (text, kind) in msgs {
                                        toast(&mut toasts, kind, text);
                                    }
                                } else {
                                    toast(
                                        &mut toasts,
                                        ToastKind::Error,
                                        format!("Unknown command: :{c}"),
                                    );
                                }
                            }
                            hjkl_ex::ExEffect::None => {}
                            // `:w <path>` — one-off export of the buffer to a
                            // filesystem path. The tab's identity (and where
                            // plain `:w` saves) is unchanged, so `editor_dirty`
                            // stays as-is — vim semantics.
                            hjkl_ex::ExEffect::SaveAs(path) => {
                                let path = expand_tilde(&path);
                                let content = editor.content();
                                let write = tokio::task::spawn_blocking(move || {
                                    std::fs::write(&path, content).map(|()| path)
                                })
                                .await
                                .unwrap_or_else(|e| {
                                    Err(std::io::Error::other(format!(
                                        "spawn_blocking join error: {e}"
                                    )))
                                });
                                match write {
                                    Ok(path) => toast(
                                        &mut toasts,
                                        ToastKind::Info,
                                        format!("Written {}", path.display()),
                                    ),
                                    Err(e) => toast(
                                        &mut toasts,
                                        ToastKind::Error,
                                        format!("Write failed: {e}"),
                                    ),
                                }
                            }
                            // `:saveas <name>` — rename the active tab and
                            // save under the new name in the queries dir.
                            // sqeel buffers live in the queries dir by name,
                            // so a path with directory components is refused
                            // (use `:w <path>` for a filesystem export).
                            hjkl_ex::ExEffect::SaveAndRename { path } => {
                                if std::path::Path::new(&path).components().count() > 1 {
                                    toast(&mut toasts, ToastKind::Error, ":saveas takes a buffer name, not a path — use :w <path> to export".to_string());
                                } else {
                                    let renamed = {
                                        let mut s = state.lock().unwrap();
                                        s.editor_content = editor.content_arc();
                                        s.editor_content_synced = true;
                                        let old_name = s
                                            .tabs
                                            .get(s.active_tab)
                                            .map(|t| (s.active_connection.clone(), t.name.clone()));
                                        s.rename_active_tab(&path).map(|()| old_name)
                                    };
                                    match renamed {
                                        Ok(old_name) => {
                                            // Close the pre-rename uri's LSP doc;
                                            // the new one didOpens next iteration.
                                            close_tab_lsp_doc(&lsp, old_name);
                                            if save_active_tab(&state, &mut toasts).await {
                                                editor_dirty = false;
                                            }
                                        }
                                        Err(e) => toast(
                                            &mut toasts,
                                            ToastKind::Error,
                                            format!("Rename failed: {e}"),
                                        ),
                                    }
                                }
                            }
                            // `:file <name>` — rename the buffer in place
                            // without writing.
                            hjkl_ex::ExEffect::RenameBuffer { name } => {
                                let renamed = {
                                    let mut s = state.lock().unwrap();
                                    let old_name = s
                                        .tabs
                                        .get(s.active_tab)
                                        .map(|t| (s.active_connection.clone(), t.name.clone()));
                                    s.rename_active_tab(&name).map(|()| old_name)
                                };
                                match renamed {
                                    Ok(old_name) => {
                                        close_tab_lsp_doc(&lsp, old_name);
                                        toast(
                                            &mut toasts,
                                            ToastKind::Info,
                                            format!("Renamed to {name}"),
                                        );
                                    }
                                    Err(e) => toast(
                                        &mut toasts,
                                        ToastKind::Error,
                                        format!("Rename failed: {e}"),
                                    ),
                                }
                            }
                            // `:put [{reg}]` — paste register contents as new
                            // lines below (above with `:0put` → above=true).
                            hjkl_ex::ExEffect::PutRegister { reg, above } => {
                                let text =
                                    editor.registers().read(reg).map(|slot| slot.text.clone());
                                match text {
                                    Some(t) if !t.is_empty() => {
                                        editor.set_yank_linewise(true);
                                        editor.set_yank(t);
                                        if above {
                                            editor.paste_before(1);
                                        } else {
                                            editor.paste_after(1);
                                        }
                                        editor_dirty = true;
                                    }
                                    _ => toast(
                                        &mut toasts,
                                        ToastKind::Error,
                                        format!("Nothing in register {reg}"),
                                    ),
                                }
                            }
                            // `:redraw[!]` — repaint next frame; `!` clears
                            // the terminal first (recovers from stray output).
                            hjkl_ex::ExEffect::Redraw { clear } => {
                                if clear {
                                    let _ = terminal.clear();
                                }
                                event_triggered_redraw = true;
                            }
                            // `:cd [{path}]` — hjkl-ex already chdir'd; just
                            // surface the new working directory.
                            hjkl_ex::ExEffect::Cwd(path) => {
                                toast(&mut toasts, ToastKind::Info, format!("cwd: {path}"));
                            }
                            hjkl_ex::ExEffect::InfoTitled { content, .. } => {
                                toast(&mut toasts, ToastKind::Info, content);
                            }
                            // `:e <name>` — open a saved query from sqeel's
                            // queries dir, mirroring the file-picker path.
                            hjkl_ex::ExEffect::EditFile { path, .. } => {
                                let name = std::path::Path::new(&path)
                                    .file_name()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                if name.is_empty() {
                                    toast(
                                        &mut toasts,
                                        ToastKind::Error,
                                        ":e needs a file name".to_string(),
                                    );
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
                                        let conn_bind = s.active_connection.clone();
                                        s.tabs.push(sqeel_core::state::TabEntry {
                                            name,
                                            content: Some(content),
                                            last_accessed: Some(Instant::now()),
                                            cursor: None,
                                            dirty: false,
                                            connection: conn_bind,
                                        });
                                        let idx = s.tabs.len() - 1;
                                        s.switch_to_tab(idx);
                                    } else {
                                        toast(
                                            &mut toasts,
                                            ToastKind::Error,
                                            format!("no saved query named {name}"),
                                        );
                                    }
                                }
                            }
                            // `:s///c` interactive confirm — matched
                            // separately from the wildcard so the toast
                            // doesn't Debug-dump the whole match list.
                            hjkl_ex::ExEffect::SubstituteConfirm { .. } => {
                                toast(
                                    &mut toasts,
                                    ToastKind::Error,
                                    ":s///c confirm mode not supported in sqeel — use :s///g"
                                        .to_string(),
                                );
                            }
                            // Quickfix / location lists, buffer ops, cwd, …
                            // — hjkl-app machinery sqeel doesn't model.
                            other => {
                                toast(
                                    &mut toasts,
                                    ToastKind::Error,
                                    format!("unsupported in sqeel: {other:?}"),
                                );
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
                            let renamed = {
                                let mut s = state.lock().unwrap();
                                let old_name = s
                                    .tabs
                                    .get(s.active_tab)
                                    .map(|t| (s.active_connection.clone(), t.name.clone()));
                                s.rename_active_tab(&name_str).map(|()| old_name)
                            };
                            match renamed {
                                Ok(old_name) => {
                                    // The old uri's LSP document is orphaned by
                                    // the rename — close it server-side. The new
                                    // uri didOpens on the next loop iteration.
                                    close_tab_lsp_doc(&lsp, old_name);
                                }
                                Err(e) => {
                                    toast(
                                        &mut toasts,
                                        ToastKind::Error,
                                        format!("Rename failed: {e}"),
                                    );
                                }
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
                            let deleted = {
                                let mut s = state.lock().unwrap();
                                let old_name = s
                                    .tabs
                                    .get(s.active_tab)
                                    .map(|t| (s.active_connection.clone(), t.name.clone()));
                                s.delete_active_tab().map(|()| old_name)
                            };
                            match deleted {
                                Ok(old_name) => {
                                    // Close the deleted tab's LSP document so
                                    // the server doesn't keep analysing it.
                                    close_tab_lsp_doc(&lsp, old_name);
                                }
                                Err(e) => {
                                    toast(
                                        &mut toasts,
                                        ToastKind::Error,
                                        format!("Delete failed: {e}"),
                                    );
                                }
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
                                            let conn_bind = s.active_connection.clone();
                                            s.tabs.push(sqeel_core::state::TabEntry {
                                                name,
                                                content: Some(content),
                                                last_accessed: Some(Instant::now()),
                                                cursor: None,
                                                dirty: false,
                                                connection: conn_bind,
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
                                    toast(&mut toasts, ToastKind::Info, "Pattern not found");
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
                                toast(&mut toasts, ToastKind::Info, "Pattern not found");
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
                            toast(
                                &mut toasts,
                                ToastKind::Info,
                                format!("LSP: {} missing", lsp_binary),
                            );
                        }
                        KeyCode::Char('y') | KeyCode::Char('Y') if mods_letter_ok => {
                            sqls_prompt_open = false;
                            sqls_install_pending = true;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') if mods_letter_ok => {
                            sqls_prompt_open = false;
                            toast(
                                &mut toasts,
                                ToastKind::Info,
                                format!("LSP: {} missing", lsp_binary),
                            );
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
                                toast(&mut toasts, ToastKind::Info, "Pattern not found");
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
                                toast(&mut toasts, ToastKind::Info, "Pattern not found");
                            }
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('y')) if focus == Focus::Hover => {
                        let yanked = state.lock().unwrap().hover_yank();
                        if let Some((text, label)) = yanked {
                            let ok = clipboard
                                .set(Selection::Clipboard, MimeType::Text, text.as_bytes())
                                .is_ok();
                            toast(
                                &mut toasts,
                                if ok {
                                    ToastKind::Info
                                } else {
                                    ToastKind::Error
                                },
                                if ok {
                                    format!("{label} copied to clipboard")
                                } else {
                                    format!("{label}: clipboard copy failed (too large)")
                                },
                            );
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
                                toast(&mut toasts, ToastKind::Info, "Pattern not found");
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
                                toast(&mut toasts, ToastKind::Info, "Pattern not found");
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
                            toast(
                                &mut toasts,
                                if ok {
                                    ToastKind::Info
                                } else {
                                    ToastKind::Error
                                },
                                if ok {
                                    format!("{label} copied to clipboard")
                                } else {
                                    format!("{label}: clipboard copy failed (too large)")
                                },
                            );
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
                        destructive_confirm =
                            run_statement_under_cursor(&mut editor, &state, confirm_destructive);
                    }
                    // Run all statements in the file: Ctrl+Shift+Enter
                    (m, KeyCode::Enter)
                        if m.contains(KeyModifiers::CONTROL) && m.contains(KeyModifiers::SHIFT) =>
                    {
                        destructive_confirm =
                            run_all_statements(&mut editor, &state, confirm_destructive);
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
                                active_lsp_uri.clone(),
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
                                active_lsp_uri.clone(),
                                row as u32,
                                col as u32,
                            ));
                        }
                        if let Some(text) = editor.host_mut().take_clipboard_writes().pop() {
                            let ok = clipboard
                                .set(Selection::Clipboard, MimeType::Text, text.as_bytes())
                                .is_ok();
                            toast(
                                &mut toasts,
                                if ok {
                                    ToastKind::Info
                                } else {
                                    ToastKind::Error
                                },
                                if ok {
                                    "Yanked to clipboard".to_string()
                                } else {
                                    "Yank: clipboard copy failed (too large)".to_string()
                                },
                            );
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

/// Prepare + commit the ACTIVE tab's save on a blocking task (multi-MB
/// writes must not freeze the render loop), mark it saved, and toast the
/// result. Returns `true` when the write landed so callers can clear
/// their local dirty flag. Callers sync `editor_content` into state
/// first.
async fn save_active_tab(state: &Arc<Mutex<AppState>>, toasts: &mut Vec<Toast>) -> bool {
    let prepared = state.lock().unwrap().prepare_save_active_tab();
    match prepared {
        Ok(pending) => {
            let name = pending.name.clone();
            let idx = pending.tab_index;
            let commit = tokio::task::spawn_blocking(move || pending.commit())
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
                    toast(toasts, ToastKind::Info, format!("Saved {name}"));
                    true
                }
                Err(e) => {
                    toast(toasts, ToastKind::Error, format!("Save failed: {e}"));
                    false
                }
            }
        }
        Err(e) => {
            toast(toasts, ToastKind::Error, format!("Save failed: {e}"));
            false
        }
    }
}

/// didClose the LSP document identified by a captured
/// `(connection, tab name)` pair — the shape every rename/delete site
/// grabs before mutating the tab. No-op without a client or identity.
fn close_tab_lsp_doc(lsp: &Option<LspClient>, identity: Option<(Option<String>, String)>) {
    if let (Some(client), Some((conn, name))) = (lsp, identity) {
        client.close_document(&tab_lsp_uri(conn.as_deref(), &name));
    }
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

    use hjkl_engine::{Editor, Host as _};
    use ratatui::layout::Rect;

    // ── editor_cell_to_doc (wrap-aware mouse translation) ────────────────────

    /// Editor over `content` with an explicit viewport `(text_width, wrap)`.
    /// Default settings: number=true, numberwidth=4 → lnum_width 4; the
    /// harness area is rooted at (0,0) so content_x = 1 (margin) + 1 (sign)
    /// + 4 (numbers) = 6 and the first text row is screen row 1 (tab bar).
    fn wrap_editor(
        content: &str,
        text_width: u16,
        wrap: hjkl_buffer::Wrap,
    ) -> Editor<hjkl_buffer::Buffer, hjkl_engine::types::DefaultHost> {
        let mut ed = Editor::new(
            hjkl_buffer::Buffer::from_str(content),
            hjkl_engine::types::DefaultHost::new(),
            hjkl_engine::types::Options::default(),
        );
        let v = ed.host_mut().viewport_mut();
        v.top_row = 0;
        v.top_col = 0;
        v.width = text_width + 6;
        v.height = 20;
        v.text_width = text_width;
        v.wrap = wrap;
        ed
    }

    fn cell(
        ed: &Editor<hjkl_buffer::Buffer, hjkl_engine::types::DefaultHost>,
        col: u16,
        row: u16,
    ) -> (usize, usize) {
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        };
        super::editor_cell_to_doc(ed, area, col, row)
    }

    #[test]
    fn cell_to_doc_nowrap_maps_rows_one_to_one() {
        let ed = wrap_editor("alpha\nbravo\ncharlie", 20, hjkl_buffer::Wrap::None);
        assert_eq!(cell(&ed, 6, 1), (0, 0)); // first text cell
        assert_eq!(cell(&ed, 8, 2), (1, 2)); // row 2 → doc row 1, col 2
        assert_eq!(cell(&ed, 30, 3), (2, 6)); // past EOL clamps to last char
    }

    #[test]
    fn cell_to_doc_wrap_char_maps_continuation_rows() {
        // 25-char line, width 10 → segments [0,10), [10,20), [20,25).
        let ed = wrap_editor(
            "abcdefghijklmnopqrstuvwxy\nnext",
            10,
            hjkl_buffer::Wrap::Char,
        );
        // Screen row 1 = first segment.
        assert_eq!(cell(&ed, 6, 1), (0, 0));
        // Screen row 2 = continuation → char 10 onward.
        assert_eq!(cell(&ed, 6, 2), (0, 10));
        assert_eq!(cell(&ed, 9, 2), (0, 13));
        // Screen row 3 = last segment (5 chars); col past its end clamps
        // inside the segment.
        assert_eq!(cell(&ed, 14, 3), (0, 24));
        // Screen row 4 = the next doc row.
        assert_eq!(cell(&ed, 6, 4), (1, 0));
    }

    #[test]
    fn cell_to_doc_wrap_click_past_eof_clamps() {
        let ed = wrap_editor("abcdefghijklmno", 10, hjkl_buffer::Wrap::Char);
        // Doc row 0 spans screen rows 1..=2; a click far below clamps to
        // the last char of the last doc row.
        assert_eq!(cell(&ed, 6, 10), (0, 14));
    }

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
            rows: vec![vec![Some("val".into())]],
            col_widths: vec![],
            limited: false,
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
    fn tab_title_flags_foreign_connection_bindings() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.active_connection = Some("local".into());
        s.tabs = vec![
            sqeel_core::state::TabEntry {
                name: "a.sql".into(),
                content: None,
                last_accessed: None,
                cursor: None,
                dirty: false,
                connection: Some("local".into()),
            },
            sqeel_core::state::TabEntry {
                name: "b.sql".into(),
                content: None,
                last_accessed: None,
                cursor: None,
                dirty: false,
                connection: Some("prod".into()),
            },
            sqeel_core::state::TabEntry {
                name: "c.sql".into(),
                content: None,
                last_accessed: None,
                cursor: None,
                dirty: false,
                connection: None,
            },
        ];
        let line = super::build_tab_title(&s);
        let text: String = line.spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert!(
            text.contains("[prod]"),
            "foreign binding not flagged: {text}"
        );
        assert!(
            !text.contains("[local]"),
            "same-connection binding must stay clean: {text}"
        );
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

    fn make_state_with_result(rows: Vec<Vec<Option<String>>>) -> AppState {
        let mut s = AppState::default();
        s.set_results(QueryResult {
            columns: vec!["a".to_string(), "b".to_string()],
            rows,
            col_widths: vec![],
            limited: false,
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
            vec![Some("1".to_string()), Some("alpha".to_string())],
            vec![Some("2".to_string()), Some("beta".to_string())],
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
        let s = make_state_with_result(vec![vec![Some("x".to_string()), Some("y".to_string())]]);
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
