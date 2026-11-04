# cquery output-format fixture (genuine Bazel 9.3.1)

- `workspace/`: offline scratch module (local `platforms` module under
  `.bazelignore`, local function-transition allowlist so the native
  explicit-module profile analyzes it): Starlark rule with provider, select on
  a Starlark flag and on `compilation_mode`, exec tool, Starlark attribute
  transition, attribute aspect, alias, genrule/output file, filegroup, and a
  directly/indirectly incompatible target plus an alias to it.
- `cases.tsv`: one case per line (name, expression, options), tab-separated.
- `genuine/<case>.out|rc|stderr`: genuine stdout, exit code and `ERROR`/traceback
  lines, captured with `capture.sh` (`--ignore_all_rc_files --batch`, private
  output root, `--repository_disable_download --incompatible_autoload_externally=
  --lockfile_mode=off --platforms=//:linux --host_platform=//:linux`).
  ELF: bazel-9.3.1-linux-x86_64, sha256
  `7f775b54bbec487efaf0f49ccda1d2ed02cd34f582edfdd6aeeec76e39ef585c`.
- `genuine/{configured,unconfigured}.{streamed_proto,jsonproto,textproto}`: one
  query captured in three formats (with `--transitions=lite`, and with
  `--noproto:include_configurations`) on the BCR-platforms variant of the same
  workspace; tests/cquery_output_text.rs transcodes the streamed records and
  expects genuine's jsonproto/textproto bytes. Paths and checksums record the
  capture scratch directory.
- `build*`, `transitions_*`, `fragments_*` (2026-11-04): `--output=build`,
  `--output=transitions` with `--transitions=none|lite|full` and
  `--show_config_fragments=direct|transitive|direct_host_only` (its
  converter error), same capture options.
- `checksum_poc.py`: recomputes genuine BuildOptions checksums from
  `configured.jsonproto` (docs/CQUERY_OUTPUT.md).
