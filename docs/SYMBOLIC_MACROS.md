# Bounded symbolic macros

This profile implements real macro expansion and declaration ownership, not a
loading-only `macro` placeholder. It remains a subset of Bazel 9.3 symbolic
macros; user-defined attributes, inheritance, finalizers and nested symbolic
macro invocations are outside this increment.

## Construction and execution

The constructor is available in BUILD-loaded and MODULE-loaded `.bzl` files,
and requires `.bzl` initialization. It is not installed in BUILD or MODULE files
themselves. Implementations must be Starlark functions; private helpers and
closures are supported, native builtins and callable descriptors are not.

The supported schema has only implicit `name` and `visibility`. Empty `attrs`
and nonreserved entries removed with `None` are accepted. Explicit `name` or
`visibility` descriptors are rejected even when their value is `None`.
Nonempty effective attribute schemas, non-`None` inheritance and true finalizers
are explicitly unsupported rather than silently ignoring configuration.

The first global assignment establishes export identity, including a private
underscore-prefixed assignment. Aliases and reexports preserve it. A descriptor
only held in a container is not exported. When an imported factory constructs a
macro, its definition package belongs to the initializing `.bzl` file, not the
factory implementation's lexical package.

Invocation is BUILD-only, with keyword arguments `name` and optional `visibility`.
The implementation receives those two named arguments and must return `None`.
The name is a required string. Unlike the pinned reference's crash on `name=None`,
the native implementation rejects that malformed invocation with a controlled
error; this is an intentional robustness difference.

## Visibility and source existence

Invocation visibility uses the explicit value or the package default at the time
of the call. `None` means default; an empty list means private. The caller package
is added as an exact grant unless already present. Public visibility remains
public; private tokens are removed when other grants exist. The callback receives
a **frozen list of canonical Labels**, sorted by repository, package and name.
Repeated nonprivate labels remain repeated, and an ancestor's recursive grant
does not suppress the caller's exact grant. Caller-owned lists are not frozen.

Macro-generated rules with omitted visibility are private regardless of BUILD
package defaults. Their effective visibility includes the macro definition
package. Dependency access uses the producer and consumer definition packages
for macro-owned rules, not their physical BUILD packages. Thus a caller-package
rule cannot gain access to a macro-private target solely by sharing its physical
package. Generated outputs inherit their generating rule's ownership.

Existence is checked separately from access. A macro-generated rule cannot create
an implicit source target merely by referencing a physical file. Sources must be
explicitly exported outside the macro or established by an ordinary declaration's
supported dependency-label attributes, including defaults. Ordinary declarations
need not be selected for analysis and may appear before or after the macro call.
Canonical repository and package identities must match. Reserved macro names
cannot become implicit input targets.

Later `package(default_visibility=...)` calls preserve the existing ordinary-rule
behavior but do not retroactively change a macro's earlier visibility snapshot.
Package groups and configured visibility remain outside the existing bounded
visibility profile.

## Names and phase restrictions

Each invocation reserves its exact name even if it declares no targets. Existing
rules, predeclared outputs, exported sources and other macros cannot share that
name. The invocation may declare its own same-name target. Overlapping prefixes
alone do not reserve one another's names or confer ownership.

A macro-owned target name must equal the invocation name or start with that name
followed by `_`, `-` or `.` and a nonempty suffix. Violations remain query-visible
but fail when analyzed. An otherwise valid rule may produce an out-of-namespace
output; explicitly building that output fails. A valid output name cannot bypass
an invalid generating rule. Checks precede analysis memoization, so root order
cannot hide an invalid requested output. Dynamic action artifact filenames are
not independently declared package targets and are not subject to this check.

`glob()` and `package()` are forbidden inside ordinary symbolic macros, including
through legacy helpers. `native.package_name()` continues to identify the physical
caller package. In-macro `exports_files()` is explicitly unsupported by this
profile, although Bazel allows it. Ordinary legacy helper behavior is unchanged.

## Evidence and limits

Pinned reference constructor/phase probes are under
`/tmp/opencode/next-d-macro-contract/`; output, collision, visibility and implicit
source probes are under `/tmp/opencode/symbolic-macro-edge-contract/`.
The native/reference matrix passes eight families and 108 phases, with 191 actual
CLI commands, in `artifacts/symbolic-macros-validation.json`. It records frozen
source/binary bindings, reference shutdown and scratch cleanup. The final gate
passes 838 Rust and 469 Python tests, including 17 real CLI macro tests and 23
harness tests. Seven adjacent suites pass on the same candidate.

The preserved application query advances past
the `macro` global and stops at missing `PackageSpecificationInfo` in generated
globals code. See `artifacts/mainrepo-symbolic-macros-loading.json`; checkout,
required symlink contents and scratch cleanup remain verified.
Full application builds/tests, complete macro compatibility, SDKs, configured
analysis and performance parity remain unproven.
