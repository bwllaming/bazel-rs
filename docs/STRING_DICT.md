# Ordered scalar string dictionaries

This implementation adds `attr.string_dict` using genuine scalar string pairs,
not singleton lists. The isolated worktree gates below pass; combined integration
with host metadata remains a separate verification step.

`allow_empty=True` is positional-or-named; `default`, `doc`, `mandatory` and
`configurable` are named-only. Omission gives an empty dictionary; constructor
`default=None` is invalid. Every explicit `configurable` value remains unsupported
in this bounded profile; no selector or symbolic-macro schema support is implied.

Keys and values must be actual strings. Insertion order is retained through owned
snapshots and independently frozen context projections, without freezing the
caller. BUILD `None` keeps existing fallback semantics; mandatory omission still
fails. `allow_empty=False` rejects empty ordinary-rule values during analysis, not
query, and does not prohibit empty repository or tag values.

Constructor and BUILD inputs are copied before later caller mutations. MODULE
tags and direct declarations capture MODULE-end values; extension repository calls
capture each invocation separately. Ordinary strings retain Unicode, whitespace
and label-looking text literally. Existing repository/module ASCII restrictions
remain unchanged.

Valid loaded constructor aliases and helpers can construct metadata during BUILD
evaluation without introducing a BUILD `attr` global. Genuine Bazel 9.3 crashes
in the recorded direct loaded-constructor case; safe native construction is an
explicit **INTENTIONAL_DIVERGENCE**, not a reference parity pass.

Scalar shape validation precedes owned conversion. Budgeting charges the map root,
each key/value node and their bytes before copying; only string-list dictionaries
perform nested sequence traversal. Existing dictionary/list, provider, action and
environment behavior is not replaced by a generic coercion layer.

The reference contract is `artifacts/string-dict-contract.json` in the integration
workspace. Portable tests and real output comparisons establish their own evidence;
ignored research receipts are not build/test dependencies. Full Bazel parity,
application build/test success and performance goals remain unfinished.

## Isolated worktree verification

The gate passes 1,183 Rust and 700 Python tests, including 18 real CLI groups and
29 portable harness tests. The current primary comparison passes eight families,
140 phases and 369 commands. The mandatory reference BUILD-constructor crash is
recorded separately as a known defect, not a passing parity result; native BUILD
observations establish descriptor type/representation, not default introspection.

Five targeted matrices pass: existing string-list dictionaries, repository
environment inputs, module extensions, repository rules and custom rules. Exact
outputs, ordering, snapshots, query/analysis timing, publication/cache behavior and
explicit profile boundaries remain checked. Native diagnostic categories are
fixture-bound; full Java diagnostic-text parity is not claimed. Failed checker
and BUILD-print fixtures remain preserved.

Analyzer findings remain exactly the twelve known dependency diagnostics with
zero new root-source errors. Owned comparison processes and scratch are cleaned.
The isolated application query preserves checkout contents and the required Git
symlink, and advances to unsupported `module_extension(os_dependent=True)` in
`rules_go`. The independent host-metadata feature addresses that surface, but
combined application acceptance has not yet run.

Bound evidence is in this worktree's
`artifacts/string-dict-verification-manifest.json`, `string-dict-validation.json`
and `mainrepo-string-dict-loading.json`.
