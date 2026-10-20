# Backlog

## Dependencies

- **`deny.toml` ignores RUSTSEC-2026-0367, -0368 and -0369 for `zip` 8.** All
  three are bugs in `ZipArchive::extract` and `ZipStreamReader::extract` (SUID
  bits kept, symlinks escaping the destination, permissions applied through
  symlinks), fixed in `zip` 9. sqeel gets `zip` 8 only from `hjkl-anvil`, whose
  `installer` module and `sync_anvil` binary read entries one at a time with
  `ZipArchive::by_index`, check each path with `safe_join` and write plain files
  with a fixed mode, so neither extract function is reached (checked in the
  hjkl-anvil 0.41.0 source). hjkl 0.43.1, the latest release, still requires
  `zip = "8"`. To drop the ignores: move `hjkl-anvil` to `zip` 9 in hjkl,
  release it, then move sqeel's `=0.41.0` hjkl pins to that release.

## Tests

- **Two `sqeel-config` tests race and fail intermittently.**
  `connection_filesystem_roundtrip` and
  `inline_password_roundtrip_percent_decodes` each take their own
  function-local `static LOCK`, so they do not serialize against each other,
  and each passes its own `tempfile::tempdir()` to `set_config_dir_override`,
  which is a first-call-wins `OnceLock`. Whichever runs second writes into the
  first one's tempdir, which is deleted when that test returns, and fails with
  `NotFound`. Seen locally on 2026-10-20, a different one of the two failing on
  each run; CI has passed so far by timing. Fix: give both one shared lock and
  one process-wide config dir (or merge them, as the doc comment on
  `connection_filesystem_roundtrip` already says the filesystem tests should
  be).
- **`isolated_data_dir` in `sqeel-core`'s `state` tests races the same way.**
  It makes a fresh tempdir on every call and points `XDG_DATA_HOME` at it with
  `std::env::set_var` while the other tests run in parallel, so one test can
  write under another's (possibly already deleted) directory.
  `persist_result_filed_under_query_connection_not_active` failed once in a full
  `cargo test --workspace` on 2026-10-20 and passed run alone. Its `SAFETY`
  comment says the tests share one tempdir because the setter is idempotent;
  neither is true, and `set_var` while other threads read the environment is
  exactly what makes it `unsafe`. Fix: one process-wide dir behind a
  `OnceLock`, set once, or route the data dir through an override like
  `sqeel-config`'s instead of the environment.
