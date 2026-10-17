# Depsets (Bazel 9.3)

Code: `src/starlark_types.rs` (`Depset`, `DepsetNode`, `Item`, `depset()`),
`src/depset_items.rs` (N49 element checks and snapshots). Tests: unit tests
in both modules, `tests/depset_structs.rs`, `tests/depset_identity_keys.rs`. Pinned source:
`/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0); Java paths are
relative to `src/main/java/`.

Native depsets are owned Rust data: a persistent DAG of `DepsetNode`s shared
by `Arc` across targets, with the four orders (default, postorder, preorder,
topological), a homogeneous element type and first-encounter deduplication
in `to_list()`. Their elements are owned values, never Starlark heap
references, so a depset can be frozen, shared and expanded after analysis.

## Element check (`depset(direct, ...)`)

Depset.fromDirectAndTransitive checks each direct element in turn
(`com/google/devtools/build/lib/collect/nestedset/Depset.java:354-376`):

1. **Depset.checkElement** (`:125-152`) with the strict
   `--incompatible_always_check_depset_elements` (default true): an element
   that is not `Starlark.isImmutable` is `depset elements must not be mutable
   values`; then a top-level list or dict is `depsets cannot contain items of
   type 'list'` (`'dict'`). isImmutable (`net/starlark/java/eval/Starlark.java:156-167`
   and the classes' overrides): strings, bools, ints, None, Labels, Files,
   SymlinkEntry, depsets, configured targets, runfiles and StarlarkFunctions
   (`def`s, also closures and lambdas) are immutable; a tuple when its
   elements are; a list, dict or set only when frozen (so one created in the
   current analysis is mutable, one from a dependency's provider is not); a
   `struct()` or Starlark provider instance when its provider is exported
   and every field value is immutable (StarlarkInfoNoSchema.java:196-208,
   StarlarkInfoWithSchema.java:141-155; native instances exist only for
   exported providers); a built-in function never (StarlarkValue default).
2. **Depset.checkType** (`:181-193`): `cannot add an item of type 'T' to a
   depset of 'U'`, for direct elements and then non-empty transitive depsets.
   Types are the StarlarkBuiltin classes (ElementType.getTypeClass,
   `:455-480`): `struct()` values and provider instances are both `struct`
   (StructApi) and may be mixed; also `NoneType`, `depset`, `Target`,
   `runfiles`, `function`, `File`, `string`, `Label`, `int`, `bool`, `tuple`,
   `SymlinkEntry`.

## Struct and provider elements (N49)

An accepted element is **snapshotted** into owned data (`depset_items.rs`):
struct fields and provider instances (provider key, display name, fields in
name order, creation location) with their values — scalars, Files, Labels,
None, tuples, nested depsets (shared node), configured targets, runfiles
(shared nested sets) and global `def`s of loaded `.bzl` files (the build's
function registry, `crate::analysis_runtime::LoadedFunctions`). `to_list()`
allocates the struct or instance anew; because every element Bazel accepts
is immutable, the snapshot is observably the same value (type, fields,
`dir()`, repr `struct(a = ..., b = ...)`, equality).

Deduplication uses Bazel's equality (StructImpl.equals/hashCode,
`com/google/devtools/build/lib/packages/StructImpl.java:97-132`): the same
provider (`struct()` never equals a provider instance) and equal field
values — strings/ints/bools/None/Labels/Files by value, tuples element-wise,
depsets like Depset.equals (`Depset.java:552-562`: the NestedSet, which has
no equals override, by identity; the empty depsets of an order are one shared
set; `depset([1]) == depset([1])` is False, also as a Starlark `==`; paired
capture run 1 showed it), configured targets, runfiles and functions by
identity. So two equal-looking structs holding separately created depsets
are both kept.

Explicit gaps (Bazel accepts them): a frozen set inside an element, a
closure or lambda, and any other value whose immutability is not modelled
(provider constructors, built-in provider instances such as DefaultInfo,
built-in functions inside a frozen list, ...): `a depset element containing
... is not implemented in the native profile`. Known approximations: native
functions are per-package `.bzl` evaluations, so the same `def` reached
through two packages' evaluations is two functions natively (one in Bazel);
a configured target element compares by label and the identity of its
provider objects, and `to_list()` re-allocates it, so it is not `==` to the
original Target value (no per-configured-target native handle on this base;
for integration with N44's shared configured targets).

## Frozen lists and dicts inside elements (N49b)

A frozen list or dict is immutable (`Starlark.isImmutable`), so it may sit
inside a struct, provider instance or tuple element — rules_cc's
LinkerInputInfo (`cc/private/link/create_linker_input.bzl:63-68`) holds
`_cc_internal.freeze(list)` values and goes into `linker_inputs` depsets. A
**top-level** frozen list or dict stays rejected (`depsets cannot contain
items of type 'list'`, `'dict'`). Natively it is an owned snapshot
(`Item::List`, `Item::Dict` with its insertion order): a list equals a list
with equal elements and never a tuple (StarlarkList.equals,
`net/starlark/java/eval/StarlarkList.java:244-252`); a dict equals a dict
with the same entries in any order (Dict.equals/hashCode,
`Dict.java:203-211`), and the first one encountered is kept. `to_list()`
re-allocates the container frozen (on a temporary module heap, frozen and
referenced by the receiving heap — the `ctx_surface` frozen-value pattern),
so `append`, `pop`, `setdefault`, ... fail with Bazel's `trying to mutate a
frozen list value` (`dict`) (Mutability.checkMutable,
`net/starlark/java/eval/Starlark.java:264`), also for nested containers.

`cc_internal.freeze(x)` (`com/google/devtools/build/lib/rules/cpp/CcStarlarkInternal.java:950-960`)
is Dict.immutableCopyOf for a dict, StarlarkList.immutableCopyOf for any
other iterable (list, tuple, range, set; the empty result is the shared
empty list) and `x` itself otherwise (depset, struct, string, None, ...).
Natively the copy is such a frozen list/dict re-allocated from owned
snapshots of the elements (`depset_items::frozen_copy`). Bazel's copy is
shallow, so an element without a native frozen copy fails explicitly
instead of yielding a mutable copy: a mutable value (`cc_internal.freeze of
a list containing a mutable value of type 'list' (Bazel's shallow immutable
copy keeps it mutable) is not implemented in the native profile`), a value
holding a configured target, a closure, or a value whose immutability is not
modelled. On this base every `freeze` reached by the rules_cc/cc_toolchain
tests and the rules_go go_library fixture is of an empty list.

## Identity of a depset built from one transitive set (N49b)

Depset.fromDirectAndTransitive (`collect/nestedset/Depset.java:353-405`)
skips empty transitive depsets before any check (`:380-381`), so neither the
order nor the element type of an empty child matters (also a typed native
empty depset such as an empty `target.files`); each other child is
type-checked, then order-checked: `Order 'preorder' is incompatible with
order 'postorder'` (`:382-386`, Order.isCompatible `Order.java:185-187`:
equal orders or one of them `default`). If the builder is empty the result
is the order's shared empty depset. NestedSetBuilder.buildInternal
(`NestedSetBuilder.java:181-202`) returns the one transitive set itself when
there is exactly one distinct non-empty transitive set (the builders
deduplicate by identity) of the **same** order (not merely compatible), and
either no direct element or only one that equals that set's single element;
fromDirectAndTransitive then returns the original Depset (`:396-402`).
Natively `Depset::compose` returns the same DAG node, so
`depset(transitive = [x]) == x`, `depset(transitive = [depset(), x, x]) == x`,
`depset([1], transitive = [depset([1])])` is that inner depset, `in`, dict
keys (a depset is hashable by its identity, `Depset.java:552-556`) and
depset/struct dedup all agree with Bazel; `depset(transitive = [x],
order = "preorder")` of a default `x` is a new depset. Elements, order and
action keys are unchanged (`tests/depset_identity_keys.rs`, golden keys
recorded with the pre-N49b binary).

## Traversal

NestedSet.toList (`NestedSet.java:485-498, 604-627`) walks each node's
children array depth-first left to right, visits a node once, keeps an
element's first occurrence and reverses the result iff the depset itself is
topological. Each node's array is laid out by its own order when built
(`NestedSet.java:180-250`, `NestedSetBuilder.java:314-380`): preorder =
direct then transitive, default/postorder = transitive then direct,
topological = transitive reversed (deduplicated after reversing) then direct
reversed. N49 fixes mixed orders natively (before, a default/preorder node
inside a topological depset, or a topological node inside another, was
traversed with the outer order's layout; paired capture run 1, O/reports
r_topo); depsets whose nodes all share one order are unchanged.

Existing element kinds keep their storage, dedup and action keys;
the type-mismatch text is now Bazel's (it was `depset elements must have
the same type; cannot mix ...`), and mutable elements are Bazel's
`depset elements must not be mutable values` (was `... is unsupported`).

Paired capture: `artifacts/depset_structs-capture.py`, receipt
`tests/fixtures/depset_structs/reference_receipt.json` (N49b rows: package
`df` of the fixture workspace; the N49 capture is in its `history`).
