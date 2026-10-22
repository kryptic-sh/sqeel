# Backlog

## Dependencies

- **`tree-sitter-sequel` and `hjkl-bonsai` have to agree on one `cc`.** Cargo
  resolves a single 1.x `cc` for every target in the graph.
  `tree-sitter-sequel` pins it as a build dependency (`~1.2.1` in 0.3.11,
  `~1.0.90` before 0.3.7), and `hjkl-bonsai` needs it on MSVC targets for its
  grammar compiler. hjkl 0.41.7 through 0.43.3 required `cc 1.4.6`, so no
  `tree-sitter-sequel` release could resolve beside them; hjkl 0.43.4 lowered
  the floor to `1.2.1`. If either side moves its requirement again, the
  workspace stops resolving with "failed to select a version for `cc`".
  `cargo tree -i cc --target all` shows both requirements.

## Considered and declined

- **Calling `hjkl_bonsai::install_mimalloc_allocator`.** hjkl's own `main` calls
  it first thing to route tree-sitter's C allocations through mimalloc; hjkl
  measured about 8% off a cold parse of a 100 KB file. sqeel does not call it,
  so tree-sitter uses libc's allocator, which is safe. Opting in means calling
  it before any tree-sitter use and before any thread starts, i.e. as the first
  statement of `main`; it was left out of the hjkl 0.43.4 upgrade because the
  gain is unmeasured for sqeel's small SQL buffers.
