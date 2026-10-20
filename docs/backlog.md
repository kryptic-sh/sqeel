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
