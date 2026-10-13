# Starlark set compatibility work

The core repair passes the verification gates below. The subsequent production
integration enables sets in BUILD, MODULE and both ordinary/MODULE-loaded `.bzl`
environments. Its frozen-binary comparison passes the bounded profile described
below; this is not unrestricted Bazel compatibility.

Provider payloads containing sets are traversed in order, including generated
Files. Existing cycle/depth guards, first-encounter deduplication and provider
identity checks remain intact. This is not an implicit conversion to a sequence:
use `list(s)` where an action, attribute or other API requires a list/tuple.

## Core contract

The repaired implementation uses the existing managed set value, tracing,
freezing and iteration guards. The constructor accepts zero or one positional
argument, returns a fresh mutable set, and has type
`builtin_function_or_method`. It is unhashable, has no `.type` attribute or type
subscription, and its representation is `<built-in function set>`.

The method surface is `add`, `clear`, `difference`, `difference_update`, `discard`,
`intersection`, `intersection_update`, `isdisjoint`, `issubset`, `issuperset`,
`pop`, `remove`, `symmetric_difference`, `symmetric_difference_update`, `union`
and `update`. Ordering, first-element pop, positional binding and variadic
operands follow the pinned reference contracts.

Bazel's `string.elems()` produces a list and is a valid collection operand. The
native interpreter's character-iterable representation needs a narrow typed
adapter for set methods, including frozen operands. This does not admit arbitrary
iterables or the separate native codepoint iterator, and does not claim to fix
the broader string API's representation differences.

Mutating operations preserve aliases. Validation and mutation commit one operand
at a time: an invalid later operand must not erase earlier valid updates or
partially apply that invalid operand. Frozen and actively iterated receivers
reject mutation, including no-op cases. Ordinary binary operations remain pure.

Augmented operations use assignment-specific interpreter dispatch. They retain
the existing target evaluation and writeback order; they do not rewrite user
syntax or make ordinary binary operators mutate. Both operands must be sets
before entering set mutation; other operands retain the previous binary path.

The host regressions exposed a preexisting iterator-unwind defect: an evaluator
error could leave a retained collection mutation-locked. The correction tracks
individual bytecode iterator acquisitions and releases only the failed call's
acquisitions. Nested/reentrant calls and host-owned iterators must retain their
own locks. This does not introduce Starlark exception handling, Rust-panic
recovery or new support for garbage collection during live iteration.

## Deliberate reference-defect divergence

Bazel 9.3 crashes with `ConcurrentModificationException` and exit 37 for
multi-element self-aliased `s ^= s` and `s.symmetric_difference_update(s)`.
The native behavior safely empties the original receiver, preserving
aliases and mutation guards. This is classified
**KNOWN_REFERENCE_DEFECT — INTENTIONAL_DIVERGENCE**, not differential PASS.
Empty/singleton reference cases succeed; pure self-XOR remains nonmutating.

## Evidence and limits

Pinned research receipts are `artifacts/starlark-set-contract.json`,
`artifacts/starlark-set-edge-contract.json` and
`artifacts/starlark-set-constructor-contract.json`.
Collection-adapter evidence is in
`artifacts/starlark-set-string-elements-contract.json`. The optional Java-host
post-error experiment was not executed because the installed runtime lacks a
compiler; `artifacts/starlark-set-post-error-contract.json` records that limit.
Post-error expectations derived from Java source must remain labeled as such;
native host tests inspect surviving module state after evaluator errors.

The unchanged interpreter test baseline passes 932 library tests and 157
doctests, with 17 preexisting upstream ignores. Its isolated test graph reuses
approved root dependency versions and is frozen for before/after comparison;
it is not the published archive's dependency graph. See
`artifacts/h1-vendor-baseline-receipt.json` and `artifacts/h1-test-graph-audit.json`.

After repair, the same frozen interpreter test graph passes **953 library tests
and 157 doctests**, retaining exactly the same 17 upstream ignores. The ordinary
project gate passes **989 Rust and 533 Python tests**, including 37 independent
host-test groups for set semantics, assignment ordering, real GC/freeze and
iterator acquisition ownership. Twelve adjacent native/reference suites pass on
the frozen candidate. All H1-owned QA scratch/processes are cleaned.

The analyzer retains exactly the twelve pristine-dependency diagnostics recorded
by H0, with no new diagnostics and none in root sources/tests or modified files;
its status remains **KNOWN_BASELINE_ERRORS**, not clean. First failures remain
recorded: iterator error cleanup was corrected, static constructor typing was
restored, profiling expectations gained only the three new opcode rows, and the
genuine `.elems()` input gap was fixed without weakening the Go fixture.

Historical core-only evidence is bound in `artifacts/h1-set-core-verification-manifest.json`, including
full before/after interpreter results, host tests, analyzer comparison, regression
receipts and preserved application probe. Production globals remain deliberately
disabled in that core-only stage, so its preserved probe fails at missing `set`.
The later production integration requires its own candidate, reference matrix
and preserved application probe; it does not rewrite that historical result.

## Verified production integration

The production gate passes **1,011 Rust and 572 Python tests**, including 16
real set-driven CLI groups and 39 strict harness tests. The new candidate passes
seven comparison families, 33 probes, 62 phases and 136 CLI commands in
`artifacts/starlark-sets-validation.json`. Its status is
`PASS_ONLY_BOUNDED_PROFILE`: both reference self-XOR crashes remain explicit
intentional divergences. Twelve adjacent suites also pass.

Actual generated Files carried only through a custom provider's set are scheduled
for their consumer, with empty implicit DefaultInfo files. Cold/warm builds and
source changes produce verified ordered bytes. Frozen factories and genuine
module-extension/repository callbacks likewise produce consumed action outputs.

The comparison exposed and closed a preexisting Target membership gap. Membership
now requires an exported provider constructor, like indexing: invalid values and
unexported constructors error; valid absent providers yield `False`, and
DefaultInfo remains present. Original debug-false observations are preserved,
while `membership_gap_closure` binds new paired native/reference rejection.

The preserved frozen application query advances to missing `subrule` in generated
globals code. Checkout contents and the required Git symlink remain unchanged;
probe scratch and comparison servers are cleaned. Bindings and receipts are in
`artifacts/h2-set-integration-verification-manifest.json` and
`artifacts/mainrepo-starlark-sets-loading.json`. Analyzer results retain the exact
twelve known dependency diagnostics, with zero new or root-source errors.

General value formatting, frozen-provider hashing and broader depset-element
compatibility remain separate limitations. The disabled-set command-line flag
is not implemented or silently accepted. No full Bazel parity, application
build/test success or performance improvement is claimed.
