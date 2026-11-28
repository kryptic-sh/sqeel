# Backlog

## CI

- **Add a cargo-audit job to CI.** (2026-11-28.) The owner wants every
  kryptic-sh repo we own to run both cargo-deny and cargo-audit in CI. This one
  runs cargo-deny (`.github/workflows/ci.yml`) but has no cargo-audit job. lyr's
  `.github/workflows/ci.yml` is the model: an `audit` job on `ubuntu-latest`
  that installs `cargo-audit` with `taiki-e/install-action@v2` and runs plain
  `cargo audit`, which fails only on vulnerabilities (unmaintained and yanked
  crates are warnings). Related: cargo-deny went red here on 2026-11-23 on
  RUSTSEC-2026-0543 (chrono unmaintained, no fix; jiff or time recommended). lyr
  ignores that ID in `deny.toml` with a reason until it moves off chrono.

## Dependencies

- **`tree-sitter-sequel` and `hjkl-bonsai` have to agree on one `cc`.** Cargo
  resolves a single 1.x `cc` for every target in the graph. `tree-sitter-sequel`
  pins it as a build dependency (`~1.2.1` in 0.3.11, `~1.0.90` before 0.3.7),
  and `hjkl-bonsai` needs it on MSVC targets for its grammar compiler. hjkl
  0.41.7 through 0.43.3 required `cc 1.4.6`, so no `tree-sitter-sequel` release
  could resolve beside them; hjkl 0.43.4 lowered the floor to `1.2.1`. If either
  side moves its requirement again, the workspace stops resolving with "failed
  to select a version for `cc`". `cargo tree -i cc --target all` shows both
  requirements.

- **`deny.toml` ignores RUSTSEC-2026-0459 (bundled SQLite before 3.54.0).**
  SQLite can return uninitialized page-cache memory for a database opened beside
  a crafted `-wal` file whose page size differs. sqeel bundles SQLite through
  `sqlx`'s `sqlite` feature (`sqlite-bundled`) and opens database files the user
  names, so the advisory does apply when a file came from someone untrusted. The
  fix is `libsqlite3-sys` 0.39, but `sqlx-sqlite` 0.9.0, the latest `sqlx`
  (checked 2026-11-17), requires `>=0.30.1, <0.38.0`, so nothing resolves to it.
  Drop the ignore when a `sqlx` release accepts 0.39
  (`cargo tree -i libsqlite3-sys --all-features` then shows 0.39). Not taken:
  moving to `sqlite-unbundled`, which links the system SQLite and changes what
  the release binaries need at run time.

## Considered and declined

- **Calling `hjkl_bonsai::install_mimalloc_allocator`.** hjkl's own `main` calls
  it first thing to route tree-sitter's C allocations through mimalloc; hjkl
  measured about 8% off a cold parse of a 100 KB file. sqeel does not call it,
  so tree-sitter uses libc's allocator, which is safe. Opting in means calling
  it before any tree-sitter use and before any thread starts, i.e. as the first
  statement of `main`; it was left out of the hjkl 0.43.4 upgrade because the
  gain is unmeasured for sqeel's small SQL buffers.
