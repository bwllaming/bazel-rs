# Custom-rule milestone: scoped differential evidence

This is an experimental native implementation, not full Bazel parity or proof
that the main application builds. Official Bazel **9.3.0** is a separate test
oracle, never an execution fallback.

## Recorded evidence

The inspected `artifacts/custom-validation.json` run finished on
**2026-10-12 at 22:33:31 UTC**: **PASS, 39 checks**, `errors: []`,
`scratch_cleaned: true`. Its 160 command records include argv, return codes,
stdout/stderr and elapsed seconds. Expected negative cases have nonzero exits;
“no errors” means no harness errors, not that every command succeeded.

`artifacts/loading-validation.json` separately records **PASS, 24 checks**,
`errors: []` and cleaned scratch space. These are report check counts, not Rust
or Python test totals. Neither report establishes a complete module-graph
differential result.

The custom report identifies its tested executables by SHA256:

| Executable | Bytes | SHA256 |
| --- | ---: | --- |
| Native release binary | 11216480 | `fab150e41b4c4003be9c8d7ae82074e377777e5ef6ae8b08ba085c9efbbad126` |
| Official Bazel 9.3.0 | 77023347 | `d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8` |

These identify that run, not necessarily a subsequently rebuilt binary. Output
records retain byte lengths, SHA256 and base64 bytes even after scratch cleanup.
For example, both engines produced the same 36-byte `chain-final.txt`, SHA256
`e94c7b10086f479406992d875da6352aa9d2fcf0f8b49347fa0af8106a669369`.
After the fixture source edit it became 44 bytes, SHA256
`e13d1f700bc38cd3d3a1b5873a94f8c7ceae24e8118d7dd5bf7223b00165bf91`.
Absolute generated `File.path` values are passed to commands, not compared as
portable output content.

## Implemented custom-rule scope

- Typed string/list/int/bool/label/output attrs; declarations and caller values
  are snapshotted. Explicit `None` falls back to the attribute default and cannot
  bypass mandatory attrs. Label defaults resolve relative to their definition;
  caller strings remain caller-relative and immutable `Label` values preserve
  their identity through macros. Main-repository Label strings use `@@//`.
- Implementations run during analysis, not query. `ctx.attr`, `ctx.files`,
  `ctx.file`, `ctx.outputs` and `ctx.label`, bare/list `DefaultInfo`, empty
  defaults and explicitly requested predeclared outputs are supported.
- Custom providers survive freezing, preserve identity across package loads and
  support membership/indexing. Provider-held generated files remain dependencies
  even when absent from the producer's default outputs. Invalid fields, missing
  or duplicate providers and frozen-value mutation fail explicitly.
- Persistent-DAG depsets implement `default`, `postorder`, `preorder` and
  `topological`, with homogeneous File/string/Label/bool/int32/immutable-tuple
  elements. Artifact identity survives propagation. This broader implementation
  has native tests; the differential report's stated depset scope is postorder,
  not exhaustive differential validation of every order/type combination.
- Native `Args.add`, `add_all` and `add_joined` support formatting, mapping and
  deduplication. Analysis retains a registry of actual top-level mapper function
  identities, including imported/private helpers; unverified closures require
  `allow_closure=True`. Registering an action snapshots its arguments.
  `artifacts/args-mutation-reference.json` independently records official output
  `first.txt = "first\n"` and `second.txt = "first\nsecond\n"`; the custom suite
  compares both outputs and warm-cache reuse. It does not prove all Args features
  against the oracle.
- `declare_file` includes sibling placement. Native `write` preserves bytes and
  executable mode. `run` launches a program directly with literal arguments and
  environment (File or string executable); it does not interpolate a shell.
  Custom `run_shell` does not inherit genrule's `bash -e -o pipefail` flags.
  Only actions needed by requested outputs execute.
- Local action keys use `native-action-v4`. Cache hits verify output bytes and
  executable modes. Linux `ETXTBSY` **spawn failures only** receive at most eight
  attempts (seven sleeps: 1, 2, 4, 8, 16, 32, 64 ms; 127 ms total requested
  backoff). Successful spawns, wait errors and process exit failures are never
  retried. This bounded startup workaround is not an action retry policy.

The suite covers a four-action custom/native chain with a forward action
dependency and independent branch, empty defaults, hidden output selection,
provider-only files, direct execution, label resolution and negative cases.
The foreign-source-output fixture is a deliberate exception to diagnostic
comparison: Bazel 9.3 crashes internally (`Source in outputs`, exit 37), while
native analysis rejects it. Its PASS means **failure outcome only**, explicitly
`diagnostic_parity: false`.

## Cold/warm observations, not a speed claim

For `//:chain_all`, native command elapsed times in the custom report are:

| Cache/run | Native jobs=1 | Native jobs=4 | Executed / cache hits |
| --- | ---: | ---: | --- |
| Fresh action cache | 17.277 ms | 16.445 ms | 4 / 0 |
| Warm rebuild | 6.598 ms | 5.987 ms | 0 / 4 |
| Fixture source changed | 18.227 ms | 16.514 ms | 2 / 2 |

These are individual process measurements (run IDs 5/8/17 and 11/14/20), not
medians or OS-cold benchmarks. Each jobs setting has its own native cache. The
reference server/cache is reused: its first chain build took 1.206921 s and its
next rebuild 0.063740 s, after earlier query/startup work. Those are not matched
cold conditions. Action counts do not prove wall-clock concurrency, and Bazel
internal action totals are not equated with native totals. No full-repository
speedup or scaling conclusion follows from these tiny fixtures.

## Source-checkout preservation

The custom harness runs private fixture copies and does not probe `~/code`.
The separate loading harness queries/builds six allowlisted source-only targets
without executing repository scripts. Its `repository_state.unchanged` is true;
before/after status and staged/unstaged diff hashes match. Existing checkout
changes were preserved, not reverted. This is the report's observation, not a
claim about unrelated concurrent edits or a comprehensive content snapshot.

Reference invocations bypass the repository wrapper and rc files, isolate
output roots, disable lockfile writes, convenience symlinks, remote cache/
executor/BES writes and known telemetry channels, and shut down their own
server. These controls are not a network or security sandbox. No application
test suite, deployment or full-repository build is validated here.

## Separate module increment: implemented, validation in progress

`MODULE.bazel` evaluation records dependency, override, extension-usage and
registration metadata. The local resolver additionally implements explicit
module discovery, minimal version selection (MVS), nodep fixed-point discovery,
multiple-version override selection and pruning to the selected reachable
graph. Neither metadata recording nor selection materializes repositories.

The new CLI profile is:

```sh
bazel-rs --ignore_all_rc_files mod graph --output=json
# When registry-backed metadata is needed, supply local registries explicitly:
bazel-rs --ignore_all_rc_files mod graph --output=json --registry=file:///absolute/path
```

Registries are optional for graphs whose metadata is available without them,
such as local-path overrides; there is no implicit network registry. This is
selected-module JSON only, with hidden extension information, not full `mod`
parity. Implicit built-in modules, extension execution, unused-module/verbose
views and text output are unsupported. Active extension usages fail even with
`--extension_info=hidden`. Repository downloading/materialization, repository
rules and external BUILD/`.bzl` loading remain unsupported.

**Module differential validation and full-graph review remain separate work in
progress.** The custom-rule/None-default increment does not establish their
completion, and this document makes no external-repository implementation claim.

## Reproduction and evidence sources

```sh
direnv exec . cargo test --locked
direnv exec . cargo clippy --locked --all-targets -- -D warnings
direnv exec . cargo build --locked --release
./scripts/fetch-bazel.sh
python3 scripts/validate_custom.py
python3 scripts/validate_loading.py
python3 -m unittest discover -s tests -p 'test_*.py'
```

These are reproduction commands, not an assertion that this documentation edit
reran them. Reviewed evidence: `artifacts/custom-validation.json`,
`artifacts/loading-validation.json`, `artifacts/args-mutation-reference.json`,
`scripts/validate_custom.py`, `tests/custom_rules.rs`, and the implementations in
`src/custom_rule.rs`, `src/starlark_types.rs`, `src/args.rs`, `src/analysis.rs`,
`src/action.rs`, `src/module_graph.rs` and `src/module_cli.rs`.
