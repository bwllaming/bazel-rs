# Genuine root-policy evidence

Pinned Bazel 9.3.0 ELF SHA256:
`d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8`.
Source tag9.3.0: `e89755babf3e941bc1e9d8120b1cd48854ab9dfa`.

Receipts and raw stdout/stderr/BEP:
`/home/coder/opencodetmp/target-compatibility/build-skip/captures/`.
Harness/source: `reference.py`, `reference-workspace/` beside those captures.
Every invocation is foreground --batch, ignores rc files, has private absolute
HOME/TMP/output/cache paths, empty remotes and a 110s timeout. Genuine children
deny Internet socket creation and repository downloads; required real release
archives were independently fetched from pinned public GitHub URLs, integrity
checked, and supplied through a private distdir/CAS. No user proxy or application
checkout was accessed.

27 semantic captures, with eight separately retained dependency-setup failures:

| Capture | Observed result |
|---|---|
| 01 | singleton wildcard skips; exit0 |
| 02 | explicit after wildcard rejects; exit1 |
| 03 | path expansion is implicit; exit0 |
| 04 | positive explicit then negative then wildcard remains explicit; exit1 |
| 05 | negative-only explicit mention does not make re-added wildcard explicit; exit0 |
| 06 | explicit skip option permits compatible output and skips incompatible; exit0 |
| 07 | keep-going rejects explicit incompatible with cause chain, builds compatible; exit1 |
| 08–09 | generated output/alias headings and cause chains; exit1 |
| 10 | nobuild wildcard succeeds without build result lines |
| 12–13 | all-skipped/explicit-skipped tests: SKIPPED, no shard validation, exit0 |
| 15 | explicit incompatible test rejects before invalid shard count; exit1 |
| 16 | nobuild skipped tests: unable to run, exit1 |
| 17 | skipped non-test roots: no tests found, exit4 |
| 18 | run skipped test: no targets found to run, exit2 |
| 19 | coverage all-skipped: exit0, no test actions/report |
| 20 | ordinary missing dependency remains an error despite skip option; exit1 |
| 21 | keep-going only rejected explicit test: no tests found, exit1 |
| 22 | explicit test suite does not make members explicit; exit0 |
| 29 | mixed test suite: one passes, one skipped; exit0 |
| 30 | mixed keep-going explicit test: compatible test passes, command exits1 |
| 31 | mixed skip-explicit tests: pass plus skipped, exit0 |
| 32 | MODULE alias shadows skip option: it does not enable skipping; exit1 |
| 33 | invalid native boolean conversion precedes MODULE alias reparse; exit2 |
| 34 | genuine analysis failure retains known skipped test; exit1 |
| 35 | real failing test plus skipped member: failed+skipped summary; exit3 |

Skipped tests' BEP contains TargetConfigured and targetCompleted/Aborted(SKIPPED),
not TestResult/TestSummary. Explicit keep-going rejection has ANALYSIS_FAILURE
on targetConfigured and an empty aborted targetCompleted payload. Raw receipts
remain authoritative; configured checksum differences are normalized only for
diagnostics, never embedded as native expected constants.

Setup failures 11,14,23–28 requested unavailable default test-toolchain source or
patch archives. They were preserved, not counted as successful mixed-test proofs.
29–31 use the original default test rule after verified sources were supplied;
the attempted explicit-exec-group variant in27–28 is not used as evidence.

The native tests use the same local Starlark rule shapes, real platforms archive
and rules_license fixture, with the native explicit-module profile to keep tests
offline. Existing built-in-profile test/toolchain suites remain strict. All 13
matching/11 historical build divergence cases from target_compatible_with's
original receipt are reused, not recaptured or deleted.
