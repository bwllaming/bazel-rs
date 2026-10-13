# Direct repository-rule execution

This is a bounded native implementation of `use_repo_rule`, not complete Bazel
repository or module-extension compatibility. The official Bazel 9.3 executable
is used only for comparison, never as an execution fallback.

## Execution and ownership

Direct declarations in root and selected dependency modules are indexed without
loading their definitions. Demanding a generated repository loads the requested
public `repository_rule` export and its complete transitive `.bzl` closure. Frozen
implementation functions, including private helpers, remain alive throughout
execution. All declarations belonging to the same innate usage are type-checked
before any of that usage's repositories execute. Only demanded callbacks run.

Canonical names and mappings are MODULE-owner scoped. Generated repositories
inherit that owner's mappings plus their own usage's sibling repository names.
Labels supplied in MODULE attributes resolve in the declaring module; label
defaults resolve where their descriptors are defined. Shared runtime handles use
the same resolved graph and cache configuration. Cycles fail with a demand chain;
concurrent demands for the same in-flight slot in one library instance are
explicitly unsupported rather than waiting. Separate CLI processes can publish
to the same cache.

Ordinary extension usages reserve names but do not execute. Recorded isolated
extension identities lack exported variable names; their reservations are skipped
only where their underscore-prefixed namespace is disjoint from public direct
rule symbols. Other ambiguous namespace combinations fail explicitly.

## Supported surface

`repository_rule` supports callable implementations, `attrs`, and `doc`; `local`
and `configure` must be false and `environ` empty. Attribute kinds are string,
string-list, int32, bool, label and label-list. Defaults, mandatory attributes,
explicit `None`, and the pinned 9.3 behavior for declared underscore-prefixed
attributes are supported. Unknown `None` attributes are ignored, while unknown
non-`None` attributes fail. Output attributes are invalid for repository rules.

Callbacks receive real `ctx.attr` values, `ctx.name` (canonical), and
`ctx.original_name` (the invocation name). `ctx.attr.name` is also canonical.

* `ctx.file(path, content="", executable=True, legacy_utf8=False)` writes
  synchronously, creates parents, and replaces existing regular files. `path` is
  positional-only; the deprecated boolean `legacy_utf8` is a no-op.
* `ctx.template(path, template, substitutions={}, executable=True,
  watch_template="auto")` reads synchronously before replacing the output.
  The two paths are positional-only. Substitutions are sequential, literal,
  dictionary-insertion-order replacements; empty search keys do nothing.
  Only the `auto` watch setting is supported.

Output paths must be clean relative ASCII components containing letters, digits,
`_`, `-`, `.`, or `+`. Absolute paths, empty components, `.`/`..`, symlink traversal
and special files are rejected. Template inputs may be stage-relative strings or
actual source Labels. Label input packages require a regular BUILD file, but the
input need not be exported or visible as a target. Same-file templating works.
Arbitrary template bytes, including invalid UTF-8 and CRLF, are preserved.

Only an implicit or explicit `None` callback return is supported. Other returns
fail explicitly even where upstream accepts them as repository metadata.
Execution, downloads, extraction, environment access, symlink creation, path
objects and general host filesystem access are not implemented.

## String and resource bounds

Bazel strings are byte-oriented; Rust Starlark uses Unicode strings. To avoid
silently computing different contents, direct execution currently requires ASCII
string literals and attributes. Every definition's transitive source is checked
before evaluation, including unused expressions. MODULE violations are captured
before evaluation and enforced only when its direct repository usage is demanded;
metadata-only operations keep their existing behavior. Even an unrelated Unicode
MODULE constant can therefore block a demanded direct repository.

`chr` references, including aliases and shadowed uses, are rejected conservatively.
Original literal spellings are checked: high-byte octal escapes are unsupported,
and hex/Unicode escapes are invalid in the pinned Bazel grammar. ASCII octal,
escaped backslashes, raw ASCII strings and Unicode comments remain allowed.
File contents and template substitution strings are ASCII; template file bytes
never become Starlark strings and are not subject to that restriction. This does
not establish complete Starlark language parity.

Callbacks and each demanded definition-file evaluation are bounded to 128 call
frames, one million evaluator ticks and a 128 MiB evaluator heap. Ordinary BUILD
loading is unchanged. Files and template inputs are at most 16 MiB each;
cumulative I/O and operation counts are bounded. Publication allows at most
10,000 entries, depth 64, 4096-byte paths and 64 MiB file contents. These are
implementation limits, not a security sandbox.

## Publication and freshness

Each new invocation reevaluates demanded callbacks. Persistent cache hits never
skip evaluation. Exact loaded source bytes, mappings, typed attributes, template
reads and output snapshots identify the result. Snapshots include directories,
file contents and modes. After successful execution, a missing repository
boundary is supplied as an empty `REPO.bazel`, not a fabricated MODULE or BUILD.

Callbacks write only to private staging trees. Failures do not publish their
partial trees. Publication locks are acquired after evaluation and dependency
resolution; no interpreter runs under those locks. Verified matching trees may
be reused, while corrupted trees are replaced. Tree and manifest publication use
separate atomic renames; missing metadata after interruption forces reevaluation
and repair. The cache is user-owned and published trees must remain immutable
within a library invocation.

## Application boundary

This executor is infrastructure for later module-extension support. It does not
execute `go_sdk`, generate nogo by substitution, or bypass transitive features.
Successful application query/build/test and full Bazel 9.3 parity remain unproven.

## Verification

The implementation passes 608 Rust tests, 270 Python tests, formatting, strict
all-target Clippy, release compilation and error-level Rust Analyzer diagnostics.
`artifacts/repository-rules-validation.json` records seven genuine Bazel 9.3
differentials and nine native-only boundary/lifecycle checks. Seven adjacent
Git-link, Git, local-repository, declaration, patch, remote and module reports
pass against the same release binary. Reports retain hashes, actual output
bytes/modes, reference shutdown and scratch-removal evidence.
Final review approved this bounded profile; the verification manifest binds the
release, source files and reports without claiming unrestricted compatibility.

The final read-only application probe still fails at `@io_bazel_rules_nogo`,
preserves the required Git symlink, and leaves checkout fingerprints unchanged:
`artifacts/mainrepo-direct-repository-loading.json`. These results are not
application query/build/test success or a whole-application performance claim.
