# ralphus-test-support

Test-only crate (a `dev-dependency`, never shipped). It owns how tests create
and destroy scratch directories, so no suite leaks folders into `%TEMP%`
(RAL-690; on Windows the profile service walks every file there at logon).

- `TestDir` — RAII directory: stops registered child processes, then deletes
  with retry (Windows sharing violations, read-only git objects), and prints a
  warning if it finally cannot.
- `scratch(leaf)` — drop-in for `std::env::temp_dir().join(leaf)` in tests: the
  path lives under this process's run directory and is deleted when the
  creating thread (the test) ends, panic included.
- `scratch_root()` — the run directory itself, for code that wants "a temp dir".
- First use sweeps leftovers of dead runs (throttled to ~10 minutes through a
  marker file) and legacy flat `ral<N>-*`/`ralphus-*` directories in the system
  temp folder older than 24 h.

Location: `RALPHUS_TEST_TMP` if set, else `%LOCALAPPDATA%\ralphus\test-tmp` on
Windows, else `<system temp>/ralphus-tests`. Each process uses
`<location>/<pid>-<start-secs>/`. `RALPHUS_KEEP_TEST_TMP=1` keeps directories.

`tests/source_scan.rs` fails if `std::env::temp_dir()` appears in `daemon/src`
or `daemon/tests` outside its explicit allow-list; do not extend the list for
test code, use this crate.

Run its tests with `cargo nextest run -p ralphus-test-support`.
