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

MODULE-loaded native `existing_rule` / `existing_rules` calls support Bazel's
active-extension no-op semantics with dynamic phase checks and frozen results.
See [EXISTING_RULES.md](EXISTING_RULES.md); this does not implement BUILD rule
introspection or skip duplicate repository declarations.

`module_extension` accepts a callable implementation and `tag_classes`, plus
`doc`. Named boolean `os_dependent` and `arch_dependent` flags retain only their
selected host factors in preparation identity, independently of `ctx.os` reads.
`environ` and `facts_version` are supported (`facts_version` keys the
lockfile facts, [REGISTRY_LOCKFILE.md](REGISTRY_LOCKFILE.md)). See
[HOST_OS.md](HOST_OS.md) for the Linux x86_64/aarch64 compile-target profile and repository
fingerprint migration. Other unsupported options fail explicitly.
`tag_class(attrs={}, *, doc=None)` owns a frozen descriptor schema and is unhashable.

Supported attribute kinds are string, string-list, ordered scalar string dictionary,
ordered string-list dictionary, int32, bool, label and label-list. See
[STRING_DICT.md](STRING_DICT.md) and [STRING_LIST_DICT.md](STRING_LIST_DICT.md)
for tuple normalization, frozen dictionary values and snapshot timing.
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
Only `None` or inert [`extension_metadata`](MODULE_SOURCE_INPUTS.md) returns are
accepted; metadata is discarded. Then every recorded repository's attributes and
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

## Non-blocking downloads

`download(block = False)` (module and repository contexts) overlaps its
transfer with the callback, as Bazel's downloader executor does
(StarlarkBaseExternalContext.java:585-697, bounded by
`--http_max_parallel_downloads`, default 8). rules_python's `pip` extension
with `experimental_index_url` issues one non-blocking Simple-API request per
package and then waits for them all: on ~/code (371 requests) the sequential
transfer cost the extension ~13 s of round trips, the cold
`build --nobuild //src/python:bworker_env` pattern loading 30 s → 15 s.
Implementation: `src/repository_ctx/pending_downloads.rs`.

Only the network transfer and the copy into the download cache run on a pool
of eight threads, into a private file next to the output. Everything the
callback can observe stays where the sequential transfer shows it: a pending
download is *committed* — its mirror-failure warnings written, the file
renamed onto the output (a failed mirror's bytes too, as before), the execute
bits set, its result or error fixed — in call order, before the callback
observes anything else: before every other context method (`path(string)`,
`getenv`, and `read`/`path.exists`/`path.is_dir` of paths no pending output
is, contains or lies in excepted), every `print()`, `wait()` (which commits
through its own download), and when the callback returns or fails (the
warnings precede its error). A non-blocking download whose output is, contains
or lies in a pending output, or that expects a pending download's checksum
(the download cache could serve it), commits the pending ones first. Hence
stdout, stderr, errors, lockfile, recorded inputs and published trees are the
sequential transfer's (tests/parallel_download.rs compares cold builds
byte for byte against `BAZEL_RS_DEBUG_NO_ASYNC_DOWNLOAD=1`, the sequential
reference, at `--loading_phase_threads` 1/8/32, and a failing extension).
A download a credential helper applies to (its warnings print while the
request is formed), with downloads disabled, or onto an existing directory
transfers at the call. Residual: a credential helper that applies only to a
redirect target prints its warning from the transfer thread. A speculative
repository fetch's cancellation reaches its transfers.
`PERF-REPO non-blocking downloads overlapped=N` counts them.

## Profiling

`BAZEL_RS_TIMING_TIMELINE=1` prints one `PERF-TL <start>s <end>s <seconds>
<thread> <kind> <name>` line per module extension evaluation (`extension`,
by group), repository materialization (`executed`, `reused`), `execute()`
subprocess and download (`download`, `download-nonblocking`: the call, not
the transfer), on the diagnostics stream, relative to the process start (`main.start`).
On ~/code's cold bworker_env build it showed the pattern-loading span
(module extensions) to be rules_python's `pip` (24 of 30 s: 13 s sequential
Simple-API round trips, 3 s interpreter download, the rest Starlark), and
phase 1 to wait 14 s for `zig_config` (hermetic_cc_toolchain) before the
wheel repositories' select() conditions are decided.

## Explicit boundaries

Module/tag projections and their nested attribute lists are immutable. ASCII
Starlark source/attribute guards and call-time snapshots apply; binary template
files remain byte-preserving. Like Bazel, callbacks and their definition files
have no tick or heap limit, only a 128-frame call depth (N43). This is not a
security sandbox or complete Bazel byte-string implementation.

Extension preparation (bound tags, recorded calls, generated specs, provenance)
has no size or count ceiling since N43 (Bazel has none); only arithmetic
overflow and a nesting depth of 64 (a cycle guard) fail. The accounting below
still applies to the bounded non-extension profiles: The conservative charge is
eight bytes per payload byte plus 128 bytes per node, including repeated schemas
and omitted defaults. Catalog mappings and attribute-descriptor defaults are also
checked before expanding or copying them. Label conversion reserves mapped repository/package
expansion before constructing and retaining each canonical label. This deliberately
reserves space for retained copies and
projections, rather than claiming an exact process-memory bound. Provenance is
streamed from borrowed values into an uncapped buffer (allocation failure is an
explicit error); serialized
bytes are length-framed rather than expanded into another JSON number array.

`module_ctx` has Bazel's full StarlarkBaseExternalContext surface, shared with
repository_ctx ([EXTENSION_CTX_SURFACE.md](EXTENSION_CTX_SURFACE.md)), plus
`tag_sort_key`; the [source-input profile](MODULE_SOURCE_INPUTS.md) documents
`facts` and `extension_metadata` (validated direct-deps lists and root
`use_repo` fixup warnings; no `bazel mod tidy`). Facts persist in
MODULE.bazel.lock, and lockfile entries are written and reused like Bazel's
([REGISTRY_LOCKFILE.md](REGISTRY_LOCKFILE.md), N52). Isolation remains
unsupported. No placeholder platform or SDK repository is substituted. `mod` evaluates an extension only when its output shows the extension's
repositories (docs/MOD_COMMAND.md).
Genuine built-in module closure is the default
[built-in profile](BUILTIN_MODULES.md); complete
language/rule/toolchain support remain unfinished. Successful whole-application query/build/test and performance
improvements have not been established by this increment.

## Historical ordinary-extension verification

At that increment, the candidate passed 695 Rust tests, 306 Python tests, formatting, strict all-target
Clippy, release compilation and error-level Rust Analyzer diagnostics. The actual
CLI report `artifacts/module-extensions-validation.json` passes eight genuine
Bazel 9.3 differential cases and eight native-only boundary/lifecycle cases.
It retains outputs, input/executable hashes, reference shutdown and scratch cleanup.
Fourteen separate CLI checks cover resource-limit diagnostics and small-input
controls, with no partial publication: `artifacts/ordinary-host-limits-final.json`.
All eight adjacent validation suites pass against the same binary. Final review
approved this bounded deliverable; the verification manifest binds its source,
release and reports without claiming unrestricted compatibility.

That increment's read-only application probe failed during nogo repository demand
because the implicit `bazel_tools` dependency closure was absent. The required Git symlink
is preserved, checkout fingerprints are unchanged, and scratch is removed:
`artifacts/mainrepo-module-extension-loading.json`. This is an observed remaining
boundary, not an application query/build/test or performance success.
