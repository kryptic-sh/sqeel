# Backlog

## Scope PR CI runs to the jobs a change needs (asked by the owner, 2026-11-28)

A pull request runs every CI job, whatever files it touches. Run only the jobs
whose inputs changed, judged from the PR's changed files. Not started.

- **What exists:** pull requests run `ci.yml` in full; no workflow filters by
  path.
- **How, without a new dependency:** a first job lists the PR's changed files
  against its base (`git diff --name-only`) and maps them to the jobs that need
  them, and the other jobs read its outputs in their `if:`. Changes to
  `.github/`, the lockfile or the toolchain pin select everything; in a Cargo
  workspace a changed crate selects its reverse dependencies too. A path-filter
  action such as `dorny/paths-filter` would be a new dependency, which is the
  owner's call.
- **Keep:** pushes to the default branch, tags and scheduled runs still run
  everything. A skipped job says why in the run summary rather than passing
  silently, and since a job skipped by `if:` reports as skipped, required checks
  and any release job's `needs` must tolerate it; one summary job to require is
  the usual answer.

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

- **Replace chrono with jiff.** (2026-11-28.) **Needs the owner's final decision
  and discussion; nothing is changed until then.** RUSTSEC-2026-0543
  (2026-09-14) marks every chrono version unmaintained, with no patched release.
  Its maintainer soft-deprecated chrono and chrono-tz in chronotope/chrono#1768
  and recommends jiff (1.0 since; the advisory waited for it); the advisory also
  lists `time`. kryptic-sh/lyr is moving to jiff on the owner's call
  (2026-11-28); kryptic-sh/infr dropped chrono for the C library's `strftime`
  instead, since it used one function. Here chrono is used two ways. sqeel-tui
  stamps a file name with `chrono::Utc::now().format(..)` (`ex.rs`), which jiff
  covers. sqeel-core decodes date and time columns through sqlx's `chrono`
  feature (`db.rs`,
  `try_get::<chrono::NaiveDate | NaiveTime | NaiveDateTime | DateTime<Utc>>`),
  and sqlx 0.9 has no jiff support, only `chrono` and `time`. So dropping chrono
  here means sqlx's `time` feature for the decoding (or decoding those columns
  as text), plus jiff or `time` for the stamp; jiff alone cannot do it. The
  question is which, or whether to keep chrono with an ignore until sqlx
  supports jiff.

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
