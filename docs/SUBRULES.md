# Zero-attribute subrule execution profile

This bounded profile executes real subrule callbacks and actions; it is not a
constructor-only placeholder or full subrule/configuration parity.

## Intended supported surface

The keyword-only constructor accepts a Starlark-defined implementation function
and empty `attrs`, `toolchains` and `fragments`, plus directly declared `subrules`.
Factory closures are valid; native callables such as `len` are not. Definitions
retain their frozen implementations and dependency values, exported identity and
aliasing. Subrule values are unhashable.

`rule(subrules=[...])` declares direct call permissions. A rule declaring Outer
does not thereby gain permission to call Outer’s Leaf. Each invocation injects a
fresh subrule context before user arguments and returns the actual callback
result. Generated Files and actions belong to the consuming target.

The constructor belongs in ordinary and MODULE-loaded `.bzl` files, not top-level
BUILD or MODULE globals. Nonempty private attributes, configuration/toolchain/
fragment requirements, aspects, inheritance and run/test APIs remain unsupported
rather than accepted and ignored. The four subrule context property names are
`actions`, `fragments`, `label` and `toolchains`; configuration-property reads
raise explicit unsupported errors in this profile, not fabricated empty data.

## Context and receiver lifetimes

Existing-property reads on a locked parent context fail, including
`getattr(parent, "label", None)`. Structural `hasattr` and `dir` do not invoke
getters; a missing-property default still works. Child contexts expire after
return and do not become valid again during another invocation of the same
subrule. Ordinary-error restoration must preserve the caller’s scope.

Action registration/declaration methods require the current invocation’s
receiver, including captured objects and bound methods. `actions.args()` is an
evidence-backed exception: a captured, still-live ancestor factory remains usable
inside a child. Already-created Args retain the root analysis lifetime and may be
returned to, extended by and consumed by the parent.

Expired child Args factories crash genuine Bazel 9.3 with a null-context exception.
Native controlled rejection is an explicit
**KNOWN_REFERENCE_DEFECT — INTENTIONAL_DIVERGENCE**, not differential PASS or an
attempt to reproduce that crash. No new Rust-panic recovery or frozen runtime
context capability is claimed.

## Evidence and deferred work

`ctx.actions.write` accepts a string or a concrete, active Args builder. Strings
retain their original bytes; Args use the verified default shell-quoted format,
with one LF after each argument. Empty Args writes zero bytes, and an empty
argument writes `''` followed by LF. Quoting preserves embedded newlines and
UTF-8. Receiver validation precedes serialization and action registration.

This does not enable `use_param_file` or `set_param_file_format`; both remain
explicitly unsupported. Existing native Args mappers still expand eagerly,
unlike Bazel's deferred write/action expansion. The write-format evidence in
`artifacts/actions-write-args-contract.json` does not establish mapper-phase parity.

Reference facts and executable fixtures are recorded in
`artifacts/subrule-contract.json`, `artifacts/subrule-scope-contract.json` and
`artifacts/subrule-args-factory-contract.json`. Post-error restoration expectations
derived from Java source remain labeled as such; native host tests must inspect
actual restored state. The original fragment-reading fixture is preserved and
not presented as a supported native case; a separately named no-configuration-
read fixture establishes the intended functional path.

Private dependency attributes require separate default-state, hidden dependency
edge, lexical-resolution and visibility-authority work. The observed exec
label-list reference crashes are not evidence of native support. Explicit
experimental command-line flag control is not implemented; its rejection is not
Bazel’s explicit-false allowlist behavior.

Full Bazel compatibility, application build/test success and performance goals
remain unfinished.

## Verification

The combined gate passes **1,057 Rust and 609 Python tests**, including 27 real
CLI groups and 37 portable harness tests. The frozen comparison records **54
probes, 99 phases and 162 commands**: 95 passing phases, two known reference
crashes and two intentional native controlled-error divergences. Its status is
`PASS_ONLY_BOUNDED_PROFILE`, not unrestricted parity. Thirteen adjacent suites
pass on the same candidate.

Constructor metadata, bounded argument forwarding, parent restoration, actual
File/provider/action outputs, cache/source changes, context guards and method-
specific Args behavior are covered. Test fixtures are tracked; compilation and
unit verification do not depend on ignored research reports. Historical failures
and the original configuration-reading reference fixture remain preserved.

Analyzer diagnostics retain exactly the twelve known pristine-dependency errors,
with zero new or root-source errors. The preserved application query advances to
missing `json` in `rules_go` SDK loading; checkout contents and the required Git
symlink remain unchanged. All I0-owned servers, processes and scratch are cleaned.
See `artifacts/subrules-validation.json`,
`artifacts/i0-subrule-verification-manifest.json` and
`artifacts/mainrepo-subrules-loading.json` for the bound evidence.
