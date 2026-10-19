# rules_go cgo context and cc linking (N57)

N57 = N25d6: every `go_library` runs rules_go's `cgo_context_data_impl`
(`go/private/context.bzl:847`, reached through `go_context()` at
`go/private/context.bzl:639-648` for rules declared with `go_rule`), and a
cgo-enabled build compiles and links through the C++ toolchain. The evidence
is a paired genuine Bazel 9.3 capture
(`tests/fixtures/go_cgo/reference_receipt.json`, fixture layout in
`artifacts/cgo-fixture-probe.py` / `artifacts/cgo-capture.py`), not a parity
claim beyond the recorded scenarios.

## What runs natively (all byte-identical to genuine Bazel 9.3)

The fixture: genuine rules_go 0.63.0 + rules_cc 0.2.17 (module-cache copies
behind `local_path_override`), `go_sdk.host()` over the host Go SDK, so the
C++ toolchain is rules_cc's genuine `@local_config_cc` (host autoconf, F2 in
`docs/CC_TOOLCHAIN.md`).

- `go_library(cgo = True)` with `import "C"`: `cgo_context_data_impl`
  resolves the optional `@@bazel_tools//tools/cpp:toolchain_type` toolchain
  (`CGO_TOOLCHAINS`, `config_common.toolchain_type(..., mandatory = False)`),
  `find_cc_toolchain` (`@rules_cc//cc:find_cc_toolchain.bzl`) returns the
  Starlark `CcToolchainInfo` (rules_cc 0.2.17 routes `cc_common` through
  rules_cc Starlark; `cc_common.configure_features`, `create_compile_variables`
  and `create_link_variables` are Starlark, and the native
  `get_tool_for_action` / `get_memory_inefficient_command_line` /
  `get_environment_variables` / `action_is_enabled` / `empty_variables`
  builtins serve the rest), and the whole `CgoContextInfo` (env with
  `CC`/`CGO_CFLAGS`/`CGO_LDFLAGS`/`PATH`, `cgo_tools` paths and filtered
  option lists) matches genuine value-for-value (capture `context_probe`,
  16/16 print lines equal). `cppopts`-denied compiler options, the
  `_UNSUPPORTED_C_COMPILERS` check and `apple_ensure_options` (a no-op off
  Apple) all run as written.
- The compile outputs: `cgo.a`, `cgo.x`, the generated `_cgo_imports.go` /
  `_cgo_gotypes.go` / `cgo.cgo1.go` intermediates and the cgo standard
  library (`stdlib_/pkg/linux_amd64/runtime/cgo.a`) are byte-identical
  (capture `library` + `stdlib` rows).
- `go_binary` over a cgo library (external linking through
  `cgo_tools.ld_executable_options`, `_cc_runtime_libs_for_mode` /
  `extldflags_from_cc_toolchain`) and `go_test` over cgo (build outputs
  byte-identical; `bazel test` passes natively).
- cgo off: `--@io_bazel_rules_go//go/config:pure=1` builds the same library
  through the pure stdlib path with byte-identical outputs.

## The stdlib package set and execution strategies

The genuine `GoStdlib` action runs `go install std runtime/cgo ...` inside
the replicated GOROOT. Under sandboxing (genuine's default, and the native
profile's semantics) the builder sees exactly the declared inputs, so
rules_go's `STDLIB_SRCS_EXCLUDE` (`go/private/sdk_build_defs.bzl`) filters
the SDK `srcs` filegroup and the stdlib resolves to 350 archives —
byte-identical to the native set. Under `--spawn_strategy=local` genuine
leaks the full SDK `src` through the execroot symlink (an undeclared-input
read) and builds 7 more packages; that is genuine-strategy behavior, not a
native divergence, and is recorded in the receipt's `stdlib` note.

The native profile has no unsandboxed leak path: `--spawn_strategy` (and
`--strategy`) are rejected explicitly — options are never silently ignored —
and every process action runs in a private staging execroot containing only
its declared inputs (`stage_inputs`, docs/HARDLINK_STAGING.md), whatever its
"not sandboxed" progress line means (no linux-sandbox wrapper; the staging
discipline is unchanged). Measured on this tree with
`artifacts/n57-followup-check.py`: the cgo fixture build produces a
**350-archive** stdlib set under the native (unsandboxed) execution —
identical to the captured `stdlib` scenario and to sandboxed genuine.

## Attribute flags (the cc rules' load path)

rules_cc 0.2.17's Starlark rules declare label lists with
`flags = ["ORDER_INDEPENDENT", "DIRECT_COMPILE_TIME_INPUT"]` (for example
`cc_import`'s `hdrs`). N57 implements the load-time semantics:
`ORDER_INDEPENDENT` canonicalizes the converted list value with a natural
sort (labels by `Label.compareTo`: repository, package, name —
`BuildType.convertFromBuildLangType`, packages/BuildType.java:245-253),
applied to explicit BUILD values and simplified unconditional selects;
defaults and analysis-resolved select branches are never sorted
(`tests/attribute_flags.rs`; a label value that fails canonical parsing is
an internal error, never a silently unsorted list). The repository/package
cross-ordering (`//a:x` < `//a-b:x` < `//a/b:x` < `@other//:x`), the
select and default cases are checked against genuine 9.3 by
`artifacts/n57-followup-check.py` (EXACT_PRINT, recorded in the receipt's
`post_capture_changes`). `DIRECT_COMPILE_TIME_INPUT` is accepted and
inert (only `--compile_one_dependency` reads it, and that option is
rejected explicitly — `tests/attribute_flags.rs`).

## Explicit boundaries (fail with the named gap)

- `go_binary(linkmode = "c-archive"/"c-shared")` routes through
  `go_non_executable_binary`, whose `deps`/`embed` attributes declare
  `_go_cc_aspect` (`go/private/rules/binary.bzl:517-518`). Since N58
  (integration-17) `attr(aspects = ...)` propagation is analyzed — the
  private same-file aspect loads, its application resolves the base rule's
  edges including Starlark-transitioned prerequisites (`go_library`'s
  `srcs` go config transition), and `new_cc_import`
  (`binary.bzl:79-102`) runs: the c-archive path now stops inside the
  rules_cc analysis-time C++ API at `cc_internal.create_header_info` with
  header artifacts (`cc_info.bzl:338` `create_compilation_context`),
  `cc_common.create_linker_input` / `create_library_to_link` themselves
  being implemented and proven at the toolchain level.
- `cc_import` / `cc_library` targets (a `go_library` `cdeps` closure):
  analysis runs `create_library_to_link` + `create_linker_input` +
  `create_linking_context` (rules_cc Starlark, frozen lists since N49b) and
  then stops at `cc_common.compile`, whose first native dependency
  `cc_internal.actions2ctx_cheat` fails explicitly (`cc_import` through
  the c-archive aspect reaches `create_header_info` first). The next
  increment
  needs the `cc_internal` surface of `cc/private/compile/*.bzl` and
  `cc/private/link/*.bzl`: `actions2ctx_cheat`,
  `get_artifact_name_for_category`, `freeze`, `combine_cc_toolchain_variables`
  (already implemented), `cc_toolchain_variables` (already implemented),
  `create_cc_compile_action`, `is_tree_artifact`, `dynamic_library_symlink`
  (+`2`/`_soname`), `wrap_link_actions`, `declare_other_output_file`,
  `create_header_info`, `compute_output_name_prefix_dir`,
  `solib_symlink_action`, `per_file_copts`, `intern_seq` and `rule_class`.
