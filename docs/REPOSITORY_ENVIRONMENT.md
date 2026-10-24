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

## Relative paths in the client environment

The client environment reaches repository rules verbatim, as in Bazel 9.3
(the client forwards its environment as `--client_env`,
`option_processor.cc:723-726`; `CommandEnvironment.java:333-346` builds the
repository environment from it): a relative `HOME` or `TMPDIR` is not made
absolute. `repository_ctx.execute` runs its child in the repository directory
(Bazel: `external/<name>`; native: the private stage), so the child resolves
such a value there and writes its `$HOME` files into the repository itself.
Genuine Bazel 9.3.0 does exactly this (scratch fixture: `HOME=rel/home` puts
`external/+r+r/rel/home/.config/go/counter` into the repository).

Symptom: every gazelle `go_repository` fetch fails with a different
`fetch_repo: resulting module with sum h1:...; expected sum h1:...` on each
attempt — under genuine Bazel too (two attempts, two different sums; the same
fetch passes with an absolute `HOME`) — and seemingly passes "under strace",
because that invocation used absolute paths. `fetch_repo` runs
`go mod download` with the client's `HOME`; the Go command's telemetry
(`$HOME/.config/go/telemetry/local/*.count`, `upload.token`, `weekends`;
time-dependent bytes) then lands in the module directory that `fetch_repo`
hashes with `dirhash.HashDir`. This was the "nondeterministic Go module
checksum on a cold cache" of the 2026-10-24 PA-1 runs (their harness exported
`HOME=runs/<name>-home` relative to its own working directory; every failing
run's home stayed empty). Run the CLI with absolute `HOME` and `TMPDIR`
(harnesses: resolve per-run directories before exporting them). Test:
`relative_home_reaches_execute_verbatim_and_resolves_in_the_repository`.

Checked without ptrace (inotify on `direct/stages/` during cold fetches of
`//src/beyondwork.ai/utils/grpcfx:all` on ~/code): with a relative `HOME` the
first fetched module's stage receives
`runs/<name>-home/.config/go/telemetry/local/*.count` and `upload.token`, then
`fetch_repo` reports the mismatch and removes the module's cache directory;
with an absolute `HOME` no `.config` appears in any stage. In both runs the
shared Go cache (`GOPATH`, which `go_repository_cache` records as its own stage
path `direct/stages/<slug>`) is created by its stage, renamed away at
publication and recreated by `go`, and never deleted while modules are
fetched. A latent hazard remains across processes: a second native process on
the same cache that re-executes `go_repository_cache` starts its fresh stage at
that path and so deletes the module cache under the first process's fetches
(Bazel's single server per output base cannot interleave so).

The reverse direction exists as well. While one process re-executes
`go_repository_cache` (under its exclusive `direct/locks/stage-<slug>`, in a
fresh stage at exactly that path), another process's `go` — a `go_repository`
fetch (`fetch_repo`, `go mod download`) whose environment comes from the
published `go.env` — writes `pkg/mod/` and the build cache (`GOCACHE`,
`<stage>/gocache`) into the live stage. The re-executing `go_repository_cache`
can then snapshot part of a foreign module cache into its tree (content, and
an identity, that depend on the other process's timing) or fail mid-snapshot
when a file changes or disappears while it is hashed; once its publication
renames the stage away, the other process's `go` loses what it had written
there.

Planned fix (not implemented): a reader/writer lease on the stage lock. A
consumer that resolves a label into a direct repository whose published
content embeds that repository's own stage path (as `go.env` does; detectable
at publication, when the stage path's bytes occur in a published file) takes
`direct/locks/stage-<slug>` SHARED once that repository is materialized and
holds it until its own `materialize_inner` returns, i.e. for as long as its
rule may run tools that write below that path; writers — a re-execution of
that repository (`lock_stage`, then `fresh_stage`) — keep taking it
EXCLUSIVELY. A re-execution then waits for running consumers and consumers
wait for a running re-execution, in both directions and across processes,
and a consumer never waits for a writer of its own (the lease is taken only
after the repository's own materialization returned). Persistent
per-repository working directories (a `GOPATH` outside every stage) would
remove the shared path altogether.

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
