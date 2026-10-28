# Java runtime classpath for archives

`java_common.internal_DO_NOT_USE().get_runtime_classpath_for_archive(jars,
excluded_jars)` implements the pinned Bazel 9.3 helper. It is a pure value
operation with the normal private Starlark API allowlist, usable during `.bzl`
initialization and analysis. It requires no `ctx`, toolchain or filesystem access.
Both arguments are required, positional-only depsets.

## Exact branches

- **Empty exclusions:** return the original `jars` value, retaining its identity,
  order, DAG and freezing behavior. Genuine Bazel does not inspect the runtime
  element type in this branch; a depset of strings is therefore passed through.
  The native emptiness check is O(1), with no DAG traversal or allocation.
- **Nonempty exclusions:** runtime elements must be Files. Flatten in the input
  depset's order, remove members equal to an excluded Artifact, and return a
  fresh stable/default-order File depset. Empty results have the normal default
  empty-depset identity. Nonempty unchanged results are still new depsets.
- The exclusion depset is deliberately untyped in genuine Bazel. Strings,
  Labels, structs and None do not equal Files, so they remove nothing but still
  select the fresh/stable branch. The native implementation avoids traversing
  such non-File exclusion DAGs.
- Membership uses existing File artifact identity, including configuration/root
  and ownership distinctions represented by the artifact. It does not compare
  basenames, short paths, label strings, extensions or file contents. Directories
  and unresolved symlink artifacts are Files too and are not rejected here.

Filtering is invocation-local O(runtime DAG + File-exclusion DAG) work and uses
no persistent or global cache. Nonempty filtering follows genuine's flattening
boundary; the empty-exclusion path shares the original DAG. No action/config
keys, snapshot formats or general depset ordering rules change.

## Caller policy remains in rules_java

rules_java 9.1.0 `java_binary_impl.bzl:124-147,293-320` assembles the transitive
runtime classpath, unions the `deploy_env` providers' classpaths, then calls this
helper to subtract them. `JavaInfo.transitive_runtime_jars` already implements
`neverlink`; this helper adds no independent neverlink or dependency heuristic.
The binary's own class jar and launcher jars are added separately by the deploy
archive caller (`java_binary_deploy_jar.bzl:64-95`).

This implementation supplies the helper's real semantic result. It does not
claim that every Java-binary launcher/native-library/deploy feature is supported.
The existing JavaBuilder backend/processor restrictions still apply.

## Evidence

Pinned reference: Bazel 9.3.0 source
`e89755babf3e941bc1e9d8120b1cd48854ab9dfa`:

- `JavaCommonApi.java:646-653`: binding and parameter types.
- `JavaStarlarkCommon.java:336-350`: allowlist, identity shortcut, typed runtime
  flattening, untyped exclusions and stable result.
- `Depset.java:161-167,207-245` and `NestedSetBuilder.java:215-224`: empty/type,
  flattening and wrapping behavior.

`tests/fixtures/java_runtime_classpath` records genuine order/identity/type/error
results and a real archive consumer. Tests cover all four depset orders, shared
transitives/duplicates, empty/all/irrelevant exclusions, non-File exclusions,
source/generated/same-basename artifacts, canonical/apparent aliases, frozen
provider values, execution-free cquery and bounded cache/source/flag/dependency
edits. The actual rules_java `create_deploy_archive` function consumes the helper
result with genuine JavaInfo/JavaRuntimeClasspathInfo and real SingleJar; its
jar and manifest match bytes, and `java -jar` prints `archive:kept`.

Application follow-up on the readonly PR clone: the targeted AddJarManifestEntry
cquery passes this helper and stops at `java_common.collect_native_deps_dirs`,
`java_common_internal.bzl:452` via `java_binary_impl.bzl:162` (integrated candidate:
2.565s,702.1MiB,
no output, unchanged canaries). Full cquery was not repeated because this next
binary API still blocks the target. No Java-binary/whole-graph/TD success claim.
