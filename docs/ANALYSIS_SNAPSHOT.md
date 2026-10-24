# Analysis snapshot: the store, the hit path and verify mode (L2-W5, L2-W6, L2-W5f, L2-W5g, L2-W5h)

Design: PLAN-L2 §1a–§1e, §5, §6 and PLAN-L2 update 1 (§(a) pinned replay,
§(b) printing protocol v1, decision (1) per-tree validation). Builds on the
recording layer and the local-repository replay
(docs/ANALYSIS_SNAPSHOT_RECORDING.md) and on the planned request
(docs/PLANNED_REQUEST.md). Code: `src/snapshot/` (`key`, `exe_id`,
`format`, `store`, `hit`, `save`, `capture`, `diag`, `telemetry`, plus
`validate`, `replay`, `codec`, `record`), wired in `src/main.rs`.

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
   the file's modification time is set (LRU). When the command fails — in
   the build part, in the tests (`test` before it exits 3) or after the
   build part (`test`'s test runner staging a runfile, `run`'s
   preparation) — the hit may have trusted a damaged tree (per tree, a
   file's own bytes and mode are not validated, a local repository's
   replay does not look into its published tree; the full analysis repairs
   both), so the snapshot is removed, and one line says so, when
   * the error names a path in the repository store at all — quoted or
     not, existing or not (L2-W5f review F4: `bazel-rs: removed the
     analysis snapshot of this build: its failure involves <path> in the
     repository store; the next build re-analyzes and repairs it`); or
   * a tree of the snapshot no longer verifies (L2-W5g, verification-based
     healing): each verified store tree as its store's warm check does
     before reusing it — content, modes and links, trusted by stat outside
     the racy window, every file read under `--verify_repo_tampering` —
     and each local repository's published tree against its manifest,
     which must record the replayed execution's identity and tree digest
     (`bazel-rs: removed the analysis snapshot of this build: it failed,
     and the repository tree <root> no longer matches what the analysis
     recorded (<entry>); the next build re-analyzes and repairs it`, the
     entry being the first that differs, L2-W5h: `extra`, `missing`,
     `kind`, `mode`, `content` or `link` with its path below the root —
     or `manifest`, `unreadable`, `other`).

   The failure happens once: the next run's full analysis repairs the tree
   (the hit does not: re-executing a repository rule needs the analysis).
   A failure with intact trees keeps the snapshot — the check walks the
   trees (`PERF-SNAPSHOT heal trees=<n> healed=<bool> ms=<t>` under
   `BAZEL_RS_TIMING=1`) — and a failure that merely mentions a store path
   costs one full analysis next time. Each removal appends a `kind=healed`
   line to the event log (below), whose reason is `<rule>:<entry>` and
   whose path is the differing entry's.

**Repository tree files on a hit (an accepted deviation, L2-W5f lead
decision on review F3; corrected and narrowed by L2-W5g).** Per tree, a
hit validates a repository tree's root, manifest and directories, not its
files: a file added, deleted or renamed anywhere in a tree is a miss (its
directory's lstat key moved; the full analysis repairs the tree,
`a_file_or_directory_deleted_from_a_repository_tree_misses`), but an
in-place edit of a file's bytes or mode keeps every directory key. A local
(`local = True`) repository's replay reproduces its identity inputs and
tree digests in a fresh stage and checks that its published tree exists,
but does not look into the published tree either. So **a hit does not see
byte or mode tampering of a file in a store tree or in a local
repository's published tree that execution uses** (an action input, a
tool, a test's or `run`'s runfile): it executes with the tampered file,
where a run without the snapshot repairs the tree first (its warm probe
sees the mode, size or change time differ, re-executes the rule and
republishes the tree). The L2-W5f re-review reproduced the consequence
before L2-W5g: `chmod 644` on a store-tree tool failed its genrule with
`Permission denied` (exit 1) on every `on` run while the snapshot-less run
repaired the tree and exited 0; an in-place edit of a test's store
runfile failed the test on every `on` run; the same for both in a
`local = True` repository's published tree — only failures whose error
named an absolute store path healed. Since L2-W5g a failed hit verifies
the snapshot's trees (step 6): such a failure happens once, the snapshot
goes, and the next run's full analysis repairs the tree and succeeds
(`a_failed_hit_on_a_tampered_store_tree_heals`,
`a_failed_hit_on_a_tampered_local_repository_heals`). **What persists
under `on` is tampering that does not fail the command**: the execution
phase hashes every action input — crate::digest_memo keys the files of the
tree stores by their *change time*, so an edit that keeps the size and
restores the modification time is still re-hashed — so the action runs
again with the edited bytes and may produce a different output
(`a_hit_hashes_an_edited_tree_file_by_its_change_time`; never a stale
cache hit); a test that still passes runs on the edited runfile; and the
program `run` starts replaces bazel-rs's process, so its failure is never
seen. Like Bazel, which trusts an external repository's files once its
marker is up to date, a hit uses a tampered store file as it is.
`BAZEL_RS_SNAPSHOT=verify` avoids it: the shadow full analysis repairs the
tree before the hit executes — for a store tree that is a mismatch
(`first=taint`, the full analysis's plan executes), for a local repository
it is silent (its re-execution republishes the repaired tree and still
matches the replay record, so the hit is verified and executes with the
repaired tree). For store trees `--verify_repo_tampering` /
`BAZEL_RS_VERIFY_REPOS=full` avoids it too (every INTEGRITY entry
validated: the tampered file is a miss, which repairs the tree); a local
repository's published files have no INTEGRITY entries (the analysis STATs
them, which records the kind only), so there the tampering is unseen
under that policy as well (checked by L2-W5g with the reviewer's chmod and
in-place cases). The directory
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
the differences are reported. For a soak: `BAZEL_RS_SNAPSHOT=verify` (plus
`BAZEL_RS_SNAPSHOT_EXPLAIN=1` for the `verified` lines), then read the log
— and the event log below (`scripts/snapshot_soak_report.py <cache>`
summarizes both).

**The event log (L2-W5g `miss.log`, renamed and completed by L2-W5h).**
`<cache>/snapshots/events.log` (the same encoding) holds one line per
snapshot event of every command with the snapshot on — in `on` and
`verify` mode alike, plus `record`'s report — so the soak has a
denominator: every lookup's hit or miss with the attempt's wall time, every
save, every full analysis that was not saved, verify mode's outcome of
every hit (`verified`, or `mismatch` with verify.log holding the details),
every failed hit that removed its snapshot (`healed`, step 6) and every
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
| `hit` | a lookup hit (in verify mode before its shadow analysis); `detail` splits the time (key, inputs, validate, replay, decode, status) | - | the attempt, key included |
| `miss` | a lookup missed; `detail` is the reason EXPLAIN prints | its class (below) | the attempt |
| `saved` | a full analysis was saved; `detail`: bytes, actions, replays | - | check point to rename |
| `not-saved` | a full analysis was not saved | its class (below) | - |
| `verified` | verify mode: the shadow analysis agreed | - | analysis and comparison |
| `mismatch` | verify mode: it disagreed (`path`: the first differing path) | the first difference's class | - |
| `healed` | a failed hit removed its snapshot (`path`: the first differing entry) | `<store-path\|tree>:<entry>` | the trees' check |
| `evicted` | garbage collection removed the snapshot of `key`; `detail` names the save that collected it | `count` or `bytes` | - |
| `recorded` | `BAZEL_RS_SNAPSHOT=record` reported (no key) | eligible: `yes`, `no`, `unless-pruned` | - |

`scripts/snapshot_soak_report.py <cache dir>` (`--json` for a JSON object)
prints the counts per mode and kind, misses and unsaved runs by reason,
the number of keys and per key the hit rate on unchanged re-runs (hits
over the lookups after the key's first save that missed for no input:
`absent` without a removal since the save, `unreadable`, `capture`,
`panic`, `other` — a miss naming a changed input, or an `absent` miss
after an eviction, heal or verify mismatch, is counted apart), verified
hits by command, always-miss patterns (at least three consecutive misses
of one key with the same reason and path and no hit between them), heals
by reason, every verify.log line verbatim, the hit latency's p50 and p95
per mode and command, and the store's size.

A miss's class is `absent` (no snapshot for the key), `unreadable`, `env`,
the failing check's operation (`read`, `readdir`, `stat`, `lstat`, `canon`,
`readlink` or `integrity`, with the path it names), `tree` (a racy tree
that no longer matches its manifest), `replay`, `status-stderr`, `capture`,
`panic` or `other`; `detail` holds the reason EXPLAIN prints (its first
240 bytes; without EXPLAIN a replay cancelled by the first failure is not
named).
A not-saved run's class is the first taint's name (`repository-rule-executed`,
`network`, `extension-evaluated`, `lockfile-write-pending`, `errors`, ...),
`failed` (the command failed), `capture`, `too-large`, `incomplete` or
`other`. A heal's entry is the first differing entry's class
(crate::repository_store::TreeMismatch: `extra`, `missing`, `kind`,
`mode`, `content`, `link`, `manifest`, `unreadable`, `other`; `named` for
a failure whose error named a store path). Each line is one `write`
(O_APPEND); both logs rotate at 1 MiB to `<name>.1`, keeping one previous
generation (best effort: two commands rotating at once may drop one).

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
| `cache-root`, `store-directories` | the cache dir; `repositories` and its areas (`trees`, `direct`, `git`, `builtins`, with their `trees` and `manifests`), `direct/locks`, `direct/stages` | created, stat'ed or readlink'ed on realpath chains; no content is read |
| `snapshot-store` | `snapshots/` | the hit's own file (header, sections and their checksums verified; its mtime set for LRU), `exe.id` (keyed by the binary's stat), the store directory, the refresh's temporary file, the event log the hit appends to (written, never read) |
| `digest-table` | `digests/` | crate::digest_memo: a record holds a file's stat key with its digest and is used only while the file's current stat matches it |
| `replay-stage` | `repositories/direct/stages/<slug>`, `repositories/direct/locks/stage-<slug>` | a replayed local rule's private stage, created fresh under its lock, compared (identity inputs and tree digests) and removed: nothing in it is trusted |
| `verified-tree` | published store trees and manifests the INPUTS cover (REPO roots and manifests, INTEGRITY entries, paths inside a verified tree not below its links) | the per-tree validation's subject (a racy tree is verified against its manifest) |
| `local-root` | a replayed local repository's published root (REPLAY `root:`) | stat'ed for existence only; its files are not read by the hit (a failed hit verifies them, step 6) |
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
class list had missed: added to `store-directories`.

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
oauth2token:all `--nobuild` 6.5 s against 4.0–4.2 s unset; B2 `--nobuild`
11.1 s against 7.9–8.7 s); hits are unaffected. Follow-up: per-unit
observation buffers merged only for committed units.

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
non-executable (`chmod 644`) and a test's runfile edited in place, each in
a store tree and in a local repository's published tree — fail the hit
once, remove the snapshot (one line, a `kind=healed` log line), and the
next run repairs the tree and succeeds like the run without the snapshot
(`a_failed_hit_on_a_tampered_store_tree_heals`,
`a_failed_hit_on_a_tampered_local_repository_heals`; under verify the
store tree's repair is a mismatch, the local repository's silent); a unit
test drives the check on real publications (intact trees keep the
snapshot).

**Mutation fuzz** (`tests/snapshot.rs::mutation_fuzz`, L2-W5f, extended
by L2-W5g): random sequences over a catalogue of 50 mutations (sources —
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
MODULE.bazel, failing builds and tests, a 2 s settle, and — in non-strict
verify mode only — byte and mode tampering of a store-tree file, which a
hit must log as `first=taint` with `taint=repository-rule-executed`) and
10 commands (`build` including a consumer of the generated repository's
`glob(["sub/**"])`, `--nobuild //...`, `test --test_output=all`,
`run --script_path`, the `--action_env`, status, `--repo_env` and
override targets, a relative pattern run from a subpackage, the symlink
swaps), the snapshot side in `on` or (30%) strict `verify` mode: every step
byte-identical (the heal line aside), built outputs and MODULE.bazel.lock
included. About half the steps run without `BAZEL_RS_SNAPSHOT_EXPLAIN`
— the production path, where the first failing check cancels the rest of
the attempt — and take the outcome from the `PERF-SNAPSHOT` lines. 50
steps in the gate (`BAZEL_RS_SNAPSHOT_FUZZ_STEPS`;
`BAZEL_RS_SNAPSHOT_FUZZ_SEED` replays a sequence; the seed is printed, and
a failure prints the seed and the steps).

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
