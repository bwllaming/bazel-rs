# Direct nullable `cc_internal.get_link_args` fixture

Captured with genuine Bazel **9.3.0**, ELF SHA256
`d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8`,
2026-10-26. The exact probe is `tools/build_defs/cc/link_args.bzl`, which
uses this fixture's existing `probe.bzl` provider/feature constructors. No
compiler or system C++ toolchain is involved: an action records the executed
argv and any referenced param file using `/usr/bin/python3`.

The compact `link_args_reference.json` factors the common argv/param payload
from the twelve successful None/GCC_QUOTED/UNQUOTED ×
variable/absent/custom/literal cases. Literal mode appends `literal_placeholder`
to argv (None) or the parameter payload (string types); it lacks the linker
variable, so filtering must NOT remove that literal. Variable mode filters the
placeholder flag; custom mode filters its non-placeholder variable value.
Absent/custom modes never split, for any of the three supported argument values.
Param paths are normalized by replacing only the action's output path with
`<OUTPUT>`; no flags or file contents are normalized.

Missing, int, bool, list, invalid text (`invalid`, empty, lowercase), WINDOWS
and SHELL_QUOTED were each captured with and without a placeholder. Missing
is an error, not a None default. Wrong types fail with exit 1. Invalid enum
text crashes genuine with exit 37 / IllegalArgumentException even without
a placeholder; native intentionally returns an analysis error instead.
WINDOWS/SHELL_QUOTED succeed in genuine but remain explicitly unsupported
by the native API. Their rejection is checked in both placeholder modes.

GCC_QUOTED raw bytes differ in the existing implementation: native uses
Shell single quoting, genuine uses `GccParamFileEscaper`. The tests explicitly
retain that behavior, not silently rewrite the genuine expectation. None
argv, UNQUOTED bytes, and param-reference `%` escaping match exactly.

## Capture isolation and reproduction

Raw capture: `/tmp/opencode/cc-link-args-genuine/receipt.json`, with individual
stderr logs in `logs/`. Harness:
`/tmp/opencode/bazel-cc-link-args-none/artifacts/capture-link-args.py`.
It assembles a scratch workspace from `cc_toolchain/vendor_shared.txt`,
`cc_features/vendor`, and `cc_toolchain/vendor`, with the existing
`test_flags/stubs` overrides and `cc_toolchain/stubs/protobuf`. The genuine
rules_license 1.0.0 archive is unpacked locally. All MODULE overrides and the
registry (`tests/fixtures/builtin_modules/registry`) are local; no production
cache, remote service or `~/code` access is involved. Genuine is run in batch
mode, with a private output root/HOME/TMPDIR and a 120s per-command timeout:

```
bazel --batch --ignore_all_rc_files --host_jvm_args=-Xmx2048m \
  --output_user_root=<private>/output build --lockfile_mode=off \
  --registry=file://<tree>/tests/fixtures/builtin_modules/registry \
  --jobs=2 --noshow_progress --color=no --curses=no \
  --incompatible_autoload_externally= //:<case>
```

Source authority: `CcStarlarkInternal.java:407-440` (required nullable argument;
None leaves splitCommandLine false), `LinkCommandLine.java:92-153` (independent
variable filtering and optional metadata), and `ParameterFile.java:37-66`
(enum names), in the pinned `/tmp/opencode/bazel-9.3-reference` tree.
