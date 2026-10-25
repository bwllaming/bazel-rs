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
(Bazel: `external/<name>`; native: the same slot `<cache>/repositories/external/
<name>`, docs/REPOSITORY_SLOTS.md), so the child resolves such a value there and
writes its `$HOME` files into the repository itself.
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

Checked without ptrace (inotify on the former `direct/stages/` during cold
fetches of `//src/beyondwork.ai/utils/grpcfx:all` on ~/code): with a relative
`HOME` the first fetched module's directory receives
`runs/<name>-home/.config/go/telemetry/local/*.count` and `upload.token`, then
`fetch_repo` reports the mismatch and removes the module's cache directory;
with an absolute `HOME` no `.config` appears in any repository directory.

### The shared GOPATH across processes (implemented)

gazelle's `go_repository_cache` records its own repository directory as
`GOPATH` (and `GOCACHE` below it) in `go.env`; every `go_repository` fetch
(`fetch_repo`, `go mod download`) and `go_repository_tools` run `go` with that
environment and write the module cache there. Before the slots that directory
was the private stage `direct/stages/<slug>`, which a re-execution of
`go_repository_cache` in another native process on the same cache deleted
under running fetches, or snapshotted with part of a foreign module cache
(Bazel's single server per output base cannot interleave so).

Now (docs/REPOSITORY_SLOTS.md) the directory is the repository's slot
`<cache>/repositories/external/<name>`, genuine's `external/<name>`. The
store detects at publication that the published bytes contain the slot's own
path (self-referencing): the slot stays a real, mutable directory, a copy is
published as the tree, and a marker records it. Readers and writers
coordinate on the slot's locks: a re-execution (or a replay, or remaking a
missing slot) takes `direct/locks/slot-<slug>.gate`, then `slot-<slug>`
exclusively; every `execute` of another repository that resolved a label into
a self-referencing repository (or names a mutable slot in its arguments,
environment or working directory) holds that slot shared while its
subprocess runs — not for the whole execution, which could deadlock across
processes (go_repository reads `go.env`, then demands go_repository_tools,
which reads `go.env` too). So a re-execution waits for running consumer
subprocesses, consumers wait for a running re-execution, and a waiting
writer's gate holds new readers off. tests/repository_slots.rs exercises both
directions with two processes on one cache (fails without the readers).
No subprocess outlives its hold: the process-wrapper kills what an `execute`
leaves (docs/PROCESS_WRAPPER.md). Not covered: build actions writing into a
slot, and a tool that finds a mutable slot's path at run time without a label
into it or its path in its arguments, environment or working directory.

**Git worktrees make the hazard ordinary (docs/WORKTREES.md).** The linked
worktrees of a repository share their main checkout's cache by default, so
"a second native process on the same cache" is any build in another
worktree. Two worktrees on commits with the same Go SDK and gazelle
`go_repository_cache` specification (the usual case) never re-execute it
and are safe. A worktree on a branch that changes what `go_repository_cache`
is keyed by — a Go SDK or gazelle version bump, a changed `go_env` —
executes it once for that new specification in the shared cache (every
specification keeps its own publication afterwards, but all of them name
the same stage path as their live `GOPATH`): while that execution runs, a
concurrent build in another worktree can lose or contaminate its module
downloads as described above. Until the lease exists, do not build such a
branch for the first time while another worktree of the repository builds
(or give that worktree its own `--cache-dir`).

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
