# sqeel-vim — vim-feature backlog

The buffer migration (Phases 0–8) and the first round of unlocked features
(Phase 9: registers, macros, special marks, ex global, folding) are done. Git
history holds the per-phase commits.

Below: concrete plans for vim features the engine is still missing. Grouped by
area, ranked **S/M/L** by effort. Pick a chunk, work it, keep tests green.

---

## Folding follow-ups (S–M)

Folding shipped manually — selection-based `zf`, the open/close/
toggle/`zR`/`zM`/`zd` chord set, and edit-side invalidation. What's left:

- **`zf{motion}` in normal mode (S).** Today `zf` only fires from visual modes.
  Add a `Pending::FoldMotion` variant; in `step_normal` the bare-`f` after `z`
  arms it; the next motion's `(top, bot)` range becomes the fold. Reuse
  `apply_motion_cursor` to compute the range.
- **Fold-aware `j` / `k` (M).** Closed folds should count as one visual line.
  Add `Buffer::next_visible_row(row)` and `prev_visible_row(row)` (skip rows
  where `is_row_hidden`); rewrite `Buffer::move_up` / `move_down` to use them.
  Mind cursor placement — landing on a hidden row makes the cursor invisible
  (already a latent bug if the user closes a fold over the cursor).
- **`foldmethod=indent` (M).** Auto-derive folds from leading- whitespace runs.
  Triggered manually via a new ex command (`:foldindent`?) since
  auto-fold-on-edit is expensive. Drop into `Buffer::add_fold` for each run.
- **`foldmethod=syntax` (L).** Tree-sitter already runs in sqeel- tui's
  `apply_window_spans` flow. Tap the same parser to extract block ranges (CTEs,
  subqueries, parenthesised lists) and pipe them as folds. Needs a per-row →
  fold cache that survives edits via the same dirty-gen scheme spans use.

---

## Registers + macros (S–M)

- **Macro storage in registers (M).** Macros currently live in a separate
  `HashMap<char, Vec<Input>>`. Vim stores them as text inside the matching
  register so `"ap` pastes the macro and `"ay` saves an edited macro back.
  Decide on an `Input ↔ string` encoding (probably vim's `<C-x>` notation), wire
  `record_*` / `read` to translate. Drop the separate map.
- **Nested `@b` inside `qa` recording (S).** Today the recorder stops at the
  bare `q`, so `qa@bq` captures the literal `@`/`b` keys; replay re-runs them.
  That's actually correct for vim — but re-verify and add a test, then move on.
- **`Ctrl-R {reg}` in insert mode (S).** Paste a register's contents at the
  cursor without leaving insert. Add a `Pending::InsertRegister` (state lives on
  the insert handler) and feed each char of the register through
  `handle_insert_key` as `Char`s.
- **`:reg` / `:registers` ex command (S).** Tabular dump of every named slot.
  Great for debugging macros. Returns `ExEffect::Info(table)`.
- **System clipboard registers `"+` / `"*` (M).** Map the two selectors to the
  host's clipboard via the existing `last_yank` pipe (sqeel-tui drains it
  through `arboard`). Needs a paste hook too.

---

## Marks (S–M)

- **File-global marks `A-Z` (M).** Vim stores `A-Z` per buffer _file_, not the
  editor session. Sqeel has tabs (one buffer per tab); store global marks on
  `AppState` keyed by `(tab_id, char)` and surface via a host accessor.
  Lowercase / special marks stay buffer-local.
- **`:marks` ex command (S).** Print every set mark + its `(row, col)` for
  inspection.
- **Mark migration on edit (M).** When rows shift up/down via insert/delete,
  marks above the edit row stay; marks below shift. Add a
  `Buffer::shift_marks(after_row, delta)` helper, call from `apply_edit`
  line-changing variants (`InsertStr` containing `\n`, `DeleteRange { Line }`,
  `JoinLines`, `SplitLines`).
- **`g;` / `g,` (M).** Walk the change list (each `mutate_edit` pushes onto a
  ring). Already have `last_edit_pos` — promote to a bounded ring; `g;` pops
  back, `g,` pops forward.

---

## Text objects (S–M)

We support some `OpTextObj` chords. Audit + fill gaps:

- **Audit existing.** Run through `i{`, `i(`, `i[`, `i<`, `i"`, `i'`, `it`,
  `ip`, `iw`, `iW` and the matching `a*` variants. Add a test per shape.
  Document the supported set in `lib.rs`.
- **`it` / `at` (M).** XML-tag inner / around. Less useful in SQL but cheap to
  add — find the surrounding `<…>` tags.
- **`ip` / `ap` (S).** Paragraph (blank-line-delimited block). Useful for
  multi-statement SQL files. Walk row by row from cursor.
- **`is` / `as` (M).** Sentence — split on `.`, `?`, `!`. Even less useful for
  SQL but standard.

---

## Motions (S–M)

- **`(` / `)` — sentence motions (M).** Same idea as `{`/`}`, but sentence
  splitter. Defer until `is`/`as` lands.
- **`gM` (S).** Halfway across the longest line of the screen. Niche — skip
  until someone asks.
- **`*` / `#` already exist; add `g*` / `g#` (S).** Same word search but without
  the `\b` boundaries (substring match).

---

## Operators (S)

- **`R` — Replace mode (M).** Continuous overstrike. Each typed char overwrites
  the cell under the cursor instead of inserting. Needs an
  `InsertReason::Replace` variant + a small branch in `handle_insert_key` that
  does delete-then-insert per char.
- **`gq{motion}` — text reflow (L).** Vim wraps to `textwidth`. SQL doesn't
  usually want this; hold off unless someone asks.
- **`>>` / `<<` already exist; add `>{motion}` / `<{motion}` audit (S).**
  Confirm the operator works against arbitrary motions, not just doubled-form.
  Add tests.

---

## Insert mode (S)

- **`Ctrl-R {reg}` (S).** Already listed under registers.
- **`Ctrl-W` / `Ctrl-U` / `Ctrl-H` (done).** Verify against vim behaviour for
  edge cases (Ctrl-W at line start should join with prev row's last word —
  doesn't today).
- **`Ctrl-O` already exists.** Double-check the one-shot semantics end up in
  normal mode for exactly one command.
- **Bracket auto-pairing (out of scope).** Leave for an opt-in plugin layer if
  it ever exists.

---

## Ex commands (S–M)

Today: `:q`, `:q!`, `:w`, `:wq`, `:x`, `:noh`, `:s/`, `:%s/`, `:g/`, `:v/`, `:N`
(line jump). Backlog:

- **`:read file` / `:r file` (M).** Insert file contents below the cursor.
  Host-side I/O — sqeel-tui owns the path resolution.
- **`:r !cmd` (L).** Insert shell command output. Powerful but needs a sandbox
  story; defer.
- **`:set` (M).** Tiny subset — `shiftwidth`, `tabstop`, `foldenable`,
  `ignorecase`. Stash a `Settings` struct on Editor.
- **`:earlier` / `:later` (L).** Time-tree undo. Out of scope — the current undo
  is a flat stack.
- **`:registers` / `:reg` (S).** Listed under registers.
- **`:marks` (S).** Listed under marks.
- **`:sort` (M).** Sort lines in a range (default whole buffer). Useful for SQL
  DDL cleanup.
- **`:! cmd` (L).** Run shell, insert nothing. Same security caveats as `:r !`.
- **`:!{filter}` over a range (L).** Pipe range through external filter. Same
  caveats.
- **`:undo` / `:redo` (S).** Aliases for `u` / `Ctrl-R`. Trivial.
- **Range support before commands (M).** Vim accepts `:5,10s/…/` and `:5,10d`.
  Today scope is hard-coded (current-line vs whole). Add a tiny range parser
  (`N,M`, `.`, `$`, `'a`).

---

## Search (S)

- **Search history (M).** `Ctrl-P` / `Ctrl-N` in the search prompt walks past
  patterns. Add a bounded `Vec<String>` on `VimState`.
- **`?` — backward search prompt (audit).** Verify it commits with
  `search_backward(true)` and that `n` / `N` invert as vim expects.
- ~~**`/<CR>` — repeat last search (S).**~~ Done. Empty `<CR>` reuses
  `last_search` in the prompt's direction; `enter_search` no longer wipes the
  pattern when opening the prompt.

---

## Visual (S)

- _(no open items — `gv` and `o`-swap shipped)_

---

## Render polish (M–L)

- **Soft-wrap render (L).** Long SQL lines often blow past terminal width. Add a
  `wrap: Wrap::None | Wrap::Char | Wrap::Word` enum on `BufferView`; the render
  walks a synthetic "screen line" stream. Affects motion (`gj`/`gk` start to
  matter), gutter (line numbers on continuation rows), cursor placement.
- **Concealed regions (M).** Render-time hide/replace of byte ranges (e.g. URL
  prettying). Buffer ignores it; `BufferView` takes a list of
  `(row, byte_range, replacement)`.
- **Cursorcolumn (S).** Vertical highlight column matching cursor. Add a bg pass
  in `paint_row`.
- **Better fold marker (S).** Use the surrounding row's content prefix instead
  of `+-- N lines folded --` so the marker hints at what's inside.

---

## Polish / parity (S)

- **Macro / register interop tests.** Confirm `"ay…@a` does what vim does.
  Confirm `"ap` after `"ay`. Confirm capital-register append + macro append both
  share semantics.
- **Replay still respects mode-switching mid-macro.** Verify recording
  `iX<Esc>0` then replay leaves us in normal at col 0.
- **`.` after a macro** repeats the macro's last effective change, not the macro
  itself. Test + fix if not.

---

## Out of scope (for now)

- Multi-cursor.
- Window splits / `Ctrl-W` chord.
- Bidirectional text.
- `:terminal`.
- LSP-driven rename / code action chords (separate axis from vim parity).
