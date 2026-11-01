# external_locations

Event locations of files in external repositories, pinned by
tests/external_locations.rs.

Genuine capture: Bazel 9.3.1 (`.direnv/reference/bazel-9.3.1-linux-x86_64`,
sha256 7f775b54bbec487efaf0f49ccda1d2ed02cd34f582edfdd6aeeec76e39ef585c),
`--batch --ignore_all_rc_files` with a fresh `--output_user_root`, on a copy of
`workspace/`:

- `genuine_build.txt`: `build //:m //:e //arcold:d //arcuse:d --curses=no
  --color=no` — the exit code and the `DEBUG:` / `WARNING:` lines.
- `genuine_query.txt`: `query --output=location @arc//:a`.

The output base is `<OUTPUT_BASE>`, the workspace `<WORKSPACE>`. Bazel names a
file of another repository below `<output_base>/external/<canonical name>/`:
the `local_path_override` module `ext` (`external/ext+/e.bzl`, although the
files are read from `ext/`), the repository rule's `arc`
(`external/+gen+arc/defs.bzl` and its `BUILD.bazel`, the location of the
deprecation WARNING and of the query); a main-repository `.bzl` file by its
workspace path. bazel-rs prints the same paths below its own output base
(`bazel-rs info output_base`); the order of the events across targets is not
compared.
