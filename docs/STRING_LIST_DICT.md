# Ordered string-list dictionary attributes

`attr.string_list_dict` defines an actual typed attribute for custom rules,
repository rules and module-extension tags. It is not a loading-only alias.
Values are insertion-ordered dictionaries with string keys and list/tuple values
containing only strings. Tuple values normalize to lists; element order and
duplicates are retained. Dictionary strings are not discovered as dependencies
or output paths.

## Declaration and validation

The supported constructor is:

```starlark
attr.string_list_dict(allow_empty=True, *, default={}, doc=None, mandatory=False)
```

Only `allow_empty` may be positional. Boolean options require actual booleans;
`doc` must be a string or `None`. A supplied `configurable` argument, including
`None`, is explicitly unsupported rather than ignored. No configurable-analysis
or symbolic-macro parity is implied.

Constructor defaults must be dictionaries; explicit `default=None` and callable
defaults fail. Defaults are copied at construction without freezing or aliasing
the caller's original dictionary or nested containers. At invocation, omission
and `None` use the default for optional attributes. Mandatory omission or `None`
fails even when a default exists; an explicit empty dictionary is supplied data.

`allow_empty=False` rejects an empty outer dictionary during custom-rule analysis,
not declaration/query. An outer dictionary containing empty nested lists remains
valid. Following the observed Bazel 9.3 repository contract, repository/tag
binding does not enforce this analysis-only emptiness restriction.

## Ownership, order and execution

Analysis and repository contexts expose deeply frozen dictionaries and nested
lists. Tag projections are likewise frozen. Assignment to the dictionary or
mutation of a nested list fails; repeated reads do not supply mutable copies.
This differs from `ctx.build_setting_value` and TemplateVariableInfo's
copy-on-read fields.

BUILD calls capture owned values at invocation. Repository calls inside ordinary
extensions also capture values immediately. MODULE calls/tags retain their
existing end-of-evaluation capture semantics: later MODULE mutations are visible.
Neither pathway sorts dictionary payloads. Ordered values enter repository
provenance and tree identity; root maps of attribute names keep their existing
representation.

List/tuple normalization also applies intentionally to existing typed string,
label and output-list attribute values. It does not enable arbitrary iterables,
depsets, or strings as lists, nor alter unrelated function-argument validation.
Existing ordinary-list projection behavior is otherwise unchanged.

Borrowed schemas, supplied values, omitted defaults and MODULE conversion are
charged against native host-preparation budgets before copying. Repository and
module-extension ASCII restrictions apply recursively to keys and nested strings;
this increment does not expand that language profile.

## Evidence and remaining boundaries

`artifacts/string-list-dict-contract.json` preserves pinned Bazel 9.3 probes of
constructor typing, ordering, snapshots and mutation rejection. Real native
consumers exercise action outputs, repository files, tags, snapshots and cache
identity; these are not substitutes for full Bazel compatibility.

The completed native/reference matrix passes eight families and 146 phases in
`artifacts/string-list-dict-validation.json`, including cleanup and unchanged
source/binary receipts. The final gate passes 781 Rust and 408 Python tests;
`artifacts/string-list-dict-verification-manifest.json` binds the candidate and
reports.

The genuine `bazel_features` helper additionally needs supported local-repository
lifecycle metadata and MODULE-loaded `native.bazel_version`. Those are separate
requirements, not silently accepted options or fabricated globals here. Full
application builds/tests, configured analysis, SDKs and performance parity remain
unfinished.

The preserved application query now passes the dictionary constructor and stops
at `repository_rule(local=True)` in genuine `bazel_features` code. Its checkout
and required Git symlink remain unchanged. The fixture version does not establish
the application's selected dependency version; see
`artifacts/mainrepo-string-list-dict-loading.json`.
