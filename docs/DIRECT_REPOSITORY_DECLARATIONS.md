# Direct repository declarations: the metadata phase

`use_repo_rule` in MODULE evaluation returns a callable and records retained
invocations as innate extension usages. It does not load the rule's `.bzl` file,
validate its exported symbol or instantiate its repository implementation.
Demanded direct repositories now have a separate bounded native executor;
see [REPOSITORY_RULES.md](REPOSITORY_RULES.md). This document describes recording.

The declaration takes exactly two positional strings. Those strings are retained
without premature label/symbol validation, including when an unused declaration
refers to an unavailable file. Identity follows Bazel 9.3's raw concatenation of
the two strings, separated by a space. Repeated identical identities share an
ordered usage; declarations with no retained invocations produce no usage.

Calls accept keyword-only `name`, optional Boolean `dev_dependency`, and additional
attributes. Names are validated before dev filtering. Dependency modules ignore
dev calls; the root does so only with `--ignore_dev_dependency`. Ignored direct
calls do not reserve names or capture extra attributes. By contrast, ignored dev
`bazel_dep` and `use_repo` imports still reserve names in Bazel 9.3. The same
effective ignore predicate also ignores overrides, including root overrides.
Ignored ordinary extension tags similarly avoid capturing unused attribute values;
their `use_repo` imports still participate in name-collision validation.

Retained calls reserve names in their owner's namespace and capture `name` with
the extra attributes; `dev_dependency` remains separate metadata. Attribute
bindings retain their original Starlark values until MODULE evaluation finishes:
later list/dictionary mutations are visible, while replacing a dictionary entry
used as an argument does not replace the previously captured value. Ordinary
extension tags follow the same rule. MODULE syntax rejects top-level name
rebinding, all `*args`, and `**kwargs` whose outer expression is not a literal
dictionary; these checks run before evaluation, including in unevaluated code.
The final owned representation supports bounded primitive/list/string-keyed
dictionary values as extension tags, not arbitrary Starlark objects or complete
repository-rule attribute semantics. Schema validation and label conversion
belong to later execution, not this recording phase.

A demanded direct repository now triggers real definition loading, whole-usage
attribute validation and its callback. Unsupported operations still fail rather
than creating fabricated source directories. Unrelated selected-module aliases
do not trigger definition loading or extension namespace allocation. Native
`mod graph` continues to reject active innate usages under its bounded profile.

Full compatibility still requires genuine built-in repository sources, complete
repository contexts and byte-string semantics, complete extension APIs,
downstream configured analysis and toolchains. Recording `http_archive(...)` is
**not** implementing `http_archive`.

Contract references are pinned to Bazel 9.3 commit
`e89755babf3e941bc1e9d8120b1cd48854ab9dfa`: `ModuleFileGlobals.java` lines 799–868,
`ModuleThreadContext.java` builder finalization, and `InnateRunnableExtension.java`
deferred load/symbol validation.

The original declaration increment's application probe progressed beyond `rules_swift`
and stops at the `hermetic_cc_toolchain` single-version patch override. Evidence:
`artifacts/mainrepo-direct-loading.json`. No successful application query or
build/test parity is claimed.

That original increment passed 349 Rust tests and 169 Python harness tests, formatting,
strict Clippy and release compilation. A separate public-native-API versus
reference-execution probe checks deferred list mutations for both innate calls
and ordinary extension tags (`artifacts/direct-attribute-reference.json`). That
probe executes repository implementations only in official Bazel: it is evidence
for native MODULE metadata, not native repository execution.

The current declaration harness retains 46 CLI differential cases, one separately
classified lazy-loading probe, and one native-only nonzero-extension-facts-version
boundary check. It retains fixture bytes, command diagnostics, binary hashes and
cleanup receipts. The lazy-loading probe also passes against the reference;
the unsupported facts-version error is explicitly not execution parity. Adjacent module,
local-repository and remote-archive reports pass 16, 20 and 17 checks respectively.
