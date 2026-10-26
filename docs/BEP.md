# Build Event Protocol file outputs

`build`, `test` and `run` write Bazel 9.3's Build Event Protocol
(`src/main/java/com/google/devtools/build/lib/buildeventstream/proto/build_event_stream.proto`)
to local files. Implementation: `src/bep/` (`mod.rs` the streamer and the
events, `proto.rs` the encodings, `plan.rs` what the planned request carries).
Tests: `tests/bep.rs` against the genuine captures in `tests/fixtures/bep/`
(receipt `reference_receipt.json`, 7/7 scenarios structurally equal).

## Options

| option | behaviour |
|---|---|
| `--build_event_json_file=F` | one `JsonFormat.printer().omittingInsignificantWhitespace()` event per line (JsonFormatFileTransport): lowerCamelCase names, int64 as strings, enums by name, Gson's HTML-safe escaping (`=` is `\u003d`) |
| `--build_event_binary_file=F` | length-delimited `BuildEvent` protos (`writeDelimitedTo`, BinaryFormatFileTransport) |
| `--build_event_text_file=F` | `event {\n<TextFormat>}\n\n` blocks (TextFormatFileTransport; octal escapes of non-ASCII bytes) |
| `--build_event_publish_all_actions` | every executed action posts ActionExecuted (cache hits never), not only failed ones |
| `--build_metadata=K=V` | the BuildMetadata event's map |
| `--[no]build_event_{json,binary,text}_file_path_conversion` (and `experimental_` names), `--build_event_*_upload_mode`, `--build_event_max_named_set_of_file_entries` | accepted, validated; without a Build Event Service both path conversions produce `file://` URIs (BuildEventServiceModule.java:830-910), so they change nothing |
| `--bes_backend=URL` | non-empty: explicit error, exit 2 (no gRPC upload) |

Relative file names resolve against the working directory. All files are
opened when the command starts (an unwritable path fails the command, exit 2)
and closed when it ends, followed by `INFO: Build Event Protocol files produced
successfully.` like Bazel. Without these options nothing here runs: every hook
is one relaxed atomic load, nothing is recorded, the output is unchanged.

## The stream

`Streamer::post` is BuildEventStreamer.post (runtime/BuildEventStreamer.java
:284-389): each event announces its `children`; an event that was not
announced is linked by a Progress event (`progress {opaque_count: N}`, children
`[progress N+1, the event]`); the event that posts the last announced id has
`last_message`; announced ids that never post are closed with `aborted`
(`NO_BUILD` under `--nobuild`, `INCOMPLETE` after a failed build,
`LOADING_FAILURE` before planning). The final progress event follows
BuildFinished (ProgressEvent.finalProgressUpdate).

| when | events |
|---|---|
| command start (`bep::install`, from main after option parsing) | BuildStarted (children: progress, the command lines, BuildMetadata, OptionsParsed, WorkspaceStatus, Pattern, BuildFinished), BuildMetadata, UnstructuredCommandLine, OptionsParsed, StructuredCommandLine `original` and `tool` |
| after finalize (`bep::planned`, crate::request::execute_with_tests: build, test and run, miss and hit alike) | Pattern (expanded; children the TargetConfigured ids), WorkspaceConfig, StructuredCommandLine `canonical`, Configuration (the top-level one first, then the trimmed one of non-test targets, like TestTrimmingTransition), WorkspaceStatus (the stable and volatile keys, sorted), TargetConfigured per requested target (`target_kind`, `test_size`, `tag`; children its TargetComplete id) |
| command end (`bep::finish`, from main's exit funnel; `run` before it execs the target) | ActionExecuted (failed actions; with publish_all every executed one; a `none` Configuration event before the workspace status action), NamedSetOfFiles and TargetComplete per target (`output_group` per reported group, rule tags, test timeout; a failed target names its failed actions as children and carries the failure detail), TestResult per test attempt and TestSummary (`test`), ConvenienceSymlinksIdentified, BuildFinished (exit code name/number, failure detail), final Progress, Aborted, BuildToolLogs (`elapsed time`, `critical path`, `process stats`, `command.profile.gz` with `--profile`), ExecRequestConstructed (`run`), BuildMetrics |

The scheduler's worker hook (`bep::action_done`, crate::scheduler::run_job)
records every finished action: outputs of the successful ones (TargetComplete's
files: each requested artifact at its place in the output tree below the
execution root, digest and length of the file), failures with their argv and
stderr (saved as `bazel-out/_tmp/actions/stderr-N` for ActionExecuted.stderr).
A target succeeded when every producer of its reported files did; one whose
producer never ran (a failure elsewhere stopped the build) is aborted.

### The analysis snapshot

TargetConfigured/NamedSetOfFiles need analysis data a snapshot hit does not
recompute, so planning always records the build event plan
(`PlannedRequest::bep`, crate::bep::plan): per requested target its label, kind,
tags, test size/timeout, configuration index and reported output groups (indices
into `requested`), plus the configurations' payloads. It is a few strings per
top-level target (codec FORMAT 4). The BEP, profile and execution log options
are in crate::snapshot::key::EXCLUDED, so turning them on neither misses nor
invalidates a snapshot; `tests/bep.rs` checks a hit streams the miss's events.

## Structural parity

The capture (`/tmp/opencode/perf-research/bep-capture.py`, genuine `--batch`
with private output bases, the stubbed builtin closure of the keep-going
captures) compares per scenario the normalized event multiset: configuration
checksums renamed by first appearance (`none`/`system` kept), named sets by
their file names, children sorted, progress links as one set, payloads reduced
to their structural fields (kinds, tags, sizes; file names, prefixes, digests,
lengths; statuses, exit codes, aborted reasons; ActionExecuted type, label,
argv, stderr presence, spawn failure code), the progress count, `last_message`
and the announcement invariant. Scenarios: build with all three transports,
test with a failing test, an action failure, `-k` with
`--build_event_publish_all_actions`, `--nobuild`, `run` of a test target, the
compact execution log.

Declared differences (normalized away, by design or not modelled):

* Event order within the stream: Bazel's skymeld interleaves analysis and
  execution, the native plan analyzes everything first; the announcement
  structure and the event set are the same.
* Progress events carry no stdout/stderr (the console is not captured).
* Configuration ids are the native configuration checksums, not Bazel's.
* Command line events list the native option set: no launcher-generated
  options (`--rc_source`, `--client_env`, `--default_override`), no effect or
  metadata tags; `optionsDescription` is the rc-expanded option list.
* `buildToolVersion` is `9.3.0` (the compatibility target).
* NamedSetOfFiles: one flat set per (target, output group) instead of the
  nested-set DAG (consumers flatten either the same way).
* BuildMetrics: action summary per mnemonic, target counts, timing
  (wall, CPU from /proc, analysis/execution phases, critical path), cumulative
  counts; the JVM, Skyframe, network and worker metrics are left out.
* BuildToolLogs: native process stats and critical path wording; Bazel always
  lists its default `command.profile.gz`, native only with `--profile`.
* ConvenienceSymlinksIdentified is empty (native creates no `bazel-*` links).
* Not posted: Fetch, TestProgress, TargetSummary, aspect TargetComplete
  events, analysis-failure TargetComplete events under `--keep_going` (their
  TargetConfigured ids are announced and aborted).
