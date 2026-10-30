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
one analysis per revision instead of TD's two cqueries per revision. The
analyses run in one git worktree, so the second one can be incremental, and
nothing is checked out in place. It prints TD's output format: one label
per line.

On three ~/code commit pairs, its result agrees with TD's, with every
difference explained (§4):

| situation | time |
|---|---|
| TD today (bazel-rs cquery backend) | 7.4–14.1 min |
| prototype, both revisions fully analyzed | 8.6–9.4 min |
| prototype, head incremental | 6.2 min |
| prototype, both snapshot hits | 0.5 min |

The big win comes from the next phases (§6):

- keep each main commit's target digests (or its snapshot), so a PR run
  analyzes only its head, often incrementally;
- plan the CI's `build`/`test` from the same head analysis.

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
   reports those that became incompatible). A base revision that fails to
   plan makes everything affected, as TD's default does.

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
    [--include_incompatible] [BUILD OPTIONS...] [PATTERNS... | -- PATTERNS...]
```

**What it does.**

- It resolves the revisions in the current repository and checks out the
  base in a worktree, `--worktree` or `<git-common-dir>/bazel-rs-affected`.
  The worktree is created or repaired as needed, with `checkout -f` and
  `clean -ffdx`.
- It runs this binary with the startup options (including `--cache-dir`
  and `--native-lockfile`):

  ```
  test --nobuild <options> -- <patterns>
  ```

  `//...` is the default pattern. `BAZEL_RS_TARGET_DIGESTS` names the
  digests file.
- It then checks out the head in the same worktree and plans again. That
  run is an incremental analysis or a snapshot hit when possible. Without
  `--head`, the head is `HEAD`. A working copy with uncommitted changes is
  planned in place instead (its paths are normalized).
- It prints the affected labels in TD's spelling (`//pkg` for `//pkg:pkg`),
  sorted, one per line. The planning output goes to stderr.

**Hook.** Any `build`/`test`/`run` with `BAZEL_RS_TARGET_DIGESTS=<file>`
writes `label<TAB>digest` lines (header `bazel-rs-target-digests-v1`). It
does so for a full analysis, a hit or a splice; the variable is read after
planning and is no snapshot input. `test --nobuild` exits 1 as Bazel does
("Couldn't start the build. Unable to run tests"); the orchestrator
accepts a run whose digests were written.

**As a drop-in for the script.** In `src/python/tools/build/__init__.py`,
replace

```
target-determinator -filter-incompatible-targets=false <commit>
```

with

```
bazel-rs affected --base=<commit> <the storagebox overrides>
```

The output contract is the same. One script detail matters:

- TD also reports **manual** targets. A plain `build`/`test //...` never
  plans them, so the prototype does not.
- The script's `rdeps(//src/..., set(X)) except attr(tags, 'manual',
  set(X))` removes only the manual targets *in X*. With the prototype's X,
  manual rdeps such as the `oci_push`/`image_*` targets would then be
  built.
- So use `… except attr(tags, 'manual', //src/...)`. With TD's X the result
  is identical: every manual target in `rdeps` is affected, hence in X.
  Alternatively, a phase-2 `--include_manual` plans the manual targets too.

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

**Without a restored cache.** The prototype is 2 analyses (≈ 8.5 min on
~/code) against TD's 4 cqueries: about 1.4–1.7× faster on the slow pairs.
It never touches the checkout (worktree), and its answer is more precise.

**Phase 1 (this branch).** The prototype command and the digest hook, as a
drop-in with the script tweak above.

**Phase 2.**

1. **Per-commit digests (c).** The main-branch CI run uploads
   `head.digests` keyed by `(commit, bazel-rs version, flags, patterns)`,
   and PR runs pass `--base_digests=<file>`. Only the head is analyzed:
   ≈ 4.3 min cold, seconds to 2 min with a restored bazel-rs cache.
2. **Restore the bazel-rs cache** (repositories, digest memo and the
   worktree state) between CI runs. With the snapshot of the previous main
   commit, the head is incremental. P1: 118 s; a typical
   one-package edit: 5–20 s, docs/INCREMENTAL_ANALYSIS.md.
3. **Persist the action digests in the snapshot.** A hit then needs no
   digest work (12 s → < 1 s), and a splice recomputes only the dirty
   owners' closure.
4. **`--include_manual`.** Plan `attr(tags, manual, PATTERNS)` explicitly,
   for exact TD parity without the script tweak.
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

- **Manual targets** are not planned (§4).
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
  the cost of one TD cquery. Its memory (≈ 8.7 GB peak for `//...`,
  docs/INCREMENTAL_ANALYSIS.md) was not re-measured here.
- **Version alignment.** bazel-rs must report the lockfile's Bazel version
  (9.3.1 for ~/code, the other worker's branch). Otherwise every analysis
  rewrites the lockfile and is never saved.
