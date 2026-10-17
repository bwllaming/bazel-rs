# RunEnvironmentInfo metadata profile

This bounded native profile implements the metadata behavior below. It is not
operational run/test support or full Bazel parity.

The scope is genuine metadata construction, snapshots, equality, nested provider
transport and explicit action consumption. It does not add a `test` command;
since N23f the `run` command applies a target's `environment` and
`inherited_environment` (docs/RUNFILES_TREES.md). `rule(executable=True)` and
`rule(test=True)` declarations load natively like Bazel (query and the `_test`
rule-class-name check are the validator's differential `rule-kind` family), and
since N23a (docs/RUNFILES.md) such targets are analyzed and built: returning
`DefaultInfo(executable=...)` with RunEnvironmentInfo builds on both engines, and
without an executable both fail with `The rule 'check' is executable. It needs to
create an executable File and pass it as the 'executable' parameter to the
DefaultInfo it returns.` (differential `rule-build` family). Only the `test`
command remains a native-only boundary (`bazel-rs: unsupported command or
startup option "test"; run --help for the native subset`); `run` exists since
N23f (docs/RUNFILES_TREES.md).

## Constructor and ownership

`RunEnvironmentInfo(environment={}, inherited_environment=[])` accepts positional
and named arguments. Environment keys and values must be strings; inherited names
must be a string list or tuple. Explicit `None`, extra arguments and unknown
keywords are rejected. There is no public `error_on_non_executable` parameter.

The constructor is a genuine provider key in BUILD- and MODULE-loaded `.bzl`
environments. It is not predeclared in top-level BUILD or MODULE files; a BUILD
can load an explicitly exported `.bzl` alias.

The original metadata increment retained a loading-policy gap: local-only
queries/builds could skip root `MODULE.bazel` evaluation. The subsequent
[root-validation increment](ROOT_MODULE_VALIDATION.md) closes that bypass before
target loading. The original failed comparison remains unchanged in
`artifacts/run-environment-info-validation-first-attempt.json`; new evidence,
rather than rewritten history, establishes closure.

Instances own copies without freezing caller containers. Both fields return fresh
mutable dictionaries/lists in the current evaluation context, even after the
provider crosses a frozen module or target boundary. Dictionary insertion order,
inherited-name order and duplicates remain observable. A property snapshot saved
as a module global is itself subject to normal module freezing.

Instances have structural equality and hashing. Environment insertion order does
not affect equality; mapping contents and inherited-list order/duplicates do.
Unicode, empty strings and NUL values are valid metadata. Whether a string can be
used in an OS environment is a separate execution concern.

The native provider's representation is part of this contract. The underlying
interpreter still has general Unicode/container formatting differences outside
that provider surface; this increment does not replace its general formatter.

## Functional ordinary-rule use

An ordinary rule may return a custom provider containing this native value:

```starlark
EnvironmentMetadata = provider(fields=["info"])

def _produce(ctx):
    return [EnvironmentMetadata(info=RunEnvironmentInfo(
        {"EXAMPLE": "literal"}, ["EXAMPLE"],
    ))]

def _consume(ctx):
    info = ctx.attr.dep[EnvironmentMetadata].info
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(
        outputs=[out],
        command="printf '%s' \"$EXAMPLE\" > \"$1\"",
        arguments=[out.path],
        env=info.environment,
    )
    return [DefaultInfo(files=depset([out]))]

produce = rule(implementation=_produce)
consume = rule(implementation=_consume, attrs={
    "dep": attr.label(providers=[EnvironmentMetadata]),
})
```

This is explicit rule-authored environment application. Merely constructing the
provider does not change ordinary build actions. Inherited names remain metadata;
this profile neither reads host variables nor performs run/test merging.

The wrapper target provides `EnvironmentMetadata`, **not** `RunEnvironmentInfo`.
Native-key membership/indexing and required-provider predicates must not infer
top-level provider presence from nested values.

## Direct-return policy and remaining execution work

Returning the regular `RunEnvironmentInfo` directly from an ordinary,
non-executable, non-test rule is an analysis error in Bazel 9.3, even alongside
`DefaultInfo`. Native rejection must occur after callback return and before action
execution. Nesting the value in a custom provider is different and is allowed.

Deprecated `testing.TestEnvironment` uses a private compatibility flag and has
different warning/return behavior. It is not implemented here and is not used as
a bypass. Since N23a, executable and test rules (docs/RUNFILES.md) may return
RunEnvironmentInfo directly; `run` applies it since N23f
(RunfilesSupport.computeActionEnvironment, docs/RUNFILES_TREES.md); no `test`
command consumes it yet.

The complete run/test behavior remains future work. In the reference, inherited
values override literals only when present, otherwise literal fallbacks survive.
That verified reference fact is not a claim of native execution support.

## Evidence

`artifacts/run-environment-contract.json` records 35 genuine reference cases,
source pins, corrected exploratory attempts, preservation and cleanup receipts.
The preserved application probe advances past this provider and stops at missing
`set` in generated globals code. See
`artifacts/mainrepo-run-environment-loading.json`; checkout contents and the
required Git symlink are unchanged, and probe scratch is removed. Ten adjacent
suites pass on the same frozen candidate, with a clean pinned analyzer scan.
The original metadata gate passed 923 Rust and 519 Python tests, including 14 real metadata
CLI tests and 29 harness tests. Full gate and source/report bindings are recorded
in `artifacts/run-environment-check.log` and
`artifacts/run-environment-verification-manifest.json`.

The original bounded differential matrix evaluated MODULE through module demand.
It passed seven families, 96 phases and 184 CLI commands in
`artifacts/run-environment-info-validation.json`, with stable source, registry and
executable bindings plus successful shutdown and scratch cleanup. A small tracked
excerpt at `tests/fixtures/run_environment_info/module_gap.json` preserves the
original mismatch's exact phase and command records; the full failed report
remains unchanged. Harness unit tests do not require that ignored full report.
That historical acceptance did not fix the local-query mismatch. The updated
matrix in `artifacts/g-run-environment-info-validation.json` passes seven families,
98 phases and 186 commands, retaining external-demand coverage and adding the
exact local-query case. Required `historical_parity_gaps` preserves the old
native-success/reference-failure observations; `gap_closure` links new native
exit 1/reference exit 48 rejection before loading or actions. The plan now has
nine families and 114 phases: the stale executable/test declaration rejections
became the `rule-kind` and `rule-build` differentials plus the `test`/`run`
command native boundary. The 58 native phases pass against the N23a debug
binary. The preceding eight-family matrix (106 phases, including `rule-kind`)
was rerun against genuine Bazel 9.3.0 with the native release binary built from
`e870d74` (SHA-256 `e1cbcfbc…`) and passed: 196 commands, no errors (status
`PASS`; the run receipt is untracked). The new `rule-build` family's reference
side (predicted from the N23a paired capture) has not been executed yet, so it
is not yet differential evidence and must be run at integration.
Full Bazel parity, whole-application build/test success and broad performance
claims remain unfinished.
