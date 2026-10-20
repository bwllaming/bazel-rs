# Tracked repository environment profile

This bounded profile implements real environment-dependent repository execution,
not merely declaration metadata or a fabricated environment dictionary.

## Supported behavior

`repository_rule(environ=...)` snapshots a list or tuple of string names without
reading their values. Actual repository execution observes declared names, even
when the callback does not read them. `repository_ctx.getenv(name, default=None)`
also tracks undeclared reads; both arguments are positional-only. Missing and
present-empty values are distinct, and defaults are not substituted into recorded
observations.

The shared repository owner keeps the first observation of each admitted name:
value, absence or sanitized error. Clones and source-cache reconfiguration retain
that owner; independent configured runtimes observe afresh. This is lazy first-
observation behavior, not an atomic snapshot of the process environment at startup.

A warm native CLI invocation reuses an unchanged publication without executing
its callback (N61); declared and dynamic value changes refetch it. This does not
implement Bazel's persistent-server callback suppression.
Declared and dynamic inputs affect repository identity even when output bytes
remain constant; unrelated observations from other callbacks must not do so.

## Privacy, identity and resource boundaries

Only requested names are read; the process environment is never enumerated.
Observation-state Debug output and getenv diagnostics redact captured values.
Successful callback watch sets produce a framed, merged SHA-256 digest; raw values
and named per-variable hashes are not serialized as environment inputs. An empty
watch set preserves the existing environment-free inner serialization. The later
[host fingerprint envelope](HOST_OS.md) intentionally rekeys all repositories once;
within that profile, environment-relative identity behavior remains unchanged.

The digest is an identity hash, not encryption or protection against guessing
low-entropy values. Explicit Starlark `print`, `fail` or requested file output is
outside observation-redaction guarantees. This is not taint tracking: passing a
returned value to another API can expose it through that API's output or errors;
existing template-input records still retain requested file bytes. Publication
manifests remain inventory-only. Verification uses synthetic values, not actual
credentials or GOROOT.

The initial owned allocation inside the OS lookup cannot be precharged. Returned
bytes are inspected and budgeted before conversion, retained copies and cache
insertion. The bounded native profile permits ASCII names/values, with sanitized
errors for unsupported encoding. Names are bounded to 4 KiB, values to 1 MiB and
distinct names to 10,000 per owner. The 10,000-observation callback limit counts
declarations plus dynamic reads, including repetitions. These and the cumulative
weighted-allocation quotas are native safety bounds, not Bazel semantic limits
or a process memory sandbox.

## Explicit exclusions

`os.environ` is the whole client environment (see [HOST_OS.md](HOST_OS.md)),
never an empty or partial dictionary. `module_extension(environ=[...])` and
`module_ctx.getenv` use this same bounded observation profile; their digest is
part of the extension provenance. `--repo_env` (N40, [RC_OPTIONS.md](RC_OPTIONS.md))
changes this environment for the whole command, before the first observation.
A repository rule called by a module extension is distinct from the extension
callback itself.

`--repo_env` values are never echoed: rc notes redact them, an `Unknown startup
option` error shows `--repo_env=NAME=<redacted>`, and non-UTF-8 arguments fail
before any parsing. The library `module_cli::run` entry still rejects
`--repo_env` with a fixed value-free message.

Reference contracts are preserved in
`artifacts/repository-environ-contract.json` and
`artifacts/repository-environ-name-contract.json`.

## Verification

The current gate passes **1,149 Rust and 671 Python tests**, including 18 real
environment-driven CLI groups, four override-privacy tests, 25 environment-harness
tests and 15 local-repository harness tests. The bounded comparison passes eight
families, 37 probes, 67 phases and 353 commands (125 native, 228 reference).
Sixteen adjacent suites pass on the same frozen binary.

The comparison separately verifies changed repository identity and consumer action
reuse: a watched change with identical output bytes changes repository identity
without forcing an otherwise identical consumer action to execute. Earlier failed
preflights and harness expectations are preserved, not rewritten as passes.

The analyzer retains the exact twelve known dependency diagnostics, with zero new
or root-source errors. All reference clients, servers and owned scratch are cleaned.
The application probe uses an explicit isolated child environment—not the user's
ambient environment—and preserves checkout contents and the required Git symlink.
It advances to missing `attr.string_dict` in `rules_go`; this supersedes the earlier
debug-only `ctx.os` boundary and is not application build/test success.

See `artifacts/repository-environment-validation.json`,
`artifacts/k-environment-verification-manifest.json` and
`artifacts/mainrepo-repository-environment-loading.json`. Full Bazel parity and
performance goals remain unfinished.
