# java_classpath_options

B67 of docs/OSS_CORPUS.md: `--experimental_java_classpath` (old name
`--java_classpath`) and `--experimental_run_android_lint_on_java_rules` are
configuration options (crate::configuration::VALUE_OPTIONS): the command
line and `analysis_test_transition` set them, `config_setting(values)` and
the ST- hash see them, `ctx.fragments.java.run_android_lint` reads the
latter (`reduce_java_classpath()` is private to rules_java), and the exec
transition propagates both.

Captured with genuine 9.3.1 (`--batch --ignore_all_rc_files`, flags
`--platforms=//:p --host_platform=//:p`):

- `plain.txt`: `bazel-bin/plain.txt` of `build //:plain` (the defaults);
- `command_line.txt`: the same with `--experimental_java_classpath=off
  --experimental_run_android_lint_on_java_rules`;
- `old_name.txt`: the same with `--java_classpath=bazel_no_fallback`;
- `shown.txt`: the lines after the separator of
  `bazel-testlogs/shown/test.log` of `test //:shown` (an analysis test's
  transition to `javabuilder` and lint on), which fails with them as its
  message.

Pinned by tests/java_classpath_options.rs.
