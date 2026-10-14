# Callback host metadata and fingerprints

This bounded implementation targets native Linux/x86_64 builds. It reports
`name="linux"` and `arch="amd64"`, matching the pinned reference getters. It is
not runtime platform detection and does not detect or claim emulation support.
The isolated worktree verification below is complete; combined integration remains
a separate gate.

Repository and ordinary module-extension callbacks receive `ctx.os`, a real
immutable `repository_os` value retained once per context. Its identity/hash
survives freezing; equal metadata does not merge distinct context objects.
`name` and `arch` return actual captured metadata. `environ` remains present in
structural introspection but reading it raises an explicit unsupported error;
`getattr(..., default)` cannot substitute a fake empty dictionary.

No process environment, subprocess or JVM is consulted to provide this verified
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

This adds no BUILD/native/analysis globals, OS environment dictionary, execution
or download SDK, module-context getenv, or CLI host overrides. Repository getenv
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
matrices pass; the superseded `ctx.os.name` negative is now the intentionally
unsupported `ctx.os.environ` boundary, with publication barriers retained.

Analyzer results retain the exact twelve known dependency diagnostics and no new
root-source errors. Failed fixture/classifier attempts are preserved. The primary
and regression drivers clean their owned clients, servers and scratch. Receipts
are in this worktree's `artifacts/host-os-verification-manifest.json`,
`host-os-validation.json` and `host-os-native-unit-verification.json`.
