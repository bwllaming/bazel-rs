# Bounded ordinary module extensions

Native `use_extension` supports non-isolated extension callbacks that return
`None`, aggregate typed MODULE tags, and declare repositories using genuine
`repository_rule` values. This is a restricted execution profile, not complete
Bazel extension, SDK or application compatibility. There is no upstream fallback.

## Evaluation and identity

An extension is identified by its requested canonical `.bzl` label and symbol,
not by the implementation function or the value's first export. Private and
imported globals can be requested, as in pinned Bazel 9.3. Different requested
aliases can therefore identify different extensions. Canonical names share the
existing root-first allocation order with direct `use_repo_rule` declarations.
MODULE extension-file labels normalize `@@name` spellings back to apparent
`@name` before dependency mapping; this syntax cannot bypass MODULE visibility
using a canonical repository name. Ordinary BUILD labels are unchanged.

`ctx.modules` contains immutable native module objects for participating modules,
in selected-graph breadth-first order. A declaration without tags still counts;
nonparticipants are absent. Modules expose `name`, normalized declared `version`,
`is_root` and `tags`. Every declared tag class has an immutable list, including
empty lists. Tags preserve MODULE call order even when differently spelled usage
labels merge into one canonical extension identity.

`ctx.is_dev_dependency(tag)` accepts a genuine tag, positionally. The Boolean
`ctx.root_module_has_non_dev_dependency` property reflects retained root usage
declarations, independently of tags or imports. Dependency dev usages are ignored;
root dev filtering follows `--ignore_dev_dependency`. That flag also retains the
existing MODULE behavior of ignoring root overrides.

## Constructors and attributes

`module_extension` accepts a callable implementation and `tag_classes`, plus
`doc`. It accepts only empty `environ`, false `os_dependent`/`arch_dependent`, and
zero `facts_version` in this profile. Unsupported nondefault options fail explicitly.
`tag_class(attrs={}, *, doc=None)` owns a frozen descriptor schema and is unhashable.

Supported attribute kinds are string, string-list, int32, bool, label and label-list.
Private attributes may be supplied or mandatory. Tag attributes have no automatic
`name` or dev field; those names can instead be explicitly declared. Following the
pinned implementation, explicit `None` means omission, even for an unknown key.
Mandatory checks still apply. Unknown non-`None` keys fail.

Tag-supplied label strings resolve in their MODULE owner's root package. Defaults
retain the descriptor's lexical `.bzl` ownership, including helper factories.
All participating MODULE source-profile errors and typed tags are checked before
running the extension implementation.

## Repository declarations and publication

Repository-rule calls during an extension take keyword arguments, immediately
validate the generated name and duplicates, and deeply snapshot supported argument
data at call time. Mutating a list afterward does not change the recorded call.
Names must follow user-provided repository grammar; `+` is not allowed. Actual
callee values are rooted in a traced callback module, then retained by owned
frozen values after evaluation. Private/transitive helpers survive owner drops.

The complete callback runs before any of its repository implementations execute.
Only `None` returns are accepted. Then every recorded repository's attributes and
every participating usage's imports are checked. A missing unused import or an
invalid unused declaration invalidates the whole prepared group. Valid unused
repository implementations are not executed.

Raw repository label strings bind after all siblings are known, relative to the
**requested extension file's package**. Siblings overlay the extension host's full
mapping. Explicit `Label()` values retain their original lexical identity and are
not rebound. Forward sibling strings and unimported generated siblings therefore
work without inventing forward visibility for `Label()`.

Generated repositories are routed by exact allocated prefix and internal name,
not by guessing string prefixes. Stable per-group demand slots own dynamically
declared repositories and their per-repository fetch slots. Direct/ordinary
preparation and fetch cycles fail with a dependency chain rather than waiting.

Only after complete preparation does a demanded repository run the existing
[file/template executor](REPOSITORY_RULES.md) and checked publication store. Errors
publish no partial repository from the failing group. Prerequisite repositories
successfully fetched while loading another group are not rolled back. Each fresh
runtime reevaluates the extension and demanded repository callback before reuse.

## Explicit boundaries

Module/tag projections and their nested attribute lists are immutable. ASCII
Starlark source/attribute guards, evaluator limits and bounded call-time snapshots
apply; binary template files remain byte-preserving. Each demanded definition and
callback has its own evaluator budget, not an aggregate workspace budget. This is
not a security sandbox or complete Bazel byte-string implementation.

Host preparation phases additionally charge borrowed inputs before copying:
64 MiB weighted bytes, 100,000 nodes and depth 64. The conservative charge is
eight bytes per payload byte plus 128 bytes per node, including repeated schemas
and omitted defaults. Catalog mappings and attribute-descriptor defaults are also
checked before expanding or copying them. Label conversion reserves mapped repository/package
expansion before constructing and retaining each canonical label. This deliberately
reserves space for retained copies and
projections, rather than claiming an exact process-memory bound. Provenance is
streamed from borrowed values into a separately bounded 64 MiB buffer; serialized
bytes are length-framed rather than expanded into another JSON number array.

Isolation, metadata construction/returns, facts, OS/environment inspection,
module-context reads/downloads, and SDK APIs are unsupported. No placeholder
metadata, facts, platform or SDK repository is substituted. `mod graph` retains
its existing explicit-module profile and does not execute active extensions.
Genuine built-in module closure is now available through the separate opt-in
[built-in profile](BUILTIN_MODULES.md); default semantics and complete
language/rule/toolchain support remain unfinished. Successful whole-application query/build/test and performance
improvements have not been established by this increment.

## Verification

The candidate passes 695 Rust tests, 306 Python tests, formatting, strict all-target
Clippy, release compilation and error-level Rust Analyzer diagnostics. The actual
CLI report `artifacts/module-extensions-validation.json` passes eight genuine
Bazel 9.3 differential cases and eight native-only boundary/lifecycle cases.
It retains outputs, input/executable hashes, reference shutdown and scratch cleanup.
Fourteen separate CLI checks cover resource-limit diagnostics and small-input
controls, with no partial publication: `artifacts/ordinary-host-limits-final.json`.
All eight adjacent validation suites pass against the same binary. Final review
approved this bounded deliverable; the verification manifest binds its source,
release and reports without claiming unrestricted compatibility.

The read-only application probe still fails during nogo repository demand because
the implicit `bazel_tools` dependency closure is absent. The required Git symlink
is preserved, checkout fingerprints are unchanged, and scratch is removed:
`artifacts/mainrepo-module-extension-loading.json`. This is an observed remaining
boundary, not an application query/build/test or performance success.
