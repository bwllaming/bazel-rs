# `py_internal.is_singleton_depset` (Bazel 9.3)

This follow-up adopts the helper/dispatch landed in main `001d321` unchanged.
Its semantics and argument validation agree with the independent capture.
The incremental source change only replaces `DepsetNode.single_item`'s
per-call DAG walk with derived structural metadata. Main's existing fixture
is preserved; the independent edge capture has a separate `_edges` name.

Pinned source: `rules/python/PyBuiltins.java:77-88`,
`builtins_bzl/common/python/py_internal.bzl:70-71`,
`collect/nestedset/NestedSet.java:180-269,370-378`, and
`NestedSetBuilder.java:181-202,288-390`.

The wrapper forwards one **positional-only depset** (`value`) and returns a
bool. This pure helper is available in guarded `.bzl` loading, an imported
BUILD callable, analysis, and repository callbacks. It neither requires nor
creates an analysis context. Existing namespace guards are unchanged;
`py_internal` is not itself a BUILD global.

## Structural semantics, not a `to_list()` implementation

Java tests whether its canonical nested-set child is a scalar, not an array
or deserialization future. The constructor removes empty subsets, hoists
scalar children, deduplicates those scalars against each other and the direct
elements, and unwraps a lone child. A lone array remains an array; repeated
multielement subsets cannot become singletons. Builder identity reuse also
depends on order, but scalar classification does not.

The native DAG deliberately does **not** adopt Java's physical array layout:
that would risk changing depset identity, traversal, freeze, provider sharing
and keys. Each node instead gets 16 bytes (on this 64-bit build) of derived
metadata: nonsingleton, direct singleton, or an Arc pointing **straight to the
node containing the singleton's direct element**. Construction checks only
the deduplicated direct elements and immediate children's metadata, comparing
scalar elements with the existing Item equality. No item is copied, including
struct/provider frozen values. The existing single-transitive identity shortcut
uses the same constant-time representative instead of a deep walk.

* Any nonsingleton nonempty child forces a nonsingleton result.
* Otherwise all direct/hoisted scalar elements must be equal, and at least one
  must exist. Empty children are already filtered by the existing constructor.
* The nested-set predicate only reads the enum/representative: **O(1), no flattened Vec,
  traversal stack, visited set, equality checks, or repeated deep walk**.
  Main's existing argument binding is unchanged (including its small argument
  Vec, unrelated to depset size).
* Metadata is recomputed when paged nodes are read; the original paged fields
  and their order are retained. No persistent store, key/schema rekey, action
  or configuration change, and no new dependency.

For ordinary immutable Starlark elements with lawful equality, this structural
predicate agrees with logical deduplicated cardinality one: inductively, a
nonempty array subtree has at least two unequal scalars, while equal singleton
children are hoisted/deduplicated. Thus no honest Starlark fixture can fabricate
a structural-vs-logical boolean difference merely by wrapping/repeating equal
subsets. The tests distinguish **raw native shape/counts** from Java's canonical
shape (duplicate direct values, independent equal subsets, direct + transitive
duplicates, compatible but differing orders), and capture both the genuine
boolean and flattened count rather than assuming equivalence. Java's future
case is not constructible through this native Starlark depset API. Future
support must explicitly preserve its nonsingleton classification.

## Captured evidence and errors

`tests/fixtures/py_singleton_depset_edges/reference_receipt.json` pins ELF SHA256
`d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8`.
Only a tiny offline workspace and private HOME/TMP/output/cache were used.
There are 86 matrix rows in each of loading, analysis and repository execution,
plus File/provider consumers. All four orders, empty/direct singleton/multiple,
chains, repeated/shared subsets, diamonds, duplicate values, compatible order
changes, ints/bools/None/Labels/heterogeneous tuples, equal/different structs,
same/different provider constructors, mixed struct/provider values and depsets
as elements are covered. An actual provider carries the bool and depset to a
dependent rule; repository output/BUILD content and rule output use the result.

Twelve binding-error probes pin positional type-before-keyword-before-excess
argument precedence, positional-only `value`, unknown/typo keywords, and wrong
None/list/int types. Missing value passes Java's `unbound` default into a typed
method and **crashes genuine (exit 37, IllegalArgumentException)**. Native
deliberately returns an explicit evaluation error explaining that crash instead
of crashing or returning false. Ordinary error messages match the capture.

Capture note: the initial `--nofetch` attempt prevented even the local platforms
override; the successful attempt used `--repository_disable_download` instead.
After that successful build, the capture script's repository-name glob failed
before saving stdout/stderr. The generated output bytes were recovered without
repeating the build; this limitation is recorded in the receipt. Error captures
completed normally. Fixture hashes pin the input bytes.

Native integration explicitly imports the depset constructor from the same
`.bzl` file for its BUILD call: native currently lacks the genuine BUILD-global
`depset`. This existing, unrelated namespace gap is not changed here. The
captured helper calls and matrix inputs/outputs are unchanged.

The bounded unit performance proof constructs two shared 32,768-leaf DAGs
(131,070 nodes total), checks the one-hop representative invariant, then runs
2,000,000 queries, checking that no Arc cloning occurs. It also retains the
exact previous walker **only inside the test**, for a bounded 64-call baseline
on the same singleton graph. A 1,023-wrapper alternating-order chain separately
checks that every representative remains one hop. Timings printed by the test
are indicative, not assertions. Additional depset identity/order/freeze and
provider suites are part of the gate.
