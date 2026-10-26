# cquery wire formats (Bazel 9.3)

`cquery_proto` is a **serializer and owned metadata model**, not a cquery command,
configured-graph implementation, or claim of full cquery compatibility. The CLI
and graph adapter are separate work. It has no engine, state, CLI, protobuf-runtime,
or JSON dependency. DTOs derive serde and round-trip through postcard.

## Caller contract

```rust
use bazel_rs::cquery_proto::{CqueryWriter, OutputFormat, Transitions, WriterOptions};

// target/configurations are owned DTOs supplied by configured analysis.
# fn example(target: &bazel_rs::cquery_proto::Target,
#            configurations: &[bazel_rs::cquery_proto::Configuration]) -> std::io::Result<()> {
let mut writer = CqueryWriter::new(std::io::sink(), OutputFormat::StreamedProto,
    WriterOptions { transitions: Transitions::Lite, ..Default::default() });
writer.write_target(target, configurations.first())?;
writer.write_configurations(configurations)?;
writer.finish()?;
# Ok(()) }
```

- `Target` contains a `Rule`, `SourceFile`, `GeneratedFile`, or `PackageGroup`.
  Payloads and `Configuration` have public fields and `Default` implementations.
- Attributes use `AttributeValue`, not untyped values. Scalars, ordered lists,
  ordered dict entries, license, Boolean, and Tristate are distinct variants.
  `Unset(AttributeType)` means no value field, **not** empty string/zero/false.
  `Option` preserves proto2 field presence. An explicit false differs from unset.
- `Attribute::new(name, value, explicit)` supplies ordinary AttributeFormatter
  metadata. Override `nodep` for NODEP labels, and `source_aspect_name` for aspects.
  Synthetic `$rule_implementation_hash` has String value and all three metadata
  fields `None`. The adapter supplies the actual transitive implementation hash.
- Core must resolve selects, preserve defaults and explicitness, provide readable
  source locations, preserve alias identity and generated-file owner, and supply
  graph edges. No selection/analysis/hashing takes place in this module.
- Labels and full native configuration checksums pass through **verbatim**.
  Non-null configs need positive output-local IDs, nonempty checksum != `"null"`.
  The writer does not mint identity values, deduplicate, or maintain an ID map.
  Native checksums need not equal Bazel's BuildOptions checksums; that is an
  analysis compatibility distinction, not a serializer normalization.
- Call `write_target(&target, None)` for null configuration. Inline deprecated
  Configuration contains `checksum="null"`, and target configuration_id defaults
  to zero. For configured targets the inline message contains only checksum;
  full config metadata goes in the separately emitted table.
- Null edges have **both** configuration fields absent (not `"null"`/Some(0)).
  Non-null edges carry actual checksum and ID; inconsistent pairs are rejected.
  `configured_rule_input` is emitted only for Transitions::Lite/Full.
- Caller emits each table config once, including edge-only configurations, usually
  after all targets. Null has no table entry. Attributes and all repeated values
  are emitted in supplied order; no hidden sort/dedup changes list/dict semantics.
  Upstream sorts ordinary attributes, then appends the implementation hash, then
  sorted aspect attributes. `rule_input` can include unselected branches even
  though `configured_rule_input` must represent the configured graph.

## Shapes and presentation options

| Format | Configurations | Wire shape |
|---|---|---|
| proto | included | One unframed analysis.CqueryResult |
| streamed_proto | included | varint length + CqueryResult, one target **or** config per frame |
| proto | excluded | One unframed blaze_query.QueryResult |
| streamed_proto | excluded | varint length + QueryResult, one target per frame |

Unframed output streams repeated fields directly; it never assembles a graph-sized
message. Streamed output is **not bare ConfiguredTarget**. Empty output is empty
bytes in either format. `finish()` flushes and reports sink errors; discard output
after any error because a prior complete record or partial write may exist.

`WriterOptions` controls configurations, transitions, default attributes, locations,
rule input/output lists, attribute source aspects, and definition/instantiation
stacks. Defaults match Bazel. Synthetic and aspect attributes survive disabling
default values as in upstream. Per Bazel 9.3's actual callback, transitions still
emit edge checksums/IDs with `--noproto:include_configurations`, despite the schema
comment suggesting otherwise.

No support is claimed for environment-group targets, unresolved SELECTOR_LIST,
Fileset entries, Stardoc rule_class_info, deprecated Rule fields, textproto/JSON
output, or computing synthetic attr hashes. Callers **must reject** unsupported
format flags, or explicitly implement attribute selection / relative locations
before passing DTOs. Stacks must be captured by analysis before enabling those
writer options. No unsupported values should be silently coerced to strings.

## Wire details pinned by tests

- Proto2 required fields are emitted even when empty; optional zero/empty values
  retain presence. Proto3 non-optional defaults are omitted normally.
- int32 negatives are sign-extended ten-byte varints, **not zigzag** or five-byte
  unsigned casts. Integer lists use unpacked proto2 representation.
- Boolean false writes `boolean_value=false`, `int_value=0`, `string_value="false"`.
  Tristate No/Yes/Auto uses enum 0/1/2, legacy integer 0/1/-1, strings no/yes/auto.
- UTF-8, empty dict keys/values, insertion order, duplicates in ordered lists,
  explicitness, unset label values, and aspect metadata are retained.
- **Upstream quirk:** LABEL_LIST_DICT discriminator 12 uses
  `string_list_dict_value` field **11**, not `label_list_dict_value` field 10.
  This follows actual AttributeFormatter 9.3 and is present in genuine captures.

## Genuine fixture and independent oracle

`tests/fixtures/cquery_proto/` contains a small offline scratch module, original
Bazel outputs, upstream schema descriptor set, generated **typed Rust DTOs**, and
regeneration/check tooling. No archive, repository cache, downloaded dependency,
or JSON cache is committed. `schema.pb` includes analysis_v2.proto, build.proto,
and stardoc_output.proto from Bazel **9.3.0**, source commit
`e89755babf3e941bc1e9d8120b1cd48854ab9dfa`; `SHA256SUMS` pins the source files and ELF.
The runtime contract is additionally checked against ProtoOutputFormatterCallback,
ProtoOutputFormatter, AttributeFormatter, and target-determinator v0.34.0 commit
`d4b6125546979713431e63b5c3e65810fa989446` (which reads inline checksums).

Capture uses a private HOME/TMPDIR/output base, `--batch`, ignored rc files,
disabled repository downloads and autoload, and a tiny local platforms module.
It analyzes **15 targets / 2 configurations**, including source/generated/alias/
package-group nodes, a resolved select, implicit aspect input, all three genrule
tristates, bool/default/explicit-false, int limits/lists, UTF-8 and all supported
dict types. Genuine logs reported **0 total actions**; generated files are absent.
All captures include `--transitions=lite --proto:include_attribute_source_aspects`.

```sh
# Python google.protobuf + grpc_tools must already be installed; capture installs nothing.
python3 tests/fixtures/cquery_proto/fixture.py capture \
  --scratch /tmp/opencode/cquery-proto-capture \
  --reference /tmp/opencode/bazel-9.3-reference \
  --bazel /home/coder/bazel/.direnv/reference/bazel-9.3.0-linux-x86_64

# Ordinary gates require only Rust: no Bazel/Python/network needed.
cargo test --locked --test cquery_proto_format --test cquery_proto_streaming

# Independently parse actual Rust output with upstream-generated descriptors.
BAZEL_RS_CQUERY_PROTO_DUMP=/tmp/opencode/cquery-proto-capture \
  cargo test --locked --test cquery_proto_format
python3 tests/fixtures/cquery_proto/fixture.py check /tmp/opencode/cquery-proto-capture/native.proto
python3 tests/fixtures/cquery_proto/fixture.py check \
  /tmp/opencode/cquery-proto-capture/native.streamed_proto --streamed
```

The original proto is compared **byte-for-byte** to serialization of captured
DTOs. Canonical streamed/query fixtures are constructed by the independent
upstream-schema Python protobuf implementation, not this writer. Original genuine
streamed and no-config streamed captures are compared semantically with original
proto (only output-local IDs and target/edge order normalized; never attribute,
list or dict ordering). `IsInitialized` checks required fields recursively.
Source paths/config checksums in checked-in goldens intentionally record the
capture workspace; these tests are format replay, not native graph parity tests.

## Bounded-memory evidence

One Vec holds a single wrapper/record. Nested lengths are reserved and compacted
in place: no per-child Vec, no graph traversal/size prepass, and no second full
serialization buffer. Retained memory is O(largest record), plus BufWriter's fixed
8 KiB; the graph/config table remains caller-owned. A large record can naturally
require a large buffer; this is not a global byte cap.

`tests/cquery_proto_streaming.rs` installs an isolated test-only System-allocator
counter and writes to a counting sink. After warming the largest record, it
asserts **zero alloc/realloc calls**, unchanged scratch capacity, and actual byte
counts, for both formats. Production code remains safe Rust. Measurement on
2026-10-26 (debug binary, shared machine, **not CLI/analysis performance**):

| Format | Target records | Output bytes | Scratch capacity | Post-warm allocations | Wall |
|---|---:|---:|---:|---:|---:|
| proto | 1,000,001 | 8,742,008,812 | 16,732 B | 0 | 18.485 s |
| streamed_proto | 1,000,001 | 8,744,008,815 | 16,732 B | 0 | 15.672 s |

Whole test-process peak RSS: **18,944 KiB** (Python `getrusage(RUSAGE_CHILDREN)`),
34.164 s wall. Output is counted/discarded, not retained or written to disk.
Reproduce with `CQUERY_STREAMING_RECORDS=1000000 cargo test --locked --test
cquery_proto_streaming -- --nocapture`; ordinary gates use 2,000 additional
records per format. No throughput claim is made for actual disks or full analysis.
