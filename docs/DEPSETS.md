# Depsets (Bazel 9.3)

Code: `src/starlark_types.rs` (`Depset`, `DepsetNode`, `Item`, `depset()`),
`src/depset_items.rs` (N49 element checks and snapshots). Tests: unit tests
in both modules, `tests/depset_structs.rs`. Pinned source:
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

Explicit gaps (Bazel accepts them): a frozen list/dict/set inside an element
(an owned snapshot would be mutable), a closure or lambda, and any other
value whose immutability is not modelled (provider constructors, built-in
provider instances such as DefaultInfo, ...): `a depset element containing
... is not implemented in the native profile`. Known approximations: a
depset that Bazel collapses to its single transitive child
(`depset(transitive = [x])` is `x`'s NestedSet) is a new node natively, so
it compares unequal to `x`; native functions are per-package `.bzl` evaluations, so the same
`def` reached through two packages' evaluations is two functions natively
(one in Bazel).

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
`tests/fixtures/depset_structs/reference_receipt.json`.
