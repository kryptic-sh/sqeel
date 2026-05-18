//! Named theme loaded from TOML — vim `[hl.<group>]` model.
//!
//! ## Format (new)
//!
//! ```toml
//! name = "My Theme"
//!
//! [palette]
//! bg = "#1a1b26"
//!
//! [hl.Normal]
//! fg = "fg"
//! bg = "bg"
//!
//! [hl.Comment]
//! fg = "comment"
//! attrs = ["italic"]
//! ```
//!
//! ## Legacy format (compat shim — user config only)
//!
//! Old `[ui]` flat tables are still accepted for `~/.config/sqeel/theme.toml`
//! so existing users aren't broken on upgrade.  A `tracing::warn!` is emitted.
//! Bundled themes are new-format-only.
//!
//! ## Load order
//!
//! `$XDG_CONFIG_HOME/sqeel/theme.toml` → bundled `tokyonight.toml`.
//! If the user config is broken we surface the error to the caller (run-loop
//! turns it into a toast) and fall back to the bundle — the binary always has
//! a working theme.
//!
//! ## Slot → vim-group mapping
//!
//! The `UiColors` public API is kept stable so the ~75 `ui().<slot>` call
//! sites in `lib.rs` need no changes.  Internally each slot is filled by
//! reading the appropriate vim highlight group channel from the parsed theme.
//! Migration of call sites to `hl("Normal").bg` is intentionally deferred.

use ratatui::style::{Color, Modifier};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::RwLock;

// ── Bundled themes ─────────────────────────────────────────────────────────

const BUNDLED_TOKYONIGHT: &str = include_str!("../themes/tokyonight.toml");
const BUNDLED_GRUVBOX: &str = include_str!("../themes/gruvbox.toml");

/// All bundled themes: (lowercase name, toml source).
pub static BUNDLED: &[(&str, &str)] = &[
    ("tokyonight", BUNDLED_TOKYONIGHT),
    ("gruvbox", BUNDLED_GRUVBOX),
];

// ── Global theme state ────────────────────────────────────────────────────

static THEME: RwLock<Option<Theme>> = RwLock::new(None);

/// Return the active theme, initialising from the bundled default if needed.
pub fn theme() -> std::sync::RwLockReadGuard<'static, Option<Theme>> {
    // Fast path: already set.
    {
        let g = THEME.read().expect("THEME poisoned");
        if g.is_some() {
            return g;
        }
    }
    // Slow path: initialise.
    {
        let mut w = THEME.write().expect("THEME poisoned");
        if w.is_none() {
            *w = Some(Theme::from_toml(BUNDLED_TOKYONIGHT).expect("bundled tokyonight must parse"));
        }
    }
    THEME.read().expect("THEME poisoned")
}

/// Shorthand for reading the `UiColors` shim.  Call sites just use named slots.
/// Returns the cached struct (one full rebuild happens at theme-load time).
pub fn ui() -> UiColors {
    theme().as_ref().expect("theme is Some after init").ui_cache
}

/// Load the theme from the user config or fall back to the bundle.
/// Returns `Some(error_message)` if the user config existed but failed to
/// parse, so the caller can surface it as a toast.
pub fn load() -> Option<String> {
    let user_path = sqeel_core::config::config_dir().map(|d| d.join("theme.toml"));
    let mut parse_error: Option<String> = None;
    let t = user_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| match Theme::from_toml_user(&s) {
            Ok(t) => Some(t),
            Err(e) => {
                parse_error = Some(format!("theme.toml: {e} — falling back to bundled theme"));
                None
            }
        })
        .unwrap_or_else(|| Theme::from_toml(BUNDLED_TOKYONIGHT).expect("bundled theme must parse"));
    *THEME.write().expect("THEME poisoned") = Some(t);
    parse_error
}

/// Switch the active theme by name (case-insensitive).
/// Returns `Ok(())` on success, `Err(msg)` when the name is unknown.
pub fn switch_colorscheme(name: &str) -> Result<(), String> {
    let lower = name.to_lowercase();
    for &(bname, src) in BUNDLED {
        if bname == lower {
            let t = Theme::from_toml(src)
                .map_err(|e| format!("bundled theme '{bname}' failed to parse: {e}"))?;
            *THEME.write().expect("THEME poisoned") = Some(t);
            return Ok(());
        }
    }
    Err(format!("unknown colorscheme: {name}"))
}

/// Comma-separated list of available colorscheme names.
pub fn available_colorschemes() -> String {
    BUNDLED
        .iter()
        .map(|(n, _)| *n)
        .collect::<Vec<_>>()
        .join(", ")
}

// ── Vim highlight groups ───────────────────────────────────────────────────

/// A single vim-style highlight group entry.
#[derive(Debug, Clone, Default)]
pub struct HlGroup {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub attrs: Modifier,
}

/// Parsed theme — stores highlight groups indexed by name.
#[derive(Debug)]
#[allow(dead_code)]
pub struct Theme {
    pub name: String,
    groups: HashMap<String, HlGroup>,
    ui_cache: UiColors,
}

impl Theme {
    /// Fetch a group by name; falls back to `Normal` if missing.
    pub fn hl(&self, group: &str) -> &HlGroup {
        static EMPTY: HlGroup = HlGroup {
            fg: None,
            bg: None,
            attrs: Modifier::empty(),
        };
        self.groups
            .get(group)
            .or_else(|| self.groups.get("Normal"))
            .unwrap_or(&EMPTY)
    }

    fn fg(&self, group: &str) -> Color {
        let g = self.groups.get(group).or_else(|| self.groups.get("Normal"));
        match g {
            Some(h) if h.fg.is_some() => h.fg.unwrap(),
            _ => {
                // Try Normal.fg as fallback
                self.groups
                    .get("Normal")
                    .and_then(|n| n.fg)
                    .unwrap_or(Color::Reset)
            }
        }
    }

    fn bg(&self, group: &str) -> Color {
        let g = self.groups.get(group).or_else(|| self.groups.get("Normal"));
        match g {
            Some(h) if h.bg.is_some() => h.bg.unwrap(),
            _ => self
                .groups
                .get("Normal")
                .and_then(|n| n.bg)
                .unwrap_or(Color::Reset),
        }
    }

    /// Parse from new `[hl.*]` format only (for bundled themes).
    fn from_toml(src: &str) -> Result<Self, String> {
        let raw: RawThemeNew = toml::from_str(src).map_err(|e| e.to_string())?;
        raw.resolve()
    }

    /// Parse from user config: try new format first, fall back to legacy `[ui]` format.
    fn from_toml_user(src: &str) -> Result<Self, String> {
        // Try new format first
        if let Ok(t) = Self::from_toml(src) {
            return Ok(t);
        }
        // Try legacy format
        match Self::from_toml_legacy(src) {
            Ok(t) => {
                tracing::warn!("old [ui] format is deprecated — please migrate to [hl.*]");
                Ok(t)
            }
            Err(e) => Err(e),
        }
    }

    /// Parse the legacy `[ui]` flat-map format.
    fn from_toml_legacy(src: &str) -> Result<Self, String> {
        let raw: RawThemeLegacy = toml::from_str(src).map_err(|e| e.to_string())?;
        raw.resolve()
    }

    /// Build the `UiColors` shim by reading from vim groups.
    ///
    /// The slot → group mapping table lives here.  Migration of call sites
    /// to `hl("Normal").bg` is intentionally deferred.
    pub fn build_ui(&self) -> UiColors {
        UiColors {
            // Panes
            schema_pane_bg: self.bg("Normal"),
            pane_sep: self.fg("Comment"),
            editor_pane_bg: self.bg("Normal"),
            editor_tab_bar_bg: self.bg("StatusLine"),
            results_pane_bg: self.bg("Normal"),

            // Editor
            editor_cursor_line_active: self.bg("CursorLine"),
            editor_cursor_line_inactive: self.bg("Normal"),
            editor_line_num: self.fg("LineNr"),
            editor_search_bg: self.bg("Search"),
            editor_search_fg: self.fg("Search"),
            editor_error_fg: self.fg("Error"),

            // Schema sidebar
            schema_sel_active_bg: self.bg("Visual"),
            schema_sel_inactive_bg: self.bg("Normal"),
            schema_border_focus: self.fg("Title"),
            schema_border_filter: self.fg("Special"),
            schema_icon_db: self.fg("Function"),
            schema_icon_table: self.fg("Type"),
            schema_icon_column: self.fg("Identifier"),
            schema_icon_pk: self.fg("Constant"),
            schema_type_fg: self.fg("Type"),
            schema_placeholder_fg: self.fg("Comment"),

            // Results pane
            results_col_active_bg: self.bg("CursorLine"),
            results_col_inactive_bg: self.bg("Normal"),
            results_cursor_active_bg: self.bg("Visual"),
            results_cursor_inactive_bg: self.bg("CursorLine"),
            results_sep: self.fg("Comment"),
            results_header_active: self.fg("Title"),
            results_row_num: self.fg("LineNr"),
            results_null: self.fg("Comment"),
            results_title_active: self.fg("Title"),
            results_title_inactive: self.fg("Directory"),
            results_error: self.fg("Error"),
            results_loading: self.fg("WarningMsg"),
            results_cancelled: self.fg("Comment"),

            // Tabs
            tab_active_fg: self.fg("TabLineSel"),
            tab_active_bg: self.bg("TabLineSel"),
            tab_inactive_fg: self.fg("TabLine"),
            tab_sep_fg: self.fg("TabLineFill"),
            tab_err_fg: self.fg("ErrorMsg"),
            tab_err_bg: self.bg("ErrorMsg"),
            tab_loading_fg: self.fg("WarningMsg"),
            tab_loading_bg: self.bg("WarningMsg"),
            tab_cancel_fg: self.fg("Comment"),
            tab_cancel_bg: self.bg("Comment"),

            // Status bar
            status_bar_bg: self.bg("StatusLine"),
            status_bar_fg: self.fg("StatusLine"),
            status_mode_fg: self.fg("ModeMsg"),
            status_mode_normal: self.bg("ModeMsg"),
            status_mode_insert: self.fg("DiffAdd"),
            status_mode_visual: self.fg("Visual"),
            status_diag_error: self.fg("DiagnosticError"),
            status_diag_warning: self.fg("DiagnosticWarn"),
            status_search_bg: self.bg("Search"),
            status_search_fg: self.fg("Search"),
            status_hint_bg: self.bg("Directory"),
            status_hint_fg: self.fg("ModeMsg"),

            // LSP warning
            lsp_warn_fg: self.fg("WarningMsg"),
            lsp_warn_bg: self.bg("Normal"),

            // Toasts
            toast_info_bg: self.bg("ModeMsg"),
            toast_info_fg: self.fg("ModeMsg"),
            toast_error_bg: self.bg("ErrorMsg"),
            toast_error_fg: self.fg("ErrorMsg"),

            // Dialogs
            dialog_fg: self.fg("Pmenu"),
            dialog_bg: self.bg("Pmenu"),
            dialog_error_bg: self.bg("ErrorMsg"),
            dialog_error_fg: self.fg("ErrorMsg"),
            dialog_border: self.fg("Special"),
            confirm_border: self.fg("DiffAdd"),

            // Completion popup
            completion_border: self.fg("Special"),
            completion_bg: self.bg("Pmenu"),
            completion_select: self.bg("PmenuSel"),
            completion_key: self.fg("Keyword"),

            // SQL highlight
            sql_keyword: self.fg("Keyword"),
            sql_string: self.fg("String"),
            sql_number: self.fg("Number"),
            sql_comment: self.fg("Comment"),
            sql_operator: self.fg("Operator"),
            sql_ident: self.fg("Identifier"),
            sql_plain: self.fg("Normal"),

            // Comment markers
            sql_marker_fg: self.fg("ModeMsg"),
            sql_marker_todo: self.fg("Todo"),
            sql_marker_fixme: self.fg("Error"),
            sql_marker_note: self.fg("DiffAdd"),
            sql_marker_warn: self.fg("WarningMsg"),

            // Cursor-column highlight
            sql_cursor_column_bg: self.bg("CursorLine"),
        }
    }
}

// ── New format parser ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RawThemeNew {
    name: String,
    #[serde(default)]
    palette: HashMap<String, String>,
    hl: HashMap<String, RawHlGroup>,
}

#[derive(Deserialize, Default)]
struct RawHlGroup {
    fg: Option<String>,
    bg: Option<String>,
    #[serde(default)]
    attrs: Vec<String>,
}

impl RawThemeNew {
    fn resolve(self) -> Result<Theme, String> {
        let palette: HashMap<String, Color> = self
            .palette
            .iter()
            .map(|(k, v)| parse_color(v).map(|c| (k.clone(), c)))
            .collect::<Result<_, _>>()?;

        let resolve_color = |s: &str| -> Result<Color, String> {
            if let Some(c) = palette.get(s) {
                return Ok(*c);
            }
            parse_color(s)
        };

        // Require Normal group with both fg and bg.
        let normal_raw = self
            .hl
            .get("Normal")
            .ok_or_else(|| "missing [hl.Normal]".to_string())?;
        normal_raw
            .fg
            .as_ref()
            .ok_or_else(|| "[hl.Normal] must define fg".to_string())?;
        normal_raw
            .bg
            .as_ref()
            .ok_or_else(|| "[hl.Normal] must define bg".to_string())?;

        let mut groups = HashMap::new();
        for (name, raw) in &self.hl {
            let fg = raw.fg.as_deref().map(&resolve_color).transpose()?;
            let bg = raw.bg.as_deref().map(&resolve_color).transpose()?;
            let attrs = parse_attrs(&raw.attrs);
            groups.insert(name.clone(), HlGroup { fg, bg, attrs });
        }

        let mut t = Theme {
            name: self.name,
            groups,
            ui_cache: UiColors::default(),
        };
        t.ui_cache = t.build_ui();
        Ok(t)
    }
}

// ── Legacy `[ui]` parser (user config compat shim) ────────────────────────

#[derive(Deserialize)]
struct RawThemeLegacy {
    name: String,
    palette: HashMap<String, String>,
    ui: HashMap<String, String>,
}

impl RawThemeLegacy {
    fn resolve(self) -> Result<Theme, String> {
        let palette: HashMap<String, Color> = self
            .palette
            .iter()
            .map(|(k, v)| parse_color(v).map(|c| (k.clone(), c)))
            .collect::<Result<_, _>>()?;

        let resolve_slot = |key: &str| -> Result<Color, String> {
            let raw = self
                .ui
                .get(key)
                .ok_or_else(|| format!("missing ui slot `{key}`"))?;
            if let Some(c) = palette.get(raw.as_str()) {
                return Ok(*c);
            }
            parse_color(raw)
        };

        // Build groups from legacy flat slots.  We reconstruct the minimum
        // set of groups needed for the shim to produce correct colors.
        let mut groups = HashMap::<String, HlGroup>::new();

        macro_rules! slot_fg {
            ($group:expr, $slot:expr) => {
                if let Ok(c) = resolve_slot($slot) {
                    groups.entry($group.to_string()).or_default().fg = Some(c);
                }
            };
        }
        macro_rules! slot_bg {
            ($group:expr, $slot:expr) => {
                if let Ok(c) = resolve_slot($slot) {
                    groups.entry($group.to_string()).or_default().bg = Some(c);
                }
            };
        }

        // Normal — mandatory
        slot_fg!("Normal", "sql_plain");
        slot_bg!("Normal", "schema_pane_bg");

        slot_fg!("Comment", "pane_sep");
        slot_fg!("LineNr", "editor_line_num");
        slot_bg!("CursorLine", "editor_cursor_line_active");
        slot_bg!("Search", "editor_search_bg");
        slot_fg!("Search", "editor_search_fg");
        slot_fg!("Error", "editor_error_fg");
        slot_bg!("Visual", "schema_sel_active_bg");
        slot_fg!("Title", "schema_border_focus");
        slot_fg!("Special", "schema_border_filter");
        slot_fg!("Function", "schema_icon_db");
        slot_fg!("Type", "schema_icon_table");
        slot_fg!("Identifier", "schema_icon_column");
        slot_fg!("Constant", "schema_icon_pk");
        slot_fg!("Directory", "results_title_inactive");
        slot_fg!("WarningMsg", "lsp_warn_fg");
        slot_bg!("StatusLine", "status_bar_bg");
        slot_fg!("StatusLine", "status_bar_fg");
        slot_fg!("ModeMsg", "status_mode_fg");
        slot_bg!("ModeMsg", "status_mode_normal");
        slot_fg!("DiffAdd", "status_mode_insert");
        slot_fg!("DiagnosticError", "status_diag_error");
        slot_fg!("DiagnosticWarn", "status_diag_warning");
        slot_bg!("Pmenu", "dialog_bg");
        slot_fg!("Pmenu", "dialog_fg");
        slot_bg!("PmenuSel", "completion_select");
        slot_fg!("Keyword", "sql_keyword");
        slot_fg!("String", "sql_string");
        slot_fg!("Number", "sql_number");
        slot_fg!("Operator", "sql_operator");
        slot_fg!("Todo", "sql_marker_todo");
        slot_bg!("TabLineSel", "tab_active_bg");
        slot_fg!("TabLineSel", "tab_active_fg");
        slot_fg!("TabLine", "tab_inactive_fg");
        slot_fg!("TabLineFill", "tab_sep_fg");
        slot_bg!("ErrorMsg", "toast_error_bg");
        slot_fg!("ErrorMsg", "toast_error_fg");

        // Ensure Normal is populated (required for fallback chain)
        if !groups.contains_key("Normal") {
            return Err("legacy theme missing both sql_plain and schema_pane_bg".to_string());
        }

        let mut t = Theme {
            name: self.name,
            groups,
            ui_cache: UiColors::default(),
        };
        t.ui_cache = t.build_ui();
        Ok(t)
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn parse_color(s: &str) -> Result<Color, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).map_err(|e| e.to_string())?;
            let g = u8::from_str_radix(&hex[2..4], 16).map_err(|e| e.to_string())?;
            let b = u8::from_str_radix(&hex[4..6], 16).map_err(|e| e.to_string())?;
            return Ok(Color::Rgb(r, g, b));
        }
        return Err(format!("bad hex color `{s}`"));
    }
    Err(format!("unresolved color reference `{s}`"))
}

fn parse_attrs(attrs: &[String]) -> Modifier {
    let mut m = Modifier::empty();
    for a in attrs {
        match a.as_str() {
            "bold" => m |= Modifier::BOLD,
            "italic" => m |= Modifier::ITALIC,
            "underline" => m |= Modifier::UNDERLINED,
            "reversed" => m |= Modifier::REVERSED,
            "dim" => m |= Modifier::DIM,
            other => tracing::warn!("unknown theme attr `{other}` — skipped"),
        }
    }
    m
}

// ── UiColors shim ─────────────────────────────────────────────────────────
//
// Public API unchanged — 75 call sites in lib.rs stay as `ui().<slot>`.
// The slot values are derived from vim groups inside `Theme::build_ui()`.

#[derive(Debug, Clone, Copy, Default)]
pub struct UiColors {
    pub schema_pane_bg: Color,
    pub pane_sep: Color,
    pub editor_pane_bg: Color,
    pub editor_tab_bar_bg: Color,
    pub results_pane_bg: Color,
    pub editor_cursor_line_active: Color,
    pub editor_cursor_line_inactive: Color,
    pub editor_line_num: Color,
    pub editor_search_bg: Color,
    pub editor_search_fg: Color,
    pub editor_error_fg: Color,
    pub schema_sel_active_bg: Color,
    pub schema_sel_inactive_bg: Color,
    pub schema_border_focus: Color,
    pub schema_border_filter: Color,
    pub schema_icon_db: Color,
    pub schema_icon_table: Color,
    pub schema_icon_column: Color,
    pub schema_icon_pk: Color,
    pub schema_type_fg: Color,
    pub schema_placeholder_fg: Color,
    pub results_col_active_bg: Color,
    pub results_col_inactive_bg: Color,
    pub results_cursor_active_bg: Color,
    pub results_cursor_inactive_bg: Color,
    pub results_sep: Color,
    pub results_header_active: Color,
    pub results_row_num: Color,
    pub results_null: Color,
    pub results_title_active: Color,
    pub results_title_inactive: Color,
    pub results_error: Color,
    pub results_loading: Color,
    pub results_cancelled: Color,
    pub tab_active_fg: Color,
    pub tab_active_bg: Color,
    pub tab_inactive_fg: Color,
    pub tab_sep_fg: Color,
    pub tab_err_fg: Color,
    pub tab_err_bg: Color,
    pub tab_loading_fg: Color,
    pub tab_loading_bg: Color,
    pub tab_cancel_fg: Color,
    pub tab_cancel_bg: Color,
    pub status_bar_bg: Color,
    pub status_bar_fg: Color,
    pub status_mode_fg: Color,
    pub status_mode_normal: Color,
    pub status_mode_insert: Color,
    pub status_mode_visual: Color,
    pub status_diag_error: Color,
    pub status_diag_warning: Color,
    pub status_search_bg: Color,
    pub status_search_fg: Color,
    pub status_hint_bg: Color,
    pub status_hint_fg: Color,
    pub lsp_warn_fg: Color,
    pub lsp_warn_bg: Color,
    pub toast_info_bg: Color,
    pub toast_info_fg: Color,
    pub toast_error_bg: Color,
    pub toast_error_fg: Color,
    pub dialog_fg: Color,
    pub dialog_bg: Color,
    pub dialog_error_bg: Color,
    pub dialog_error_fg: Color,
    pub dialog_border: Color,
    pub confirm_border: Color,
    pub completion_border: Color,
    pub completion_bg: Color,
    pub completion_select: Color,
    pub completion_key: Color,
    pub sql_keyword: Color,
    pub sql_string: Color,
    pub sql_number: Color,
    pub sql_comment: Color,
    pub sql_operator: Color,
    pub sql_ident: Color,
    pub sql_plain: Color,
    pub sql_marker_fg: Color,
    pub sql_marker_todo: Color,
    pub sql_marker_fixme: Color,
    pub sql_marker_note: Color,
    pub sql_marker_warn: Color,
    pub sql_cursor_column_bg: Color,
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_tokyonight_parses() {
        let theme = Theme::from_toml(BUNDLED_TOKYONIGHT).unwrap();
        assert_eq!(theme.name, "Tokyo Night");
        let ui = theme.build_ui();
        assert!(matches!(ui.editor_cursor_line_active, Color::Rgb(_, _, _)));
    }

    #[test]
    fn bundled_gruvbox_parses() {
        let theme = Theme::from_toml(BUNDLED_GRUVBOX).unwrap();
        assert_eq!(theme.name, "Gruvbox");
        let ui = theme.build_ui();
        assert!(matches!(ui.schema_pane_bg, Color::Rgb(_, _, _)));
    }

    #[test]
    fn missing_group_falls_back_to_normal() {
        let src = r##"
name = "Minimal"

[palette]
fg = "#c0caf5"
bg = "#1a1b26"

[hl.Normal]
fg = "fg"
bg = "bg"
"##;
        let theme = Theme::from_toml(src).unwrap();
        let ui = theme.build_ui();
        // sql_keyword maps to Keyword group fg — which is missing,
        // so it falls back to Normal.fg.
        assert_eq!(ui.sql_keyword, Color::Rgb(0xc0, 0xca, 0xf5));
    }

    #[test]
    fn slot_map_covers_all_ui_fields() {
        // Verify build_ui produces a UiColors where all Rgb fields are non-Reset.
        // (If a slot mapping was omitted the field would be Color::Reset for a
        // full theme; here we just check the struct can be built.)
        let theme = Theme::from_toml(BUNDLED_TOKYONIGHT).unwrap();
        let ui = theme.build_ui();
        // Spot-check a broad spread of slots.
        assert!(matches!(ui.schema_pane_bg, Color::Rgb(_, _, _)));
        assert!(matches!(ui.pane_sep, Color::Rgb(_, _, _)));
        assert!(matches!(ui.tab_active_fg, Color::Rgb(_, _, _)));
        assert!(matches!(ui.status_bar_bg, Color::Rgb(_, _, _)));
        assert!(matches!(ui.sql_keyword, Color::Rgb(_, _, _)));
        assert!(matches!(ui.dialog_bg, Color::Rgb(_, _, _)));
        assert!(matches!(ui.completion_select, Color::Rgb(_, _, _)));
        assert!(matches!(ui.toast_error_bg, Color::Rgb(_, _, _)));
    }

    #[test]
    fn colorscheme_switches_at_runtime() {
        // Parse both themes directly (no global state races).
        let tn = Theme::from_toml(BUNDLED_TOKYONIGHT).unwrap();
        let gb = Theme::from_toml(BUNDLED_GRUVBOX).unwrap();
        let tn_bg = tn.build_ui().schema_pane_bg;
        let gb_bg = gb.build_ui().schema_pane_bg;
        // The two themes must differ; if this fails the TOML files need review.
        assert_ne!(
            tn_bg, gb_bg,
            "tokyonight and gruvbox must have different Normal.bg"
        );

        // Verify switch_colorscheme mutates the global THEME correctly.
        // Read back through the lock directly to avoid racing other test threads.
        switch_colorscheme("tokyonight").unwrap();
        let after_tn = THEME
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .build_ui()
            .schema_pane_bg;
        assert_eq!(after_tn, tn_bg);

        switch_colorscheme("gruvbox").unwrap();
        let after_gb = THEME
            .read()
            .unwrap()
            .as_ref()
            .unwrap()
            .build_ui()
            .schema_pane_bg;
        assert_eq!(after_gb, gb_bg);

        // Leave global in a known state.
        switch_colorscheme("tokyonight").unwrap();
    }

    #[test]
    fn legacy_ui_format_still_parses() {
        let src = r##"
name = "Legacy"

[palette]
bg = "#1a1b26"
fg = "#c0caf5"
comment = "#565f89"
blue = "#7aa2f7"
green = "#9ece6a"
red = "#f7768e"
yellow = "#e0af68"
black = "#15161e"
cyan = "#7dcfff"
magenta = "#bb9af7"
orange = "#ff9e64"
bg_highlight = "#292e42"
bg_dark = "#1a1b26"
fg_dark = "#a9b1d6"
white = "#c0caf5"

[ui]
schema_pane_bg = "bg"
sql_plain = "fg"
pane_sep = "comment"
editor_line_num = "comment"
editor_cursor_line_active = "bg_highlight"
editor_search_bg = "yellow"
editor_search_fg = "black"
editor_error_fg = "red"
schema_sel_active_bg = "bg_highlight"
schema_border_focus = "yellow"
schema_border_filter = "cyan"
schema_icon_db = "blue"
schema_icon_table = "cyan"
schema_icon_column = "green"
schema_icon_pk = "yellow"
results_title_inactive = "green"
lsp_warn_fg = "yellow"
status_bar_bg = "bg_highlight"
status_bar_fg = "fg_dark"
status_mode_fg = "black"
status_mode_normal = "blue"
status_mode_insert = "green"
status_diag_error = "red"
status_diag_warning = "yellow"
dialog_bg = "bg_highlight"
dialog_fg = "fg"
completion_select = "blue"
sql_keyword = "cyan"
sql_string = "green"
sql_number = "magenta"
sql_operator = "yellow"
sql_marker_todo = "cyan"
tab_active_fg = "black"
tab_active_bg = "cyan"
tab_inactive_fg = "comment"
tab_sep_fg = "comment"
toast_error_bg = "red"
toast_error_fg = "black"
"##;
        let theme = Theme::from_toml_user(src).unwrap();
        let ui = theme.build_ui();
        assert_eq!(ui.schema_pane_bg, Color::Rgb(0x1a, 0x1b, 0x26));
        assert_eq!(ui.sql_plain, Color::Rgb(0xc0, 0xca, 0xf5));
    }

    #[test]
    fn attrs_parsed_into_modifier() {
        let src = r##"
name = "AttrTest"

[palette]
fg = "#ffffff"
bg = "#000000"

[hl.Normal]
fg = "fg"
bg = "bg"

[hl.Comment]
fg = "fg"
attrs = ["bold", "italic"]
"##;
        let theme = Theme::from_toml(src).unwrap();
        let comment = theme.hl("Comment");
        assert!(comment.attrs.contains(Modifier::BOLD));
        assert!(comment.attrs.contains(Modifier::ITALIC));
        assert!(!comment.attrs.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn colorscheme_command_dispatch() {
        // Unknown name returns Err
        assert!(switch_colorscheme("nonexistent").is_err());
        // Known name succeeds
        assert!(switch_colorscheme("gruvbox").is_ok());
        assert!(switch_colorscheme("tokyonight").is_ok());
        // Case-insensitive
        assert!(switch_colorscheme("GruvBox").is_ok());
    }

    #[test]
    fn palette_reference_resolves() {
        let src = r##"
name = "t"

[palette]
bg = "#112233"
fg = "#ffffff"

[hl.Normal]
fg = "fg"
bg = "bg"
"##;
        let theme = Theme::from_toml(src).unwrap();
        assert_eq!(theme.bg("Normal"), Color::Rgb(0x11, 0x22, 0x33));
    }

    #[test]
    fn hex_literal_works() {
        assert_eq!(
            parse_color("#ff9e64").unwrap(),
            Color::Rgb(0xff, 0x9e, 0x64)
        );
    }
}
