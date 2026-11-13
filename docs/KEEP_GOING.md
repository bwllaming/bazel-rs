# `--keep_going` / `-k`

`build`, `test` and `run` accept `--[no]keep_going` and `-k`
(runtime/KeepGoingOption.java); `query` already did (docs/QUERY.md).
`--nokeep_going` (the default) is unchanged: the first loading, analysis or
execution failure fails the command with the native `bazel-rs: <error>`
report and exit 1.

Under `--keep_going` every failure fails exactly what depends on it, and
everything else is analyzed, built and tested, like Bazel 9.3:

## Loading (target patterns)

TargetPatternPhaseFunction.getTargetsToBuild (:280-335) with keepGoing
(`Engine::select_keep_going`):

* a pattern that does not parse is skipped: `ERROR: Skipping 'P': MESSAGE`
  (all parse failures first, as Bazel parses every pattern before it
  evaluates any);
* a pattern whose evaluation fails (`no such package`, `no such target`,
  a package that fails to load for a single target) is skipped the same way;
* a wildcard (`//...`, `//pkg/...`) keeps every package that loads; each
  package that fails to load is reported once (`ERROR: <loading error>`,
  in loading order) and skipped;
* then `WARNING: Target pattern parsing failed.`; the build goes on with the
  remaining targets, and a build that otherwise succeeds ends with
  `ERROR: command succeeded, but there were errors parsing the target
  pattern` (BuildView.createAnalysisFailureDetail :760-784).

Packages load on the loading threads and phase-1 prefetch, but every error
is committed and reported at its sequential commit slot, so the report does
not depend on `--loading_phase_threads` or worker timing.

**Divergence:** natively a package with errors contributes no targets. Bazel
keeps the targets its BUILD file declared before the error (a package "with
errors"); they then fail analysis and count as top-level targets (one more
`errors encountered while analyzing target` warning, a larger `N of M`).
With a BUILD file that fails before its first target, both agree.

## Analysis

SkyframeErrorProcessor.processAnalysisErrors under keepGoing
(crate::engine::keep_going):

* phase 1 (the configured target graph, depth-first per requested label):
  a failure propagates through the active chain, so the requested label is
  skipped as a whole; its error is a root cause;
* phase 2 (rule implementations in post-order): a target's own failure is a
  root cause and fails only that configured target; every target above it
  (through the dependency edges phase 1 recorded) is skipped before its
  implementation could read the failed dependency's providers;
* a `config_setting` with Starlark `flag_values` (its flags planned by a
  phase-1 flush) fails only when one of its own flag keys failed (its
  implementation or a dependency of the flag; the flag's root cause), and
  a requested target whose own select() keys fail is the target's own
  failure — no `Analysis failed` line (genuine 9.3.1 capture,
  tests/keep_going_flag_flush.rs). Another target's failure in the flush
  fails only what is above it;
* top-level aspects: a failing application skips its requested label;
* attribute aspects: a rule's aspects' own dependencies (their label
  attributes, toolchains) are the rule's dependencies (genuine Bazel:
  AspectFunction computes them for the rule's ConfiguredTargetValue), so
  when one fails in phase 2 the rule is skipped like any target whose
  dependency failed. A requested target failing only through them has no
  `Analysis failed` line (genuine 9.3.1, `tests/keep_going_aspect_dependency.rs`);
  a target above it has. **Divergence:** Bazel also prints one
  `ERROR: Loading failed` per failed aspect application (its
  AspectCreationException); natively the failing dependency's own error is
  the one root cause printed;
* on the parallel analysis evaluator, phase 2 runs in its frozen walks
  through the merge buffer (docs/EVALUATOR.md "eval-keepgoing-speed"):
  rule implementations on workers ahead of the cursor, every skip and
  failure decided at the entry's turn in plan order, as above (a skipped
  entry's job never runs or is dropped; what a failed or skipped entry's
  job published is withdrawn: its outputs are never registered);
* a target whose commit fails (an action conflict, a lowering) registers
  nothing — none of its outputs, declarations or actions, not even those
  before the failing one — so a later target sharing one of its outputs
  produces it itself (genuine 9.3.1 builds the sharer,
  `tests/keep_going_conflict.rs`). **Divergence** (the conflict model):
  Bazel finds conflicts after analysis, so a target above the conflicting
  one is analyzed and then not built without a warning of its own;
  natively it is skipped at its turn (warned, `Analysis failed`);
* each distinct root-cause error is printed once, `ERROR: <message>`,
  before execution, in the engine's sequential order;
* each requested target that did not analyze gets
  `WARNING: errors encountered while analyzing target 'L', it will not be
  built.`, followed by the line `Analysis failed` when a dependency (not
  the target itself) failed (SkyframeErrorProcessor :671-685), in label
  order (Bazel prints them in its error map's hash order);
* a `test` request runs only the tests that analyzed: a test whose analysis
  failed is not in the test summary (genuine capture).

## Execution

`crate::scheduler::execute_graph_keep_going`: a failed action stops only its
consumers (and the test actions reading it); every other action and test
runs. Input failures planning finds (`missing input file`, source directory
traversal errors) fail the actions and targets they name, not the build:
those actions and everything above them are dropped from the plan, the rest
runs. After execution:

* the input failures, then each failed action's error
  (`ERROR: action 'ID': ERROR`, by action id);
* the analysis warnings above;
* `INFO: Build succeeded for only N of M top-level targets` when
  `0 < N < M` (`Analysis succeeded ...` under `--nobuild`; BuildView
  :440-448), M counting every top-level target, N those that analyzed and
  whose top-level artifacts (requested, hidden, validation, aspect outputs)
  all built;
* the built targets' outputs (only those) and the action summary;
* when anything failed: the failure detail (none after an execution failure,
  else the target pattern phase's, else `command succeeded, but not all
  targets were analyzed`), `ERROR: Build did NOT complete successfully`,
  exit code 1 (BuildFailedException; every keep-going failure code maps to
  exit 1).

`test`: a test whose inputs did not build is `FAILED TO BUILD` (not run);
the others run and are summarized. Exit 1 when the build part failed (it
wins over failed tests), else 3 when a test failed, else 0;
`All tests passed but there were other errors during the build.` when every
test that ran passed but the build failed.

`run`: RunCommand.validateTargets under keepGoing (:1052-1122): more than
one target, none, or a target that is not executable is a warning
(`MESSAGE. Will continue anyway`); the targets are built, then the command
fails with that message. A failed build ends with `ERROR: Build failed. Not
running target`.

## Analysis snapshot

`--keep_going` is part of the snapshot key. A keep-going command with any
failure is never saved (the planning taints the recording and the command
fails); a snapshot hit can only replay a clean keep-going plan (its
`KeepGoingPlan` is encoded with the request).

## Evidence

Fixture `tests/keep_going.rs` (a loading error, an analysis error, an action
failure, a test failure and missing inputs in one workspace; build/test/run/
query with and without `-k`); paired genuine Bazel 9.3.0 capture
`tests/fixtures/keep_going/reference_receipt.json` (13/13 EXACT on exit
code, keep-going summary lines, test summary and built targets; without
`-k` on exit code and built targets, native keeps its failure format).
