# cquery output formats beyond proto (Bazel 9.3.1)

`cquery` prints `label`, `label_kind`, `proto`, `streamed_proto` (see
[CQUERY.md](CQUERY.md), [CQUERY_PROTO.md](CQUERY_PROTO.md)) and, natively:

| `--output=` | genuine source | native |
|---|---|---|
| `starlark` | StarlarkOutputFormatterCallback | yes (provider **names**, not values) |
| `files` | FilesOutputFormatterCallback | yes (own analysis key) |
| `jsonproto` / `textproto` | ProtoOutputFormatterCallback.writeData | yes, byte-identical printers |
| `graph` | GraphOutputFormatterCallback / GraphOutputWriter | yes |
| `build` | BuildOutputFormatterCallback | explicit error |
| `transitions` | TransitionsOutputFormatterCallback | explicit error |
| `--show_config_fragments` | RequiredConfigFragmentsProvider | explicit error |

An unknown format fails like genuine (exit 2): `Invalid output format 'X'.
Valid values are: label_kind, label, transitions, proto, streamed_proto,
textproto, jsonproto, build, graph, starlark, files`. `config` lists, shows and
diffs the configurations of the last successful cquery (below).

Line-oriented results are printed in label order (then discovery order), like
the native `label` output; genuine prints Skyframe order. Labels and
configuration checksums keep the native spellings documented in CQUERY.md.

## What the graph records

The configured graph keeps no providers. At the analysis boundary (still in the
live planning state, before it is dropped) the engine records per configured
identity:

- the `providers(target)` key names in genuine order
  (AbstractConfiguredTarget.toProvidersDictForQuery): the rule's declared
  providers in return order (Starlark providers as `//pkg:file.bzl%Name`,
  built-ins by name), `FileProvider`, `FilesToRunProvider`, `OutputGroupInfo`,
  `DefaultInfo`; platform/constraint rules list the files providers first;
  source files `VisibilityProvider, FileProvider, FilesToRunProvider,
  DefaultInfo`; output files add `InstrumentedFilesInfo`; incompatible targets
  `IncompatiblePlatformProvider, FileProvider, FilesToRunProvider,
  DefaultInfo`; package groups None. Lists are interned (graph `provider_sets`).
- `build_options(target)` per configuration: every 9.3 fragment option the
  native options model, as `//command_line_option:NAME` with the value
  `Starlark.fromJava` exposes (bool/int/str/list/`Label`, `CompilationMode`
  `fastbuild`, TriState/StripMode `<unknown object …>`), plus the Starlark
  settings the configuration holds. Unmodelled native options are absent
  (genuine has ~313 keys; the native target configuration models 54 of the 319
  genuine fragment options), never invented.
- for `--output=files` only: the exec paths of the important artifacts of the
  requested output groups (TopLevelArtifactHelper.getAllArtifactsToBuild:
  `default` = files to build, other groups from OutputGroupInfo, `_`-groups
  hidden; `--output_groups` honoured). `--output=files` therefore keys its own
  analysis (cquery_state `presentation`); every other format shares one graph.

Measured on the private ~/code clone, `deps(//...)` with the snapshot off
(release builds, one run each, shared host): recording the facts adds 5.0 s
(analysis boundary 179.9 → 184.9 s) and 132 MiB peak RSS (6,724 MiB hwm;
main f7d0c36e: 6,592 MiB, no boundary step); RSS at graph-ready is 76 MiB
higher. Label
output is byte-identical to main's. (A first version that built dependency
views for every key cost +921 MiB and was replaced by the light read.)

## `--output=starlark`

`--starlark:expr` (default `str(target.label)`) or `--starlark:file` defining
`format(target)`. The constructor checks and messages are genuine's, printed as
`Error doing post analysis query: …` with exit 1: both options, unreadable
file (`invalid --starlark:file: failed to read F`), no `format`, non-function
`format`, wrong arity, and a non-expression `--starlark:expr` (the native
parser's message text differs). A per-target evaluation error is an
`ERROR: Starlark evaluation error for LABEL: Traceback …` event; the target
prints no line and the command still succeeds, as in genuine (call columns
follow Java's `(` location; other message texts are starlark-rust's).

`target` is a `Target` with genuine repr/str (`<target L>`, `<alias target A of
L>`, `<input file target L>`, `<output file target L>`, `<unknown target L>`)
and `target.label` (an alias's actual label). `providers(target)` is a dict of
the recorded names; its values are opaque and reading a field fails with
"does not retain provider values". `target.files` and the other configured
target fields fail likewise. Globals: the Starlark universe, `depset`, `json`,
`struct`, `providers`, `build_options`. No step limit is enforced.

### target-determinator default mode

TD v0.34.0 with `-filter-incompatible-targets=true` (its default) runs
`//... - kind(alias, //...)` with `target.label if "IncompatiblePlatformProvider"
not in (providers(target) or []) else ""` and `kind(alias, //...)` with
`repr(target).split(" ")[2] …`. On the private ~/code clone, pair P4
(`09d94173 → ff579ddf`, docs/AFFECTED_TARGETS.md), genuine 9.3.1 (client/server,
private output root) and native bazel-rs (private cache) were run under the
official binary with `-nocache_results`:

| | TD result | filter cquery (both revisions) | alias filter | wall |
|---|---|---|---|---|
| genuine 9.3.1 | `//src/beyondwork.ai/bworker/wcb/wcbservice:wcbservice_test` | 42,358 lines | 31 lines | 264 s |
| bazel-rs | identical | identical when sorted | same set; genuine repeats 2 aliases in a second configuration (known alias identity difference) | 1,019 s cold (new binary: repository publication + uncached analysis) |

Both filter cqueries are graph hits natively (6–11 s, the deps universe is the
same `//...`).

## `--output=files`

Distinct exec paths in result order (FilesOutputFormatterCallback's
uniquified results): rules and output files their important artifacts,
source files their own path (default group only), hidden (`$`) rule classes
skipped, incompatible targets empty. Top-level aspects (`--aspects`) are not
included.

## `--output=jsonproto|textproto`

The same `CqueryResult` (or `QueryResult` with
`--noproto:include_configurations`) as `proto`. The writer streams one record
per target/configuration; `cquery_text::Transcoder` decodes each with the
upstream descriptor set (`src/cquery_schema.pb`, the 9.3 analysis_v2/build
schema) and prints it like protobuf-java's `JsonFormat.printer()` (camelCase
names, field-number order, 64-bit ints quoted, enum names, Gson HTML-safe
escaping, `[{` element layout) and `TextFormat.printer()` (octal escapes of
non-ASCII bytes). Retained memory is one record. Transcoding genuine's own
streamed records reproduces genuine's jsonproto/textproto byte-for-byte,
with and without configurations (tests/cquery_output_text.rs). Content
differences to genuine are those of the proto DTOs (CQUERY_PROTO.md).

## `--output=graph`

GraphOutputWriter with sorted labels: nodes `LABEL (short config)`, ordered by
label then checksum (null first), edges filtered by `--[no]implicit_deps`,
`--[no]tool_deps`, `--[no]include_aspects` to results; `--graph:factored`
(default) groups equivalent nodes, `--graph:node_limit` truncates.

## `config`

`config` (listing `Available configurations:` / `{"configuration-IDs": …}`),
`config ID` (prefix; `Displaying config with id` event), `config ID1 ID2`
(diff), `--dump_all`, `--output=text|json`; ConfigCommand's errors (`No
configuration found with ID prefix P`, ambiguous prefix, `Too many config
ids.`). It reads the last successful cquery's configuration table; native
configurations have no TestOptions fragment and are never shown as
`(test-trimmed)`.

## Configuration checksums (investigated, not changed)

Genuine's checksum is `BuildOptions.checksum()`: SHA-256 over protobuf
`writeStringNoTag` of each `FragmentOptions.cacheKey()` (sorted by class name;
`CLASS{name="value", …}` over every option sorted by name, `EMPTY`/`NULL`
forms), then the Starlark options map and the flag `scopes` map.
`tests/fixtures/cquery_output/checksum_poc.py` recomputes all five checksums of
a genuine capture from its jsonproto configuration table. Making native
checksums equal requires a value for **every** one of the ~319 genuine options
of the 17 fragment classes in each configuration (exec and trimmed
configurations included), in Java `toString` form, while native models 54 and
accepts more options than it models. A default table would mismatch silently
whenever an unmodelled option is set (rc files, exec transition). This is a
large change, not a moderate one: configuration identities stay native
ConfigIds (CQUERY.md), and cross-engine comparisons keep normalizing them.

## Validation

`tests/fixtures/cquery_output` holds an offline workspace (local platforms
module, Starlark transition, select, aspect, alias, genrule, incompatible
direct/indirect/alias targets), `cases.tsv` and genuine 9.3.1 captures
(`capture.sh`, ELF sha256 `7f775b54…585c`). `tests/cquery_output_formats.rs`
runs every case natively and compares sorted distinct lines with checksum
prefixes normalized (plus exit codes and error events); the genrule setup
source of the explicit-module profile is dropped on both sides. A toolchain
case (`toolchain_type`, `ToolchainInfo` keys) and the BCR-platforms variant
were compared manually and matched.
