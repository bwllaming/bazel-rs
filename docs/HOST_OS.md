# Callback host metadata and fingerprints

This bounded implementation targets native Linux/x86_64 builds. It reports
`name="linux"` and `arch="amd64"`, matching the pinned reference getters. It is
not runtime platform detection and does not detect or claim emulation support.
The isolated worktree verification below is complete; combined integration remains
a separate gate.

Repository and ordinary module-extension callbacks receive `ctx.os`, a real
immutable `repository_os` value retained once per context. Its identity/hash
survives freezing; equal metadata does not merge distinct context objects.
`name` and `arch` return actual captured metadata.

`repository_ctx.os.environ` and `module_ctx.os.environ` are Bazel's `repoEnv`
(StarlarkOS.java:44-58, StarlarkBaseExternalContext.java:194-196): every access
returns a NEW mutable dictionary of the complete client environment (assignment
and `pop()` succeed and are invisible to the next access, as captured), keys in
byte (Java `TreeMap`) order. Names and values are decoded the way the JVM decodes
Bazel's `--client_env` arguments: UTF-8 with malformed bytes replaced by U+FFFD,
re-encoded as Bazel's internal Latin-1 bytes (BlazeRuntime.java:915-916,
StringEncoding.java:116-120), so a raw `0xff` reaches Starlark, files, `execute`
arguments and child environments as the three bytes `EF BF BD`; valid UTF-8 is
unchanged. This is the captured `--batch`/JVM-argv behavior; a persistent Bazel
server receives the same arguments over gRPC as raw Latin-1 bytes
(GrpcServerImpl.java:601-605,784-786) and would keep `0xff`; the native CLI is
serverless and follows the JVM-argv form. The map is the process environment with
`--repo_env` applied (N40, [RC_OPTIONS.md](RC_OPTIONS.md); `--action_env` is not a
native option and `--experimental_strict_repo_env` defaults to false,
CommonCommandOptions.java:624-651, CommandEnvironment.java:342-416), fixed once
per invocation and shared by every repository of that invocation. A snapshot failure (bounds:
10,000 entries, 1 MiB per value, 16 MiB total) is cached and reported only when
`environ`, `which` or `execute` needs it. Like Bazel, reading `environ` records no
dependency; the native store re-executes every repository in every invocation, so
a changed environment is still observed (Bazel may instead serve a cached
repository until a recorded input changes). `module_ctx.getenv` and
`module_extension(environ=[...])` record extension environment inputs exactly like
`repository_ctx.getenv` and `repository_rule(environ=...)` (a value-free digest in
the extension provenance; values are never persisted).

No subprocess or JVM is consulted to provide this verified
compile-target view. Unsupported compile targets reject when metadata is needed.
Synthetic host injection is private test machinery, not evidence of parity on
other platforms or a user override.

## Independent identity contracts

`module_extension(os_dependent=..., arch_dependent=...)` retains both named boolean
flags, defaulting to false. Only declared dimensions enter ordinary extension
preparation factors. Reading metadata does not implicitly enable either flag.
Canonical OS factors are represented separately from the getter's JVM-shaped name;
their equality on Linux is not a general cross-platform assumption.

Every executed repository, direct or extension-generated, hashes both host name
and architecture whether read or not. A versioned `repository-host-v1` envelope
surrounds existing execution inputs. This intentionally rekeys publications once
relative to the prior environment-only profile. Existing execution-input bytes,
environment-watch semantics, outer SHA-256 framing and the four-field publication
manifest remain unchanged. Object identities never enter these fingerprints.

Fresh native CLI callbacks still execute before publication reuse. Constant output
bytes can reuse a consumer action even when repository identity changes; physical
repository paths are not substituted for logical action inputs.

## Scope and evidence

The genuine contract is `artifacts/host-os-contract.json` in the integration
workspace. Real callback-to-repository-to-consumer outputs complement native-only
synthetic factor and migration tests; those do not establish other-host runtime
parity. Whole-object JSON rejection was initially source-derived and must not be
advertised as an executed reference result without a matching probe.

This adds no BUILD/native/analysis globals, module-context environment dictionary,
module-context getenv, or CLI host overrides. Repository execution, `which` and
downloads are described in [REPOSITORY_RULES.md](REPOSITORY_RULES.md). Repository getenv
and its privacy/resource boundaries remain independent. Full Bazel compatibility,
application build/test success and performance goals are unfinished.

## Isolated worktree verification

The gate passes 1,181 Rust and 720 Python tests, including 16 actual CLI groups.
The current-bound primary matrix is `SCOPED_CLI_PASS`: seven families, 52 phases
and 124 commands. Both direct and ordinary K-to-L migrations preserve bytes,
modes and inventories while rekeying repository identity; warm reuse and consumer
action-cache behavior are checked independently.

Fifteen named, executed native-unit proofs cover the separate injected factor
matrix, unconditional unread host inputs, identity/freeze ownership, legacy inner
serialization, resource bounds and lazy errors. Synthetic alternate hosts are not
runtime evidence on other platforms. Four repository/environment regression
matrices pass; the superseded `ctx.os.name` negative was later the
`module_ctx.os.environ` boundary, now supported (N21).

N21 adds an eighth validator family, `environ`, a single cold differential state
per probe over the controlled client environment: `direct`/`ordinary` read
`repository_ctx.os.environ`, `extension` reads `module_ctx.os.environ` and
`module_ctx.getenv`, `ctor` declares `module_extension(environ=[...])`. The former
environ/getenv/ctor native-boundary probes were removed. Its genuine reference
phases and the full scoped matrix need recapture; the receipts above predate it.

Analyzer results retain the exact twelve known dependency diagnostics and no new
root-source errors. Failed fixture/classifier attempts are preserved. The primary
and regression drivers clean their owned clients, servers and scratch. Receipts
are in this worktree's `artifacts/host-os-verification-manifest.json`,
`host-os-validation.json` and `host-os-native-unit-verification.json`.
