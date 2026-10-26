# `shutdown`, `clean`, `fetch`, `vendor`, `help`

Bazel 9.3's ShutdownCommand, CleanCommand and FetchCommand mapped to the
native layout (pinned source `/tmp/opencode/bazel-9.3-reference`:
`runtime/commands/{ShutdownCommand,CleanCommand}.java`,
`bazel/commands/{FetchCommand,FetchOptions,TargetFetcher,RepositoryFetcher}.java`,
`bazel/bzlmod/BazelFetchAllFunction.java`). Code: `src/main.rs`
(`shutdown_command`, `clean_command`, `fetch_repositories`, `help_command`,
the dispatch in `run`), `src/output_base.rs` (the output base lock, `clean`),
`RepositorySet::{fetch_repository_name, repository_defined,
all_repository_names}` in `src/repository.rs`,
`repository_store::set_force_fetch`. Tests: `tests/cmd_clean_fetch.rs`
against the genuine capture `tests/fixtures/cmd_clean_fetch/reference_receipt.json`
(re-capture with `python3 tests/fixtures/cmd_clean_fetch/capture.py`: scratch
workspace, HOME and `--output_user_root` under `/tmp/opencode`, `--batch
--ignore_all_rc_files`, an `http_archive` served by a loopback HTTP server),
unit tests in `src/output_base.rs`.

## The native output base

`<cache>` is the native cache (`--cache-dir`, else `<workspace>/bazel-rs-out`
or, in a linked git worktree, the main checkout's; docs/WORKTREES.md). Its
`<cache>/worktrees/<name>-<hash>/` is the workspace's state directory, the
native output base (`bazel-rs info output_base`, docs/INFO.md):

| entry | what |
|---|---|
| `execroot/` | the output tree (`bazel-bin`, ...; links into the shared action cache) |
| `snapshots/` | the analysis snapshots |
| `tests/index/` | the last test result key per test |
| `test-run/` | execution roots of `run` of tests |
| `MODULE.bazel.lock` | the hidden lockfile |
| `lock` | the output base lock (below) |

Everything else in the cache is shared by every workspace using it — the
repository store (`repositories/`: downloads, registry and Git trees,
repository rule publications), the action cache and its CAS (`actions/`,
`cas/`), test results (`tests/<key>/`), the digest memo — like Bazel's output
user root (`<output_user_root>/cache/repos`, the install base).

### The output base lock

Bazel's client takes the output base lock for every command
(blaze_util_posix.cc AcquireLock). bazel-rs: every command that uses the
state directory (`build`, `test`, `run`, `query`, `fetch`) holds
`<state>/lock` **shared** for its whole run — concurrent native commands of
one workspace stay allowed, as before — and `clean` holds it **exclusive**.
So `clean` waits for the running commands of its workspace and a command
started during a `clean` waits for it. The waiting process prints the holder
once, like Bazel:

```
Another command holds the output base lock: 
pid=1234
owner=clean
cwd=/path/to/workspace
Waiting for it to complete...
```

(an exclusive holder writes `pid=`/`owner=`/`cwd=` into the lock file; shared
holders cannot, so `clean` waiting for them prints `a bazel-rs command using
the output base <state>`), and retries every 500 ms. A lock file deleted or
replaced while a process waited (`clean --expunge`) is opened again
(StillExists). A command on a cache directory that does not exist yet takes
no lock: a command that fails early must create nothing (and a `clean`
meanwhile finds nothing to delete). `info` takes no lock (it creates
nothing), nor does
`mod graph` (shared store only). `--noblock_for_lock` is a startup option the
native client does not model, so commands always wait. Another worktree's
commands never take this workspace's lock: `clean` never touches their
state.

## `shutdown`

bazel-rs has no server, so there is nothing to stop: `shutdown` exits 0 and
prints nothing — what genuine Bazel does when no server runs. Its option
`--iff_heap_size_greater_than=N` (an int) is accepted; residue fails with
`ERROR: Unrecognized arguments: x` and an unknown option with `ERROR: --foo
:: Unrecognized option: --foo`, both exit 2 (as genuine). It runs outside a
workspace too (`mustRunInWorkspace = false`). Options of rc files are
ignored (rc `common` lines apply to every command; nothing else changes
anything). Divergence: genuine `--batch` prints `WARNING: Running command
"shutdown" in batch mode. ...`; the native client never claims a batch mode.

## `clean [--expunge] [--async | --expunge_async]`

| | genuine 9.3 | native |
|---|---|---|
| `clean` | deletes everything below `<output_base>/execroot` (outputs, the execroot's workspace links), clears the action cache and in-memory state (Skyframe), removes the `bazel-*` convenience links | deletes everything below `<state>/execroot` (the output tree and its record) and every other per-workspace state — `snapshots/`, `tests/index/`, `test-run/` — keeping the hidden lockfile and the lock |
| `--expunge` | deletes the whole output base (external repositories, action cache, hidden lockfile, ...), stops the server | deletes the whole state directory |
| `--async` / `--expunge_async` | renames the execroot / output base to `<name>_tmp_<pid>_<uuid>` next to it, prints `INFO: Output tree moved to <path> for deletion` / `INFO: Output base moved to <path> for deletion`, deletes it in a daemonized `rm -rf` | renames and prints the same, then deletes it in the foreground (the command returns when it is gone) |

Output as genuine (stderr): `INFO: Starting clean (this may take a while).
Use --async if the clean takes more than several minutes.` (synchronous) or
`INFO: Starting clean.` (`--async`), then the moved line; exit 0.
`clean x`: `ERROR: Unrecognized arguments: x`, exit 2 (Starlark options are
accepted and ignored). `clean` inherits the build options and rc `build`
lines (`inheritsOptionsFrom = BuildCommand`). A deletion failure is
`ERROR: <path>: <error>`, exit 36. Like every Bazel command, `clean` has an
output base: it creates the state directory if missing (to hold its lock).

Deletion never follows a symlink and handles read-only trees: a directory
lacking owner `rwx` gets it before its entries are removed (Bazel's `find
-type d -not -perm -u=rwx -exec chmod u=rwx`); files are never chmod-ed —
the output tree's regular files are hardlinks of the read-only outputs in the
shared action cache (one inode), and unlinking needs write access to the
directory only.

**What is not deleted, and why.** Genuine `clean` drops the action cache,
which lives in its output base, so the next build executes every action
again. The native action cache, its CAS, the repository store and test
results are shared by every worktree using the cache (docs/WORKTREES.md);
deleting them for one workspace would silently cost every other worktree a
full rebuild and refetch, and race their running builds. So the next build
after a native `clean` (or `--expunge`) relinks its outputs from cache hits:
`bazel-rs: 0 actions executed, N cache hits`, where genuine reports `N
processes` (the receipt's `build_after_clean`). Repositories are likewise
kept by `--expunge` (genuine re-extracts them from its repository cache
without downloading, also 0 downloads in the receipt). To drop the shared
store, delete the cache directory (with no command running).

The `bazel-*` convenience links: bazel-rs never creates them, so `clean`
leaves any it finds alone (they belong to genuine Bazel's output base).

## `fetch`

FetchCommand inherits the test (and build) options and rc `build`/`test`
lines. Exactly one of: target patterns, `--repo` (repeatable), `--all`,
`--configure`; none = `--all`. Otherwise `ERROR: Only one fetch option can
be provided for fetch command`, exit 2. `--nofetch`: `ERROR: You cannot run
fetch with --nofetch`, exit 2.

* `fetch TARGET...` is TargetFetcher: a `build --nobuild` of the patterns —
  loading and analysis demand exactly the repositories a build would, no
  action executes, nothing is linked into the output tree, no outputs are
  reported on stdout and no native summary line is printed. Success: `INFO:
  All external dependencies for the requested targets fetched
  successfully.` A failure: `ERROR: Fetching some target dependencies failed
  with errors: <detail>`, exit 2 (divergence: genuine prints the loading
  error events before it, and its detail omits the `no such package 'X':`
  prefix native keeps).
* `fetch --repo=@apparent` maps through the main repository mapping,
  `--repo=@@canonical` is taken as is; anything else is `ERROR: Invalid repo
  name: The repo value has to be either apparent '@repo' or canonical
  '@@repo' repo name` (also an invalid name), exit 8. Unknown names are
  collected: `ERROR: Fetching some repos failed with errors: No repository
  visible as '@x' from main repository; Repository '@@y' is not defined`,
  exit 8 (a canonical name is defined when it is a selected module, a
  `use_repo_rule` repository or generated by its module extension, which is
  evaluated to find out). Success: `INFO: All requested repos fetched
  successfully.`
* `fetch --all` (or no argument) is BazelFetchAllFunction: every repository
  of the resolved module graph except the main one, then every repository of
  every module extension (each evaluated or reused from the lockfile).
  `--configure`: only the repositories of repository rules declared
  `configure = True`. Success: `INFO: All external dependencies fetched
  successfully.`
* A failing repository is `ERROR: <its error>`, exit 8 (genuine prints the
  rule's error event first). The first failure stops native `--all`.
* `--force` (RepositoryDirectoryValue.FORCE_FETCH): no warm publication is
  reused in this command, so every repository rule it materializes executes
  again (downloads still hit the download cache, as genuine's repository
  cache does). Divergence: registry and Git module sources are
  content-verified trees and are not extracted again (genuine re-runs their
  `http_archive`); module extensions are not re-evaluated (neither does
  genuine).

Repositories are materialized exactly as a build demands them: into the
shared repository store (`<cache>/repositories/`, reused by every worktree;
docs/REPOSITORY_SLOTS.md). The lockfile is written like a build's (on
success). `fetch` neither looks up nor saves an analysis snapshot (a hit
would skip the repositories). Native `--all` materializes the repositories
one after another (no parallel speculation).

On the receipt's scratch workspace genuine `fetch --all` itself fails on
the `rules_jvm_external` maven repositories of the built-in module closure
(coursier cache paths of the capturing host), so the receipt pins `--repo`,
`--configure` and target patterns; `--all` is covered natively
(`fetch_all_configure_and_force`, explicit-module profile).

## `sync`, `vendor`, unknown commands

`sync` was removed from Bazel with WORKSPACE support: genuine 9.3 prints
`Command 'sync' not found. Try 'bazel help'.` and exits 2; native the same
with `'bazel-rs help'` (any name that is no Bazel command). `vendor`
(`--vendor_dir`: copies fetched repositories into a directory later builds
read instead of the repository store) is not implemented: it fails
explicitly, naming the command, exit 1. Bazel commands that the native
profile lacks (`cquery`, `aquery`, ...) keep `unsupported command or startup
option`.

## `help`

`help` prints the native usage index; `help COMMAND` prints the command's
usage line, Bazel's one-line summary and the native profile's notes; an
unknown topic lists the commands (exit 2). Minimal by design: no option
listings, no byte parity with genuine `help`.
