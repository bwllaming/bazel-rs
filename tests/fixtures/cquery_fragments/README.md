# cquery required-fragments / transitions fixture (genuine Bazel 9.3.1)

- `workspace/`: offline scratch module (the local `platforms` module and
  function-transition allowlist of `../cquery_output`): Starlark rules
  declaring each configuration fragment (`cpp`, `java`, `py`, `bazel_py`,
  `android`, `bazel_android`, `apple`, `objc`, `j2objc`, `coverage`,
  `proto`, `platform`, an unknown name), `config_setting`s over `values`
  (cpu, strip, compilation_mode, java_runtime_version, `define`),
  `define_values`, `flag_values` and `constraint_values`, a rule selecting on
  all of them, a rule transition writing `--copt`, an attribute transition
  reading `compilation_mode` and writing a Starlark flag, an exec attribute,
  a Starlark flag, an alias to it, a `label_flag`, an aspect declaring
  `fragments = ["java"]`, filegroup, genrule, toolchain/toolchain_type,
  test_suite and package_group.
- `cases.tsv`: one case per line (name, expression, options), tab-separated.
- `genuine/<case>.out|rc|stderr`: captured with `capture.sh BAZEL SCRATCH`
  (`--ignore_all_rc_files --batch`, private output root,
  `--repository_disable_download --incompatible_autoload_externally=
  --lockfile_mode=off --platforms=//:linux --host_platform=//:linux`) on
  2026-11-04. ELF: bazel-9.3.1-linux-x86_64, sha256
  `7f775b54bbec487efaf0f49ccda1d2ed02cd34f582edfdd6aeeec76e39ef585c`.
- `tests/cquery_output_formats.rs` (`fragments_and_transitions_match_genuine`)
  compares sorted distinct lines with checksum prefixes and absolute
  workspace/external roots normalized; see that test for the platform
  configuration divergence it filters.
