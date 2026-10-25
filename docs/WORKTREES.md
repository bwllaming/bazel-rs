# Git worktrees: one cache per repository

Code: `src/worktree.rs` (layout, state directories, path mentions),
`src/main.rs` (the default cache), `src/repository_ctx.rs` and
`src/repository_store.rs` (workspace-dependent repositories). Tests:
`tests/worktrees.rs` (real `git worktree`s, the default cache), unit tests in
`src/worktree.rs` and `src/repository_store.rs`. Options report:
/tmp/opencode/perf-research/WORKTREES.md.

## The default cache

Without `--cache-dir`, the cache is found from the git metadata above the
workspace root, read with plain file reads (no git process, nothing written):
the walk goes up to the first `.git` entry.

| layout | default cache |
|---|---|
| a main checkout (`.git` directory) | `<workspace>/bazel-rs-out`, as before |
| a linked worktree (`git worktree add`: a `.git` file `gitdir: X` whose `X/commondir` names the common directory) | the main checkout's: `<main>/<the workspace's path below the git root>/bazel-rs-out` |
| no git, a bare repository's worktree (no main checkout), a submodule or `--separate-git-dir` checkout (a `.git` file without `commondir`), malformed or unreadable metadata, a main checkout that lacks the workspace directory (added on the worktree's branch) | `<workspace>/bazel-rs-out` |

The main checkout is the parent of a common directory named `.git`, or the
common config's `core.worktree`. Relative `gitdir`/`commondir` values
(`git worktree add --relative-paths`), CRLF line ends and a symlinked `.git`
are understood. Every path is canonical: a worktree reached through a symlink
is the same worktree. `--cache-dir` still overrides the default; worktrees
given the same `--cache-dir` share it the same way.

A linked worktree that still has the cache it used before
(`<worktree>/bazel-rs-out`, a directory) gets one line on every command:
`bazel-rs: <worktree>/bazel-rs-out is no longer used: this git worktree
shares the cache of its main checkout, <cache>; delete <worktree>/bazel-rs-out
to free its space`. It is never moved (its trees embed its own path) or
deleted.

## Shared and per-worktree state

Shared by every workspace of a cache (content-addressed or keyed, safe across
processes): repositories (downloads, registry and git trees, repository rule
publications), actions and their CAS, test results (`tests/<key>/`), the
embedded tools, the digest memo (`digests/`, path-keyed, bounded at 2^20
records), execution roots of actions and tests (unique names).

Per workspace, in its state directory `<cache>/worktrees/<name>-<hash>/`
(`<name>`: the workspace directory's name, ASCII letters, digits, `.`, `_`,
`-` kept, others `_`, at most 40; `<hash>`: the first 8 bytes of the SHA-256
of its canonical path, hex), whatever chose the cache:

| | why per workspace |
|---|---|
| `snapshots/` — the analysis snapshot store, its logs, `mismatched/`, `exe.id` (docs/ANALYSIS_SNAPSHOT.md) | the key names the workspace anyway; each worktree keeps its own LRU budget (16 snapshots / 1 GiB) |
| `tests/index/` — the last test result key per test (docs/TEST_COMMAND.md) | each worktree is its own output base; a test it never ran accepts the shared result of its exact key, so switching worktrees reruns nothing |
| `test-run/` — execution roots of `run` of a test | concurrent `run`s of one test in two worktrees no longer delete each other's TEST_TMPDIR |
| `MODULE.bazel.lock` — the hidden lockfile (docs/REGISTRY_LOCKFILE.md) | Bazel's `<output_base>/MODULE.bazel.lock`; seeded from the main checkout's |
| `execroot/_main/` — the output tree (docs/INFO.md) | `bazel-rs info bazel-bin`, `execution_root`, `output_path`, `bazel-testlogs` name the workspace's own builds: another worktree's build of another commit never relinks them (the linked outputs themselves are the shared action cache's) |

`bazel-rs info output_base` is the state directory — like Bazel's
`<output_user_root>/<md5 of the workspace>`, one per workspace — while the
store is shared like Bazel's output user root (`info repository_cache` is
`<cache>/repositories/downloads`). The state directory of a deleted worktree
stays until removed by hand (no garbage collection yet).

A cache of the flat layout before per-worktree state is not migrated: its
`snapshots/`, `hidden-lockfiles/`, `test-run/`, `tests/index/` and
`execroot/` are no longer used (an older binary sharing the cache still uses
them) and can be deleted. The hidden lockfile is the exception: the flat
layout's one seeds the workspace's own, so no extension is evaluated again.

## A new worktree's first build

* Repositories are reused when their recorded inputs validate against the
  worktree's files (a repository reading `//:seed.txt` that differs on the
  worktree's branch runs again).
* Actions hit, except the runfiles manifests and trees, whose keys hold
  absolute source paths (on ~/code 3 of 689 oauth2token actions).
* Tests are `(cached)` when another worktree ran the same key.
* The analysis snapshot misses once (its own store), then saves and hits.
* The hidden lockfile is seeded from the main checkout's: reproducible
  extensions are not evaluated again unless their entry fails validation or
  names the main checkout's path (paths into the shared cache excepted).

## Repositories that depend on the workspace's path

A non-local repository rule depends on where the workspace is when it read
`repository_ctx.workspace_root`, wrote a file (`ctx.file`, `ctx.template`)
naming the workspace's path, or left a link into the workspace. Paths into
the cache are no such dependence, also when the cache lies inside the main
checkout (`go.env`'s `GOPATH` into the store, links into other repositories'
trees). A mention counts at a file-name boundary (`/a/code-wt1` does not name
`/a/code`). Such an execution records the canonical workspace: in its
identity and in a version-3 probe that validates only in that workspace
(older binaries never trust it), and its publication gets the workspace's
own specification pointer `repositories/direct/specs/<spec>.ws-<hash>`
instead of the shared one. Each worktree executes the rule once and keeps
its publication; switching worktrees re-executes nothing and changes no
pointer another worktree's snapshot read. Lookups follow the shared pointer
first, then the workspace's.

Local rules (`local = True`: `local_repository`, `new_local_repository`),
`local_path_override` modules and `file://` registries inside the worktree
are per worktree anyway (never reused, or resolved in place).

Not detected: a program run by `ctx.execute` that writes the workspace's path
into the repository, or downloaded content naming it — Bazel's contract for
reproducible repositories forbids depending on the workspace path, and on
~/code no publication embeds `/home/coder/code`. Publications made by older
binaries carry no workspace probe and are reused as before.

## Concurrency and known hazards

Safe across processes, measured with concurrent builds of the same and of
different commits: the per-key action lock with a re-check, the repository
store's stage lock with a re-probe, atomic pointer, manifest, snapshot,
test-index and hidden-lockfile writes, per-key test locks, unique work
directories (`worktrees_build_the_same_and_different_commits_concurrently`).

Not fixed yet (later phases of the options report):

1. **Pointer flips.** A repository whose recorded inputs differ between
   branches (it watches `//pnpm-lock.yaml`, a `//thirdparty` patch, ...)
   has one shared pointer: alternating between such worktrees re-executes it
   on each switch, and the switch is a snapshot miss. Different versions
   (attributes) are fine: distinct specifications.
2. **Extension repositories are keyed by their whole group**: one changed
   `go.mod` entry or lockfile line makes every repository of that extension
   run again once per variant.
3. **Go stage hazard.** A branch that bumps the Go SDK, gazelle or `go_env`
   executes `go_repository_cache` once for its new specification, at the
   same stage path every specification uses as its live `GOPATH`; a
   concurrent build in another worktree can lose or contaminate module
   downloads meanwhile (docs/REPOSITORY_ENVIRONMENT.md). Do not build such a
   branch for the first time while another worktree builds, or give it its
   own `--cache-dir`.
4. Two `run`s of one test in the SAME worktree still share its execution
   root (as before).
5. **No garbage collection** of the shared store, nor of the state
   directories of deleted worktrees; the digest memo grows with the
   worktrees' source files up to its bound.
6. Shared actions keep their first owner in the key: on another branch a
   different owner re-runs such an action once (time only).

The main checkout's cache must stay where it is: published trees (`go.env`,
links into the store) embed its path. Deleting or moving the main checkout
removes the cache of all its worktrees; they then fall back to their own.

## Compared with Bazel 9.3

Bazel keeps one output base per workspace (`<output_user_root>/<md5>`, with
its own `external/` and action cache) and shares only the repository cache
(downloads) and, for repositories marked reproducible, the repo contents
cache; `--disk_cache` shares action results. bazel-rs shares its whole store
between the worktrees of one repository and keeps per workspace only what
names the workspace: closer to an output base per worktree over a shared
repo contents cache plus disk cache, with no server and no GC.
