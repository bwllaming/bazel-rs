# Affected targets: target-determinator vs. a native `bazel-rs affected`

Question (user): is target-determinator (TD) the right approach for
~/code's CI change detection, or could something built into bazel-rs make it
much faster?

**Answer.** TD's method is sound, but most of its cost goes into four
analysis-phase cqueries (two per revision, 195–312 s each on ~/code). It
also hashes the build graph (attributes, the whole configuration, the
contents of every `.bzl` file), not what would actually be built. bazel-rs
already has a per-revision analysis that can be persisted and reused
incrementally, and it plans every action. So it can answer the question
directly: which requested targets would build or test differently. Code:
`src/affected.rs`; tests: `tests/affected.rs`.

The prototype is the hidden command `bazel-rs affected --base=REV`. It makes
at most one analysis per revision instead of TD's two cqueries per revision.
For a local change, the analysis is restricted to the change's scope (§8).
The analyses run in a git worktree, so nothing is checked out in place. It
prints TD's output format: one label per line.

On three ~/code commit pairs, its result agrees with TD's, with every
difference explained (§4):

| situation | time |
|---|---|
| TD today (bazel-rs cquery backend) | 7.4–14.1 min |
| prototype, both revisions fully analyzed | 8.6–9.4 min |
| prototype, head incremental | 6.2 min |
| prototype, both snapshot hits | 0.5 min |

**Cold CI (§8, this revision).** CI runs on an empty cache. So `affected`
now analyzes only what a change can reach. A change inside main-repository
packages is classified first. Both revisions are then analyzed restricted to
the loading-phase reverse dependencies of the changed packages. External
repositories are leaves, so nothing is fetched for them. Other changes
(MODULE.bazel, the lockfile, manifests, rc options the command reads) still
get a full analysis of both revisions. There are two further options:
`--concurrent_revisions`, and stored digests (`--write_digests` /
`--base_digests`). Cold, on the five validation pairs (fresh private cache,
same set as the full analysis every time):

| pair | change | before (full + full) | now | genuine 9.3.1 + TD |
|---|---|---|---|---|
| P4 | one Go test file | 813 s | **104 s** (88 s with stored base digests) | 269 s |
| P3 | `.bazelrc` (unused config) + Go | 823 s | **291 s** (219 s concurrent) | 267 s |
| P1 | BUILD + testdata | 1001 s | **312 s** (227 s concurrent) | 259 s |
| P2 | lockfile + requirements (global) | 827 s | 800 s; **528 s** concurrent; **537 s** stored base digests | 270 s |
| P5 | one BUILD edit | 809 s | **96 s** (783 s before the analysis-order fix, §8.6) | 259 s |

A full analysis (no saved snapshot) is about twice as fast since
docs/COLD_ANALYSIS.md: on the private clone at `80d6724f3a` with warm
repositories, `affected --base=HEAD^` (802 targets in scope) takes 81 s
instead of 179 s, with the same output and digests.

Since §8.11 the base's digests also go through the configured remote cache
(`--remote_cache`, HTTP or gRPC): the second `affected` on a base looks up
the digests of the head's targets there and plans the base only for the
missing ones, not at all when none is missing.

Since §8.9 a local change plans both revisions at once by default, the
planning itself is ~25% faster, and the CI's `build` after `affected`
derives its plan from it: PR 13252's CI sequence (`affected`, `query`,
`build`) takes 501 s cold (690 s with 0.4.7), 287 s lukewarm (467 s) and
151 s warm (379 s) on the private clone.

## 1. target-determinator v0.34.0: exactly what it does

Source: github.com/bazel-contrib/target-determinator at tag `v0.34.0`
(`pkg/target_determinator.go`, `pkg/hash_cache.go`, `pkg/walker.go`,
`pkg/cache.go`, `pkg/configurations.go`). The binary is the official
release: sha256 `115e1c63…8011`, the same as ~/code's
`multitool.lock.json`.

### Per revision (before, then after)

1. **Check out the base revision.**
   - `gitSafeCheckout`: `git checkout <base>` *in place* when the working
     copy is clean.
   - Otherwise, or if the checkout leaves it unclean, it uses a cached
     worktree `~/.cache/target-determinator/worktrees/td-worktree-…`
     (`checkout -f` and `clean -ffdx`), then `git submodule update`.
   - "After" is the current working copy. The original revision is checked
     out again at the end.
2. **Results cache.** TD looks up its results cache
   (`~/.cache/target-determinator/results/<sha256>`). The key covers:
   - the TD binary hash, the Bazel release, the git *tree* sha and the
     pattern;
   - `-filter-incompatible-targets` and the ignored files.

   It does not cover user/system rc files, the environment or the host. A
   dirty working copy disables the cache.

   This is already approach (c) below. In CI it is empty unless
   `~/.cache` is persisted.
3. **Bazel calls:**
   - `info release`;
   - bzlmod detection, then `mod dump_repo_mapping`;
   - `cquery --output=streamed_proto --transitions=lite 'deps(//...)'`:
     every configured target with its `configured_rule_input`s;
   - `cquery --output=streamed_proto //...`: the matching targets and their
     configurations;
   - `config --output=json --dump_all`, used only for `-verbose`
     explanations.

   With the default `-filter-incompatible-targets=true` it adds two
   `--output=starlark` cqueries. bazel-rs implements no starlark output,
   which is why ~/code passes `=false`.
4. **Hashing, per (label, configuration), recursively** (`hashRule`):
   - the Bazel release string;
   - the rule class;
   - `skylark_environment_hash_code`: a digest of the defining `.bzl`
     files and their transitive loads;
   - the configuration checksum;
   - every attribute as a proto, sorted, with `explicitly_specified`
     cleared and `generator_location` blanked;
   - then each configured rule input: its label, configuration and hash.

   Other target types:
   - A **source file** hashes its contents plus the user-executable bit. A
     missing file or a directory gets an empty hash: the contents of a
     directory listed in `srcs` are not seen.
   - A **generated file** hashes its generating rule's hash.
   - A **package group** gets an empty hash.
5. **Diff** (`WalkAffectedTargets`). Every label matching `//...` at
   "after" is reported when it is new, has a configuration it did not have
   before, or its hash changed. Deleted targets are never reported. A
   failed "before" query means everything is affected
   (`-before-query-error-behavior=ignore-and-build-all`, the default).

**What it gets right.**

- Any attribute change.
- Any transitively consumed source file.
- Rule implementation changes.
- Configuration and flag changes. `.bazelrc` edits enter through the
  configuration checksum.
- MODULE.bazel and lockfile changes, as far as they change external
  targets' attributes or files.
- Toolchains, through the configured rule inputs.

**What it over-reports.**

- Any change of the configuration (all fragments, including unrelated
  languages) affects everything in it.
- Any `.bzl` edit, even a comment, affects every target of every rule that
  file defines.
- Attribute-only changes such as `tags`, `visibility` or a `size`, and
  their whole reverse closure.
- Dependencies in several configurations are mixed together.

**What it misses (non-hermetic inputs).**

- Inherited `--action_env` values and host tools.
- Files a repository rule read, unless they surface as source files of the
  external repository.
- Contents of directory sources.

**Cost model.** Two analysis-phase cqueries per revision, run sequentially,
plus hashing in Go. Measured below with bazel-rs as the Bazel: 195–312 s
per heavy cquery, 7.4–14.1 min per run.

## 2. Alternatives

- **bazel-diff** (Tinder/bazel-diff, now a Rust CLI and server). It runs
  `generate-hashes` for each revision, then `get-impacted-targets` to
  compare the two hash files.

  *Default mode*: a loading-phase `query` over `//...:all-targets`, plus
  the bzlmod repositories (`mod show_repo`) or `//external:all-targets`.
  Per rule it hashes:
  - the rule class, label and `skylark_environment_hash_code`;
  - the attributes, minus `generator_location`, the `--ignored…` ones, and
    URLs pinned by an integrity;
  - the macro `.bzl` files of the instantiation stack;
  - the rule inputs, transitively;
  - source file contents;
  - optional `--seed-filepaths` contents, which are mixed into every hash.

  External repositories are hashed coarsely, by their definition, unless
  `--fineGrainedHashExternalRepos` is given.

  *Strengths*: it needs no analysis, so it is cheap: one `query`, much
  faster than a cquery. It reports distance metrics and has an `explain`
  command. A `serve` mode with a persistent server and
  `--modified-filepaths` skips unchanged files.

  *Weaknesses*: it has no configurations. `select()`s are seen as all
  branches; transitions and toolchain resolution are not seen at all. So
  `.bazelrc`/flag changes and resolved toolchains need seed files or
  `--useCquery`, which is a cquery like TD's.

  Hash files are commonly stored per commit, as a CI artifact or cache, so
  that only the head is hashed.
- **Bazel itself** has no "affected targets" command. Skyframe's
  invalidation is per server and per output base. It is not exposed across
  revisions, and a cold server pays the full analysis anyway.
  - Bazel's own repository has `tools/aquery_differ`, which diffs two
    `aquery` outputs. That is the action-level idea, but it is offline and
    not per target.
  - Commercial CI products (Aspect Workflows, BuildBuddy, EngFlow)
    integrate TD- or bazel-diff-style selection, or rely on the remote
    cache to make "build everything" cheap. None of them can see more than
    Bazel exposes.

| | TD | bazel-diff (query) | bazel-rs action digests (this) |
|---|---|---|---|
| flag/rc change | config checksum → everything in the configuration | missed (seed files) | only targets whose actions change |
| MODULE/lockfile change | targets whose external inputs or attributes change | coarse per repository | targets whose actions' inputs change (tree contents, not identities) |
| toolchain change | yes (rule inputs) | no (query) | yes (the toolchain's actions and files) |
| `.bzl` comment | every target of the rules it defines | the macro's targets | nothing |
| rule implementation edit | every target of the rule | same | targets whose actions change |
| `tags`/`visibility` edit | the target and its reverse closure | same | nothing (a test's `tags` and `size`: the test) |
| deleted target | not reported | not reported | not reported |
| test `size`/`timeout`/`args`/`env` | yes (attributes) | yes | yes (test properties, run settings) |
| non-hermetic (host tools, inherited env) | missed | missed | missed |
| cost on ~/code | 2 analyses ×2 cqueries | 2 queries | 1 analysis per revision; 0 when cached; incremental |

## 3. The bazel-rs-native design

### What a target digest is

The analysis already plans every action with its complete identity: the
command, environment, mnemonic, execution info, inputs and outputs by path.
The executor keys those *without an owner* (`native-action-v10`). So
"would building or testing X do something different?" can be computed
from the plan, before any execution:

1. **Action digest (Merkle).** It covers:
   - the planned action as data, with its id and its producers' ids
     cleared. Paths are normalized: the workspace root becomes
     `<workspace>`, and a repository store tree identity
     (`…/trees/<64 hex>`) becomes `<tree>`. A tree's identity covers its
     specification and extension group, so an unrelated change of the group
     renames every tree in it while the files stay the same.
   - each source input's contents: the user-x bit and the SHA-256, through
     the digest memo. A directory input hashes its listing, recursively.
   - each generated input's output index, destination and its *producer's*
     digest.
   - shared input groups (a tool's runfiles tree), digested once per group.

   The local part is computed on threads; the Merkle step runs in
   post-order.
2. **Target digest**, per requested label. It covers:
   - its reported outputs (sources by contents, generated outputs by their
     producer's digest);
   - every action the target owns (`L#n`, `L#runfiles_manifest`, its
     aspects' `L%…`);
   - the actions of the targets producing its outputs (an alias's actual,
     a filegroup's members), with their runfiles trees;
   - for a test, its run plan (executable, runfiles entries, the completion
     producers, `args`/`env`, the configuration's `--test_env`) and its
     `TestRuleInfo` (size, timeout, flaky, shard count, local, tags), but
     not its BUILD location.

   It is computed **before** `finalize`. The workspace status nodes are
   still placeholders then, so stamping never changes a digest.
3. A target is **affected** when its digest differs from the base's, or it
   is new. Incompatible targets are not reported (`--include_incompatible`
   reports those that became incompatible). A revision that fails to plan
   fails the command; `--base_failure=all` makes a failed base report
   everything instead, as TD's default does (§8.3).

This is TD's question at the action level. A target is reported exactly
when the build or test the CI would run for it can do something different.

### Candidate approaches

**(a) Action-key diff: analyze both revisions, digest, diff.**

- It is implemented, and it is correct by construction for everything the
  plan sees.
- The base is analyzed in a git worktree that shares the cache
  (docs/WORKTREES.md), so nothing is checked out in place. Repositories and
  actions are shared.
- Cost: one analysis per revision. On ~/code that is 235–280 s for
  `test --nobuild //...` (1,906 targets, 57,941 actions), plus the digests
  (12 s, §5).
- That is already about half of TD's cost: one analysis per revision
  instead of two cqueries, which each pay an analysis.

**(b) Incremental from the base snapshot.**

- Run both revisions in the *same* worktree. The second analysis then
  starts from the first's snapshot, and the incremental machinery
  (docs/INCREMENTAL_ANALYSIS.md) re-analyzes only the dirty configured
  targets and splices them into the plan.
- **Measured across a real commit:** one BUILD file plus testdata
  (P1) dirtied 1 configured target. The partial analysis took 95 s,
  because the dirty `wcb_test`'s forward closure is large (Tier A
  re-analyzes it; cost estimate 45 %). The whole head planning took 118 s
  instead of 256 s.
- Using the dirty set *as* the answer (no re-analysis) would be faster
  still. But it is a superset that depends on the attribution, and it falls
  back exactly where the incremental analysis does: MODULE, lockfile, rc
  and `.bazelignore` changes, repository inputs, `.bzl` files a definition
  load entered.
- So (b) is used as an accelerator of (a): the same digests over a spliced
  plan. A splice is verified byte-identical to a full analysis (verify
  mode), so the answer cannot change.

**(c) Persistent per-commit digests.**

- The digests file is small: 1,906 lines, about 170 KB. Keyed by
  (commit, bazel-rs binary, flags, patterns), it lets a PR run skip the
  base analysis entirely.
- This is TD's results cache, done portably: a CI artifact or a cache
  entry written by the main-branch run.
- With the bazel-rs cache restored as well, the worktree's snapshot of the
  base usually hits (13.6 s, measured), and the head is incremental.

**Recommendation.** Build on (a) and use (b) and (c) for speed:

- (a) is the semantics.
- (b) is free once both revisions are planned in one worktree.
- (c) removes the base analysis from PR runs.

The dirty-set-only shortcut of (b) is not worth its fallbacks. The action
digests are both more precise than TD and cheaper.

## 4. The prototype: `bazel-rs affected`

```
bazel-rs [STARTUP...] affected --base=REV [--head=REV] [--worktree=DIR]
    [--affected_command=test|build] [--keep_digests=DIR] [--explain_affected]
    [--include_incompatible] [--include_manual] [--scope=auto|full]
    [--[no]concurrent_revisions] [--write_digests=FILE] [--base_digests=FILE]
    [--affected_remote_digests=off|read|readwrite]
    [--affected_remote_digests_epoch=STRING] [--base_failure=error|all]
    [BUILD OPTIONS...] [PATTERNS... | -- PATTERNS...]
```

**What it does.**

- It resolves the revisions in the current repository and checks out the
  base in a worktree. That is `--worktree`, else
  `<cache-dir>/affected-worktrees/<id>/checkout` with an explicit
  `--cache-dir`, else `<git-common-dir>/bazel-rs-affected`. Keep it outside
  the checkout: genuine Bazel's `//...` does not skip `.git` (bazel-rs's
  does), so it would load the worktree's packages (seen in §8.5).
  The worktree is created or repaired as needed, with `checkout -f` and
  `clean -ffdx`.
- It runs this binary with the startup options (including `--cache-dir`
  and `--native-lockfile`):

  ```
  test --nobuild <options> -- <patterns>
  ```

  `//...` is the default pattern. `BAZEL_RS_TARGET_DIGESTS` names the
  digests file.
- It then checks out the head in the same worktree and plans again. A full
  (unscoped) run is an incremental analysis or a snapshot hit when
  possible. Without `--head`, the head is `HEAD`. A working copy with
  uncommitted changes is planned in place instead, never scoped. Its paths
  are normalized; before this revision they never matched the worktree's,
  see §8.4.
- With a local change (§8), both plannings are restricted to the change's
  scope (`BAZEL_RS_AFFECTED_SCOPE`).
- It prints the affected labels in TD's spelling (`//pkg` for `//pkg:pkg`),
  sorted, one per line. The planning output goes to stderr, without its
  DEBUG, WARNING and INFO events (`BAZEL_RS_AFFECTED_QUIET_EVENTS=1`,
  console-only like `--quiet`; TD's cquery shows none of its events):
  `print()` output, warnings and the build summary of the analyses stay
  hidden, errors and the `bazel-rs` progress notes are shown.

**Hook.** Any `build`/`test`/`run` with `BAZEL_RS_TARGET_DIGESTS=<file>`
writes `label<TAB>digest` lines (header `bazel-rs-target-digests-v1`). It
does so for a full analysis, a hit or a splice; the variable is read after
planning and is no snapshot input. Under it, a `test --nobuild` whose
analysis succeeded exits 0 (`bazel-rs affected: analysis of N test(s)
succeeded`) instead of Bazel's exit 1 ("Couldn't start the build. Unable
to run tests") or exit 4 ("No test targets were found"). The orchestrator
takes any other exit for a failure of that revision, whatever it wrote
(§8.3). Through 0.4.1 it accepted any exit once digests were written, which
logged `(exit 1)` for every successful planning and could not tell a
failed analysis from a successful one.

**`--include_manual`.** Wildcard patterns then also select `manual` rules
(Bazel's `query` semantics), and `test`'s test determination keeps
`manual` tests. The planning processes get
`BAZEL_RS_AFFECTED_INCLUDE_MANUAL=1`, which the target pattern phase,
incremental analysis's relabelling and the snapshot key read, so a
snapshot of one mode never answers the other. Both the scoped and the full
paths honour it, and the `--write_digests`/`--base_digests` key includes
it. Manual targets selected this way are wildcard-selected: an
incompatible one is skipped (reported only with `--include_incompatible`,
as any other), and one that fails analysis fails the planning (TD's
cquery of `//...` fails the same way). On ~/code it widens `//...` from
about 2,500 to 18,571 targets, so a full analysis costs more (P2: 1,182 s
and 14.4 GB instead of about 800 s and 9.2 GB); a scoped one costs little
more (P1: 252 s).

**As a drop-in for the script.** In `src/python/tools/build/__init__.py`,
replace

```
target-determinator -filter-incompatible-targets=false <commit>
```

with

```
bazel-rs affected --base=<commit> <the storagebox overrides>
```

`bin/push-changed-containers` (`get_changed_targets(include_manual=True)`)
adds `--include_manual`: its `attr(tags, dockerpush, set(changed))` picks
the push targets, which are `manual` (`src/docker/docker.bzl`,
`src/ts/npm.bzl`, …), out of the affected set, and a change of only a
push target's attributes (a docker macro argument) is reported only that
way. `bin/build-changed-files` keeps the default: it drops manual targets
anyway.

Use the same rc and flags the CI's later `build`/`test` uses. Then add
`--cache-dir=<a directory outside the checkout>`, and optionally
`--concurrent_revisions` (§8.3). The output contract is the same. One
script detail matters, already done in bwllaming/code#13177:

- TD also reports **manual** targets. A plain `build`/`test //...` never
  plans them, so the prototype does not.
- The script's `rdeps(//src/..., set(X)) except attr(tags, 'manual',
  set(X))` removes only the manual targets *in X*. With the prototype's X,
  manual rdeps such as the `oci_push`/`image_*` targets would then be
  built.
- So use `… except attr(tags, 'manual', //src/...)`. With TD's X the result
  is identical: every manual target in `rdeps` is affected, hence in X.
  Alternatively, `--include_manual` plans the manual targets too (§8.7).

### Validation against TD on ~/code

**Setup.**

- A private clone of `bwllaming/code` (HEAD `9b14e854`), with a private
  cache and the real `.bazelrc` with the storagebox overrides.
- bazel-rs built from this branch plus the 9.3.1 worker's embedded tools
  and version, for measurement only. With 9.3.0 every run rewrites two
  `bzlTransitiveDigest`s in the lockfile: `lockfile-write-pending`, never
  saved.
- TD v0.34.0 used that same binary as its `bazel`, with
  `-filter-incompatible-targets=false`, exactly as the script calls it.
- "Final" is the script's `rdeps(//src/..., set(X)) except attr(tags,
  'manual', set(X))`. "Final′" uses `except attr(tags, 'manual',
  //src/...)`, applied to the prototype's X.

| pair (base → head) | change | TD | prototype | final TD / prototype | final′ | differences |
|---|---|---|---|---|---|---|
| P1 `470886f0 → 7ab6d35d` | `wcb/BUILD.bazel` + testdata | 31 | 20 | 22 / 33 | 22 = 22 | TD's extra 11 are `manual` `image_*` targets |
| P2 `e7b42879 → 9e6b547e` | duckdb bump: lockfile, `thirdparty/extensions.bzl`, requirements | 75 | 53 | 64 / 83 | 64 vs 63 | TD's extra 22: 20 `manual` image targets, `//src/python:pytest` (a `test_suite`; its tests are reported and `rdeps` brings it back), `//src/python:bworker_env` (a `py_library` with no actions: building it does nothing; its dependants are reported) |
| P3 `bccb9427 → 80859617` | `.bazelrc` adds `test:replay`, a Go source | 4 | 4 | 4 / 4 | 4 = 4 | none |

**Notes.**

- The prototype's raw output is a subset of TD's in all three pairs. In P3
  the new rc config changes no configuration, and both tools agree.
- Two bugs were found by this validation:
  - **Tree identities.** Without normalizing store tree identities, P2
    reported 963 targets: every Go target, through the `nogo` analyzers in
    the `workspace_deps` extension group, whose tree ids changed.
  - **Action owners.** Owner ids like `L#runfiles_manifest` initially
    missed `create_venv`'s runfiles (`requirements.txt`).

## 5. Measurements (~/code clone, shared machine, `nice -n 10`, one run each)

| | wall | notes |
|---|---|---|
| TD P1 / P2 / P3 | 739 / 845 / 446 s | heavy cqueries 312+197+195, 215+196+198+194, 211+195 s (P3's head reused bazel-rs's cquery state: 2–3 s) |
| prototype P1, both full (the first run of a binary) | 619 s | 355 s (repositories re-published for the new binary) + 264 s |
| prototype P2 / P3, full + full | 562 / 516 s | 256–281 s per revision, digests included |
| prototype P1, base full, head **incremental** | **375 s** | base 256 s; head 118 s: 1 dirty target re-analyzed in 95 s, spliced (same digests as the full analysis of that head) |
| prototype P1 repeated (base and head both incremental from each other) | 251 s | 130 + 122 s |
| prototype, base = head, both **snapshot hits** | **27 s** | 13.6 s per revision: hit 0.4 s, digests 12 s |
| digests (57,941 actions, 121,487 source files, all digest-memo hits) | 12 s | sources plus local action digests 11.1 s on 16 threads, Merkle and targets 0.8 s. The first version (sequential, serialized groups) took 19–24 s. |

The snapshot was not saved after any revision whose module extension
results differ from the previous run's (P2's lockfile change, a
`.bazelignore` change): `lockfile-write-pending` (hidden lockfile). The next
run after such a revision is a full analysis, and incremental analysis
falls back for `.bazelignore`, `MODULE.bazel.lock` and repository
specification reads.

## 6. CI implications and the phased plan

**CI today.** It runs TD (7–18 min), then `query rdeps`, then a `build` and
a `test` of the result. Each of those analyzes again, in genuine Bazel or
in bazel-rs.

**Without a restored cache.** The first prototype was 2 full analyses
(13.5–17 min cold on ~/code). Since §8 a local change costs 1.5–5 min
cold. A global change costs 9 min cold with `--concurrent_revisions`, or 9
min with stored base digests. ~/code's CI runners keep `~/.cache` (and the
checkout) between runs (`.github/selfhosted/docker-compose.yml`), so warm
runs are common there; the cold path is the first run of a runner.

**Phase 1 (this branch).** The prototype command and the digest hook, as a
drop-in with the script tweak above.

**Phase 2.**

1. **Per-commit digests (c).** Done (§8.3): `--write_digests` /
   `--base_digests`, keyed by the commit and (binary, command, options,
   patterns).
2. **Restore the bazel-rs cache** (repositories, digest memo and the
   worktree state) between CI runs. With the snapshot of the previous main
   commit, the head is incremental. P1: 118 s; a typical
   one-package edit: 5–20 s, docs/INCREMENTAL_ANALYSIS.md.
3. **Persist the action digests in the snapshot.** A hit then needs no
   digest work (12 s → < 1 s), and a splice recomputes only the dirty
   owners' closure.
4. **`--include_manual`.** Done (§4, §8.7): wildcards keep manual rules.
5. **Keep the snapshot across lockfile-changing revisions.** Save after a
   hidden-lockfile-only write: the extension results are recorded inputs
   anyway.

**Phase 3.** `bazel-rs test --affected_since=<base|digests> //...`. It
plans `//...` once at the head, digests it, keeps only the affected
targets' completions and test actions, and executes them. That removes the
script's `query rdeps` and its separate `build`/`test` analyses (the
`rdeps` closure is already in the digests: a dependant of a changed target
changes). On ~/code that is one (often incremental) analysis for the whole
CI job, instead of TD's four cqueries plus two more analyses.

## 7. Limits and risks

- **Manual targets** are not planned without `--include_manual` (§4).
- **Action-less targets.** Targets without actions or outputs, such as a
  `py_library` or a `config_setting`, are reported only when their
  reported outputs change. Their dependants are reported, and that is what
  the CI builds. A `test_suite` is expanded into its tests.
- **Shared ids.** Action owner ids are label-based. A target name
  containing `#` or `%` is not told apart from its actions' suffixes.
- **Non-reproducible repository outputs** (a repository rule writing
  timestamps or absolute paths) change their consumers' digests every time
  the repository runs. That over-reports, which is the safe direction. One
  early P2 run, made with the first prototype binary, reported 7 more
  Python targets than the final binary; this could not be reproduced.
- **Non-hermetic inputs** are not seen, as in TD: host tools, inherited
  `--action_env=NAME` values (only the name is keyed, before `finalize`),
  files outside the plan.
- **Remote digests** (§8.11) carry a writer's world to a reader. A gap in
  the key (host tools, the environment beyond what is keyed) only
  over-reports, except when a change exactly undoes it (§8.11, "Trust");
  the key is independent of the checkout's, the cache's and the rc files'
  locations and of client variables (§8.11, "Location independence").
  The real risk is trust: one entry decides "not affected" for every PR on
  its base, so only trusted jobs should write (`readwrite`), PR jobs
  `read`, and a cache that untrusted jobs can write to needs server-side
  write authentication.
- **Analysis cost.** The full `test --nobuild //...` analysis is about
  the cost of one TD cquery. Its peak is 9.1–9.2 GB; a scoped run peaks at
  4.2 GB, while loading `//...` (§8.5).
- **The scope's assumptions** (§8.2). The **analysis-order bug** that
  scoped subsets hit is fixed (§8.6).
- **Version alignment.** bazel-rs must report the lockfile's Bazel version
  (9.3.1 for ~/code, the other worker's branch). Otherwise every analysis
  rewrites the lockfile and is never saved.

## 8. Cold CI: scoped analysis (this revision)

Code: `src/affected_changes.rs` (classification, roots),
`src/engine/affected_scope.rs` (the scope, after the target pattern phase),
`src/affected.rs` (orchestration), `src/affected/value_hash.rs`. Tests:
`tests/affected.rs` (scope, global change, root fallback, an rc line the
command does not read, `--concurrent_revisions`, stored digests, the working
copy planned in place).

**~/code's CI.** `lint.yaml` runs `bin/build-changed-files` on self-hosted
runners (`.github/selfhosted`). The checkout uses `fetch-depth: 2`, so the
base `HEAD^` (the merge commit's first parent) is present. `GIT_BASE`
overrides it. Each runner keeps `/home/runner/.cache` and its work
directory on the host. Actions go to the storagebox HTTP cache
(`--remote_cache`, never contacted here). Repository downloads are not
uploaded anywhere: they live in the runner's `~/.cache`. A cold run is a
fresh runner or a cleaned cache. The user's constraint was to optimize that
case, so this section measures it.

### 8.1 Where the cold time went

A cold `test --nobuild //...` (fresh private cache, P4 head) took 530 s:

| phase | time |
|---|---|
| loading `//...` (with the parallel fetch of what the BUILD files load) | 38 s |
| analysis | 458 s |
| actions and digests | 33 s |

Warm, the analysis takes ≈ 195 s, so about 260 s of the cold analysis is
repository fetching. That is 40 GB in the cache.

A cold loading-phase `query 'rdeps(//..., X)'` is **no cheaper**: 537 s and
50 GB. It loads every external package that `//...` reaches, through all
`select()` branches. A cold `query //...` alone took 167 s, because query
executes the 3,623 repository rules its loads need one by one (the build
path runs them on 32 workers: 38 s). So the scope is computed inside the
planning process, from the packages it loaded anyway, and external
repositories are leaves.

### 8.2 The scope and why it is sound

**Classification** (`affected_changes.rs`), from `git diff --name-status
--no-renames base head` and each revision's tree (`git ls-tree`). A change
is **global**, which means a full analysis of both revisions, when it is:

- MODULE.bazel or an include, the lockfile, `WORKSPACE*`, `REPO.bazel`,
  `.bazelversion`, `.bazelignore` or `.bazeliskrc`;
- an rc file (`.bazelrc` and its `%workspace%` imports) whose *effective
  lines* for the command differ. Those are the `startup`, `common`,
  `always`, `build` and `test` lines, unconfigured or of a config they or
  the command line enable (transitively, plus the host platform configs),
  with imports inlined. P3's new `test:replay` config changes none, so P3
  is local now;
- a package-manager manifest (`package.json`, `pnpm-lock.yaml`, `go.mod`,
  `go.sum`, `Cargo.*`, `requirements*.txt`, `pyproject.toml`, …);
- a file a repository is given or records: a label MODULE.bazel names, a
  `@@//` path of the lockfile (and anything below it), the `.bzl` files of
  module extensions and repository rules with their loads (textual
  `load()`s), and the labels those files name;
- a file in no package, or in an ignored directory, at both revisions;
- outside the workspace.

Anything else is **local**. Its packages get *rule changes* when their BUILD
file changed, a file was added or removed in them (globs), or a subpackage
appeared or disappeared below them. A modified source changes no rule:
loading never reads contents. Changed `.bzl` files are listed, and so are
changed files (with their directories, and with symbolic links into them).

**Roots** are the main-repository labels that analysis reaches outside the
loading graph:

- every label in MODULE.bazel and its includes (`register_toolchains`,
  `register_execution_platforms`, nogo, …);
- the labels of every rc line and command-line option (`--platforms`,
  `--extra_toolchains`, label flags, `--run_under`, …);
- the label literals of `.bzl` files that contain `transition(`, whose
  outputs can be labels (~/code's `fixed_platform_files` sets
  `--platforms` from the string `"//platforms:linux_amd64"`);
- the labels of the extension `.bzl` files.

**The scope** (`affected_scope.rs`) is computed in each planning process,
after the target pattern phase. It walks the main-repository graph forward
from the selected targets and the roots. The edges are:

- every label of every attribute: all `select()` branches *and keys* of any
  attribute type, implicit and default values, `nodep` labels, visibility
  and package groups;
- toolchain types;
- the dependencies of attribute aspects;
- an output file's generating rule.

External labels are not followed. Then it marks:

- every rule of a package with rule changes, or of a package whose BUILD
  file transitively loads a changed `.bzl` file;
- every source file that changed (a source directory holding a change).

Marks propagate to dependants. Then:

- a root that reaches a mark means the change can reach any target through
  toolchain resolution or the configuration: the scope is everything;
- a root that is a source file counts only when it changed itself;
- otherwise the selected targets are restricted to the marked ones.

The analysis snapshot is off in a scoped process: it plans fewer targets
than its patterns name.

**Argument.** Take a target T selected at the head and not marked there.

- Nothing in T's main-repository loading closure L(T) is in a package with
  rule changes or loads a changed `.bzl` file, and no source in L(T)
  changed.
- So every BUILD file, glob result, transitively loaded `.bzl` file and
  source file in L(T) is identical at the base. Hence L(T) is identical
  there, every `select()` branch included, so a configuration-dependent
  choice cannot reach anything outside L(T). Visibility and package groups
  are edges too.
- What analysis adds beyond the loading graph comes from four places. All
  of them are unchanged:
  - the configuration: rc and options are global if they differ;
  - external repositories: MODULE.bazel, the lockfile and repository
    inputs are global;
  - toolchain resolution and execution platforms: every registered
    toolchain, platform and flag-named target is a root, and a root
    reaching a change means everything;
  - transition outputs: the literal roots.
- T's actions, and therefore its digest, are equal at both revisions.

The same argument from the base shows the other direction. A head target
that is marked and existed at the base was marked at the base too, with
the same change set. So comparing the head's scope with the base's scoped
digests misses nothing, and a missing base digest means the target is new.

The assumptions, beyond those of §7:

- an external repository's target depends on a main-repository target only
  through a label the root module's files name (MODULE.bazel and the `.bzl`
  files above are roots), not through a label its own code builds;
- a repository rule reads main-repository files only through such labels,
  the lockfile's records or the manifest names. Files it finds by path
  under a directory it was given count only through the lockfile
  `DIRENTS`/`DIRTREE` records;
- an analysis-time label in a `.bzl` file without `transition(` is an
  attribute value, so it is in the loading graph.

A change that breaks these is reported conservatively only if it also
touches a root. Otherwise it would be missed.

### 8.3 Orchestration

- **Order.** The base is planned with the scope, then the head. If either
  revision's scope comes out as everything (a root), the other is planned
  entirely too.
- **Failures.** A scoped planning that fails runs once more, which absorbs
  transient errors such as a registry 502. If it fails again, it is
  planned entirely, with its scope only reported, and the comparison stays
  within the head's scope. The analysis of a subset never decides the
  answer by failing. A planning is successful only when it exits 0 (§4,
  Hook); any other exit, `--keep_going` analysis failures and skipped
  patterns included, is a failure. A head that fails fails `affected`
  (non-zero exit, nothing on stdout). So does a base, unless
  `--base_failure=all` asks for TD's default: every head target (within
  the head's scope) is affected.
- **`--[no]concurrent_revisions`** (`=auto`, the default since §8.9). Both
  revisions are planned at once, sharing the cache: the repository store's
  stage lock serializes a shared fetch. The head is still planned in place
  when the working copy is HEAD (§8.8), else in a second worktree. It
  starts once the base selected its targets (its `.scope` file), so the
  base is past module resolution and loading and fetches first. Peak
  memory is about twice a revision's (2 × 9.2 GB for a global change,
  2 × 5 GB scoped). By default (`auto`) a **local** change is planned
  concurrently when the host has 20 GB available (`MemAvailable`;
  `BAZEL_RS_AFFECTED_CONCURRENT_MB` overrides it, 0 = always), otherwise
  one revision after the other (`bazel-rs affected: planning the revisions
  one after the other (N MB available, M MB for both at once)`). A global
  change stays sequential unless `--concurrent_revisions` asks: its
  revisions' repository specifications differ (docs/WORKTREES.md, hazard
  3: avoid it for a branch that bumps the Go SDK).
- **Stored digests.** `--write_digests=FILE` (about 230 KB) plans the head
  entirely and writes its digests. The file has `#commit` and `#key`
  lines, where the key is the sha256 of the binary, command, options and
  patterns. `--base_digests=FILE` then skips the base when the commit and
  key match, and plans it otherwise. A main-branch job would run
  `affected --base=HEAD^ --write_digests=main.digests` and upload the file
  as an artifact; a PR job downloads the file of its base (§8.11 does the
  same per target through the remote cache, without a separate job). Digests no
  longer depend on the cache location: the cache prefix of
  repository-store trees and of `<cache>/repositories/external/` is
  dropped with the tree identity.

**The script.** `bin/build-changed-files` (via `tools/build`) calls
`target-determinator -filter-incompatible-targets=false <base>` today. The
replacement is:

```
bazel-rs --cache-dir=$HOME/.cache/bazel-rs affected --base=<base> \
    [--noconcurrent_revisions] [--base_digests=main.digests]
```

It prints one label per line. Since bwllaming/code#13177 the script drops
every manual target from the `rdeps` set itself, so the default's omission
of manual targets is harmless there. `bin/push-changed-containers` passes
`--include_manual` (§4).

### 8.4 Digest fixes found by this work

- **Path-length-independent value digests.** Planned actions were digested
  as postcard bytes with the root normalized afterwards. Postcard's length
  prefixes kept the root's length, so two checkouts at paths of different
  lengths never agreed. That covered a dirty working copy planned in place
  against the worktree (every target reported), and the two worktrees of
  `--concurrent_revisions`. Values are now hashed by a serde serializer
  that normalizes each string or byte string and frames it with its new
  length (`value_hash.rs`). Digest time is unchanged (≈ 14 s for 57,941
  actions).
- **Args `map_each` functions.** Before, a deferred Args expansion
  (rules_js `js_image_layer`'s `layers#1`) could only be serialized after
  the analysis snapshot had described its function. Snapshot off (every
  scoped process) or before that description, the action's deferred part
  degraded to a constant: its content was not digested. Now the function
  is digested by identity: file, name, span, and the repr of its defaults
  and captured values. With the snapshot on or off the digests are equal
  (57,941 actions, 0 fallbacks). A change of the function's *body* that
  keeps its span is not seen (§7 already excludes `.bzl` code from
  digests).

### 8.5 Measurements

All runs used the private clone (`git clone git@github.com:bwllaming/code`),
the real `.bazelrc` with the storagebox overrides, and
`--repo_env=JAVA_HOME=…`. Each cold run had its own fresh cache, with
nothing shared between runs. Runs were one at a time, under `nice -n 10`,
on a shared 64-core machine. Network variance is ±10 %: one cold base took
722 s instead of 545 s.

**bazel-rs `affected`, cold.** Every set below equals the full-analysis
set of the main binary:

| pair | scope | before: main, full + full | now | `--concurrent_revisions` | stored base digests | peak RSS before / now |
|---|---|---|---|---|---|---|
| P4 `ff579ddf8d^→ff579ddf8d` (Go test file), 1 affected | 1 of 2479 | 813 s (542 + 268) | **104 s** (94 + 13) | 91 s | 88 s | 9.2 / 4.2 GB |
| P3 `bccb9427→80859617` (`.bazelrc` + Go), 4 | 4 | 823 s | **291 s** (190 + 113) | 219 s | – | 9.1 / 4.2 GB |
| P1 `470886f0→7ab6d35d` (BUILD + testdata), 20 | 20 | 1001 s | **312 s** (240 + 120) | 227 s | – | 9.2 / 4.2 GB |
| P2 `e7b42879→9e6b547e` (lockfile: global), 53 | full | 827 s | 800 s | **528 s** | **537 s** | 9.2 / 9.2 GB (2 × with concurrent) |
| P5 `65dd621332^→65dd621332` (BUILD edit), 3 | 16 | 809 s | **96 s** (84 + 9; 783 s before the §8.6 fix) | 905 s | – | 9.2 / 4.2 GB |

A scope's head costs its forward closure's analysis. P1's and P3's
`wcb_test` pulls in a Python closure whose `py_library` commits take
≈ 100 s even warm (86.7 % of that planning). The cold base of a scoped run
fetches only what its closure needs: 4–14 GB instead of 40.

**bazel-rs `affected`, warm-ish.** These are second runs in the same cache:
the scoped processes keep no snapshot, while full runs reuse their
repositories and snapshots.

| pair | cold | warm |
|---|---|---|
| P4 | 97 s | 22 s |
| P1 | 300 s | 196 s |
| P2 | 805 s | 510 s |

**Genuine Bazel 9.3.1 + target-determinator v0.34.0 (baseline).** The
user approved this for this measurement only, on the private clone. The
setup:

- client/server mode with a private `--output_user_root` and its fresh
  repository cache;
- `-bazel` pointed at a shim that adds the storagebox overrides to every
  command taking build options;
- `-filter-incompatible-targets=false -nocache_results`;
- `shutdown` after each run.

"Warm-ish" is the second run with the server restarted, on the same output
base and repository cache, so all analysis is redone. Peak RSS is server
plus clients, sampled every second.

| pair | cold | warm-ish | peak RSS cold / warm | TD set |
|---|---|---|---|---|
| P4 | 269 s | 84 s | 10.4 / 10.1 GB | 1 |
| P3 | 267 s | 82 s | 12.1 / 10.1 GB | 4 |
| P1 | 259 s* | 83 s | 11.7 / 10.1 GB | 31 |
| P5 | 259 s | 87 s | 12.6 / 10.4 GB | 3 |
| P2 | 270 s | 99 s | 15.2 / 12.9 GB | 75 |

\* The first cold P1 run lost the base cquery to a `proxy.golang.org` TLS
handshake timeout. TD then reported all 18,002 targets
(`ignore-and-build-all`); the repeat is shown.

Genuine Bazel's cold `deps(//...)` cquery takes 185 s including the fetch.
The head's cquery reuses the server's Skyframe (≈ 12 s), so TD pays about
one cold analysis. Compared with bazel-rs `affected`:

- a scoped bazel-rs run is faster cold for small changes (P4: 104 vs
  269 s) and comparable for P1/P3 with `--concurrent_revisions`;
- for global changes bazel-rs is about 2× slower. Its cold full analysis
  (≈ 540 s) is twice genuine's;
- warm, genuine's analysis is ≈ 70 s against bazel-rs's ≈ 195–260 s.

**The sets.**

- TD on genuine Bazel equals TD on bazel-rs exactly, for P1–P3 (the TD
  runs of §4).
- Native `affected` equals TD for P3, P4 and P5.
- For P1 and P2 it is a subset, explained in §4: the manual `image_*`
  targets (11 and 20), `//src/python:pytest` (a `test_suite`) and
  `//src/python:bworker_env` (an action-less `py_library`).

**A hazard found on the way.** The prototype's default worktree,
`<git-common-dir>/bazel-rs-affected`, broke genuine Bazel's `//...` in the
same checkout. Genuine Bazel loaded `.git/bazel-rs-affected/src/ts/...`,
where rules_js's `npm_link_all_packages` fails. The default is now under an
explicit `--cache-dir` (§4).

### 8.6 Fixed: an analysis-order bug in subsets

Analysing exactly
`//src/ts/bport:_entrypoint_amd64 //src/ts/bport:app //src/ts/bport:app_next`
at 65dd621332 failed with `internal analysis order error:
@@platforms//os:linux`. That was plain `build --nobuild` on main, with no
`affected` involved; `//...` worked. P5's scope (16 `bport` targets) hit
it, and the orchestrator then planned both revisions entirely (783 s).

**Cause.** A Starlark implementation that asks for the default host
target platform (`ctx.target_platform_has_constraint`, or a genrule's
`$(TARGET_CPU)`) gets it in phase 2: the NEEDS_TARGET_PLATFORM retry
resolves the platform there. That analyzed the platform's tree into a
throwaway plan, so its nodes (`@platforms//os:linux` in the build
configuration) were memoized in phase 1's `configured` map but never
planned. When this happened inside an early flush (a toolchain's
`config_setting` with a Starlark `flag_values` key, `plan_pending`), the
rest of phase 1 could still reach those nodes. Here a later toolchain's
`exec_compatible_with` alias `@zig_config//:exec_os` did. The memo
answered, the alias was planned, and its phase 2 found no planned actual.
Larger target sets happened to analyze the constraint value first.

**Fix.** Phase 2 now walks the plan from the engine
(`Engine::planning_plan`), and `Engine::deferred_target_platform`
appends the tree to it. The tree is planned right after the target that
asked for it, before any later consumer. Regression test:
`tests/platforms.rs` `a_platform_tree_analyzed_by_phase_two_is_planned`
(three targets and an alias to `@platforms//os:linux` in a toolchain's
`exec_compatible_with`). On that workspace genuine 9.3.1 builds
successfully (`has=True`, `toolchain=impl`) and main failed with the
same error. On the clone, the action dump of `build --nobuild //src/...`
is byte-identical before and after (6,121,706 lines). P5 cold is now
scoped: 96 s, the same 3 targets, 4.9 GB cache, 4.2 GB peak RSS.

### 8.7 `--include_manual` and the first CI run (0.4.1 → this revision)

Private clone, release binary of this branch, one shared private cache
(`aim-cache`; the first run cold), the storagebox overrides, `nice -n 10`.

| pair | change | TD | native default | native `--include_manual` | differences to TD |
|---|---|---|---|---|---|
| CI `cf770c2c89 → d4ff4d0c57` (use-bazel-rs) | `bin/bazel`, workflows, `bin/push-changed-containers`, `bin/bazel-cache.test.sh` deleted, `src/python/tools/build/__init__.py` | 0 (inferred) | 0 of 2 scoped (68 s cold); 0 of 1,923 with `--scope=full` (778 s) | – | none |
| P1 `470886f0 → 7ab6d35d` | `wcb/BUILD.bazel` + testdata | 31 | 20 | 29 (scope 31 of 18,571; 252 s) | `monolith:image_image_index_json`, `monolith:image_push_index_index_json` |
| P2 `e7b42879 → 9e6b547e` | lockfile (global) | 75 | 53 | 69 (full; 1,182 s, 14.4 GB) | `//src/python:pytest`, `//src/python:bworker_env`, 4 × `…_index_json` |
| throwaway `9e6b547e → 56faa52c0f` | `image_push(repo = …)` of `monolith` only | `image_push` (inferred) | 0 of 8 | **`monolith:image_push`** (1 of 22; 39 s) | none |

- **The CI run.** Every planning of 0.4.1 logged `(exit 1)` (`test
  --nobuild`'s "Unable to run tests") and was accepted: the result was
  right but a failure would have looked the same. The changed files are in
  no target: `bin/BUILD.bazel` only exports `bw-devenv*` and
  `bwork-runner*`, no glob of `src/python/BUILD.bazel` covers
  `tools/build/` (it has no BUILD file), and the workflows are not
  referenced. The deleted `bin/bazel-cache.test.sh` changes `bin`'s file
  list, so `bin`'s dependants are the scope: `bw_devenv_test` and
  `bwork_runner_test`, both unchanged. The full analysis agrees (0 of
  1,923). TD hashes only rules' attributes and the files they consume, so
  it reports nothing either (not rerun).
- **The remaining differences** are all action-less targets:
  `…_index_json` are rules_oci's `directory_path` targets (no actions, no
  files: the same empty digest at both revisions; their `…_index_json_cp`
  copies are reported), `bworker_env` a `py_library` without actions, and
  `pytest` a `test_suite` whose tests are reported. Every other TD label is
  reported, and nothing beyond TD's set.
- **A macro argument.** Changing only `image_push`'s `repo` in
  `src/beyondwork.ai/bworker/cmd/monolith/BUILD.bazel` (a local commit in
  the clone) is reported only with `--include_manual`; the push target is
  what `attr(tags, dockerpush, …)` then selects.

### 8.8 The head planned in place, saved for the commands after `affected` (ci-pipeline-perf)

~/code's CI builds the affected targets' reverse dependencies and tests
their tests right after `affected` (`bin/build-changed-files`), in the
working copy whose HEAD `affected` just analyzed. With genuine Bazel the
three commands share one server; here each re-analyzed (~150 s each).

* Without `--head`, a working copy whose `git status --porcelain
  --untracked-files=normal` is empty *is* the head revision: it is planned
  in place instead of checked out in the worktree (`BAZEL_RS_AFFECTED_HEAD_IN_PLACE=0`
  keeps the worktree). Ignored files are the working copy's; the commands
  after `affected` see them too (an ignored file a glob matches can only
  make a target affected, never hide one). Planned concurrently (§8.3), it
  stays in place.
* That planning (`BAZEL_RS_AFFECTED_SAVE` set) saves its analysis snapshot
  under a key of its own (the scope's spec folded into G: a scoped planning
  is never looked up, its scope is only known after its target pattern
  phase) with the derivation record of docs/ANALYSIS_SNAPSHOT.md "Derived
  hits": the `build` of the affected targets and the `test` of their tests
  derive their plans from it. Plannings in the worktree save nothing.
* Planned after the base (sequentially) on a cache where the working copy
  has no hidden lockfile yet, the head's starts as the base planning's
  (without the results naming the worktree); its extension results are
  validated against their recorded inputs as always. (Through 0.4.7 a
  planning that wrote its hidden lockfile was never saved; since §8.9 the
  snapshot records the entries it used instead.)
* Since §8.9 this serves ~/code's CI `build` too: the `query` between
  `affected` and `build` no longer invalidates the snapshot by writing the
  hidden lockfile, and the head's `test --nobuild` plan traverses the
  source directories an executing build checks (docs/ANALYSIS_SNAPSHOT.md
  "Derived hits"). Warm: the `build` of the 480 labels analyzes nothing
  (`INFO: Analyzed 480 targets (206 packages loaded, 0 targets
  configured)`), 128 → 30 s. Where the head planning cannot be saved (it
  evaluated a module extension, e.g. `pip` after a requirements bump, or
  downloaded registry files), the `build` analyzes in full and saves, and
  the `test` derives from the `build`.

### 8.9 Cold, lukewarm and warm CI cost (affected-cold-speed)

~/code's CI (PR bwllaming/code#13252) runs `bazel affected --base=HEAD^`,
then `query rdeps(...)`, then the `build` of the affected targets on
self-hosted runners whose bazel-rs cache is often cold (a runner's first
run) or lukewarm (main moved since its last run). There `affected` took
695–900 s cold (base 494–638 s: loading ~150 s, analysis 350–480 s; head
200–260 s), the most expensive step left.

**Where the time went** (private clone at the PR's merge `4445d0f`, base
`2c7b638`; scope 482 of 2,504 targets; `BAZEL_RS_TIMING=1`):

* **Analysis CPU.** A scoped planning spent ~105 s in `plan_span`, 63 s of
  it committing `py_library` targets: rules_python precompiles every `.py`
  of every wheel (~20,000 `PyCompile` actions here), and each action's
  inputs include the 1,657 files of the Python runtime, so the commit's
  per-input passes ran over 33 M inputs: the shared-group check listed and
  validated each action, `plan_action` listed and validated it again,
  `source_paths` copied and hashed every path, `SourceInputs::planned`
  hashed and compared them again, and `planted` parsed every input's owner
  label. Now the second listing and validation are skipped, the source
  inputs are derived without hashing when planning dropped no source,
  the execution-path deduplication hashes the validated bytes (Fx), and an
  artifact knows whether its owner is external: commit 70 → 42 s, the
  planning 124 → ~96 s (action dumps and digests byte-identical, 15,228
  actions). Registration (`compute`, 31 s: flattening, validating and
  keying the same inputs) and phase 1 are unchanged.
* **Base and head.** The two plannings fetch the same repositories (a
  local change changes no MODULE/lockfile), and the head reused the base's
  fetches, but each analyzed its scope on its own: ~110 s each warm. They
  now run at once by default (§8.3): wall time is the slower of the two.
* **Fetching** (cold). The base's loading takes 41 s instead of 3 s
  (module resolution and the repositories `//...` loads), its analysis
  ~180 s instead of ~90 s (the coordinator waits ~35 s on fetches; the
  remainder is fetch and extraction contention). The configured analysis
  fetches only the `select()` branches it takes (4–16 GB, not the query's
  37–50 GB). Not changed here.
* **Lukewarm.** After a requirements bump on main every wheel repository of
  the `pip` extension group is re-executed from the download cache (a
  tree's identity covers its extension group): ~270–500 executions, ~25 s
  per planning. Not changed here.
* **The build after `affected`** analyzed again (115 s): fixed (§8.8).

**Measured** (one run each, `nice -n 10`, shared 64-core host; the exact CI
sequence `affected --base=HEAD^`, `query --keep_going --query_file ...
--output streamed_jsonproto`, `build --keep_going --target_pattern_file
... --build_event_json_file ...` with the storagebox replaced by a local
HTTP cache; cold = fresh cache, lukewarm = cache of the same sequence at
main `513556c7c` (three dependency bumps older), warm = cache of the same
commit; affected's output identical in every run):

| cache | 0.4.7: affected / query / build | this revision | 
|---|---|---|
| cold | 391 (base 256 + head 133) / 131 / 168 = **690 s** | 222 (concurrent: 218 ∥ 179) / 138 / 141 = **501 s** |
| lukewarm | 276 (147 + 129) / 26 / 165 = **467 s** | 123 (120 ∥ 114) / 23 / 141 = **287 s** |
| warm | 236 (113 + 122) / 15 / 128 = **379 s** | 104 (86 ∥ 96) / 17 / 30 (derived) = **151 s** |
| lukewarm, `--noconcurrent_revisions` | – | 254 (149 + 102) / 22 / 138 = 414 s |

Peak RSS per planning is unchanged (4.7–5.0 GB scoped); concurrent
`affected` holds two (~10 GB). P1 (20 targets) and P2 (53, global) give the
same sets as 0.4.7; P2 (full + full, sequential) 757 → 420 s on a
lukewarm cache.

**The head's snapshot on a cold or lukewarm cache.** The `build` derives
from `affected` only when the head planning was saved. On a cold cache it
is not (it evaluates the module extensions and downloads registry files),
and after a dependency bump it re-evaluates `pip`
(`extension-evaluated`), concurrent or not. A repository rule's downloads
no longer keep it from saving (docs/ANALYSIS_SNAPSHOT.md: they settle with
the execution's warm re-probe). Sequential planning does not help there
(the lukewarm row: the head re-evaluates `pip` too), so concurrency stays
the default: it is never slower than sequential by more than the build's
analysis, and saves the head's whole planning.

**On CI** (an extrapolation, not measured there) the runners are 1.5–2×
slower than this host (0.4.7's head 200 vs 133 s, base 494 vs 256 s): cold
`affected` should take about the base's planning alone, ~400–450 s instead
of 695–900 s; warm ~150–200 s, plus a derived `build` when the head was
saved. What remains: fetching (cold), the lukewarm re-executions, and
registration and phase 1 of the analysis (~60 of ~96 s per planning here).

### 8.10 The head saved on cold and lukewarm caches (affected-reuse-cold)

§8.8's derived `build` needed `affected`'s head planning saved, which a cold
or lukewarm cache prevented: it evaluated module extensions (all of them
cold; `pip` whenever the cache's hidden lockfile predates a requirements
bump), fetched registry files and archives, and — with those taints already
recorded — never re-probed its repository rules either. Now such a run is
saved as the warm run would be (docs/ANALYSIS_SNAPSHOT.md, "A cold run is
saved as the warm run would be"): an evaluated extension is re-probed as the
next run's lockfile reuse and confirmed against the lockfiles the command
writes, registry files whose hashes MODULE.bazel.lock names and published
registry/Git trees settle, and a pointer absence superseded by its
publication (also the concurrent base planning's) is dropped. `pip`'s print
on every evaluation keeps an exact-key save out (`extension-output`), not the
scoped head's, whose snapshot only serves derived hits. And a lukewarm `pip`
re-evaluation re-executes only the wheels whose own specification changed
(docs/MODULE_EXTENSIONS.md: generated repositories are keyed by their own
specification, as genuine's repository contents cache keys them).

Measured (private clone at PR 13252's merge `4445d0f`, the exact CI sequence
`affected --base=HEAD^` → `query … --output streamed_jsonproto` → `build
--keep_going --target_pattern_file …` → `test --nobuild --keep_going
--test_output=errors --test_summary=terse --local_test_jobs=4
--test_tag_filters=-nightly --target_pattern_file …` (an analysis-only
stand-in for the CI `test`), a local HTTP remote cache, `nice -n 10`, one run
each; lukewarm = the same sequence run first at main `513556c7c` with the same
binary; affected's output identical in every run; seconds):

| cache | 0.4.8 (13a7f714): affected / query / build / test | this revision |
|---|---|---|
| cold | 247 / 160 / 305¹ / 9 | 228 / 127 / 147 / 9 |
| lukewarm | 121 / 27 / 134 / 5 | 118 / 18 / **67 (derived)** / 3 |
| warm | 102 / 16 / 32 (derived) / **86** | 100 / 16 / 32 (derived) / **3 (derived)** |

¹ with an empty remote cache (later rows: populated); both cold builds
analyze in full.

* Lukewarm: the head planning is saved and the build derives (`INFO:
  Analyzed 480 targets (206 packages loaded, 0 targets configured)`); the
  `query`, which visits every `select()` branch, executes 7 repositories
  instead of 287 (the re-evaluated `pip`'s unchanged wheels are reused; the
  build's own fetches 46 → 3).
* Warm, and every row: the `test` after a derived `build` derived too only
  once a hit's INPUTS refresh kept the DERIVE section (before, the refresh
  dropped it and the test analyzed in full, 86 s).
* Cold: the head is saved now, but the build still analyzes in full: the
  `query` between them builds sdists of other platforms with `pip`, which
  writes `pip/__pycache__` into rules_python's published `pypi__pip` tree;
  the derived lookup sees that tree changed (`does not match its manifest
  (extra pip/__pycache__)`) — correctly, the full analysis repairs the tree.
  Not changed here; fixed by sealed trees (below).

**Sealed trees (published-tree-writes).** Published repository trees are
sealed now (docs/REPOSITORY_SLOTS.md, "Sealed trees"): no file or directory
has a write bit, so `pip`'s build subprocesses, importing `pip` from the
`pypi__pip` tree without `-B`, skip the bytecode they cannot write, and the
tree stays as published. Measured on the same private clone at `4445d0f`,
same sequence, cold `--cache-dir` (the snapshot.debian.org blobs seeded into
its download cache identically for every run: the archive answered 503 over
IPv6 that day), a populated local HTTP remote cache, `nice -n 10`, one run
each, seconds:

| binary | affected | query | build | test |
|---|---|---|---|---|
| main `f7d0c36e` (0.4.9) | 220 | 111 | 137 (`37854 targets configured`) | 3.6 |
| sealed trees | 213 | 113 | **52 (derived: `0 targets configured`)** | 2.6 (derived) |

affected's output, the query's output, the build's and the test's stdout
and the build's completed events (480 targets, all successful) are
identical; the derived build does not print the analysis-time `DEBUG` /
`WARNING` lines of rules_python's full analysis (as every derived build).
After `affected` and `query`, main's store had one tree written into — the
`pypi__pip` tree, 353 entries below `pip/**/__pycache__` — and nothing
else (no file of any tree changed after its publication, no other extra
entry, in any tree area); the sealed store had none, and no command printed
a permission error.

### 8.11 Base digests in the remote cache (affected-remote-digests)

On ~/code's CI every PR plans its base (`HEAD^`, a main commit) again,
although many PRs share a base, and the base costs as much as the head
(PR 13252 on 0.4.11: base 228 s — 53 s loading and scope, 175 s analysis of
525 targets — and head 178 s, 233 s wall concurrently). The CI already has
a remote cache (`--remote_cache` in `.bazelrc`, uploads on). `affected` now
writes every base digest it computes there and looks them up before it plans
a base: write-through, no separate job, no file to pass around.

**Modes.** `--affected_remote_digests=off|read|readwrite`, default
`readwrite` (the design: write-through from every job). `read` looks up and
never writes; `off` does neither (the flow of §8.3 unchanged).
`--affected_remote_digests` alone means `readwrite`, `--noaffected_remote_digests`
`off` (and `=true|yes|1`, `=false|no|0`). The mode alone decides: Bazel's
`--remote_accept_cached` and `--remote_upload_local_results` govern action
results, not these entries.

**Recommended CI setup.** PR jobs `--affected_remote_digests=read`; main
and merge-queue jobs (trusted code) `readwrite`, with `--head=HEAD` so that
they write their head's digests too (below). The flag only keeps honest
jobs from writing: if untrusted PRs ever get credentials for the cache,
server-side ACLs (authenticated writes, e.g. a write-only token for the
trusted jobs) are the real protection. A bad batch (a writer's bug) is
retired without a release by a new epoch:
`--affected_remote_digests_epoch=STRING` or `BAZEL_RS_AFFECTED_DIGESTS_EPOCH`
(the flag wins) enters the key, so every entry written under another epoch
is ignored.

**Flow** (a remote cache configured, mode not `off`, no `--base_digests`
file that holds the base):

1. The head is planned first (in place or in its worktree, scoped as
   before).
2. Once its target pattern phase wrote the scope (`<digests>.scope`, now
   renamed into place whole), the base digests of the scope's targets are
   looked up. A head that is not scoped (a global change, a dirty working
   copy, `--scope=full`) or whose scope came out as everything is looked up
   once it is planned, for all its targets.
3. All hit: the base is neither checked out nor planned.
   `bazel-rs affected: base digests: 525 of 525 from the remote cache`.
4. Otherwise the base is planned for the missing targets only (`only`
   lines in its scope input; with a local change the change's scope is
   still computed there, so a root that reaches the change at the base
   still makes everything the scope and the head is planned entirely, as
   before). It starts as soon as the lookup is done when both revisions
   are planned concurrently (§8.3), after the head otherwise. A failed
   base planning runs once more, then entirely, as before.
5. In `readwrite` mode its digests of the asked targets are written
   (`absent` for an asked target the base does not select).
6. In `readwrite` mode the **head's** digests (the compared ones: its
   scope's, or all) are written too, under its own tree's key, when the head
   is a commit planned in a clean worktree — `--head=REV`, or
   `BAZEL_RS_AFFECTED_HEAD_IN_PLACE=0` — never when the working copy is
   planned in place (§8.8): its ignored files can feed globs, and they are
   not the commit's. So the newest main commit has entries for the PRs
   based on it. ~/code's main job runs `affected --base=HEAD^` in its
   checkout, which plans the head in place: it writes head digests only
   with `--head=HEAD` added, at the cost of §8.8's in-place snapshot for
   the commands after it (a main job that only writes can afford that; one
   that builds after `affected` keeps the in-place head and writes base
   digests only).
7. A checkout (base or head worktree) whose `git submodule update` failed
   may lack files its tree names: the run still plans and compares, but
   writes nothing (`... nothing is written to the remote cache in this
   run`).

The comparison is the one without the remote cache: the head's scope (or
all its targets) against the base digests, hits and planned ones merged.
Two PRs writing the same entries write the same bytes.

**Why the head's scope suffices.** §8.2's argument is per revision: a head
target outside the head's scope reaches no change, so its loading closure,
and with it its digest, is identical at the base. The base's own scope check
adds one thing: a root that reaches the change at the base but not at the
head. The roots are the same labels at both revisions (MODULE.bazel, rc
files and extension `.bzl` files are global changes) except the literals of
a changed `.bzl` file with `transition(`; such a file is a changed `.bzl`,
so every package loading it — every target whose analysis could use the
transition — is marked at the head already. A planned base still runs the
check (step 4); only an all-hit run skips it.

**Keys.** The key of a revision's entries is the sha256 of the framed
`bazel-rs-affected-remote-key-v2`, the commit's **tree**
(`git rev-parse REV^{tree}`: an identical tree reached through another
commit shares the entries), the workspace's directory in the repository,
the `--write_digests` key of this binary (its sha256), the command, the
startup options, options and patterns (with `--include_manual`) — without
the cache location (`--cache-dir`) and the remote cache's transport and
policy options (`--remote_cache`, `--remote_header`,
`--[no]remote_upload_local_results`, `--[no]remote_accept_cached`,
`--credential_helper`, `--disk_cache`, TLS, timeouts, retries: they change
no digest, and a read-only PR job shares a writer's entries) — the client
state outside the tree, and the epoch. The client state: the OS and
architecture (`std::env::consts`), what the checkout's filters do to the
commit's content, and the
options of the rc files outside the workspace that the planning reads
(system, home, `$BAZELRC`, `--bazelrc` and their imports), by command and
option text — not by the files' paths — without the options that change
no digest (the repository and disk caches' locations and policies,
console and progress output, resource limits, report files, remote
download modes, the transport options). The workspace's own rc files are
the tree's.

**Checkout filters.** Only what can change the checked-out bytes of the
revision's tree is keyed, as a normalized description (printed by the key
debug): for each filter driver the commit's `.gitattributes` files,
`$GIT_DIR/info/attributes` or `core.attributesFile` name (`filter=NAME`),
LFS as one fact — `lfs smudged` (a `filter.lfs.process` or `.smudge`
configured, not skipped by a true `GIT_LFS_SKIP_SMUDGE`, its command
available: an absolute executable that exists, else on `PATH`; `git lfs …`
needs `git-lfs`) or `lfs not smudged` — whatever the configuration's
spelling (`git-lfs filter-process` or a `smudge`/`clean` pair, the
executable's path, `required`); another driver by its `smudge` and
`process` commands without the executable's directory, or `unconfigured`;
`no filters` when the tree names none. `clean` and `required` change no
checkout, and a driver no attribute names does nothing. 0.4.13 keyed the
raw `git config --get-regexp '^filter\.'` output, which differs per runner
(system, global and `actions/checkout`'s local configuration): PR 13252's
runners ci02-02 and ci03-02 agreed on every other component and still
missed (`0 of 530`).

**Location independence.** Nothing in the key depends on where the
checkout, its worktrees, the cache or the rc files lie: the workspace's
location (as given and canonical) is normalized to a token in every
startup option, option, outer rc option and the filter configuration (the
digests normalize it too), the cache location and the worktree paths never
enter, and an outer rc file is keyed by its options. **Client variables are
not keyed**: the target digests key an inherited variable
(`--action_env=NAME`, `--test_env=NAME`) by name only, so a reader computes
the same head digests whatever their values; a variable a repository rule
reads (`--repo_env=NAME`) reaches the digests through the repository's
contents. 0.4.12 keyed the values of every variable any rc line named for
inheritance, whatever its config: ~/code's `test:replay --test_env=PATH
--test_env=KUBECONFIG ...` (a config the command does not use) put direnv's
per-runner `PATH` into the key, so every runner had its own entries (PR
13252: `0 of 530` on runner ci02 after ci02-01 wrote 530). `--include_incompatible`
only filters the output: not keyed.

**Diagnosis.** `BAZEL_RS_AFFECTED_KEY_DEBUG=1` prints the key's components
to stderr: `bazel-rs affected: key: base: tree|below|binary|options|environment|epoch|key ...`
(the options line names the startup options and options keyed, not their
values), `key: environment: host <os> <arch> checkout: <the checkout
filters' description>` (and `key: head: host ...` for the head's), one `key: environment: rc
<file>:<line> <command> <option name> <digest>` per keyed outer rc option
and the count of unkeyed ones, and `key: head: tree ... key ...` for head
digests written. Two runs whose `key: base: key` lines differ show which
component did. No option or variable value is printed.

**Trust: why a key gap only over-reports, and what remains.** A reader
compares its head's digest with an entry a writer computed for the base. A
gap in the key means the writer's world differed from the reader's (another
host tool, another environment). The entry then holds the base's digest *in
the writer's world*; the reader reports the target unaffected only when its
head digest equals that entry, i.e. when the reader's head graph — every
action, input content and producer, in the reader's world — is exactly the
writer's base graph. A world difference that reaches the target's actions
therefore makes the digests differ (reported affected: the safe
direction), unless the PR's change exactly undoes the difference (an edit
that sets explicitly what the writer's environment supplied). What a
digest never sees, keyed or not, stays unseen as in §7: host tools found
by path (`/usr/bin/...` outside the plan's inputs), tests that are not
hermetic or never cached (`external`, `no-cache`, flaky), anything a
target reads at run time. Those are risks of the digests themselves; the
remote entries only extend them across machines. The new risk is
**persistence and trust**: an entry decides "not affected" — no build, no
test, for `push-changed-containers --include_manual` no push — for every
PR on that base until it is evicted or the epoch changes, so a bad writer
(an untrusted job, or an honest writer's bug) is amplified. Hence `read`
for PR jobs, `readwrite` for trusted ones, the epoch to retire a batch,
write-skipping after a failed submodule checkout, and server-side
authenticated writes where untrusted jobs hold cache credentials.

**The record format.** Per target, one Action Cache entry whose key is the
sha256 of the framed `bazel-rs-affected-digest-v1`, base key and label
(`size_bytes` the length of those bytes). It digests no `Action`, so
genuine Bazel never asks for it. Its `ActionResult` (exit code 0) has one
output file `bazel-rs-affected-digest` whose blob — uploaded to the CAS
first, and inlined in the entry's `contents` — is:

```
bazel-rs-affected-digest-v1
entry<TAB><the entry's Action Cache key, hex>
label<TAB><label>
value<TAB><the target digest (64 hex digits) | incompatible | absent>
check<TAB><sha256 of the four lines above, newlines included>
```

About 500 bytes per target (525 targets: 590 KB in AC and CAS together).
A lookup (`GetActionResult`; gRPC asks for the file inlined with
`inline_output_files`, else it reads the blob from the CAS) is used only
when the output file's digest matches its bytes and the record's header,
entry key, label, checksum and value form all check out. Anything else is a
miss (counted: `N malformed remote base digest(s) ignored`), planned and
written again. A failed request (the cache down) stops the remaining
requests: they are misses, and the planning answers as without the cache.
A failed target's digest is never written.

**Processes.** The orchestrator has no remote cache of its own: a *helper*
— this binary, with `affected`'s startup options, command and options, in
the working copy, under `BAZEL_RS_AFFECTED_REMOTE_OP` — sets the cache up
exactly as a planning would (rc files, credential helpers, TLS), then makes
the requests (16 at a time) instead of running the command. One probes
before the head starts (no remote cache: the flow of §8.3 unchanged), one
looks up, one writes the base's and one the head's digests.

**Tests** (`tests/affected.rs`): an HTTP cache — miss, all hit (one
planning), head digests written from a worktree and hit as the next
commit's base, none from a working copy in place, the same tree through
another commit, a partial hit (sequential), a new target (`absent`), other
options, another epoch (flag; the variable), another LFS smudging (another
key each), `read` (never writes), `readwrite` with
`--noremote_upload_local_results --noremote_accept_cached` (still reads and
writes), corrupt entries (another label's record, garbage), the cache
down, no cache; a failed submodule update (planned and compared, nothing
written); two runners (clones at different absolute paths, their own
homes, caches, worktrees and `~/.bazelrc` naming their own caches, a
per-runner `PATH` under a `test:replay --test_env=PATH` line): the second
hits everything the first wrote, with the same `key: base: key`; a gRPC cache with a global
change — miss, hit, entries without inlined contents (read from the CAS),
an evicted record (a miss). Every run's output equals the run without the
remote cache.

**Measured** (private clone at PR 13252's merge `b707657f`, base `5b415ab9`;
`affected --base=HEAD^` with the CI's rc, a local HTTP cache, `nice -n 10`,
one run each; warm = the bazel-rs cache of a previous run; PR-B = the same
base plus another edit of `utils/str/str.go` and one of `utils/periodic`,
scope 552 targets, 499 of them shared with PR 13252's; CPU = user + sys of
all processes, peak = the summed RSS of the process tree; seconds):

| run | wall | CPU | peak RSS | base planned |
|---|---|---|---|---|
| cold, without (`--noaffected_remote_digests`) | 193 | 493 | 9.2 GB | 525 targets |
| cold, miss (computed, 525 written) | 184 | 499 | 9.2 GB | 525 |
| cold, all hit (fresh bazel-rs cache) | 187 | 400 | 4.9 GB | – |
| warm, without | 52 | 243 | 8.5 GB | 525 |
| warm, miss | 44 | 185 | 9.0 GB | 525 |
| warm, all hit | 42 | 94 | 4.8 GB | – |
| warm PR-B, without | 44 | 158 | 8.0 GB | 552 |
| warm PR-B, partial hit (499 of 552) | 40 | 124 | 4.8 GB | 53 |

The output is identical in every run (516 of 525 resp. 547 of 552
affected). Here concurrent plannings make the wall time the head's: a hit
saves the base's CPU and memory (half of each) more than time; on a cold
cache the head now fetches what the base fetched before. On the CI, where
the base took 228 s and the head 178 s (started 53 s later, after the
base's loading), an all-hit run should take about the head's planning with
its own loading (~210 s instead of 233 s) on half the CPU and memory, and
much less on a runner short of memory (§8.3: sequential below 20 GB
available, where the base's whole planning is saved). The build after
`affected` still derives from the head (`INFO: Analyzed 3 targets (5
packages loaded, 0 targets configured)` after an all-hit warm run and after
a cold miss).
