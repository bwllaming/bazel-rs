# Incompatible build/test root policy (Bazel 9.3)

The actionless configured results described in TARGET_COMPATIBILITY.md now feed
build/test root selection and reporting. A compatibility result is distinct from
a missing dependency, provider error or any other analysis failure.

## Original-pattern explicitness

After loading the target patterns, the engine records the labels of **original
positive SINGLE_TARGET patterns**. It uses the same parser and repository mapping
as normal selection. It does not infer explicitness from the expanded label set.

| Request | Incompatible root behavior |
|---|---|
| `//pkg:bad`, `:bad`, `//pkg` | Reject: explicitly requested |
| `//pkg:all`, `//pkg/...`, including singleton expansions | Skip |
| `pkg/bad` interpreted as a path | Skip |
| Explicit test suite, expanded to incompatible member tests | Skip members |
| Wildcard plus a positive explicit member | Reject that member |
| Positive explicit member, negative removal, wildcard re-addition | Still explicit |
| Negative explicit pattern without a positive explicit member | Does not establish explicitness |
| `--skip_incompatible_explicit_targets` | Skip explicit roots too |

`--noskip_incompatible_explicit_targets` and `=BOOL` use normal boolean conversion,
rc/config priority and parse order. MODULE flag aliases reparse this root-policy
option before it is consumed: an alias shadowing this name changes its Starlark
setting instead of silently enabling skipping. Its native first-pass converter
still rejects invalid booleans before MODULE alias resolution. Other unsupported
bootstrap/execution-option aliases retain their existing explicit errors.

The label-only Engine API treats supplied labels as explicit. CLI requests carry
provenance through `target_pattern_phase`; callers using that API get the same
original-pattern semantics.

## Errors and skipped builds

Explicit incompatibility without the flag fails with the requested label and
`Dependency chain:`. Generated output headings name the requested output, not its
generator. Alias replacement retains its own node; label_flag forwarding names
its actual in the chain. Each chain step uses its real native configured identity,
shown as six checksum characters, not fabricated genuine checksums.

Without keep-going the incompatibility aborts the request. With keep-going it is
reported under that root's `errors encountered while analyzing target ...`
warning, compatible roots still build, and the command exits 1. Genuine implicit
skips count among the non-failed top-level targets; rejected explicit roots do not.
The skip flag never converts ordinary loading/analysis/execution errors into skips.

Skipped roots have no requested outputs, run plans, normal implementations or
actions. Build results print `Target LABEL was skipped` under `--show_result`'s
budget, before successful/failed results. `--nobuild` applies the same root policy
without execution or these result lines. Normal workspace-status policy is
unchanged; no fake per-skipped-target actions or cache hits are introduced.

## Tests, run and coverage

Skipped tests remain in terminal summary totals but never enter a test session,
test preflight, shard/run validation or execution. `TestSummary.skipped` is separate
from the BlazeTestStatus enum, like genuine's `isSkipped`: it prints **SKIPPED**
and is excluded from the required passing-test count. Ordinary NO_STATUS results
are not turned into successful skipped tests.

| Genuine-captured case | Exit / summary |
|---|---|
| All selected tests incompatible and implicitly skipped | 0; `Executed 0 out of 1 test: 1 was skipped.` |
| Explicit test with skip option | Same successful skipped result |
| Compatible test plus skipped suite member | Compatible test runs; pass + skipped totals |
| Explicit incompatible test under keep-going | Excluded from tests-to-run; compatible tests run; exit 1 |
| Only rejected explicit test under keep-going | Exit 1; no test targets found |
| No test targets selected, successful skipped build | Exit 4 |
| `test --nobuild` with skipped tests | Exit 1; unable to run tests |
| `run` whose requested executable/test was skipped | Exit 2; no targets found to run |
| `coverage` with all tests skipped | Exit 0; skipped summary, no fabricated coverage output |
| Skipped test plus a genuine analysis error | Exit 1; retain already-known skipped results |
| Skipped test plus a failing test action | Exit 3; failed + skipped totals |

When fail-fast analysis stops, only already-established incompatible results are
retained for partial reporting. Unanalyzed roots are not invented as skipped or
successful. Compatible build/test validation and execution stay on their normal
paths. Existing run/coverage feature boundaries remain documented separately.

## BEP and snapshots

Skipped roots retain TargetConfigured metadata. Their TargetComplete ID is
`Aborted { reason: SKIPPED, description: "Target LABEL build was skipped." }`,
including under `--nobuild`. There is no successful TargetComplete, output group,
TestResult or TestSummary event fabricated for a skipped test. Rejected explicit
roots use analysis-failure reporting, not SKIPPED. These payloads feed JSON,
binary and text transports through the existing BuildEvent representation.

Planned requests retain plain skipped labels, root rejection details and per-root
BEP compatibility. Request format **7** replaces format 6 because META's layout
changed; no JSON store is added. Command, original pattern/negative-pattern input,
skip option and MODULE/BUILD/platform observations remain snapshot inputs. A warm
request and strict verification preserve skip summaries and BEP decisions without
recreating providers or executing skipped tests.

No DefaultConfigId, ordinary action key or cquery graph projection is changed.
Cquery keeps explicit and wildcard incompatible targets; the new policy option
does not filter its graph or change its output bytes. Its Starlark formatter
now serves TD's default incompatible-target filter (docs/CQUERY_OUTPUT.md).
The existing untrimmed label-flag/setting identity gap and runtime-unproven
aspect-owned incompatible toolchain case are independent, unchanged boundaries.

## Evidence

Pinned 9.3 sources: TopLevelConstraintSemantics:109–163,239–351;
AnalysisPhaseRunner:546–581; BuildResultPrinter:181–221,269–297;
AggregatingTestListener:250–338; TestSummaryPrinter:100–105;
BuildView/BuildDriverFunction and target_compatible_with_test.sh.

The 24 existing target_compatible_with cases (13 matches and 11 historical
divergences) now assert genuine build policy, preserving the old receipt unchanged.
New fixtures assert original-pattern provenance, cause-chain identity, mixed real
errors, skipped summaries/exits, BEP, alias parsing, binary replay and strict
verification. See tests/fixtures/incompatible_build_skip/REFERENCE.md for the
independent genuine captures and preserved setup failures.

This phase uses only small scratch native/genuine commands. It makes no new
application-query or whole-repository TD claim.
