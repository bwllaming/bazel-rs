# Analysis snapshot: the store, the hit path and verify mode (L2-W5, L2-W6, L2-W5f, L2-W5g, L2-W5h, L2-W7)

Design: PLAN-L2 §1a–§1e, §5, §6 and PLAN-L2 update 1 (§(a) pinned replay,
§(b) printing protocol v1, decision (1) per-tree validation). Builds on the
recording layer and the local-repository replay
(docs/ANALYSIS_SNAPSHOT_RECORDING.md) and on the planned request
(docs/PLANNED_REQUEST.md). Code: `src/snapshot/` (`key`, `exe_id`,
`format`, `store`, `hit`, `save`, `capture`, `diag`, `telemetry`, plus
`validate`, `replay`, `codec`, `record`), wired in `src/main.rs`.

**On by default (L2-W7).** With `BAZEL_RS_SNAPSHOT` unset or empty (or
`=on`, `=1`, `=true`) every `build`, `test` and `run` reads and writes
snapshots: an unchanged re-run is a hit, anything else runs the full
analysis and saves it. `query` and `mod` never use it. The other values:

| `BAZEL_RS_SNAPSHOT` | |
|---|---|
| unset, empty, `on`, `1`, `true` | the default: look up, hit or run the full analysis and save it |
| `off`, `0`, `false` | **the escape hatch**: nothing snapshot-related at all — no lookup, no recording window, no stderr capture, no store write (not even `<cache>/snapshots/`); every command runs the full analysis, exactly as before the snapshot existed |
| `verify` | a debugging tool for when a hit is suspected of a stale result: `on`, plus a shadow full analysis on every hit, compared with the hit before it executes (see "Verify mode" below, L2-W6); a difference is logged to `<cache>/snapshots/verify.log` (every detail to `verify-details.log`, the snapshot set aside in `mismatched/`, L2-W5i) and the full analysis's plan executes, so the command's result is the snapshot-less one |
| `record` | the recording-only report (docs/ANALYSIS_SNAPSHOT_RECORDING.md); never reads or writes snapshots |
| anything else | reported on the diagnostics stream (`ignoring BAZEL_RS_SNAPSHOT=...`) and ignored: the default |

When a result looks stale, run the command once with
`BAZEL_RS_SNAPSHOT=verify`: it runs the full analysis beside the hit, names
any difference on stderr, in `verify.log` and in `verify-details.log`, sets
that snapshot aside in `<cache>/snapshots/mismatched/` and gives the full
analysis's result (the next run analyzes and saves anew) — please report
such a mismatch ("Diagnosing a suspected stale result" below). `=off` gives the
full analysis's result without touching the store (a later default run
still uses the snapshot; the off run did repair any tampered repository
tree, see "Repository tree files on a hit"); deleting `<cache>/snapshots/`
drops every snapshot and log. `BAZEL_RS_SNAPSHOT_EXPLAIN=1` says, on the
diagnostics stream (`bazel-rs-snapshot:` lines), why a lookup missed (the
first failing input), what a hit validated and replayed, and why a run was
or was not saved. `BAZEL_RS_TIMING=1` adds a `PERF-SNAPSHOT` line (key,
inputs, validation, replay, local trees, decode, status and attempt
times). Without them the snapshot prints nothing but the one line of a
failed hit that removes its snapshot (step 6 below).

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
   lists them with the reason). The test filters, `--build_tests_only`,
   `--[no]expand_test_suites` (the target pattern phase selects the targets
   and tests before planning, and the planned request carries its expanded
   sets) and `--run_under` (a label prefix is rejected before planning) stay
   in G.
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
     environment: every replay must reproduce its identity inputs and tree
     and print nothing.
   * **local trees** (L2-W5h), beside the replay: each such repository's
     published tree — the one the build reads — must still hold what its
     execution produced (crate::repository_store::verify_local_publication:
     the manifest beside it records the execution's identity and tree
     digest, and the tree verifies against it — content, modes and links,
     trusted by stat outside the racy window, every file read under
     `--verify_repo_tampering`), on up to 4 threads; a difference is a miss
     naming the first differing entry (`local repository <name>: its
     published tree differs from its manifest: <entry> <path>`, events.log
     reason `local-tree`), and the full analysis repairs the tree.
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
   snapshot is rewritten in the background with the refreshed INPUTS, and
   renamed over the file the hit read only while its path still names
   that file (L2-W5h: its device and inode, checked right before the
   rename; L2-W5i: the hit keeps that file open until after the rename, so
   a newer file cannot reuse its inode number and pass the check) — and the
   file's modification time is set (LRU). A snapshot healed, set aside by a
   verify mismatch, evicted or saved anew before the check stays so. The
   check and the rename are two steps, though, not one atomic step: a
   removal or a save in the instant between them is overwritten by the
   refresh. The precise guarantee: at worst one lost newer snapshot (that
   command's next run misses and saves again) or one snapshot a heal or a
   verify mismatch had just removed coming back once (its next hit fails
   and heals again, or under verify mismatches again) — never a false hit:
   the refreshed file holds the recorded inputs of the file the hit read
   (with the stat keys a content check settled) and is validated on every
   lookup like any other, so it can only hit where that file could have. When the command fails — in
   the build part, in the tests (`test` before it exits 3) or after the
   build part (`test`'s test runner staging a runfile, `run`'s
   preparation) — the hit may have trusted a damaged tree (per tree, a
   store tree file's own bytes and mode are not validated, and execution
   itself may have changed a local repository's tree checked before it;
   the full analysis repairs both), so the snapshot is removed, and one
   line says so, when
   * the tree containing a repository-store path the error names —
     quoted or not, existing or not, as given or canonical (L2-W5f review
     F4) — no longer verifies; L2-W5h: that tree is checked first instead
     of removing the snapshot outright, so a failure that merely mentions
     an intact store path keeps it (`bazel-rs: removed the analysis
     snapshot of this build: its failure involves <path> in the repository
     store, whose tree <root> no longer matches what the analysis recorded
     (<entry>); the next build re-analyzes and repairs it`); or
   * a tree of the snapshot no longer verifies (L2-W5g, verification-based
     healing): each verified store tree once — an owner's tree as its
     store's warm check does before reusing it (content, modes and links,
     trusted by stat outside the racy window, every file read under
     `--verify_repo_tampering`), a tree only a link crossed into as the
     crossing did (content only, L2-W5h) — and each local repository's
     published tree against its manifest, which must record the replayed
     execution's identity and tree digest (`bazel-rs: removed the analysis
     snapshot of this build: it failed, and the repository tree <root> no
     longer matches what the analysis recorded (<entry>); the next build
     re-analyzes and repairs it`, the entry being the first that differs,
     L2-W5h: `extra`, `missing`, `kind`, `mode`, `content` or `link` with
     its path below the root — or `manifest`, `unreadable`, `other`).
     Under the full policy the store trees are left out (L2-W5h: the
     lookup validated every file's INTEGRITY entry already).

   The failure happens once: the next run's full analysis repairs the tree
   (the hit does not: re-executing a repository rule needs the analysis).
   A failure with intact trees keeps the snapshot — the check walks the
   trees (`PERF-SNAPSHOT heal trees=<n> healed=<bool> ms=<t>` under
   `BAZEL_RS_TIMING=1`). Each removal appends a `kind=healed` line to the
   event log (below), whose reason is `<rule>:<entry>` (`store-path` or
   `tree`) and whose path is the differing entry's; each failure that kept
   the snapshot a `kind=heal-kept` line (L2-W5i: the trees it checked and
   what that cost — a failure the snapshot had no part in, such as a
   compile error or a failing test).

**Repository tree files on a hit (an accepted deviation, L2-W5f lead
decision on review F3; corrected and narrowed by L2-W5g and L2-W5h).**
Per tree, a hit validates a store tree's root, manifest and directories,
not its files: a file added, deleted or renamed anywhere in a tree is a
miss (its directory's lstat key moved; the full analysis repairs the tree,
`a_file_or_directory_deleted_from_a_repository_tree_misses`), but an
in-place edit of a file's bytes or mode keeps every directory key. So **a
hit does not see byte or mode tampering of a file in a store tree that
execution uses** (an action input, a tool, a test's or `run`'s runfile):
it executes with the tampered file, where a run without the snapshot
repairs the tree first (its warm probe sees the mode, size or change time
differ, re-executes the rule and republishes the tree). A local (`local =
True`) repository's published tree no longer belongs here (L2-W5h): every
hit checks it against the replayed execution (step 3, *local trees*), so
its tampering is a miss and the full analysis repairs it
(`a_tampered_local_repository_misses`, the reviewer's chmod and in-place
cases in both modes). Such a tree's manifest carries no stat fingerprints
(its files are younger than the racy window when it is published, and a
local tree is never reused), so the check hashes its files: on ~/code 45
trees, 151 entries, 68 KB, 3–12 ms on up to 4 threads beside a replay of
120–150 ms — the attempt did not get slower (oauth2token build 144.5 →
144.1 ms, B2 `--nobuild` 141.4 → 141.5 ms, means of 6 hits each). No
always-miss: after executing all 689 oauth2token actions sandboxed and
again unsandboxed (`--spawn_strategy=standalone`, action cache emptied)
and its test unsandboxed with `--nocache_test_results`, every later lookup
hit. The L2-W5f re-review reproduced the store-tree consequence before
L2-W5g: `chmod 644` on a store-tree tool failed its genrule with
`Permission denied` (exit 1) on every `on` run while the snapshot-less run
repaired the tree and exited 0; an in-place edit of a test's store
runfile failed the test on every `on` run (the same in a `local = True`
repository's published tree, which L2-W5h turned into a miss) — only
failures whose error named an absolute store path healed. Since L2-W5g a
failed hit verifies the snapshot's trees (step 6): such a failure happens
once, the snapshot goes, and the next run's full analysis repairs the tree
and succeeds (`a_failed_hit_on_a_tampered_store_tree_heals`). **What
persists under `on` is store-tree tampering that does not fail the
command**: the execution
phase hashes every action input — crate::digest_memo keys the files of the
tree stores by their *change time*, so an edit that keeps the size and
restores the modification time is still re-hashed — so the action runs
again with the edited bytes and may produce a different output
(`a_hit_hashes_an_edited_tree_file_by_its_change_time`; never a stale
cache hit); a test that still passes runs on the edited runfile; and the
program `run` starts replaces bazel-rs's process, so its failure is never
seen. `BAZEL_RS_SNAPSHOT=verify` avoids it: the shadow full analysis
repairs the tree before the hit executes — a mismatch (`first=taint`, the
full analysis's plan executes). `--verify_repo_tampering` /
`BAZEL_RS_VERIFY_REPOS=full` avoids it too (every INTEGRITY entry
validated: the tampered file is a miss, which repairs the tree). The directory
entries cost the hit more checks — on ~/code 10,313 instead of 1,009
(oauth2token) and 16,605 instead of 3,035 (B2), validated on 16 threads in
35–62 ms on the loaded shared machine, beside a replay of 200–300 ms there —
and INPUTS grows from 265 to 651 KB (oauth2token) and 389 to 953 KB (B2): the
hit's wall stayed within the noise (0.60–0.70 s vs 0.60–0.78 s; B2
0.34–0.48 s vs 0.33–0.39 s).

*How genuine Bazel 9.3 compares* (L2-W5h; /tmp/opencode/bazel-9.3-reference).
A **live server** re-checks, before every command, the files of external
repositories that are in its Skyframe graph:
`--experimental_check_external_repository_files` (default true,
RepositoryOptions.java ~322) makes
SkyframeExecutor.handleDiffsWithMissingDiffInformation
(SkyframeExecutor.java ~3938–4033) check the `EXTERNAL_REPO` file states,
and a file whose type, size, digest (where the file system supplies one),
inode or modification time changed re-fetches its repository with a
warning (FileStateValue.compareContents ~249–285,
DirtinessCheckerUtils.java ~154–166, ExternalFilesHelper.java ~392–413:
`Repository '...' will be fetched again since the file '...' has been
modified externally`). A change of the change time alone — a `chmod`, or
a same-size edit that restores the modification time on the same inode —
is `SAME_EXCEPT_CHANGE_TIME`: the node is re-evaluated, the repository is
not re-fetched, and the tampering persists. A **fresh server** —
`--batch`, which is what every bazel-rs process corresponds to (no
daemon) — reuses a non-local repository whose marker file is up to date
(RepositoryFetchFunction.java ~252–256), and the marker records only what
the rule read (DigestWriter.java ~104–130: its recorded inputs), never the
repository's own files, so any edit of them persists; `local = True`
repositories are always re-fetched (RepositoryFetchFunction.java
~502–509). With the snapshot **off**, bazel-rs is stricter than both: its
warm probe verifies every reused tree against its manifest — mode, size
and the stat fingerprint, change time included, hashing a file on any
difference; every file under `--verify_repo_tampering` — and re-executes
the rule on a difference, and it re-executes every local rule. A hit
under `on` is as strict as a fresh Bazel server for a store tree's files
(stricter for additions, deletions and renames, which are misses) and as
strict as the snapshot-less run for a local repository's tree (L2-W5h).

*Recovering from a tampered store tree*: one run with
`BAZEL_RS_SNAPSHOT=off` repairs it — its warm probe sees the
file differ, re-executes the rule and republishes the tree, a new
directory — and the snapshot then misses on the renamed or changed
directories (`integrity` with the tree's path in events.log) and saves
anew; `BAZEL_RS_SNAPSHOT=verify` repairs it too (the shadow analysis), and
removing `<cache>/snapshots/v1/` drops every snapshot. A local
repository's tree needs nothing (every hit checks it).

*Exits that never heal*: the healing check runs only when bazel-rs itself
sees the command fail after a hit, so these leave a damaged store tree in
place until a full analysis runs: Ctrl-C, SIGTERM and SIGKILL (the process
ends before any check); the memory watchdog's exit (code 33); an uncaught
panic (the process ends with its message, code 101); `run` whose program fails
after the exec (bazel-rs's process is the program then); `run
--script_path` (the script runs later, outside bazel-rs); and tests that
still pass on the damaged runfile.

## Verify mode (L2-W6)

`BAZEL_RS_SNAPSHOT=verify` is `on` (a miss saves exactly the same way),
and every hit also runs the full analysis in its shadow
(`snapshot::verify`, PLAN-L2 §1b layer 3, update 1 row W6) before the hit
executes. With the snapshot on by default (L2-W7) it is the debugging tool
for a suspected stale result: a verified hit costs a full analysis (plus
the comparison, ~0.1 s on ~/code), so it is meant for a command or a
session, not as a permanent setting:

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
            appended to <cache>/snapshots/verify.log and that line with every difference to
            verify-details.log (L2-W5i), the snapshot set aside in <cache>/snapshots/mismatched/
            (L2-W5i; before, deleted — either way the next run analyzes and saves anew), nothing
            saved; then (the default, L2-W5f) what the full analysis printed while planning prints
            and the FULL ANALYSIS's plan executes — the command's output and exit code are those of
            a run without the snapshot; with BAZEL_RS_SNAPSHOT_VERIFY_STRICT=1 (tests, CI) nothing
            executes and the command exits 70
```

Verify mode's shadow capture (`capture::Mode::Shadow`) keeps what
the full analysis prints without printing it — the hit prints its own
bytes. When those bytes can no longer be compared it forwards them
instead (L2-W5g; before, they were lost): on more than 16 MiB
(`capture::LIMIT`) the kept bytes go to stderr at once and every later byte
follows; a panic or the memory watchdog's exit that ends the capture early
writes the kept bytes before the panic message (or the exit); and a stop
while a process still holds the pipe (a lingering child) writes the kept
bytes, after which the detached reader forwards what still arrives. Each
of these is a mismatch (`window`) or the end of the process, whose output
is the full analysis's; the mismatch path then does not print the bytes a
second time.

`<cache>/snapshots/verify.log` (binary-safe: every field one token,
`\xNN`-escaped, `-` when absent) holds one line per mismatch:
`<UTC time> <unix seconds> key=<short G> command=<command> first=<class> differences=<n> taint=<first taint> path=<first differing path> targets=<patterns>`,
the class of the first difference being one of `plan`, `window`, `taint`,
`recording`, `env`, `replay`, `lockfile`, `status`, `analysis-failed`,
`abandoned` or `cannot-verify` (a session that could not start: logged, the
snapshot kept, the full analysis runs unverified; strict: exit 70). L2-W5g
added `taint` — the name of the full analysis's first taint
(`repository-rule-executed` for a tree the full analysis repaired) — and
`path` — the first path a difference names (an observation recorded now
but not in the snapshot, or with another outcome; a tree verified now but
not in the snapshot; a local repository's published root), in the order
the differences are reported. To check a command or a series of commands:
`BAZEL_RS_SNAPSHOT=verify` (plus `BAZEL_RS_SNAPSHOT_EXPLAIN=1` for the
`verified` lines), then read the log — and the event log below
(`scripts/snapshot_soak_report.py <cache>` summarizes them; "Diagnosing a
suspected stale result" below is the guide). The test suite runs the same
way (L2-W7): `BAZEL_RS_SNAPSHOT=verify BAZEL_RS_SNAPSHOT_VERIFY_STRICT=1
cargo test` verifies every hit of the CLI tests and fails a disagreeing one
with exit 70 (the harnesses that clear the environment forward both
variables, tests/support/verify_env.rs; those that pin their environment or
set the mode themselves — tests/snapshot.rs — do not).

**What a mismatch leaves behind (L2-W5i).** A `first=plan` line alone is
not actionable, and the differences used to reach stderr only while the
snapshot was deleted. Now each mismatch also appends a block to
`<cache>/snapshots/verify-details.log` — its `verify.log` line, then
`snapshot: set aside as <path>` (or `removed`, `kept` for `cannot-verify`,
`gone already`) and every `verify mismatch:`, `verify   <detail>` and
`verify note:` line the diagnostics stream shows (indented by two spaces;
a detail's own newlines become further indented lines, control
characters `\xNN`; at most 4 KiB per line and about 64 KiB per block, the
rest counted) — bounded and rotated like the other logs (1 MiB plus one
`.1`). The mismatching snapshot is **moved** to
`<cache>/snapshots/mismatched/<UTC time>-<key>-<pid>.snap` instead of being
deleted (crate::snapshot::store::set_aside; a move that fails deletes it,
and the block says so). The next lookup misses (`absent`) as before. The
directory keeps the **3 newest** files; it is outside `v1/`, so lookups
never read it and the garbage collection neither counts it against its 16
snapshots / 1 GiB nor removes it — its own bound is those 3 files (at most
3 × 512 MiB, in practice 3 × the size of the largest snapshot, tens of MB
on ~/code).

**The event log (L2-W5g `miss.log`, renamed and completed by L2-W5h).**
`<cache>/snapshots/events.log` (the same encoding) holds one line per
snapshot event of every command with the snapshot on — in `on` and
`verify` mode alike, plus `record`'s report — so the logs have a
denominator: every lookup's hit or miss with the attempt's wall time, every
save, every full analysis that was not saved, verify mode's outcome of
every hit (`verified`, or `mismatch` with verify.log and, L2-W5i,
verify-details.log holding the details), every failed hit that removed its
snapshot (`healed`, step 6) or kept it (`heal-kept`, L2-W5i) and every
snapshot garbage collection evicted (`evicted`: an `absent` miss of an
evicted key is no snapshot that was never saved). A reason that repeats
shows without EXPLAIN — a tool that writes into a store tree's directory,
say, makes every lookup miss on that directory's INTEGRITY entry. Every
line has the same fields:

```text
<UTC time> <unix seconds> kind=<kind> mode=<on|verify|record> key=<short G> command=<command> reason=<class> path=<path> ms=<wall ms> detail=<text> targets=<patterns>
```

| kind | logged | reason | ms |
|---|---|---|---|
| `hit` | a lookup hit (in verify mode before its shadow analysis); `detail` splits the time (key, inputs, validate, replay, local, decode, status) | - | the attempt, key included |
| `miss` | a lookup missed; `detail` is the reason EXPLAIN prints | its class (below) | the attempt |
| `saved` | a full analysis was saved; `detail`: bytes, actions, replays, binary (the first 8 bytes of the binary identity, L2-W5i: the soak report sees an upgrade) | - | check point to rename |
| `not-saved` | a full analysis was not saved | its class (below) | - |
| `verified` | verify mode: the shadow analysis agreed | - | analysis and comparison |
| `mismatch` | verify mode: it disagreed (`path`: the first differing path; verify.log and verify-details.log hold the rest) | the first difference's class | - |
| `healed` | a failed hit removed its snapshot (`path`: the first differing entry) | `<store-path\|tree>:<entry>` | the trees' check |
| `heal-kept` | a failed hit kept its snapshot: every tree verified (L2-W5i; `path`: the store path its error named, if any; `detail`: `trees:<n>`) | - | the trees' check |
| `evicted` | garbage collection removed the snapshot of `key` (`command=-` and no targets: those of the evicted snapshot are not known there); `detail` names the save that collected it | `count` or `bytes` | - |
| `recorded` | `BAZEL_RS_SNAPSHOT=record` reported (no key) | eligible: `yes`, `no`, `unless-pruned` | - |

`scripts/snapshot_soak_report.py <cache dir>` (`--json` for a JSON object;
`--workspace <dir>` names the cache's workspace when the cache is not its
`bazel-rs-out`; `--archive <dir>`, L2-W5i, first appends the cache's log
lines the directory does not hold yet — counted, so an event logged twice
is kept twice; a line being written is left for the next run — and
reports over the whole archive) prints the counts per mode and kind, misses and unsaved
runs by reason, the number of keys (those seen only in an eviction counted
apart), the hit rate on unchanged re-runs over all keys and per key (hits
over the lookups once a snapshot of the key exists — saved, hit, or read by
a lookup that missed on an input — that hit or missed for no input:
`absent` right after a save with no removal since, `unreadable`, `capture`,
`panic`, `other`; a miss naming a changed input, an `absent` miss after an
eviction, heal or verify mismatch, and an `absent` miss while none is saved
are counted apart), the keys never saved (L2-W5i: no save, hit or snapshot
read; with their not-saved reasons — expected when every run was tainted,
`network` say, or failed; `capture`, `too-large`, `incomplete`, `other` or
no not-saved event at all are worth reporting), verified hits by command
and verify-mode hits by outcome (L2-W5i: `verified`, hit then mismatch by
class — no clean hit: counted apart from a key's hits and out of the hit
rates — or not concluded), always-miss patterns, heals by reason, every
verify.log line verbatim with its verify-details.log block, the hit
latency's p50 and p95 per mode and command, the local-tree check of the
hits with local repositories (L2-W5i, from the hit's `local:` and
`replay:` times: p50, p95, max, and on how many hits the check took longer
than the replay it runs beside — then it is on the attempt's critical
path), the store's size (and what `mismatched/` holds), and the soak exit
check (the thresholds L2-W5i set for switching the default on after a
soak; informational since L2-W7, see "Diagnosing a suspected stale result"
below).

An **always-miss pattern** is at least three consecutive misses of one key
with the same reason and path and no hit between them. L2-W5i (the W5h
review's F2): a save ends the run when the misses name an input you may
have changed — a workspace or host path, or an input without a path (`env`,
`replay`): the save took the change in and the next miss is the next
change, so editing one BUILD file three times (miss, save, three times) is
no pattern — but not when they name a path in the cache or an output tree,
or no input at all (`absent` after every save, `unreadable`, ...): such a
miss coming back after every save is the command's own doing (a tool
writing into a repository tree's directory), the case these patterns
watch for. Each pattern is **SUSPECT** — a path below the cache (as given or
canonical), in an output tree (a `bazel-*` entry of the workspace: Bazel's
convenience links, `bazel-rs-out`; any path through `bazel-out`,
`bazel-bin`, `bazel-testlogs`, `bazel-genfiles` or genuine Bazel's output
user root `_bazel_<user>`), or no path — or **likely your edits** (a
workspace or host path with no save between: a BUILD file edited while the
command kept failing). The report prints each with its place, the saves
between, its first and last time and the last miss's reason.

A miss's class is `absent` (no snapshot for the key), `unreadable`, `env`,
the failing check's operation (`read`, `readdir`, `stat`, `lstat`, `canon`,
`readlink` or `integrity`, with the path it names), `tree` (a racy tree
that no longer matches its manifest), `replay`, `local-tree` (a local
repository's published tree differs from its manifest; the path of its
first differing entry), `status-stderr`, `capture`, `panic` or `other`;
`detail` holds the reason EXPLAIN prints (its first 240 bytes; without
EXPLAIN a replay cancelled by the first failure is not named).
A not-saved run's class is the first taint's name (`repository-rule-executed`,
`network`, `extension-evaluated`, `lockfile-write-pending`, `errors`, ...),
`failed` (the command failed), `capture`, `too-large`, `incomplete` or
`other`. A heal's entry is the first differing entry's class
(crate::repository_store::TreeMismatch: `extra`, `missing`, `kind`,
`mode`, `content`, `link`, `manifest`, `unreadable`, `other`). Each line
(and each verify-details.log block) is one `write` (O_APPEND); every log
rotates at 1 MiB to `<name>.1`, keeping one previous generation (best
effort: two commands rotating at once may drop one).

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

## Diagnosing a suspected stale result (L2-W5h, L2-W5i, L2-W7; formerly "Soak")

The snapshot is on by default (L2-W7). L2-W5i wrote this section as the
guide of a user soak in verify mode with exit thresholds for switching the
default on; the user decided against a soak period (the snapshot is new:
problems are fixed as they appear), so nothing below gates anything. When
a command's result looks stale — a hit seems to have built, tested or run
something a full analysis would not have — or an unchanged re-run keeps
missing, per cache:

1. **Re-run the command with `BAZEL_RS_SNAPSHOT=verify`** (per command, or
   `export BAZEL_RS_SNAPSHOT=verify` for a session; optionally
   `BAZEL_RS_SNAPSHOT_EXPLAIN=1`, which adds `bazel-rs-snapshot: …` lines on
   stderr: why a lookup missed, `verified …`). A hit then also runs the full
   analysis in its shadow and compares the two before anything executes
   ("Verify mode" above). Do **not** set `BAZEL_RS_SNAPSHOT_VERIFY_STRICT`
   for this: by default a mismatch is logged and the full analysis's plan
   executes, so the command's output and exit code are those of a run
   without the snapshot. A verified hit costs a full analysis (measured on
   ~/code: oauth2token build 8.4 s vs 7.1 s without the snapshot, B2
   `--nobuild` 13.1 vs 12.3 s — the shadow analysis records, which turns
   prefetching off); a miss saves like `on`. Unset the variable again
   afterwards: the default gives hits of a few tenths of a second.
2. **What it leaves behind**, in `<cache>/snapshots/` (`<cache>` is
   `--cache-dir`, default `<workspace>/bazel-rs-out`, e.g.
   `~/code/bazel-rs-out/snapshots/`): a mismatch prints a
   `bazel-rs-snapshot: verify … MISMATCH` line on stderr (it names the first
   difference, where the snapshot went and where the details were logged),
   appends one line to `verify.log` and that line with every difference to
   `verify-details.log`, and sets the snapshot aside in `mismatched/` (the 3
   newest are kept: copy one out if more mismatches follow). `events.log`
   holds every snapshot event of every command (each lookup's hit or miss
   with its reason and path, saves, unsaved runs, verified hits, mismatches,
   heals). Each log keeps at most 1 MiB plus one rotated `<name>.1`; the two
   generations of `events.log` hold about 7,500 events (≈ 280 bytes each; a
   command logs one to three, plus evictions).
3. **Summarize** (Python ≥ 3.9; it only reads the cache):
   `python3 <bazel-rs checkout>/scripts/snapshot_soak_report.py <cache>` —
   the hit rate on unchanged re-runs per key, misses and unsaved runs by
   reason, always-miss patterns (SUSPECT or likely your edits), heals, every
   verify.log line with its verify-details.log block, hit latency, the
   store's size. `--archive <dir>` first appends the cache's new log lines to
   that directory (one file per log, never rotated) and reports over the
   whole archive, so a series of commands longer than the logs' window is
   counted completely; `--workspace <dir>` when the cache is not
   `<workspace>/bazel-rs-out` (its `bazel-*` entries are output trees for the
   SUSPECT classification); `--json` prints the report as JSON. One report
   per cache: another workspace or `--cache-dir` has its own logs. The
   report ends with a "soak exit check": the thresholds L2-W5i proposed for
   the default flip (verified hits, days, hit rate ≥ 90 % on unchanged
   re-runs, hit p95 ≤ 1 s, store ≤ 1 GiB, no mismatch, heal or SUSPECT
   pattern). They gate nothing now; read their values as a summary of the
   cache's health.
4. **What is a problem** — please report it with the MISMATCH line, the
   verify-details.log block, the set-aside snapshot from `mismatched/` (or
   the logs, rotated `.1` files included, or the `--archive` directory) and
   what you did just before (an edit, a branch switch, a fetch, something
   under `<cache>/repositories`):
   * any verify mismatch (`kind=mismatch` event, verify.log line) — the
     snapshot planned something else than a full analysis — except
     `first=taint taint=repository-rule-executed` right after you edited
     files under `<cache>/repositories` yourself (the per-tree trade-off
     above); and any `cannot-verify`;
   * a **SUSPECT** always-miss pattern in the report (a command that keeps
     missing on a path in the cache or an output tree, or on no input; the
     reason and path say why, e.g. a tool writing into a repository tree),
     and `kind=miss reason=local-tree` repeating on one path;
   * misses of a command you re-ran unchanged that the report counts
     `unexplained` (`absent` right after a save with no eviction, heal or
     mismatch, `unreadable`, `capture`, `panic`, `other`);
   * a key the report marks "please report" among the keys never saved
     (`capture`, `too-large`, `incomplete`, `other`, no not-saved event);
   * any `kind=healed` you did not cause by touching `<cache>/repositories`;
   * a command whose output or exit code differs from the same command with
     `BAZEL_RS_SNAPSHOT=off`.

   **Not** problems: misses naming what you just changed (`read`,
   `readdir`, `stat`, `env`, …; the report's "likely your edits" patterns),
   `absent` after an `evicted` (the store keeps 16 snapshots / 1 GiB),
   `not-saved` for `lockfile-write-pending` (the run after a lockfile
   change), `repository-rule-executed` (after a fetch), `network` or
   another taint, `failed` (failed commands are never saved), keys never
   saved for those reasons, and `heal-kept` (a failed hit whose trees all
   verified: the failure was the build's own).
5. **On a mismatch** nothing is needed for the command itself: its output
   and exit code are those of the full analysis, and the next run analyzes
   and saves anew.
6. **Escape hatch**: `BAZEL_RS_SNAPSHOT=off` — exactly the behaviour
   without the snapshot (nothing snapshot-related at all); deleting
   `<cache>/snapshots/` drops every snapshot and log; one run with the
   snapshot off (or in verify mode) repairs a tampered repository tree.

## Syscall audit of the hit path (L2-W6, L2-W5g)

`scripts/snapshot_fs_audit.py --hit` (PLAN-L2 §1b layer 2 for the hit
path) runs each command with `BAZEL_RS_SNAPSHOT=on` until a run saved,
then once more under `strace -f`; that run must be a hit, and with
`BAZEL_RS_SNAPSHOT_DUMP=<file>` the hit writes the INPUTS it validates and
replays (the record mode's dump format). Audited: from the check point
(`/.bazel-rs-snapshot-marker/hit-begin`, before the key) to the hit's
execution (`.../hit-end`, before the window bytes print; after it the
actions execute, which is execution). Every path touched there outside
the cache directory, `/proc` and the other process paths must be covered
by the hit's INPUTS — an observation (a READ for a file opened for
reading), a listed directory, a CANON record's resolution chain, a path
inside a verified tree (not below its links) or a pinned root (the
replay's definition loads) — except inside the executions of the replayed
impure local rules and in the processes started there (a replayed
`execute`, the workspace status). Clean (L2-W6): the default fixture
(`build --nobuild //...`, `build //...`: 173 touches each), the snapshot
fixture with a replayed local rule (118 touches, 1 replay), and ~/code
oauth2token/... build (3,767 touches, 45 replayed executions, 1 process)
and B2 `--nobuild` (5,623 touches).

Inside the cache directory (L2-W5g: it used to be ignored wholesale) each
touch must fall into a class that is safe by construction, or be covered
by the INPUTS like any workspace path, or it is reported (`CACHE <path>`):

| class | paths | why it is safe |
|---|---|---|
| `cache-root`, `store-directories` | the cache dir; `repositories` and its areas (`trees`, `direct`, `git`, `builtins`, with their `trees` and `manifests`), `direct/locks`, `direct/stages` | created, stat'ed or readlink'ed on realpath chains; no content is read — L2-W5h: any other call there (an open, a listing) is reported (`CACHE <path> [<call> in <class>]`) |
| `snapshot-store` | `snapshots/` | the hit's own file (header, sections and their checksums verified; its mtime set for LRU), `exe.id` (keyed by the binary's stat), the store directory, the refresh's temporary file, the event log the hit appends to (written, never read) |
| `digest-table` | `digests/` | crate::digest_memo: a record holds a file's stat key with its digest and is used only while the file's current stat matches it |
| `replay-stage` | `repositories/direct/stages/<slug>`, `repositories/direct/locks/stage-<slug>` | a replayed local rule's private stage, created fresh under its lock, compared (identity inputs and tree digests) and removed: nothing in it is trusted |
| `verified-tree` | published store trees and manifests the INPUTS cover (REPO roots and manifests, INTEGRITY entries, paths inside a verified tree not below its links) | the per-tree validation's subject (a racy tree is verified against its manifest) |
| `local-tree` | a replayed local repository's published tree (REPLAY `root:` and below) and its manifest (`repositories/direct/manifests/<identity>.json`) | the subject of the hit's local tree check (L2-W5h; `local-root` before: the root's existence only): read and compared with the replayed execution, a difference is a miss |
| `covered-input` | any other cache path the INPUTS cover | an input like a workspace file: the hidden lockfile (`hidden-lockfiles/<hash>/MODULE.bazel.lock`), a READ observation a racy hit re-reads |

The `--hit` report prints the counts per class (`inside the cache: N
touches (...)`). Clean (L2-W5g, nothing unclassified or uncovered): the
default fixture (11–12 cache touches: `snapshot-store` 6, `cache-root` 2,
`digest-table` 1–2, `store-directories` 1, the hidden lockfile as a
`covered-input`), the snapshot fixture with a replayed local rule and a
store tree (`build //:all_inputs`: 161 — `replay-stage` 61,
`verified-tree` 34, `store-directories` 34, `cache-root` 15, `local-root`
5, ...) and ~/code oauth2token/... build (3,762 touches outside the cache,
45 replayed executions; 17,940 inside: `verified-tree` 12,169,
`replay-stage` 2,265, `store-directories` 2,236, `cache-root` 921,
`local-root` 262, `covered-input` 79, `snapshot-store` 6, `digest-table`
2). The first ~/code run found three store directories on realpath chains
(`repositories/trees`, `repositories/builtins`, `.../builtins/trees`) the
class list had missed: added to `store-directories`. L2-W5h (the local
tree check, the event log): clean again — the default fixture 15–16 cache
touches (`snapshot-store` 10 with the event log's append), the fixture
with a local rule 192 (`local-tree` 45, `replay-stage` 61, `verified-tree`
21, ...; 96 outside), ~/code oauth2token/... build 18,949 (`local-tree`
1,653, `verified-tree` 11,783, `replay-stage` 2,265, `store-directories`
2,236, `cache-root` 921, `covered-input` 79, `snapshot-store` 10,
`digest-table` 2; 3,764 outside, 45 replayed executions, 1 process); and
with the metadata-only assertion on `cache-root` and `store-directories`
(the last L2-W5h commit): the fixtures alike, ~/code 18,943 (`cache-root`
920 and `store-directories` 2,235 touches, every one a stat, readlink or
mkdir).

## What is saved, and when

On a miss (`snapshot::save`), the check point opens the recording window
(crate::observed_fs), installs the replay capture and redirects file
descriptor 2 into a **tee capture** (`snapshot::capture`: `dup2` into a
pipe whose reader thread keeps every byte and forwards it to the original
stderr whole lines at a time — L2-W5h: a partial line at its newline, the
end of the pipe or, L2-W5i, once it holds 64 KiB). When planning returns,
the capture stops: stderr is restored first, then the reader drains the
pipe (bounded); a process that still holds the pipe makes the capture
incomplete — the stop then forwards the partial line the reader holds
back at once, and the detached reader forwards what still arrives as it
arrives (L2-W5i; before, a partial line waited for its newline or the end
of the pipe, and was lost when the process ended first). A panic during the
window restores stderr before its message prints; the memory watchdog
does the same before it exits.

The run is saved only when nothing taints it (crate::observed_fs::Taint:
network, an executed non-local repository rule, a printing local
repository rule — v1 of the printing protocol —, an evaluated extension, a
(re)published tree, a symlink into a live repository root, build-info
callbacks or `map_each` of planned actions, Unstable, and the new
`workspace-status-stderr`: a status command that wrote to stderr would
print twice on a hit), no lockfile or hidden lockfile write is pending, the
request has no analysis error, the target pattern phase reported no
test-suite expansion error (integration-21: such a `build`, `test` or `run`
fails after its build with `command succeeded, but there were ... errors`,
which a hit — it skips the phase and replays what the phase printed —
could not reproduce; the phase taints the run with `errors`), the capture
is complete and below 16 MiB, and the snapshot stays below 512 MiB. The plan is encoded on the main
thread before execution; the recording is taken, selected by the policy
and encoded on the writer thread, which writes a temporary file while the
actions run. Only when the command (for `test` and `run`: its build part)
succeeded is the file renamed into place. Failures are never saved.

**A miss runs without the phase-1 prefetch** (integration-21): the
recording window is open while it plans (`on` saving, `verify`'s shadow
analysis), and W6's prefetch never starts while it records
(`observed_fs::recording`, docs/PARALLEL_ANALYSIS.md "Phase-1 prefetch"):
the recording is process-wide, so it would also take the observations of
speculative units that are never committed. So a miss pays the
sequential loading of phase 1 (~/code, warm:
oauth2token:all `--nobuild` 6.5 s against 4.0–4.2 s with the snapshot off;
B2 `--nobuild` 11.1 s against 7.9–8.7 s); hits are unaffected. Since the
snapshot is on by default (L2-W7) every miss pays it. Follow-up: per-unit
observation buffers merged only for committed units.

**Diagnostics bypass the capture** (`snapshot::diag`; L2-W5h: never
inside a forwarded line, which goes out whole under the same order lock): the PERF marks,
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
hour; the snapshots a verify mismatch set aside in `snapshots/mismatched/`
are neither counted nor collected (their own bound: the 3 newest, L2-W5i). `exe.id` is a fixed 104-byte record (magic `BZRSEXID`, the stat key,
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

**With the default on (L2-W7**, integration-21 stage 2 + L2-W7, release
binary, private caches and lockfile, update-mode recipe, load 17–36 on the
shared machine; wall and peak RSS by `wait4`, rounds interleaved):

| | installed i20 (no snapshot) | snapshot off | miss (records, saves) | hit | snapshot file |
|---|---|---|---|---|---|
| oauth2token:all `--nobuild` | 6.6–9.0 s, 1.31 GB | 4.4–5.4 s, 1.16 GB | 7.5–7.8 s, 1.34 GB | 0.23–0.29 s, 66 MB | 10.7 MiB |
| oauth2token/... build (689 cache hits) | 7.0–7.5 s, 1.31 GB | 4.3–5.7 s, 1.16 GB | 7.1–7.2 s, 1.32 GB | 0.31–0.34 s, 73–76 MB | 10.7 MiB |
| B2 `--nobuild` | 11.2–13.7 s, 1.96 GB | 8.0–9.3 s, 1.81 GB | 11.3–14.0 s, 2.07 GB | 0.15–0.16 s, 155–160 MB | 43.0 MiB |
| tools/... `--nobuild` (`--memory-limit=20000`) | exit 1 (`overlapping output paths`) | 13.4–14.1 s, 2.75 GB | 17.2–17.8 s, 3.11 GB | 0.16–0.19 s, 257–261 MB | 80.7 MiB |

On a fresh cache the first command fetches and executes repository rules
(not saved: `network`, `lockfile-write-pending`), the second saves, the third
hits; the first hit after a save verifies the racy trees (0.37–0.71 s). A
miss costs 2.4–4.6 s and 160–360 MB over the snapshot-off run (the recording
without the phase-1 prefetch, above, and the save). A/B against the
installed i20 (exit code, stdout, stderr without diagnostics, the action
dump), every run compared — the first (miss and save) and the later ones
(hits): identical for oauth2token:all `--nobuild` (3 runs), oauth2token/...
build (3), B2 `--nobuild` (3) and `test //src/beyondwork.ai/tools/oauth2token:oauth2token_test` (2).
Two processes running the same command on one cache at once (oauth2token/...
build, B2 `--nobuild`) both missed and saved the same bytes, printed what
the snapshot-off run prints, left one snapshot and no temporary file; the
next run hit and a verify run agreed.

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

L2-W5g: the event log (L2-W5h; `miss.log` before) names each lookup's
outcome (a miss's class and path), each save and each unsaved run with
the mode and wall time (`the_event_log_names_each_outcome`), the verify
log the first taint and differing path; unit tests cover the reason
classes, every event kind, the evictions, the binary-safe lines and the
rotation, the first differing entry of each store's trees (registry, Git,
builtin; direct and local in the healing test), the soak report
(tests/test_snapshot_soak_report.py), and the shadow capture's
forwarding (an overflow, an early end, an incomplete stop: the capture's
reader and stop driven on a private pipe — a deterministic end-to-end case
would need a full analysis that prints differently from its own snapshot).
Verification-based healing (L2-W5g): the reviewer's cases — a tool made
non-executable (`chmod 644`) and a test's runfile edited in place in a
store tree — fail the hit once, remove the snapshot (one line naming the
first differing entry, a `kind=healed` log line), and the next run
repairs the tree and succeeds like the run without the snapshot
(`a_failed_hit_on_a_tampered_store_tree_heals`; under verify a mismatch);
in a local repository's published tree the same cases are a miss of the
lookup since L2-W5h, in both modes (`a_tampered_local_repository_misses`:
the miss names the entry, events.log `reason=local-tree`, no heal); a
unit test drives the healing check on real publications (intact trees
keep the snapshot).

**Mutation fuzz** (`tests/snapshot.rs::mutation_fuzz`, L2-W5f, extended
by L2-W5g and L2-W5h): random sequences over a catalogue of 51 mutations (sources —
same-size, mtime-restoring, future-stamped and backdated edits —,
glob-matching and swap files, BUILD and `.bzl` edits, new and removed
packages, `.go` files, symlink retargets and swaps of a package directory,
a BUILD file and a `.bzl` file, `.bazelignore`, REPO.bazel, a local rule's
environment, the module extension's `getenv` and watched file,
`--action_env` inherited and set, `--repo_env` re-inherited and pinned,
`--test_env`, `--incompatible_strict_action_env` and `PATH`, `--define`,
`.bazelrc`, semantic MODULE.bazel edits (the generated repository's
attribute) and lockfile edits (the extension repository's recorded
attribute), corrupt and deleted lockfiles, `--lockfile_mode`, store-tree
deletions and additions, the status command's output and stderr, damaged
and deleted snapshots, concurrent runs of one command and of two
different ones with stand-in snapshots that make a save collect (GC and
refresh races), the `local_path_override` module's BUILD, `.bzl` and
MODULE.bazel, failing builds and tests, a 2 s settle, — in non-strict
verify mode only — byte and mode tampering of a store-tree file, which a
hit must log as `first=taint` with `taint=repository-rule-executed`, and
— under `on`, L2-W5h — the generated repository's tool made
non-executable after its consumer was warmed: when the lookup hits,
exactly that step differs (the hit fails, the run without the snapshot
repairs the tree), with exactly one heal naming `tree:mode` and the tool,
and the next run is equal again) and 11 commands (`build` including a
consumer of the generated repository's `glob(["sub/**"])` and one of its
tool, `--nobuild //...`, `test --test_output=all`, `run --script_path`,
the `--action_env`, status, `--repo_env` and override targets, a relative
pattern run from a subpackage, the symlink swaps), the snapshot side in
`on` or (30%) strict `verify` mode: every step byte-identical, built
outputs and MODULE.bazel.lock included — a heal line is a difference
(L2-W5h: before, it was dropped from every step), and no step may log a
`healed` event but the tamper class's. About half the steps run without `BAZEL_RS_SNAPSHOT_EXPLAIN`
— the production path, where the first failing check cancels the rest of
the attempt — and take the outcome from the `PERF-SNAPSHOT` lines. 50
steps in the gate (`BAZEL_RS_SNAPSHOT_FUZZ_STEPS`;
`BAZEL_RS_SNAPSHOT_FUZZ_SEED` replays a sequence; the seed is printed, and
a failure prints the seed and the steps). L2-W5h offline: 16 runs, 3,600
steps (seeds 5202 5404 5505 5606 5707 5808 5909 6010 6111 6212 6313 6414
× 250, and 7101 7202 7303 7404 × 150 restricted to the tamper, failing,
concurrent, damage and settle classes), all identical, 74 failing hits of
the tamper class each healed once, no other heal. Its first batch found a
real defect: under a Tee capture a diagnostics line (a `PERF` mark) could
land between a forwarded `print`'s text and its newline — the print wrote
them into the pipe as two pieces, the reader forwarded them as two chunks,
and diagnostics write to the original stderr directly. Fixed: forwarded
bytes go out whole lines at a time under the diagnostics stream's order
lock (crate::snapshot::diag::ORDER; a partial line waits for its newline
or the end of the pipe), and the reader never writes while holding the
drain lock.

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

Integration-21 (parity-next's target pattern phase): `test` of explicit,
implicit and nested test suites hits with exactly the tests the full
analysis selected (the summaries compared), `--test_size_filters` and
`--noexpand_test_suites` select their own snapshots, `build` of a suite and
`run` of a suite of one test hit
(`test_suites_hit_with_what_the_full_analysis_selected`); a suite with a
non-test member fails `test`, `build` and `run` and is never saved, and a
suite edited into that state misses, is not saved and hits its earlier
snapshot once reverted (`failing_test_suite_expansions_are_never_saved`);
both also in `verify_mode`.

L2-W7 (on by default): the twins' reference side sets `BAZEL_RS_SNAPSHOT=off`
explicitly; `the_snapshot_is_on_by_default_and_off_turns_everything_off`
(unset, empty and unknown values save and hit like `on`, byte-identical
with `off`; `off`, `0` and `false` leave a fresh cache without
`snapshots/` and say nothing under EXPLAIN); a unit test of the values. Every
other CLI test runs with the default. Those whose subject the snapshot
legitimately changes turn it off for the commands concerned
(`SNAPSHOT_OFF`, tests/support/snapshot_off.rs; the reason at each call
site): no cache directory after a failed analysis — the store is set up at
the check point — (tests/custom_rules.rs, loading.rs, native.rs,
parallel_cli.rs, repositories.rs: 9 tests); no file in the cache under
`--nobuild`, which now saves a snapshot (nobuild.rs: 4); the full analysis's
own output on re-runs, which hits skip — PERF-STAGE rows
(plan_timings.rs: 2) and `BAZEL_RS_SNAPSHOT_PLAN_OUT`
(`encoded_requests_are_identical_across_threads_and_loading_jitter`); and
the warm probe repairing a tree file edited in place, which a hit does not
look into (`edited_repository_tree_files_are_caught_by_the_next_command`, the
per-tree trade-off above: on a hit the build failed once with
`conflicting duplicate input destination` — the edited `genrule-setup.sh`
against the genrule's embedded copy — and healed). The whole suite under
`BAZEL_RS_SNAPSHOT=verify BAZEL_RS_SNAPSHOT_VERIFY_STRICT=1` passes: 119
verified hits outside tests/snapshot.rs, no mismatch.
