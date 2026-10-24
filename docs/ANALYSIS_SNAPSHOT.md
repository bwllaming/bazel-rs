# Analysis snapshot: the store, the hit path and verify mode (L2-W5, L2-W6, L2-W5f)

Design: PLAN-L2 §1a–§1e, §5, §6 and PLAN-L2 update 1 (§(a) pinned replay,
§(b) printing protocol v1, decision (1) per-tree validation). Builds on the
recording layer and the local-repository replay
(docs/ANALYSIS_SNAPSHOT_RECORDING.md) and on the planned request
(docs/PLANNED_REQUEST.md). Code: `src/snapshot/` (`key`, `exe_id`,
`format`, `store`, `hit`, `save`, `capture`, `diag`, plus `validate`,
`replay`, `codec`, `record`), wired in `src/main.rs`.

**Off by default.** `BAZEL_RS_SNAPSHOT=on` reads and writes snapshots for
`build`, `test` and `run`; `=verify` is `on` plus a shadow full analysis
on every hit, compared with the hit before it executes (see "Verify mode"
below, L2-W6); `=record` keeps the recording-only report; `=off` or unset
does nothing; any other value is reported on the diagnostics stream and
ignored. `BAZEL_RS_SNAPSHOT_EXPLAIN=1` says, on the
diagnostics stream (`bazel-rs-snapshot:` lines), why a lookup missed (the
first failing input), what a hit validated and replayed, and why a run was
or was not saved. `BAZEL_RS_TIMING=1` adds a `PERF-SNAPSHOT` line (key,
inputs, validation, replay, decode, status and attempt times).

## What a hit does

A `build`, `test` or `run` whose every recorded input is unchanged skips
module resolution, loading and analysis — no Starlark but the local
repository rules' replay — and executes the saved plan:

```text
full analysis: check point -> registry, modules, loading, analysis -> PlannedRequest -> finalize -> execute -> report
hit:           check point -> G -> snapshot validated ∥ replayed ∥ decoded -> window stderr -> finalize -> execute -> report
```

1. **Key G** (`snapshot::key`), computed at the check point (after the rc
   files, option parsing and the other bootstrap values; before the
   registry session opens): blake3 of the postcard encoding of the snapshot
   format and `CARGO_PKG_VERSION`; the **binary identity** (blake3 of
   `/proc/self/exe`, cached in `<cache>/snapshots/exe.id` by the binary's
   stat key and written only for a binary settled two seconds before it was
   hashed); the command; the canonical workspace root and cache dir, the
   working directory, its package and its directory below the root; the
   startup options; the option units after rc and `--config` expansion,
   minus the execution-only list below; the targets (for `run` the target
   only: its arguments are used after execution); `BZLMOD_ALLOW_YANKED_VERSIONS`;
   and the validation policy. Including too much only costs misses: an
   option is left out only when its value was traced to execution,
   reporting or the code before the check point —
   `--jobs`, `--loading_phase_threads`, `--memory-limit`, `--show_result`,
   `--announce_rc`, the test execution options (`--test_output`,
   `--test_summary`, `--[no]cache_test_results`, `--test_arg`,
   `--test_filter`, `--test_timeout`, `--flaky_test_attempts`,
   `--runs_per_test`, `--test_sharding_strategy`, ...), the run options
   (`--script_path`, `--run_env`, `--[no]run`, `--run_in_cwd`,
   `--omit_run_args`), spawn strategies and sandbox paths, the remote cache,
   credential helpers and `--native-action-dump` (`snapshot::key::EXCLUDED`
   lists them with the reason). The test filters, `--build_tests_only` and
   `--run_under` (a label prefix is rejected before planning) stay in G.
   Test and debug hooks (`BAZEL_RS_LOADING_JITTER`,
   `BAZEL_RS_LOADING_NO_DEMAND`, `BAZEL_RS_VERIFY_VIEWS`,
   `BAZEL_RS_VERIFY_ARTIFACTS`, `BAZEL_RS_DIGEST_MEMO`,
   `BAZEL_RS_TIMING_ALLOC`, every `BAZEL_RS_DEBUG_*`) turn snapshots off
   for the command: neither read nor written.
2. **Lookup**: `<cache>/snapshots/v1/<first 16 bytes of G in hex>.snap`;
   the header must carry this format, this binary identity and the full G.
   The INPUTS and EVENTS sections are read by offset and checked against
   their checksums.
3. **In parallel**, with file descriptor 2 captured and discarded (a
   replayed definition's `.bzl` closure may `print()`; that print is
   already part of the recorded window bytes — with replay records, a
   capture that cannot start is a miss; a panic restores stderr first, so
   its message is seen; L2-W5f review F5, F6). The first of the three to
   fail cancels what the others have not started yet — the replay's
   remaining definition loads and executions, the plan's remaining chunks
   (F7) — so a miss's full analysis need not wait for the ~0.1 s replay;
   under EXPLAIN everything runs to completion to name every mismatching
   local repository:
   * **validation** (`snapshot::validate::validate_for_hit`, stopping at the
     first mismatch, which EXPLAIN names): per tree by default — the
     observations outside verified repository trees and those at or below a
     tree's symlinks, the environment digests, and each tree's root,
     manifest and directories (L2-W5f review F3: any file added, deleted or
     renamed in a tree moves a directory's key, a miss); everything under
     `--verify_repo_tampering` / `BAZEL_RS_VERIFY_REPOS=full` (the policy is
     part of G). The environment digests are compared with **this command's
     repository environment** — the client environment with `--repo_env`
     applied in order, the very map the full analysis reads (L2-W5f review
     F1: `--repo_env=X=v --repo_env=X` makes `X` follow the client again;
     such names were once skipped as "fixed", a reproduced false hit). Settled stat
     keys are trusted; racy or changed READ / READDIR observations are
     re-read and compared by content; a changed integrity entry misses; a
     racy one (a tree published, or its manifest refreshed, less than two
     seconds before the recording) verifies its tree against its manifest,
     and so does a manifest rewritten since the recording (L2-W6: the warm
     probe of any later full analysis — of another target, or verify
     mode's shadow — refreshes a settled tree's stat fingerprints into its
     manifest; it still describes the same content-addressed tree, so that
     is no miss). Below 4,000 checks one thread runs them; on more threads
     the reported mismatch is still the first failing check in path order.
   * **replay** of every `local = True` repository rule the recorded run
     executed (`snapshot::replay`), against the pinned repository set,
     with **this command line's** resolver options and repository
     environment: every replay must reproduce its identity inputs and tree,
     print nothing, and its published tree must still exist.
   * **plan**: the PLAN section read, checked and decoded
     (`snapshot::codec`, chunks decoded on threads for large plans).
   * **warm-up**: the digest table (crate::digest_memo) is loaded for the
     execution tail.
4. The workspace status of the plan's status placeholders — or of a plan
   whose planning computed it without any (`--nobuild` with a consumer of
   `ctx.info_file`: the command still ran; the INPUTS section records it,
   L2-W5f review F2) — is computed (quietly): a status command that now
   writes to stderr is a miss (a full analysis would print it during
   planning, inside the window, and never save the run) and the snapshot is
   removed, so later runs run the command once again, in their full
   analysis (this miss ran it twice).
5. Nothing has printed so far: any mismatch, error or panic (caught) is a
   miss, and the command continues as a full analysis exactly as if no
   snapshot existed. EXPLAIN names the first failing check and, when their
   replay differs too, the mismatching local repositories (`miss <key>:
   ENV W5_LOCAL: value changed; and local repository +local_rule+localrepo:
   the identity inputs differ`; several are named with a count). On a hit the window's stderr bytes are written, then
   the decoded request is finalized (status, inherited environment) and
   executed by the same code as a planned one (`build_tail`, `test_tail`,
   `run_tail` in src/main.rs).
6. After a hit: keys a content check found settled are written back — the
   snapshot is rewritten in the background with the refreshed INPUTS — and
   the file's modification time is set (LRU). When the command fails with
   an error that names a path in the repository store at all — quoted or
   not, existing or not, in the build part or after it (`test`'s test
   runner staging a runfile, `run`'s preparation; L2-W5f review F4) — the
   hit may have trusted a damaged tree (per tree, a file's own bytes and
   mode are not validated, a local repository's replay does not look into
   its published tree; the full analysis repairs both): the snapshot is
   removed and one line says so (`bazel-rs: removed the analysis snapshot
   of this build: its failure involves <path> in the repository store; the
   next build re-analyzes and repairs it`). A failure that merely mentions a
   store path costs one full analysis next time. **Only** a failure whose
   error names an absolute path in the store heals: damage that fails an
   action or a test without naming that path (an action citing the file by
   its execution-root path, a test that fails on a runfile's content) keeps
   the snapshot, and the same failure repeats on every hit (the deviation
   below, L2-W5g).

**Repository tree files on a hit (an accepted deviation, L2-W5f lead
decision on review F3; corrected by L2-W5g).** Per tree, a hit validates a
repository tree's root, manifest and directories, not its files: a file
added, deleted or renamed anywhere in a tree is a miss (its directory's
lstat key moved; the full analysis repairs the tree,
`a_file_or_directory_deleted_from_a_repository_tree_misses`), but an
in-place edit of a file's bytes or mode keeps every directory key. A local
(`local = True`) repository's replay reproduces its identity inputs and
tree digests in a fresh stage and checks that its published tree exists,
but does not look into the published tree either. So **byte or mode
tampering of a file in a store tree or in a local repository's published
tree that execution uses (an action input, a tool, a test's or `run`'s
runfile) persists under `on`**: every hit executes with the tampered file,
where a run without the snapshot repairs the tree first (its warm probe
sees the mode, size or change time differ, re-executes the rule and
republishes the tree). Reproduced (L2-W5f re-review): `chmod 644` on a
store-tree tool fails its genrule with `Permission denied` (exit 1) on every
`on` run while the snapshot-less run repairs the tree and exits 0; an
in-place edit of a test's store runfile fails the test on every `on` run;
the same for both in a `local = True` repository's published tree. The
execution phase hashes every action input — crate::digest_memo keys the
files of the tree stores by their *change time*, so an edit that keeps the
size and restores the modification time is still re-hashed — so the action
runs again with the edited bytes
(`a_hit_hashes_an_edited_tree_file_by_its_change_time`): a different output
or a failure, never a stale cache hit. Only failures whose error names an
absolute store path heal (step 6: `a_store_input_staged_by_the_test_runner_heals`,
the test runner staging an unreadable runfile from the store); errors that
name the file by its execution-root or runfiles path
(`external/+gen_rule+genrepo/tool.sh: Permission denied`) and tests that
merely fail keep the snapshot until something else replaces it (an edit
that misses, a deleted snapshot, garbage collection). Like Bazel, which
trusts an external repository's files once its marker is up to date, a hit
uses a tampered store file as it is. `BAZEL_RS_SNAPSHOT=verify` avoids it:
the shadow full analysis repairs the tree before the hit executes — for a
store tree that is a mismatch (`first=taint`, the full analysis's plan
executes), for a local repository it is silent (its re-execution
republishes the repaired tree and still matches the replay record, so the
hit is verified and executes with the repaired tree). For store trees
`--verify_repo_tampering` / `BAZEL_RS_VERIFY_REPOS=full` avoids it too
(every INTEGRITY entry validated: the tampered file is a miss, which
repairs the tree); a local repository's published files have no INTEGRITY
entries (the analysis STATs them, which records the kind only), so there
the tampering persists under that policy as well (checked by L2-W5g with
the reviewer's chmod and in-place cases). The directory
entries cost the hit more checks — on ~/code 10,313 instead of 1,009
(oauth2token) and 16,605 instead of 3,035 (B2), validated on 16 threads in
35–62 ms on the loaded shared machine, beside a replay of 200–300 ms there —
and INPUTS grows from 265 to 651 KB (oauth2token) and 389 to 953 KB (B2): the
hit's wall stayed within the noise (0.60–0.70 s vs 0.60–0.78 s; B2
0.34–0.48 s vs 0.33–0.39 s).

## Verify mode (L2-W6)

`BAZEL_RS_SNAPSHOT=verify` is `on` (a miss saves exactly the same way),
and every hit also runs the full analysis in its shadow
(`snapshot::verify`, PLAN-L2 §1b layer 3, update 1 row W6) before the hit
executes:

```text
hit (validated ∥ replayed ∥ decoded, nothing printed; refreshed keys held back)
  -> recording window + replay capture + stderr captured (capture::Mode::Shadow: kept, never printed)
  -> the full analysis, exactly as a miss runs it -> at the end of planning (plan_request), compare:
       the planned request   its encoding must be the PLAN section's bytes (deterministic encoding); the
                             post-planning warnings and errors are named apart, the outline field by field,
                             the first differing actions as a line diff of their Debug form
       what planning printed the captured bytes must be the EVENTS section's (a line diff)
       the recording         under the snapshot's policy: an observation (path and operation), environment
                             variable or verified tree recorded now but not in the snapshot = incomplete
                             recording; an outcome now different from the snapshot's (contents, listing,
                             type, target — stat keys and racy flags aside) = mismatch
       local repositories    each execution of the full analysis against its replay record: identity
                             inputs and tree digests, identity, published root, definition, bound
                             attributes, output, host facilities (the mismatching repository is named)
       saveable              no taint, no pending lockfile write, a complete capture, planning succeeded
        status                the full analysis computed the workspace status exactly when the snapshot did
  -> match: the hit's own window bytes print and the snapshot's plan executes (what `on` would run);
            the refreshed keys are written back
     mismatch: every difference on the diagnostics stream (`bazel-rs-snapshot: verify ...`), one line
            appended to <cache>/snapshots/verify.log, the snapshot removed (the next run analyzes and
            saves anew), nothing saved; then (the default, L2-W5f) what the full analysis printed while
            planning prints and the FULL ANALYSIS's plan executes — the command's output and exit code
            are those of a run without the snapshot; with BAZEL_RS_SNAPSHOT_VERIFY_STRICT=1 (tests, CI)
            nothing executes and the command exits 70
```

`<cache>/snapshots/verify.log` (binary-safe: every field one token,
`\xNN`-escaped) holds one line per mismatch:
`<UTC time> <unix seconds> key=<short G> command=<command> first=<class> differences=<n> targets=<patterns>`,
the class of the first difference being one of `plan`, `window`, `taint`,
`recording`, `env`, `replay`, `lockfile`, `status`, `analysis-failed`,
`abandoned` or `cannot-verify` (a session that could not start: logged, the
snapshot kept, the full analysis runs unverified; strict: exit 70). For a
soak: `BAZEL_RS_SNAPSHOT=verify` (plus `BAZEL_RS_SNAPSHOT_EXPLAIN=1` for the
`verified` lines), then read the log.

A full analysis that ends before planning completed (an error after the
check point that the hit skipped) is a mismatch too (main reports and logs
it; the full analysis's own error is the command's, or exit 70 when
strict).
Observations or local repositories the snapshot holds but the full
analysis did not make are reported as notes under EXPLAIN (a hit checks
them needlessly; no failure). `BAZEL_RS_SNAPSHOT_EXPLAIN=1` adds a
`verified <key>: ...` line with the full analysis's and the comparison's
time; `BAZEL_RS_TIMING=1` a `PERF-SNAPSHOT verify analysis_ms=...
compare_ms=...` line.

The documented trade-offs of the per-tree validation show as mismatches
in verify mode, as they should: an in-place edit of a tree file's bytes or
mode (the full analysis re-executes the repository rule to repair the tree:
`taint repository-rule-executed`; by default the full analysis's plan then
executes, `verify_mismatches_execute_the_full_analysis_by_default`). A file
deleted from a tree is a miss since L2-W5f.

## Syscall audit of the hit path (L2-W6)

`scripts/snapshot_fs_audit.py --hit` (PLAN-L2 §1b layer 2 for the hit
path) runs each command with `BAZEL_RS_SNAPSHOT=on` until a run saved,
then once more under `strace -f`; that run must be a hit, and with
`BAZEL_RS_SNAPSHOT_DUMP=<file>` the hit writes the INPUTS it validates and
replays (the record mode's dump format). Audited: from the check point
(`/.bazel-rs-snapshot-marker/hit-begin`, before the key) to the hit's
execution (`.../hit-end`, before the window bytes print; after it the
actions execute, which is execution). Every path touched there outside
the cache directory (the snapshot store, `exe.id`, the repository store and
its stages, the digest table), `/proc` and the other process paths must be
covered by the hit's INPUTS — an observation (a READ for a file opened for
reading), a listed directory, a CANON record's resolution chain, a path
inside a verified tree (not below its links) or a pinned root (the
replay's definition loads) — except inside the executions of the replayed
impure local rules and in the processes started there (a replayed
`execute`, the workspace status). Clean (L2-W6): the default fixture
(`build --nobuild //...`, `build //...`: 173 touches each), the snapshot
fixture with a replayed local rule (118 touches, 1 replay), and ~/code
oauth2token/... build (3,767 touches, 45 replayed executions, 1 process)
and B2 `--nobuild` (5,623 touches).

## What is saved, and when

On a miss (`snapshot::save`), the check point opens the recording window
(crate::observed_fs), installs the replay capture and redirects file
descriptor 2 into a **tee capture** (`snapshot::capture`: `dup2` into a
pipe whose reader thread forwards every byte to the original stderr as it
arrives and keeps it). When planning returns, the capture stops: stderr is
restored first, then the reader drains the pipe (bounded); a process that
still holds the pipe makes the capture incomplete. A panic during the
window restores stderr before its message prints; the memory watchdog
does the same before it exits.

The run is saved only when nothing taints it (crate::observed_fs::Taint:
network, an executed non-local repository rule, a printing local
repository rule — v1 of the printing protocol —, an evaluated extension, a
(re)published tree, a symlink into a live repository root, build-info
callbacks or `map_each` of planned actions, Unstable, and the new
`workspace-status-stderr`: a status command that wrote to stderr would
print twice on a hit), no lockfile or hidden lockfile write is pending, the
request has no analysis error, the capture is complete and below 16 MiB,
and the snapshot stays below 512 MiB. The plan is encoded on the main
thread before execution; the recording is taken, selected by the policy
and encoded on the writer thread, which writes a temporary file while the
actions run. Only when the command (for `test` and `run`: its build part)
succeeded is the file renamed into place. Failures are never saved.

**Diagnostics bypass the capture** (`snapshot::diag`): the PERF marks,
`PERF-STAGE`, `PERF-PLAN`, the memory watchdog's lines and the
`bazel-rs-snapshot:` lines write to the original stderr while a capture
holds file descriptor 2; they are never part of the replayed bytes.

## The file

`<cache>/snapshots/v1/<hex(G)[..32]>.snap`, binary (postcard and
fixed-width fields; no JSON):

| Offset | Content |
|---|---|
| 0 | header, 352 bytes, little-endian: magic `BZRSSNP1`, format (u32), flags (u32), binary identity (32), G (32), created (i64 ns), section count (u32), reserved (u32), six section slots of kind (u32), reserved (u32), offset (u64), length (u64), blake3-128 of the section; blake3-128 of the header |
| INPUTS | window start; paths sorted and front-coded (u16 shared prefix, suffix bytes); observations by path index; environment digests (of the repository environment's values); repository trees with the integrity entries the policy validates (per tree: root, manifest, directories); replay records (definition, bound attributes, names, identity digests, recorded output); the pinned tables; whether planning computed the workspace status (format 2, L2-W5f) |
| EVENTS | the window's stderr bytes |
| PLAN | the planned request (docs/PLANNED_REQUEST.md) |
| STATS | counts, sizes and times (diagnostics; a hit does not read it) |

Rules: a different format or binary identity reads as absent; a damaged
header or section (a torn write: no fsync) is a miss; writers use a
temporary file in the same directory and a rename, so several writers of
one key all write valid files and the last rename wins; readers never lock.
Garbage collection after each save keeps at most 16 snapshots and 1 GiB per
cache, least recently used first, and removes temporary files older than an
hour. `exe.id` is a fixed 104-byte record (magic `BZRSEXID`, the stat key,
the identity, a checksum).

## Measured on ~/code (L2-W5, warm, update-mode recipe, release binary)

`build //src/beyondwork.ai/tools/oauth2token/...` (691 planned actions, all
cache hits), without the action dump:

| | full analysis (base 95bda96) | snapshot hit |
|---|---|---|
| wall | 7.4 s | 0.31–0.33 s |
| peak RSS | 1.34 GB | 68–71 MB |

Hit breakdown: G 0.06 ms; INPUTS 1.0 ms; validation 3.0 ms (1,009 checks,
one thread); replay of the 45 local repository rules 123–140 ms (the
critical path: `local_jdk`'s definition load and `java`); plan decode
21–25 ms (10.76 MB, in parallel); status 0.1 ms; execution tail 0.18 s.
B2 `build --nobuild` (3,615 actions): 0.12–0.21 s and 153–157 MB against
11.9 s and 2.0 GB; validation 7–18 ms (3,035 checks), replay 103–128 ms,
decode 47–56 ms (44.7 MB).

The save costs a miss 20 ms (oauth2token) to 50 ms (B2) on the main thread
(the checks and the plan encoding) plus the writer thread's 45–80 ms
(the recording's selection and encoding) and the file write, which
overlap execution. A/B against the base binary (exit code, stdout, stderr
without diagnostics, `BAZEL_RS_DUMP_ACTIONS`): byte-identical for
oauth2token:all `--nobuild`, oauth2token/... build and B2 `--nobuild`, the
compared run being the hit.

## Tests

`tests/snapshot.rs` (PLAN-L2 §7 fixtures 1–18 and the update-1 W5
fixtures): every step runs with the snapshot on and off on twin copies of
one workspace renamed into one path, byte-identical outside the
diagnostics — no-op hits replaying prints; a source edit hits and re-runs
its action; BUILD and `.bzl` edits miss then hit; glob-matching files miss,
swap files hit; new `.go` files under `//...` hit, new packages miss;
watched variables miss, inherited `--action_env` values re-key; options
select their own snapshot; racy and mtime-restoring edits miss; lockfile and
MODULE.bazel edits miss, lockfile writes do not save; a file or a
directory deleted from a repository tree misses (per tree through the
tree's directories, L2-W5f; full through the entry); another binary misses; damaged files miss and are replaced;
concurrent saves; `--nobuild`, `test` and `run --script_path` hit; symlink
retargets and `.bazelignore` edits miss; status command changes hit and
re-run the consumer, a status command writing to stderr does not save;
network (a remote registry's cold run, `--lockfile_mode=off`) and printing
local repositories never save; failed builds never save; `local_jdk`-shaped
repositories miss on a changed `JAVA_HOME`, a `swiftc` on `PATH` or a file
added to the host directory a glob reads through their link; cross-repository
store-tree links hit; an edited tree file is hashed by its change time.
Unit tests cover the key, the exe.id record, the format (round trips,
damage), the store (read back, damage, GC, uncommitted writes, a reader
surviving its collection), the hit's validation and refresh, the
environment digests against the repository environment, the per-tree
directories, the cancelled replay, the verify log and the self-healing
rule.

L2-W5f (the W5 review's follow-ups and test gaps): a `--repo_env` name set
and then inherited again follows the client (F1, the reproduced false hit);
a `--nobuild` consumer of `ctx.info_file` runs the status command on a hit
and a noisy one misses and removes the snapshot (F2); a reused non-local
rule misses on its `environ` variable and its watched workspace file; a
reused extension on its environment variable and its file input; a
`print()` in MODULE.bazel replays on a hit; `local_path_override` module
and `file://` registry edits (BUILD, MODULE.bazel, the `local_path` source)
miss; a new BUILD file in a globbed subdirectory misses; REPO.bazel
`ignore_directories` edits miss; a store-tree runfile the test runner
cannot read heals (F4); hit refreshes racing a concurrent save of the same
key, and garbage collection during a hit, keep the store valid; verify
mode's default executes the full analysis's plan on a mismatch and logs it.

**Mutation fuzz** (`tests/snapshot.rs::mutation_fuzz`, L2-W5f): random
sequences over a catalogue of 33 mutations (sources, same-size and
mtime-restoring edits, glob-matching and swap files, BUILD and `.bzl`
edits, new and removed packages, `.go` files, symlink retargets,
`.bazelignore`, REPO.bazel, a local rule's environment, `--action_env`,
`--repo_env`, `--define`, `.bazelrc`, MODULE.bazel, lockfile edits and
deletions, store-tree deletions and additions, the status command's output
and stderr, damaged and deleted snapshots, concurrent runs, a
`local_path_override` module, a 2 s settle) and 8 commands (`build`,
`--nobuild //...`, `test --test_output=all`, `run --script_path`, the
`--action_env`, status, `--repo_env` and override targets), the snapshot
side in `on` or (30%) strict `verify` mode: every step byte-identical,
built outputs included. 50 steps in the gate
(`BAZEL_RS_SNAPSHOT_FUZZ_STEPS`; `BAZEL_RS_SNAPSHOT_FUZZ_SEED` replays a
sequence; a failure prints the seed and the steps).

Fixture 19 (L2-W6, module `verify_mode` of tests/snapshot.rs): every
fixture again with `BAZEL_RS_SNAPSHOT=verify` (and
`BAZEL_RS_SNAPSHOT_VERIFY_STRICT=1`) on the snapshot side — every hit must
also print its `verified` line; the per-tree trade-offs above expect their
mismatch (exit 70), the next run re-saves. L2-W6 also adds
fixtures for the W4 review: a failing `--workspace_status_command` on a hit
of `build`, `test` and `run` (the failure layout of the decoded request);
post-planning warnings printed by a hit; plans with analysis errors and
`map_each` plans never saved; and `test` writing the lockfile, so its next
run saves (genuine Bazel 9.3 writes MODULE.bazel.lock for `test`, `run` and
a failing `query` too; the native commands that end the process
themselves now write it first).
