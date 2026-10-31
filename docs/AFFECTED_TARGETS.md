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
    [--concurrent_revisions] [--write_digests=FILE] [--base_digests=FILE]
    [--base_failure=error|all]
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
  sorted, one per line. The planning output goes to stderr.

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
- **`--concurrent_revisions`.** Both revisions are planned at once, in two
  worktrees sharing the cache: the repository store's stage lock
  serializes a shared fetch. Peak memory is about twice a revision's (2 ×
  9.2 GB for a global change, 2 × 4.2 GB scoped). It is not the default,
  because CI hosts run four runners each. Avoid it for a branch that bumps
  the Go SDK (docs/WORKTREES.md, hazard 3).
- **Stored digests.** `--write_digests=FILE` (about 230 KB) plans the head
  entirely and writes its digests. The file has `#commit` and `#key`
  lines, where the key is the sha256 of the binary, command, options and
  patterns. `--base_digests=FILE` then skips the base when the commit and
  key match, and plans it otherwise. A main-branch job would run
  `affected --base=HEAD^ --write_digests=main.digests` and upload the file
  as an artifact; a PR job downloads the file of its base. Digests no
  longer depend on the cache location: the cache prefix of
  repository-store trees and of `<cache>/repositories/external/` is
  dropped with the tree identity.

**The script.** `bin/build-changed-files` (via `tools/build`) calls
`target-determinator -filter-incompatible-targets=false <base>` today. The
replacement is:

```
bazel-rs --cache-dir=$HOME/.cache/bazel-rs affected --base=<base> \
    [--concurrent_revisions] [--base_digests=main.digests]
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
