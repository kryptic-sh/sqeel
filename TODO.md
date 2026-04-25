# sqeel-vim — vim-feature backlog

The vim engine is feature-complete for sqeel's purposes — buffer migration,
folding, registers, macros, marks, text objects, motions, operators, ex
commands, search, visual modes, and soft-wrap have all shipped. Git history
holds the per-feature commits. This file now tracks only what's explicitly out
of scope.

## Out of scope

- Bracket auto-pairing — leave for an opt-in plugin layer if one ever exists.
- `:earlier` / `:later` — time-tree undo; the current undo stack is flat.
- Multi-cursor.
- Window splits / `Ctrl-W` chord.
- Bidirectional text.
- `:terminal`.
- LSP-driven rename / code action chords (separate axis from vim parity).
