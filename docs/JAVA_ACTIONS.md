# Native Java compilation actions

`java_common.internal_DO_NOT_USE().create_header_compilation_action` implements
the **direct compiler, direct classpath, no API-generating processors** slice of
Bazel 9.3. It registers an ordinary `Turbine` spawn. The selected real compiler
produces the header jar, transitive-header jar and binary `.jdeps` output. This
same registration runs in build and cquery analysis; cquery never executes it.

## Supported contract

- Bazel's positional signature, including the two final positional-or-named
  arguments. Current-context checks, resolved Java toolchain declaration,
  automatic/default execution groups, ordinary declared-output validation and
  producer edges apply.
- The actual rules_java `JavaToolchainInfo`, `JavaRuntimeInfo`, `JavaPluginInfo`
  and `BootClassPathInfo` provider identities, field types and depsets are
  consumed. No native replacement JavaInfo or JavaToolchainInfo is constructed.
- `.java` source filtering, source jars, additional inputs, toolchain tools,
  nonempty bootclasspath override (empty overrides fall back to the toolchain),
  ordered header dependencies, compiler options and target-label escaping.
- A jar tool launches with runtime Java + toolchain JVM flags + tool JVM flags
  + `-jar`. An executable tool launches directly with its own flags. Inputs
  include tool data/executable and, only for a jar, runtime files. Tool runfiles
  are intentionally absent, as in `JavaToolchainTool.addInputs`.
- Javac option depsets are reversed and tokenized as in `JavaHelper`; malformed
  quoting is passed to the compiler. Normal generic command-line splitting
  produces unquoted `<primary-output>-0.params` files when needed.
- Configured action environment plus UTF-8 locale, propagated execution tags,
  `supports-path-mapping` and the default `internal-inline-outputs` directive.
  `.jdeps` is a real on-disk output, available on local/cache/remote result paths.
  No in-memory jdeps cache is needed for the supported branch.

**Strict deps:** the mode is validated (`OFF`, `ERROR`, `DEFAULT`, `WARN`, case
insensitive). In this direct-classpath header branch, genuine Bazel passes no
strict-deps flags or input jdeps to Turbine. It uses header direct dependencies
when present; otherwise it uses the supplied ordered compilation classpath.
It passes `--reduce_classpath_mode NONE` in both cases. This is the genuine
header optimization, independent of JavaBuilder's BAZEL reduced-classpath mode.
Strict-dependency enforcement by full Java compilation remains unsupported.

## Explicit boundaries

- `create_compilation_action` remains unimplemented. Consequently an ordinary
  rules_java `java_library`, including the rules_jvm_external target from the
  application receipt, is **not unblocked** by this increment.
- Non-direct classpath headers, absent direct compiler, API-generating
  processors, tree/unresolved-symlink artifacts and a custom host execution
  platform fail explicitly. Non-API-generating plugins are validated but do
  not run during header compilation, matching Bazel.
- Native toolchain resolution currently selects only the host execution
  platform. This slice supports its default OS/locale; a custom host platform
  needs OS constraints carried into the action-construction context first.
- Output-path stripping, changing in-memory jdeps mode and command-line limits
  remain outside the native option surface. No path stripping is performed.
- Resources, resource jars, JSpecify, JavaBuilder workers, generated processor
  outputs and source-jar packaging belong to full Java compilation, not this
  header API. They are not claimed or silently handled by this implementation.

## Why JavaBuilder is a separate stage

`JavaCompileAction.java:257-281,306-398,408-510` computes a reduced classpath
**after dependency producers finish**, using binary `.jdeps` contents. It stages
only selected jars, runs `BAZEL_REDUCED`, and initially requires only the jdeps
output. If the proto requests fallback it deletes partial outputs, resets
metadata/stdout/stderr, and executes again with `BAZEL_FALLBACK` and all outputs
required. Cache hits, workers, remote execution, path mapping and persistent
plans must preserve that protocol. The current generic action contract has
tree/Args deferral but no jdeps consumer or conditional output/fallback state.

Replacing this with a full classpath or `JAVABUILDER` reduction would change the
advertised default `BAZEL` semantics. This stage keeps the full-compilation
error rather than registering an incomplete action or a cquery-only success.

## Evidence

See `tests/fixtures/java_actions/README.md` and `header_reference.json`. Source
pins: Bazel 9.3.0 `e89755babf3e941bc1e9d8120b1cd48854ab9dfa`, rules_java 9.1.0.
Key references: `JavaStarlarkCommon.java:116-175`,
`JavaCompilationHelper.java:442-489`, `JavaHeaderCompileAction.java:414-648`,
`JavaToolchainTool.java:44-131`, `JavaHelper.java:48-75`.
