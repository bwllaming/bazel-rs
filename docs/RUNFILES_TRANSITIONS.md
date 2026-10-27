# Configured runfiles policy (Bazel 9.3)

`build_runfile_links` and `build_runfile_manifests` are now real CoreOptions
values, not request-global switches. `enable_runfiles` was already a CLI/
config_setting TriState; it now also has native transition read/write support.
All three propagate to exec configurations, as in `builtin_exec_platforms.bzl`.

The rules_python executable incoming transition reads/writes both
`build_runfile_links` and `enable_runfiles` (`py_executable.bzl:2183-2194,
2243-2254`). Merely admitting those names would leave every transitioned target
using the process's original runfiles policy. This implementation instead:

- Converts CLI values in **Configuration only**, including alias reparsing.
  MODULE aliases cannot leave a runfiles side effect from the first parse.
- Carries nondefaults in configuration identity; explicit true/true/auto are
  omitted and keep default IDs/action keys unchanged. Former CLI false link/
  manifest flags intentionally get new configured IDs. No global action rekey.
- Reads/writes bools and real `TriState` transition values, including enum
  readback, string conversion, primitive-bool None/type errors, and nullable
  TriState. A null TriState is distinct in identity, reads as None to transitions,
  disables symlinks, but `FieldOptionDefinition.getValue` exposes its **unparsed
  string** default `auto` to config_setting/mnemonic code. It is not enum AUTO.
- Derives effective options separately for every rule and aspect computation,
  provider construction, and RunfilesSupport planning. Only the unrelated
  compact repository mapping policy remains request-global.
- Retains effective options in each executable's `RunSettings` through aliases,
  incoming transitions and binary snapshot handoff. The run/test tail uses the
  executable's options, not the original CLI or a global test-runner boolean.
  Run plans use the post-incoming-transition configuration and support nodes.
- Preserves true links => `.runfiles/MANIFEST`, false links =>
  `.runfiles_manifest`, false manifests => None. Repo mapping File identity
  remains executable-relative. Tool actions still get usable runfiles; `run`
  ensures a tree even with links=false and rejects manifests=false based on the
  actual target configuration. Tests stage their required entries and get
  per-target `RUNFILES_MANIFEST_ONLY` environment values.
- Honors removal of `_hidden_top_level_INTERNAL_` by `--output_groups=default`:
  a plain build then requests no runfiles manifests/mapping/tree. Run/test
  requests still pull in the support they need. Planning never executes under
  cquery or `--nobuild`.

No new ctx field is exposed. Configured proto uses the existing option/edge
fields; two split configurations of one executable have distinct configured
inputs and real manifest Files. Graph snapshots include the option settings and
support output-format/root sharing without reanalysis. The existing binary plan
payload carries the added RunSettings value; ELF identity invalidates old
snapshots, with no new cache format or JSON store.

## Evidence

`tests/runfiles_transitions.rs` and `tests/fixtures/runfiles_transitions` pin the
tiny genuine captures summarized in `reference.json`. They cover CLI/select,
macro-called native config_setting, incoming/outgoing/split/exec transitions,
same executable in multiple configurations, exact ST output mnemonics,
manifest/mapping File identity, hidden output selection, real shell tool actions,
aspect-private exec tools, run/test, converter errors, null TriState, snapshot
replay, configured proto fields and MODULE-alias reparsing.

Genuine source: CoreOptions.java:591-609,954-961;
RunfilesSupport.java:289-342,501-544;
RuleConfiguredTargetBuilder.java:144-173;
BuildConfigurationValue.java:689-696,866-876;
FunctionTransitionUtil.java:561-581,664-764;
FieldOptionDefinition.java:131-139;
OutputPathMnemonicComputer.java:385-412;
builtin_exec_platforms.bzl:237,249-250.

## Boundaries, not genuine bugs

`incompatible_compact_repo_mapping_manifest` is still a request-only option;
transitions on it remain rejected. No broader Python/native API claim is made.

An exploratory genuine-success probe using an aspect **subject**'s
`target[DefaultInfo].files_to_run` failed natively with
`actions.run executable FilesToRunProvider has no executable`. This is retained
separately in the worker report as an unaddressed provider-view gap, not counted
as fixed. The committed aspect proof uses an aspect's real private exec-tool
attribute, with configuration propagation and actual shell execution verified
on both implementations. Existing runfiles/source/assertion suites are unchanged.
