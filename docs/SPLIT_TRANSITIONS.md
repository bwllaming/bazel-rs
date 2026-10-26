# Attribute split transitions (Bazel 9.3)

Starlark attribute transitions may return a named dict of option dicts or a
positional list/tuple of option dicts. Every branch is validated and interned;
the complete dependency/provider analysis and ordinary action plan includes
every distinct configured prerequisite. Rule transitions still require exactly
one returned configuration (including when their result uses a split container).
`None`, `{}` and `[]` are no-ops, as in StarlarkDefinedConfigTransition.evaluate.

`ctx.attr` follows configured prerequisite order: original label order, then
configuration mnemonic/checksum order per label. A transitioned attr.label is
a list, even for a patch/no-op; attr.label_list is the flattened prerequisite
list. `ctx.files`, runfiles collection, validation output groups, Make-variable
locations and provider checking include all branches. Singular `ctx.file` and
`ctx.executable` prerequisites cannot have several configured results and fail
explicitly instead of picking one. Named/list splits on dependency dictionary
attributes use Bazel's configured conversion (label-keyed dictionaries have one
entry per configured target; string-keyed label maps select the last configured
target per label for ctx.attr and give per-branch views in ctx.split_attr).

`ctx.split_attr` is a frozen struct of frozen dictionaries. Patch/no-op entries
use `None` keys; positional splits use `"0"`, `"1"`, …; unset/empty attributes
have empty dictionaries. For attr.label each dictionary value is one Target;
for attr.label_list it is a list. Sources/package groups have null configured
identity but retain the transition names. Aspects propagate over every split
edge and may have their own split-configured implicit dependencies. As in Bazel,
ctx.split_attr itself is available only in rule implementations, not aspects.

## Converged configurations

Following Bazel 9.3 `PrerequisitesProducer.cleanupValues`, configured rule/output
prerequisites that converge on one final configuration retain only the first
branch's key. This includes convergence after an incoming rule transition. Null-
configured prerequisites instead aggregate every key in transition result order.
The retained configured prerequisites are sorted by configuration mnemonic and
checksum. Shared Target/provider values and deduplicated nodes/actions are kept;
all distinct configurations and inputs still undergo analysis before convergence
is resolved. The pinned named/list fixture retains `z`/`0` and drops `m`/`2` for
rules while retaining every source-file key, exactly like genuine Bazel.

The existing native ConfigId, output mnemonic and action-owner algorithms are
unchanged. Existing single-config transition keys and action cache keys remain
stable. True split metadata contains only labels/configuration IDs/map keys and
is shared through Arc; providers, runfiles and whole graphs are not copied per
branch. Ordinary action snapshots and cquery graph snapshots retain the resulting
plans/edges through their existing binary formats and validation boundaries.

## Verification

`tests/fixtures/split_transitions/genuine.json` records a fresh genuine 9.3 build
of the adjacent workspace. It covers named/list splits, convergence, ordering,
null-config sources, patch/empty/unset attributes, per-config files, exec tool
dependencies, an aspect on a split attribute and an aspect's own split dependency.
Both engines produce the proof bytes `dbg:selected\nopt:other\n`; context summaries,
including converged keys, match exactly. A second target checks incoming rule
transitions that merge initially distinct configurations into one final identity.

`tests/split_transitions.rs` additionally checks native-option splits, malformed
later branches, invalid multi-output rule transitions, configured graph identity,
graph reuse/invalidation and verification, and normal-build snapshot reuse.
The broader 1:1 transition, aspect and cquery tests remain in place.

Official TD v0.34.0 with cache disabled and before-query errors fatal was also run
on a scratch extension of the existing eleven-case matrix. Separate `dbg`/`opt`
branches select different source files and different exec tools. Editing either
branch's source changes both split consumers and their parent, with **11/11 exact
affected-label sets matching genuine 9.3**; the original eleven-case matrix also
continues to pass unchanged. No result rewriting or branch pruning is involved.
