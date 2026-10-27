# Java plugin flag bridge (Bazel 9.3)

`java_plugins_flag_alias` is analyzed at the two genuine allowlisted labels:
`//tools/jdk:java_plugins_flag_alias` and
`@@bazel_tools//tools/jdk:java_plugins_flag_alias`. This does **not** implement
javac or claim general Java compilation support.

## Contract

* `--plugin` is a repeatable `LabelListConverter`: comma-separated labels,
  concatenated in flag order, empty by default. Empty occurrences do not reset
  earlier occurrences; duplicates remain in configuration identity. `query`
  rejects this JavaOptions flag. `config_setting`, Starlark transitions, and
  `ctx.fragments.java.plugins` see the converted list. The exec transition
  clears it to avoid cycles. Default configurations retain their previous IDs;
  there is no global action rekey.
* The hidden, late-bound `:java_plugins` attribute has ordinary configured
  prerequisite edges in the exec configuration. Duplicate prerequisites are
  visited once. Only selected plugins enter the graph. Late-bound visibility
  is bypassed, but testonly validation remains active.
* Rules must supply either exact exported provider identity:
  `@@rules_java+//java/private:java_info.bzl%JavaPluginInfo` or
  `//java/private:java_info.bzl%JavaPluginInfo`. No name-only matching. As in
  genuine 9.3, source-file prerequisites are accepted without a provider.
* First collect external-identity providers; only when there are none collect
  main-identity providers. Publish **both** identities from the chosen list.
  Merge `plugins` and `api_generating_plugins` independently, with preorder
  depsets of string `processor_classes` and File `processor_jars` /
  `processor_data`. Reuse depset DAGs, preserving type/order checks.
* Validate Java outputs but discard them from the merged result. Default files,
  default/data runfiles and outputs remain empty. Plugin artifacts are **not**
  requested build outputs merely because a bridge is built. No bridge action
  is registered, and cquery executes neither actions nor workspace status.
* Genuine cquery proto reports the late-bound attribute's empty loading default;
  selected plugins appear in `configured_rule_input`, with exec configuration
  IDs. Existing wire fields suffice; no invented tags or new snapshot format.

The real rules_java constructors remain Starlark-defined. The small private
`java_common.check_provider_instances` helper used by `JavaInfo` construction
now validates exported provider identity and honors its private API allowlist.
It does not invoke Java actions or resolve a Java toolchain. Other unimplemented
Java APIs still fail explicitly; no flag is silently discarded.

## Captures and tests

`tests/java_plugins_alias.rs` extracts checksum-verified release archives and
loads the unchanged rules_java 9.1.0 provider constructors, including genuine
`JavaInfo` outputs and generated processor jars/data. See the fixture's
`reference.json` for 15 independent genuine 9.3 captures (12 successes and three
expected analysis errors), source/ELF/archive pins and capture conditions.
Tests cover empty/nonempty, duplicate merges, main-provider fallback, mixed
identity precedence, output stripping, runfiles, private plugin visibility,
required-provider/testonly errors, exec reset, transition changes, configured
edges, and graph cache reuse/invalidation.

Flag changes, BUILD/`.bzl` changes and source presence changes cannot reuse a
stale graph. Source **content only** changes remain valid graph hits, as in
`CQUERY.md`: configured analysis returns File identities, not file bytes, and
target-determinator reads source contents itself. No source-byte digest or
execution machinery was added to cquery. This is not a whole-TD-success claim.

Pinned source: `JavaPluginsFlagAliasRule.java:42-108`,
`JavaSemantics.java:49-53`, `JavaOptions.java:351-362`,
`JavaConfiguration.java:392-395`, `JavaPluginInfo.java:79-244`,
`JavaOutput.java:170-194`, `JavaStarlarkCommon.java:354-387,427-437`,
`builtin_exec_platforms.bzl:350-390`.
