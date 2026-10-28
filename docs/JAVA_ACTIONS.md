# Native Java compilation actions

`create_compilation_action` registers real JavaBuilder compilation for ordinary
sources/source jars, empty libraries and resource jars, without annotation processors. The
default advertised classpath mode remains **BAZEL**: dependency producers finish,
binary jdeps select an ordered reduced classpath, and the real compiler runs
`BAZEL_REDUCED`. Only its decoded fallback bit requests one `BAZEL_FALLBACK`
attempt with the full classpath. Ordinary rules_java constructs the JavaInfo.

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
- Configured action environment plus the resolved execution platform's UTF-8
  locale (including inherited OS constraints), propagated execution tags,
  `supports-path-mapping` and the default `internal-inline-outputs` directive.
  `.jdeps` is a real on-disk output, available on local/cache/remote result paths.
  No in-memory jdeps cache is needed for the supported branch.

**Strict deps:** the mode is validated (`OFF`, `ERROR`, `DEFAULT`, `WARN`, case
insensitive). In this direct-classpath header branch, genuine Bazel passes no
strict-deps flags or input jdeps to Turbine. It uses header direct dependencies
when present; otherwise it uses the supplied ordered compilation classpath.
It passes `--reduce_classpath_mode NONE` in both cases. This is the genuine
header optimization, independent of JavaBuilder's BAZEL reduced-classpath mode.
Full JavaBuilder compilation enforces the strict-deps mode with the real tool.

## Explicit boundaries

- Annotation processors/generated processor
  jars, additional processor outputs, extra Args, per-package toolchain data,
  JSpecify and Java coverage instrumentation fail explicitly in this slice.
  Ordinary dependent/sourcejar/empty library builds are proven; the application
  rules_jvm_external Java-library cquery passes with this implementation.
  Full application analysis next stops at the separate Java-binary
  `get_runtime_classpath_for_archive` API; no whole configured-graph claim.
- Non-direct classpath headers, absent direct compiler, API-generating
  processors and tree/unresolved-symlink artifacts fail explicitly.
  Non-API-generating plugins are validated but do
  not run during header compilation, matching Bazel.
- The chosen execution group, platform and exec properties use normal
  resolution. Resolved constraints are shared into action construction by Arc;
  the final environment is action-keyed. No global action/config rekey or
  persisted format change is introduced.
- Output-path stripping, changing in-memory jdeps mode and command-line limits
  remain outside the native option surface. No path stripping is performed.
- JavaBuilder executes locally (standalone/process-wrapper/sandboxed). Actual
  toolchain worker directives are retained, but this action policy is worker-
  incapable: default strategy selection can choose local; worker-only requests
  fail. Selected remote execution fails before submission. Remote spawn-cache
  lookup/upload is explicitly reported as skipped for JavaBuilder; native
  terminal caching and remote-only prerequisite materialization still work.

## JavaBuilder transaction

`JavaCompileAction.java:257-281,306-398,408-510` computes a reduced classpath
**after dependency producers finish**, using binary `.jdeps` contents. It stages
only selected jars, runs `BAZEL_REDUCED`, and initially requires only the jdeps
output. If the proto requests fallback it deletes partial outputs, resets
metadata/stdout/stderr, and executes again with `BAZEL_FALLBACK` and all outputs
required. Cache hits, workers, remote execution, path mapping and persistent
plans preserve that protocol using an optional typed JavaCompileSpec inside
ActionExtra. One logical key/lock covers the full semantic intent and all
potential input content identities; only the attempt's mandatory inputs and
selected jars are staged. Excluded-jar edits invalidate fallback-produced cache
entries. Both attempts remain unpublished; checked cleanup discards partial
outputs and buffered streams. All declared outputs are required before terminal
publication. Nonzero exit, parser/setup/cleanup failure and cancellation never
request fallback. Cancellation is checked between attempts, without claiming
new running-process interruption parity.

Successful fallback retains two spawn records and runner counts within one
logical action completion. Native action cache hits report no new spawns.

The proto2 decoder validates required dependency/location fields, skips unknown
fields/groups, preserves repeated/default scalar behavior and includes every
dependency kind. Limits: 16 MiB per proto, 64 MiB input metadata per compilation,
100,000 dependency records, 1,000,000 fields, nesting 100. Paths are selection
strings, never filesystem authority. Invalid UTF-8 dependency paths fail closed
(genuine Java replaces malformed bytes). No decoded metadata is memoized or
persisted. Binary snapshot format 6 stores intent only, with raw-path codecs.

Strict OFF or an absent output jdeps selects effective OFF exactly as the
genuine builder does. The CLI's other unmodelled classpath modes remain errors;
the fragment and factory share the advertised default. Source-jar packaging
continues through rules_java's own SingleJar action. Its optional manifest
fields stringify None as `None`, as shown by the saved genuine argv. The
`target_kind` bridge consumes actual rule-class metadata through aliases and
aspects; no JavaInfo or rule-class guesses are introduced.

## Resources

When resources, resource jars or classpath resources are present, JavaBuilder
writes the derived `-class.jar`, retaining the declared native-header, manifest
and jdeps outputs. A separate real SingleJar action merges that class jar and
the resource inputs into the requested output. It carries the tool's normal
FilesToRun/runfiles dependencies and the configured shell environment.

Resource entry names follow Bazel's Maven/Java source-root heuristics or the
explicit `resource_strip_prefix`. An explicitly empty prefix retains the whole
repository-relative path; it is distinct from an omitted prefix. Duplicate
resource destinations use the last artifact without reordering the entry.
An invalid prefix fails analysis. Resource output artifacts outside the declaring
package and tree/unresolved-symlink resources remain explicit boundaries.

## Evidence

See `tests/fixtures/java_actions/README.md` and `header_reference.json`. Source
pins: Bazel 9.3.0 `e89755babf3e941bc1e9d8120b1cd48854ab9dfa`, rules_java 9.1.0.
Key references: `JavaStarlarkCommon.java:116-175`,
`JavaCompilationHelper.java:442-489`, `JavaHeaderCompileAction.java:414-648`,
`JavaToolchainTool.java:44-131`, `JavaHelper.java:48-75`.
