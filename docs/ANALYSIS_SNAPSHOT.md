# Analysis snapshot: the store and the hit path (L2-W5)

Design: PLAN-L2 §1a–§1e, §5, §6 and PLAN-L2 update 1 (§(a) pinned replay,
§(b) printing protocol v1, decision (1) per-tree validation). Builds on the
recording layer and the local-repository replay
(docs/ANALYSIS_SNAPSHOT_RECORDING.md) and on the planned request
(docs/PLANNED_REQUEST.md). Code: `src/snapshot/` (`key`, `exe_id`,
`format`, `store`, `hit`, `save`, `capture`, `diag`, plus `validate`,
`replay`, `codec`, `record`), wired in `src/main.rs`.

**Off by default.** `BAZEL_RS_SNAPSHOT=on` reads and writes snapshots for
`build`, `test` and `run`; `=record` keeps the recording-only report;
`=off` or unset does nothing; any other value is reported on the
diagnostics stream and ignored. `BAZEL_RS_SNAPSHOT_EXPLAIN=1` says, on the
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
   already part of the recorded window bytes):
   * **validation** (`snapshot::validate::validate_for_hit`, stopping at the
     first mismatch, which EXPLAIN names): per tree by default — the
     observations outside verified repository trees and those at or below a
     tree's symlinks, the environment digests, and each tree's root and
     manifest; everything under `--verify_repo_tampering` /
     `BAZEL_RS_VERIFY_REPOS=full` (the policy is part of G). Settled stat
     keys are trusted; racy or changed READ / READDIR observations are
     re-read and compared by content; a changed integrity entry misses; a
     racy one (a tree published, or its manifest refreshed, less than two
     seconds before the recording) verifies its tree against its manifest.
     Below 4,000 checks one thread runs them.
   * **replay** of every `local = True` repository rule the recorded run
     executed (`snapshot::replay`), against the pinned repository set,
     with **this command line's** resolver options and repository
     environment: every replay must reproduce its identity inputs and tree,
     print nothing, and its published tree must still exist.
   * **plan**: the PLAN section read, checked and decoded
     (`snapshot::codec`, chunks decoded on threads for large plans).
   * **warm-up**: the digest table (crate::digest_memo) is loaded for the
     execution tail.
4. The workspace status of the plan's status placeholders is computed
   (quietly): a status command that now writes to stderr is a miss (a full
   analysis would print it during planning, inside the window).
5. Nothing has printed so far: any mismatch, error or panic (caught) is a
   miss, and the command continues as a full analysis exactly as if no
   snapshot existed. On a hit the window's stderr bytes are written, then
   the decoded request is finalized (status, inherited environment) and
   executed by the same code as a planned one (`build_tail`, `test_tail`,
   `run_tail` in src/main.rs).
6. After a hit: keys a content check found settled are written back — the
   snapshot is rewritten in the background with the refreshed INPUTS — and
   the file's modification time is set (LRU). When execution fails on an
   input inside the repository store that no longer exists, the per-tree
   validation trusted a damaged tree (the full analysis repairs trees): the
   snapshot is removed and one line says so
   (`bazel-rs: removed the analysis snapshot of this build: its input ... is missing from the repository store; the next build re-analyzes and repairs it`).

**Repository tree files on a hit.** A hit does not verify repository
trees beyond their heads. The execution phase hashes every action input:
crate::digest_memo keys the files of the tree stores by their *change
time*, so an in-place edit that keeps the size and restores the
modification time is still re-hashed, and the action runs with the edited
bytes (`tests/snapshot.rs::a_hit_hashes_an_edited_tree_file_by_its_change_time`)
— where the full analysis would repair the tree first. That is the
documented trade-off of per-tree validation (PLAN-L2 update 1, decision
(1)); `--verify_repo_tampering` validates every entry.

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
| INPUTS | window start; paths sorted and front-coded (u16 shared prefix, suffix bytes); observations by path index; environment digests; repository trees with the integrity entries the policy validates; replay records (definition, bound attributes, names, identity digests, recorded output); the pinned tables |
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
MODULE.bazel edits miss, lockfile writes do not save; a file deleted from a
repository tree heals (per tree: a failing hit removes the snapshot; full:
a miss); another binary misses; damaged files miss and are replaced;
concurrent saves; `--nobuild`, `test` and `run --script_path` hit; symlink
retargets and `.bazelignore` edits miss; status command changes hit and
re-run the consumer, a status command writing to stderr does not save;
network (a remote registry's cold run, `--lockfile_mode=off`) and printing
local repositories never save; failed builds never save; `local_jdk`-shaped
repositories miss on a changed `JAVA_HOME`, a `swiftc` on `PATH` or a file
added to the host directory a glob reads through their link; cross-repository
store-tree links hit; an edited tree file is hashed by its change time.
Unit tests cover the key, the exe.id record, the format (round trips,
damage), the store (read back, damage, GC, uncommitted writes), the hit's
validation and refresh, and the self-healing rule.

Fixture 19 (everything again in verify mode) and the strace audit of the
hit path belong to L2-W6.
