# print_locations

`print()` locations of another repository's `.bzl` file, and `print()` in an
`Args.map_each` callback (crate::debug_print), pinned by
tests/print_locations.rs.

`genuine.txt`: Bazel 9.3.0, a fresh `--output_user_root`, `build //:m //:x
--curses=no --color=no` on a copy of `workspace/`: the exit code and the
`DEBUG:` lines, the output base as `<OUTPUT_BASE>`. Bazel names the `ext`
module's `.bzl` file by its path below `<output_base>/external/ext+/`; the
`map_each` callbacks' prints show nothing (StarlarkCustomCommandLine
.applyMapEach gives their thread a print handler that drops the text), in
the main repository (`in map_each a`, `b`) and in `ext` alike.

Declared native difference: bazel-rs has no `external/` directory; the
location names the file where the repository's tree is (here the
`local_path_override` directory `ext/`).
