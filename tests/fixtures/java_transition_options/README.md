# java_transition_options

B67 of docs/OSS_CORPUS.md: the JavaOptions rules_java's analysis tests
transition on are configuration options — `--java_header_compilation`,
`--use_ijars`, `--java_deps`, `--experimental_one_version_enforcement`,
`--one_version_enforcement_on_java_tests`,
`--incompatible_disallow_java_import_exports`,
`--experimental_strict_java_deps`, `--java_launcher` and
`--host_java_launcher` (crate::configuration::VALUE_OPTIONS): Starlark
transitions write them, `config_setting(values)` and the ST- hash see them,
`ctx.fragments.java` reads them, `configuration_field("java", "launcher")`
resolves `--java_launcher`, and the exec transition propagates them (the
exec `--java_launcher` is `--host_java_launcher`).

Captured with genuine 9.3.1 (`--batch --ignore_all_rc_files`, flags
`--platforms=//:p --host_platform=//:p --host_java_launcher=//:host_launcher`):

- `plain.txt`: `bazel-bin/plain.txt` of `build //:plain` (the defaults);
- `transitioned.txt`: the `transitioned.txt` of `build //:java` (a
  transition on the non-experimental options, with an exec tool);
- `shown.txt`: the lines after the separator of `bazel-testlogs/shown/test.log`
  of `test //:shown` (an analysis test's `analysis_test_transition` on the
  `--experimental_*`/`--incompatible_*` options, which only analysis tests
  and builtins may transition on), which fails with them as its message.

Pinned by tests/java_transition_options.rs.
