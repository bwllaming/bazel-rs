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

## BUILD files and list attributes (B27)

`depset` is a BUILD-file global like in every `.bzl` context
(StarlarkGlobalsImpl.addCommonUtilToplevels puts Depset.DepsetLibrary in
the BUILD environment beside `json` and `proto`; `crate::starlark_types::depset_globals`).
A depset given to a list-typed rule attribute (`list(string)`,
`list(label)`, `list(output)`, `list(int)`, native or Starlark, BUILD file or
macro) is its `to_list()` (Type.ListType.convert, packages/Type.java:731-738),
also as a `select()` branch (OpenROAD's `@openssl//:BUILD.bazel`:
`select({...: depset([...])})`); `crate::starlark_types::depset_as_list` converts
the value at the rule call, after a Starlark rule's initializer. Other attribute
types keep rejecting a depset (`expected value of type 'string' for attribute
'cmd' of 'genrule', but got depset(["x"]) (depset)`), and `list + select of
depset` / `depset + list` fail with Bazel's messages. Genuine capture:
`tests/fixtures/build_depsets` (`tests/build_depsets.rs`).

## Element check (`depset(direct, ...)`)

Depset.fromDirectAndTransitive checks each direct element in turn
(`com/google/devtools/build/lib/collect/nestedset/Depset.java:354-376`):

1. **Depset.checkElement** (`:125-152`) with the strict
   `--incompatible_always_check_depset_elements` (default true): an element
   that is not `Starlark.isImmutable` is `depset elements must not be mutable
   values`; then a top-level list or dict is `depsets cannot contain items of
   type 'list'` (`'dict'`). isImmutable (`net/starlark/java/eval/Starlark.java:156-167`
   and the classes' overrides): strings, bools, ints, None, Labels, Files,
   SymlinkEntry, depsets, configured targets, runfiles, StarlarkFunctions
   (`def`s, also closures and lambdas) and CcCompilationContext.HeaderInfo
   (`CcCompilationContext.java:797-800`; snapshotted as its identity, so a
   CcInfo inside a struct or provider element — rules_rust's
   `depset(deps)` of DepVariantInfo — is accepted) are immutable; a tuple when its
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
   `SymlinkEntry`, `HeaderInfo`.

## Struct and provider elements (N49)

An accepted element is **snapshotted** into owned data (`depset_items.rs`):
struct fields and provider instances (provider key, display name, fields in
name order, creation location) with their values — scalars, Files, Labels,
None, tuples, nested depsets (shared node), configured targets, runfiles
(shared nested sets) and Starlark functions (see "Functions inside
elements"). The first
`to_list()` (or other expansion to values) allocates the struct or instance
once, frozen on its own heap with its list/dict fields, and every later one
returns that value (MEM3, `StructItem::frozen`), as Bazel returns the one
element object; because every element Bazel accepts is immutable, the
snapshot is observably the same value (type, fields, `dir()`, repr
`struct(a = ..., b = ...)`, equality).

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
function that is not frozen yet outside a rule, aspect, `.bzl` or BUILD
evaluation (see "Functions inside elements" below), and any other value whose immutability is not modelled
(provider constructors, built-in provider instances such as DefaultInfo,
built-in functions inside a frozen list, ...): `a depset element containing
... is not implemented in the native profile`. Known approximations: native
functions are per-package `.bzl` evaluations, so the same `def` reached
through two packages' evaluations is two functions natively (one in Bazel).
A configured target element is Bazel's identity comparison exactly (N55):
the engine allocates one `Target` value per configured target for the
whole analysis, keyed as Bazel keys it — the label and the post-delegation
configuration for a rule or alias edge (two edges whose pre-transition
configurations converge through one Starlark rule transition share the
instance), no configuration for a source file or package group, the
generating rule's for an output file — and the snapshot shares that
allocation (`crate::depset_items::TargetItem`, `Arc::ptr_eq`), and
`to_list()` re-allocates a value that is `==` to the original Target value
and to every other consumer's. Paired capture
`tests/fixtures/target_identity/reference_receipt.json`: every identity row
(provider-carried, depset snapshots across producers, same-transition
attributes, alias/exec distinct, source file across target- and
exec-configured edges, rule-transition convergence, Target-holding freeze)
is EXACT against genuine Bazel 9.3.0.

An aspect-applied target element (N58, docs/ASPECTS.md) is NOT one of those
per-`ConfiguredTargetKey` instances: Bazel hands each consuming edge its own
ephemeral `MergedConfiguredTarget` proxy (`MergedConfiguredTarget.java:
55-64`, created per consuming configured target, no key in the Skyframe
graph), so two consumers holding the same dependency with the same aspects
hold different (never `==`) Target objects with identical providers. The
native profile instead shares ONE `Target` value per aspect application key
— the base `ConfiguredTargetKey` plus the aspect and its directly required
aspects (`AspectKey`, crate::aspects) — so cross-consumer equality is
value-stable where Bazel's is per-edge; the base target's own value, and
every depset snapshot holding it, keeps the per-`ConfiguredTargetKey` N55
identity above.

## Functions inside elements (closures, lambdas, nested defs)

StarlarkFunction.isImmutable is true for every Starlark function
(`net/starlark/java/eval/StarlarkFunction.java:272-276`), so a depset
element may hold closures: skylib's `DirectoryInfo`
(`rules/directory/providers.bzl`, `_init_directory_info`) carries four
lambdas capturing the struct of its fields, and rules_cc's `cc_args`
(`cc/toolchains/args.bzl`) puts its dependencies' DirectoryInfos in a
depset (OpenROAD). Equality is identity: StarlarkFunction.equals/hashCode
compare the per-instantiation identity token (`:255-270`), so equal-looking
structs with separately created lambdas are both kept and the same provider
instance twice is one element.

A depset is owned Rust data outside every Starlark heap, so its element
snapshot must keep each function alive and reachable from any heap
(`depset_items::FunctionItem`):

- a **frozen** function — a global def of a loaded `.bzl` file, a
  dependency's provider's closure, a loaded module's lambda — is owned by a
  frozen heap that references every
  frozen heap the evaluating heap references
  (starlark::values::Heap::owned_frozen_value, docs/STARLARK_VENDOR.md): every
  frozen value a heap reaches lives on one of those heaps, the invariant a
  module freeze relies on, and the evaluation's own frozen module keeps the
  same heaps alive anyway;
- a function **of this evaluation**, not frozen yet (a lambda of the rule
  implementation, a `.bzl` file's top-level lambda while it evaluates), is
  retained by the evaluating heap (starlark::values::RetainedValue): a
  garbage-collection root of a module evaluation, frozen with the module,
  after which the handle owns the frozen function. A struct or provider
  element holding such a function keeps the element object itself, retained
  the same way (`StructItem::retained`): `to_list()` returns that very object
  (Bazel's element), on the evaluating heap before the freeze and frozen
  after it, instead of a copy on a frozen heap of its own, which could not
  hold an unfrozen function. Only the heaps of evaluations that freeze their
  module once they succeed retain: rule and aspect analyses, `.bzl` and
  BUILD files (`Heap::enable_retention`); elsewhere such a function stays the
  explicit gap. Before the freeze only the evaluating heap can reach the
  depset — nothing carries a depset out of an evaluation but its frozen
  module's values — so the retained value is always found.
- The freeze keeps only the retained values the module keeps: those its
  values reach, and those of every depset value it keeps (each depset node
  records the retained values its elements and children hold,
  `DepsetNode::pending`, and a depset value anchors them,
  starlark::values::Heap::anchor). A depset the implementation discarded is
  not frozen with its closures: they may capture `ctx`, which cannot be
  frozen natively (`//dc:tr`). A kept one whose closure captured `ctx` is an
  explicit error: `unsupported provider payload: … (a function kept by a
  returned value — a provider field, or an element of a depset it holds —
  captured the rule context `ctx`, which the native profile cannot freeze)`
  (genuine keeps it; `//dc:bad_kept_ctx`). Discarded evaluator attempts
  (`BAZEL_RS_EVAL_RESTARTS`) never freeze their modules; their depsets die
  with them (tests/depset_structs.rs
  `closures_under_evaluator_restarts_and_threads`).

An item's identity is the frozen function's address — known for global and
other frozen functions, and for a retained one once its module froze — or,
before that freeze, its retention handle (one per function and heap). An
unfrozen function is never the same object as a frozen one, and a handle
resolves to the same function's frozen address after the freeze, so the
equality relation is the same before and after; across the freeze a
dependency's pre-freeze depset deduplicates against the same closures read
from its frozen provider, as in Bazel. The order of unequal function items
(their repr, then the identity) may change at the freeze; it orders only
transient deduplication sets.

`cc_internal.freeze` of a list holding a function of this evaluation that is
not frozen yet returns an immutable list that is not frozen yet (below,
"Frozen lists and dicts inside elements"): it holds the very function, as
Bazel's shallow copy does (rules_cc's cc_library `linkstamp`, whose
LinkstampInfo wraps this analysis's private-API closures); as a depset
element's field it is snapshotted like a frozen list, its function item
retained with the element (the struct or provider element is retained
whole). Directly in the element or in its tuples (`depset([(freeze([f]),)])`)
it fails explicitly: `to_list()` re-allocates such a list on a temporary
module heap, where the pending function cannot be reached (`a depset
element containing a list or dict holding a function of this evaluation
that is not frozen yet (outside a struct or provider element) is not
implemented in the native profile`; genuine accepts it). Built-in functions and bound methods (`"x".upper`, `{}.get`,
`len`) are mutable as in Bazel (StarlarkValue.isImmutable default):
`depset elements must not be mutable values`.

The analysis snapshot never stores depsets: a planned action holds expanded
command-line strings and files (crate::command_line::Item), so no element
function needs a portable form; nothing new is read during loading or
analysis, so nothing new needs observing.

Paired capture (genuine Bazel 9.3.1): `tests/fixtures/depset_structs/workspace/dc`,
`closures_receipt.json`, `closures_expected.txt` (tests/depset_structs.rs
`closures_lambdas_and_nested_defs_follow_bazel`): DirectoryInfo-shaped
provider instances from dependencies in every order, `get`/`glob`/nested-def
calls after `to_list()`, identity equality and `in`, distinct twins, a
dependency's pre-freeze depset combined with its frozen struct and closure,
this implementation's closures, a consumer reading them after the freeze,
struct method values, `.bzl` top-level depsets across the module's garbage
collections, and the mutable built-in/bound-method errors.

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
referenced by the receiving heap — the `ctx_surface` frozen-value pattern;
inside a struct element, on the element's one frozen heap),
so `append`, `pop`, `setdefault`, ... fail with Bazel's `trying to mutate a
frozen list value` (`dict`) (Mutability.checkMutable,
`net/starlark/java/eval/Starlark.java:264`), also for nested containers.

`cc_internal.freeze(x)` (`com/google/devtools/build/lib/rules/cpp/CcStarlarkInternal.java:950-960`)
is Dict.immutableCopyOf for a dict, StarlarkList.immutableCopyOf for any
other iterable (list, tuple, range, set; the empty result is the shared
empty list) and `x` itself otherwise (depset, struct, string, None, ...).
Bazel's copy is shallow and immutable without being frozen
(Mutability.IMMUTABLE): a mutable element stays the same mutable value, a
function of this evaluation is held. Natively the copy is the vendored
starlark's immutable list or dict that is not frozen yet
(`Heap::alloc_immutable_list`, `Heap::alloc_immutable_dict`,
docs/STARLARK_VENDOR.md "Immutable lists and dicts not frozen yet"): it
holds the very elements on the evaluating heap, every mutation fails with
`trying to mutate a frozen list value` (`dict`), and it freezes with its
module into an ordinary frozen list or dict (rules_cc's
LtoCompilationContextInfo, `freeze` of a dict of BitcodeInfo instances
holding mutable `copts` lists). Starlark.isImmutable of such a copy is true
whatever it holds, so it may sit in a depset element whatever it holds
(OSS corpus B32: rules_cc's LibraryToLinkInfo of a ThinLTO library holds
LtoCompilationContextInfo's copy of BitcodeInfo instances with mutable
`copts` lists). The native snapshot copies a mutable list or dict below
such a copy, and the innermost struct or provider instance around it is
retained whole (StructItem.retained, like an element holding a function of
the evaluation), so `to_list()` gives the element object itself and a later
mutation stays visible as in Bazel; on a heap that does not retain (a
temporary evaluation) the element is the copy. A configured target keeps
its identity through the copy (N55).

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

`target.files` is the target's one files-to-build depset, the same one as
`target[DefaultInfo].files` (AbstractConfiguredTarget.get,
`analysis/configuredtargets/AbstractConfiguredTarget.java:216-225`, and
DelegatingDefaultInfo.getFiles, `analysis/DefaultInfo.java:153-157`: the
FileProvider's one NestedSet). So `t.files == t.files`, one dict key, one
depset element and `depset(transitive = [t.files]) == t.files` hold for rule,
source-file, output-file and alias targets, as in Bazel (paired capture
`tests/fixtures/target_identity/files_reference_receipt.json`, EXACT; before
W0-C2 every read built a new depset). Its elements, order and repr are
unchanged. Known divergences (unchanged): Bazel's files-to-build set is the
very depset the rule returned in DefaultInfo, and an alias shares its
actual's (`rules/AliasConfiguredTarget.java:205-213`); natively each Target
value builds its own depset from its files.

Only Starlark's `depset()` (and the native runfiles sets) checks the DAG
depth limit (`depset exceeds DAG depth limit 1024`;
Bazel: `Depset.java:540-545`, `--nested_set_depth_limit` 3500). The
framework's output-group compositions — `_validation`, HIDDEN_TOP_LEVEL and
the aspect merge of output groups (`Depset::compose_framework`, W1c) — never
do, as Bazel's NestedSetBuilder has no limit; a deep `_validation` group
still fails a Starlark `depset(transitive = [...])` past 1024 levels.

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

## Memoized walks (cold-analysis-speed)

The walks that list a depset's Files for natively consumed inputs
(`file_list`/`unpack_depset`: action inputs, among others) may start from a
memoized walk: a node of Files with at least 128 elements keeps its walk
(files in walk order and their set) in a process-wide table by address,
with a weak reference; one byte in the node's padding records the state. A
walk reaches a memoized node first along the first-visited children
(default/postorder first child, preorder first child without direct
elements; never under a topological depset), takes its files, and walks the
rest exactly as above, deduplicating against the memoized set too.

Which node an action's inputs start with — its shared run, a shared input
group of the planned request — is a function of the depset alone, never of
what was flattened before (`MemoUse`; before, a node was memoized from its
second request on, so the first action of a parallel analysis to list it,
whichever it was, listed it as its own inputs: `BAZEL_RS_SNAPSHOT=verify`
reported Go link actions as plan mismatches on ~/code). `ctx.actions.run`
inputs (`FirstChild`) go down the first children: a node from another
target's analysis (a provider's depset, a `.bzl` global's) is the run from
its first request; a node the registering rule or aspect implementation run
created itself (`AnalysisScope`, a `u32` in the node's padding) — a Go
archive's `libs`, a Rust crate's transitive inputs — only from its second
request in that run (only the run's own actions can list it until it
returns, one after another), and the next node down before that. A C++
compile's toolchain files and headers (`Shared`) are the depset itself, by
the same rule. Other walks (`Lookup`) only use memoized walks. The planned
request's group table is deduplicated by value, so equal groups of separate
allocations (two threads resolving one run at once) encode alike. The result is NestedSet.toList's, element for element
(`memoized_walks_list_what_flatten_lists`); the depset's identity, repr and
`to_list()` are untouched (`flatten()` itself never memoizes). Action
registration and commit reuse the walk's per-input checks
(docs/COLD_ANALYSIS.md). Deduplication hashes a File by its path only.
