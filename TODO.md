# Replace vendored tui-textarea with `sqeel-buffer`

Goal: own the editor buffer, cursor, viewport, selection, and render path in a
new crate (`sqeel-buffer`) designed around vim semantics from the ground up.
Drop `vendor/tui-textarea` once `sqeel-vim` builds against it.

## Why

- tui-textarea models a single char-range selection; vim needs charwise,
  linewise, and blockwise as first-class kinds. Today we hack three post-render
  overlays on top of one wrong primitive.
- Many `CursorMove` variants don't match vim (`$` past-end, `Back` wraps,
  empty-line behaviour). We re-implement most motions in `sqeel-vim`, ignoring
  the upstream ones.
- Render path goes through ratatui's `Paragraph`, which does grapheme + wrap
  work we don't need. A direct cell-write `Widget` is leaner and lets us merge
  selection / cursor-line / marker overlays into a single pass.
- We're already patching tui-textarea (row cache, span storage, workspace
  integration, history clippy fix). Forking has cost; owning the primitive
  removes the seam.
- A vim-shaped buffer makes future features (folding, marks beyond letters,
  named registers, macros) sit naturally where today they'd fight the upstream
  API.

## Non-goals

- Multi-cursor editing.
- Soft wrap / line wrapping (vim's `wrap` mode).
- Bidi / RTL.
- A general-purpose terminal text widget for other apps. `sqeel-buffer` is
  shaped for SQL editing inside sqeel.

---

## Phase 0 — Audit (½ day)

- Run `rg -n 'textarea\.' sqeel-vim/ sqeel-tui/`; enumerate every method called
  on `TextArea`. Flag pure reads vs mutations.
- Same for `CursorMove::*`, `Scrolling::*`, `Input`, `Key`.
- Identify each tui-textarea behaviour we currently work around in `sqeel-vim`
  (sticky col, `$` clamp, `Back` no-wrap, empty line, …). These become test
  cases for the new buffer.
- Snapshot the full sqeel-vim test suite count + names — the migration target is
  "every existing test still green".

Deliverable: `audit.md` (working doc, not committed) listing surface area +
behavioural quirks.

---

## Phase 1 — `sqeel-buffer` skeleton (1 day)

New workspace member `sqeel-buffer/`. Crate type `lib`. No ratatui dep yet —
that lands in the render phase.

Core type:

```rust
pub struct Buffer {
    lines: Vec<String>,    // never empty; one trailing empty line is fine
    cursor: Position,      // charwise on lines[row]
    viewport: Viewport,    // top_row, top_col, height, width
    spans: Vec<Vec<Span>>, // syntax / marker overlay; mirrors `lines`
    marks: BTreeMap<char, Position>,
    dirty_gen: u64,        // bumps on every mutation
}
```

- `Position { row: usize, col: usize }` — `col` is char index, not byte offset
  (matches what vim users think). Provide `to_byte_offset(&self, line: &str)`
  for slicing.
- `Viewport { top_row, top_col, height, width }` — height/width published by the
  host each draw, same idiom as today.
- No mutation methods yet — just constructors, getters, content load
  (`from_str`, `lines() -> &[String]`).

Tests: construction, line splitting on `\n`, line iteration, position
arithmetic.

---

## Phase 2 — Vim-aware selection (½ day)

First-class selection enum baked into the buffer:

```rust
pub enum Selection {
    Char  { anchor: Position, head: Position },
    Line  { anchor_row: usize, head_row: usize },
    Block { anchor: Position, head: Position },
}
```

- Ordered iteration: `selection_cells() -> impl Iterator<Item=Position>` so the
  renderer doesn't reimplement bounds for each kind.
- `extend_to(pos)` updates `head`; anchor stays.
- Char selection covers `anchor..=head` inclusive (vim default), not
  `anchor..head`. Block selection covers the inclusive rect.

Tests: each selection kind's cell coverage on representative rows (empty line,
ragged block, single-row char selection).

---

## Phase 3 — Cursor + viewport (1 day)

Vim-shaped motion API. Each motion takes `&mut Buffer` and a count. Names match
vim ops:

- `move_left(count)` — clamps at col 0, never wraps.
- `move_right_in_line(count)` — clamps at last char.
- `move_right_to_end(count)` — operator-context, allowed past last.
- `move_up(count)` / `move_down(count)` — preserves sticky col.
- `move_word_fwd(big)` / `move_word_back(big)` / `move_word_end(big)`
- `move_line_start()` / `move_first_non_blank()` / `move_line_end()`
- `move_top()` / `move_bottom(line: Option<usize>)`
- `find_char_on_line(ch, forward, till)` — for `f` / `F` / `t` / `T`.
- `match_bracket()` — `%`.

Plus:

- `sticky_col: Option<usize>` lives on `Buffer`; vim.rs no longer has to wrap
  every vertical motion to remember curswant.
- Viewport scroll follows cursor: bring-into-view helper.
- `viewport.scroll_by(rows, cols)` for explicit scroll (`Ctrl-d/u/f/b`,
  `zz/zt/zb`).

Tests: each motion against the quirks list from Phase 0. Sticky col across
`j`/`k` over short lines. `dl` past last char. `$` on empty line.

---

## Phase 4 — Edit primitives (1 day)

Single funnel `apply_edit(edit: Edit) -> EditResult` that:

1. Mutates `lines`.
2. Updates cursor / sticky col if needed.
3. Bumps `dirty_gen`.
4. Returns the inverse `Edit` for undo.

`Edit` variants:

```rust
pub enum Edit {
    InsertChar  { at: Position, ch: char },
    InsertStr   { at: Position, text: String },
    DeleteRange { start: Position, end: Position, kind: MotionKind },
    JoinLines   { row: usize, count: usize, with_space: bool }, // J / gJ
    Replace     { range: (Position, Position), with: String },
}
```

- `DeleteRange` covers char-wise, line-wise, block-wise deletes uniformly via
  `kind`.
- No external `undo_stack`; the funnel produces inverse edits the host stacks.
  (sqeel-vim's existing `undo_stack` adapts to consume these.)
- Multi-line block-wise delete preserves col anchor.

Tests: every `Edit` variant + its undo round-trips the buffer to identity. Block
delete on ragged rows. Insert into empty buffer. Delete that clears the last
line keeps the one-empty-line invariant.

---

## Phase 5 — Render path (1-2 days)

Implement `ratatui::widgets::Widget` for `&Buffer` writing cells directly to the
buffer (no `Paragraph`). Per cell:

1. Start with `style_default`.
2. Apply syntax span fg.
3. Apply cursor-line bg if row == cursor.row.
4. Apply selection bg from `Selection::cells_for_row(row)`.
5. Apply search-match bg if line matches active regex.
6. Write char + final style.

Plus:

- Move the per-row render cache from vendored tui-textarea into `sqeel-buffer`.
  Same fingerprint scheme — `(dirty_gen, row, …)` — adapted to the new state
  shape.
- Gutter signs (LSP diag dots) drawn here too — kill the post-render
  `paint_gutter_signs` overlay.
- Cursor cell is a single `REVERSED` style — kill the cursor-line bg strip the
  TUI currently pre-paints.

Tests: render into a `TestBackend` buffer for each style layer (syntax-only,
syntax + cursor-line, syntax + selection, syntax + search, syntax + gutter
sign).

---

## Phase 6 — Search (½ day)

- `Buffer::set_search_pattern(re: Option<Regex>)`.
- `Buffer::search_forward(skip_current: bool) -> bool` — moves cursor to next
  match; wraps end-of-buffer.
- `Buffer::search_backward(skip_current: bool) -> bool`.
- Match positions cached per line, invalidated on edit.
- Render layer reads matches per row to apply search bg.

Tests: forward/back wrap, empty buffer, skip-current, regex with no matches.

---

## Phase 7 — Migrate `sqeel-vim` (2-3 days)

Do this in stages so the test suite stays green between commits.

- **7a** — Replace `TextArea` field on `Editor` with `Buffer`. Keep a compat
  `textarea: TextArea` field temporarily that wraps `Buffer` internally so
  existing accessors keep compiling. Goal of this commit: zero behavioural
  change.
- **7b** — Port motion calls in `vim.rs` from `CursorMove::*` to the new
  `Buffer::move_*` methods. One arm at a time, regression-test each.
- **7c** — Port edit calls (`insert_char`, `delete_char`, …) to
  `Buffer::apply_edit`. Rebuild the undo stack on top of returned inverse edits.
- **7d** — Drop the post-render selection overlays (`paint_char_overlay` /
  `paint_line_overlay` / `paint_block_overlay`). `sqeel-vim` now hands its
  `Selection` straight to the buffer; the render path picks it up.
- **7e** — Move gutter signs into the buffer render. Drop `paint_gutter_signs` +
  the host-side wiring in sqeel-tui.
- **7f** — Rip the compat `TextArea` field. Public surface of `Editor` is now
  `Buffer`-shaped.
- Update `sqeel-vim/lib.rs` exports.

Acceptance: full workspace test suite green, including the 230+ sqeel-vim tests,
with no functional regressions surfaced by manual smoke testing of each vim
feature listed in `README.md`.

---

## Phase 8 — Drop the vendor (½ day)

- Remove `vendor/tui-textarea/` from disk.
- Delete the `[patch.crates-io]` block + workspace member entry in root
  `Cargo.toml`.
- Delete the unused `tui-textarea` dep in `sqeel-tui/Cargo.toml`.
- `cargo build --all` + `cargo test --all` clean.
- Update `README.md` workspace section: remove the `vendor/tui-textarea/` row,
  add `sqeel-buffer/`.
- Update `CLAUDE.md` / agent context if it mentions tui-textarea.

---

## Phase 9 — Vim-native features unlocked by the rewrite (separate work)

Not part of the migration, but the rewrite makes these straightforward. Track
here so they stay on the radar.

- **Folding** — `Buffer` gains `Vec<Fold>`; render skips folded ranges, draws
  fold marker. `zo`/`zc`/`za`/`zR`/`zM` operate on this.
- **Named registers** — register bank lives next to `Buffer`; `"{reg}y/p`
  populate / consume. `"0`–`"9` ring on every yank.
- **Macros** — recorder taps the input stream feeding `Editor`; replay
  re-injects events. Easier when the buffer model is ours.
- **Ex global** (`:g/pat/cmd`) — buffer exposes `lines_matching` so ex commands
  can iterate without re-parsing.
- **Marks beyond `a-z`** — `''` (last jump), `` ` ` `` (last edit), file-global
  marks `A-Z`. Buffer's `marks` field already supports arbitrary chars.

---

## Migration order, by risk

```
Phase 0 → 1 → 2 → 3 → 4 → 5 → 6 → 7a → 7b → 7c → 7d → 7e → 7f → 8
```

Each phase ends in a green test run. Phases 1-6 add code without touching
`sqeel-vim`; the integration risk concentrates in Phase 7.

## Estimated effort

~7-10 working days of focused work. The bulk is Phase 7 — migrating `sqeel-vim`
motion-by-motion with a green test suite at each step.

## Done when

- `vendor/tui-textarea/` is gone.
- `cargo build --all` + `cargo test --all` + `cargo clippy --all-targets` all
  clean.
- Manual smoke test of every vim feature in `README.md` passes.
- `sqeel-buffer` is documented enough that the next person can extend it without
  re-deriving the design.
