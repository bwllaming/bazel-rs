# Genuine Bazel 9.3.0 receipt

Captured 2026-10-26 in independent `/tmp/opencode/cquery-td-mapping-capture/ws`,
copied verbatim from this MODULE/BUILD/repos.bzl/dep fixture. No network sources
or repository-rule execution required. Official genuine ELF:
`/home/coder/bazel/.direnv/reference/bazel-9.3.0-linux-x86_64`.

```
env -i PATH=/usr/bin:/bin HOME=/tmp/opencode/cquery-td-mapping-capture/home \
  /home/coder/bazel/.direnv/reference/bazel-9.3.0-linux-x86_64 \
  --batch --ignore_all_rc_files \
  --output_user_root=/tmp/opencode/cquery-td-mapping-capture/user \
  --host_jvm_args=-Xmx512m mod dump_repo_mapping '' \
  --registry=file:///tmp/opencode/bazel-cquery-td-support/tests/fixtures/builtin_modules/registry \
  --lockfile_mode=off --repository_disable_download --incompatible_autoload_externally=
```

Exit 0, stdout exactly:

```
{"generated":"+ext+generated","":"","root_alias":"","renamed":"dep+","bazel_tools":"bazel_tools"}
```

JSON is compact plus one newline; order is extension imports, root alias names,
then module dependencies including the implicit bazel_tools. Canonical root is
empty, not `_main`. This pins `tests/td_support.rs`. The repository rule's fail()
is a sentinel: computing the mapping does not materialize its contents.

Real target-determinator v0.34.0 full differential scratch driver and receipts:
`/tmp/opencode/perf-research/cquery-td-check.py`,
`/tmp/opencode/perf-research/CQUERY-TD-ACCEPTANCE.md`.
