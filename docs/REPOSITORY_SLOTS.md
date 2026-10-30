# Repository directories (slots)

Implementation: `src/repository_slots.rs` (locks, binding, markers, the
store-path scan, copies), `src/repository_store.rs` (`materialize_inner`,
`try_reuse`, `replay_local`), `src/repository_ctx.rs` (readers around
`execute`, link replanting), `src/repository.rs` (friendly paths),
`src/source_links.rs` (no links through a slot). Tests:
`tests/repository_slots.rs` against genuine captures
(`tests/fixtures/repository_slots/`, `capture.py`), the unit tests of both
modules. Design brief: /tmp/opencode/perf-research/REPO-SLOTS-DESIGN.md.

## Behaviour (as genuine)

Bazel 9.3 fetches a repository in `<output_base>/external/<canonical name>`
(RepositoryFetchFunction.java:158-160). Natively a repository rule runs in its
slot `<cache>/repositories/external/<canonical name>`: `repository_ctx.path(".")`,
`execute`'s working directory and every path a tool derives from them are the
same in every process, invocation and worktree sharing the cache, with
`external` as the parent directory — gazelle's go_repository mode spells
`${SRCDIR}` as `external/<name>` then (language/go/generate.go:130-144), so a
cgo `-L${SRCDIR}/` (the duckdb Go bindings) is valid from the execution root.
Before, rules ran in private stages `direct/stages/<blake3>`, and the flag
was `-L../<32-hex stage>` (TEST-SURVEY L2).

## Publication

* Ordinary repository: after the rule, the slot directory is renamed into the
  content-addressed tree `direct/trees/<identity>` and replaced by the link
  `../direct/trees/<identity>` (genuine moves a reproducible repository into
  its contents cache and leaves a link, LocalRepoContentsCache.java:157-185).
  Relative paths another rule derives from its own directory
  (`../<name>/...`) resolve through it as in genuine's `external/`.
* Self-referencing repository — its published files or link targets contain
  its own slot path (a store-path scan rides on the snapshot's SHA-256 read;
  on ~/code only gazelle's `go_repository_cache`, whose `go.env` names its own
  directory as GOPATH): the slot stays a real, mutable directory; a copy
  (fresh single-link files) is published; the marker `direct/slots/<slug>`
  (a fixed-format text record: magic line, identity, canonical name) records
  that the slot is complete. The manifest says `"self_ref": true`; `refs`
  lists other store trees the bytes name (a GOROOT, absolute links), for a
  future collector.
* Written-into repository — another repository's `execute` names, in its
  arguments or environment, a path below this ordinary repository's
  published tree that the tree does not have (gazelle HEAD's relocatable
  `go.env`: go_repository_tools runs `env -i GOCACHE=<dirname of the cache
  repository's go.env>/gocache ... go build`, internal/env.bzl
  resolve_go_env, internal/common.bzl env_execute). Genuine's repository is
  a plain directory under `external/` that the tool writes into and that is
  checked by its marker and recorded inputs only, never by its contents:
  the go build cache survives, the next tools re-run is incremental (capture
  `cache_dir_reuse`). Natively the tree must stay as published (every reuse
  and the analysis snapshot verify it; before, the build cache written into
  it made the next lookup miss and re-execute the repository empty). So
  before the command runs, the slot becomes a mutable copy of the tree
  (`restore_mutable`, exact: its marker must name this publication, else it
  is made again) and every occurrence of the tree's root in the command's
  arguments, environment and working directory is replaced by the slot's
  path (genuine's `external/<name>`); the command holds the slot as a
  reader. Later commands naming the tree are redirected the same way while
  the marker names it; a re-execution of the repository replaces the slot
  (`fresh`), as genuine's re-fetch does. The manifest is unchanged (class
  ordinary): the slot is never an input, the repository is reused by its
  tree and recorded inputs as before, and an unlocked `bind` never replaces
  the marked directory. gazelle (docs/BENCHMARKS.md, `rule/BUILD.bazel`
  edit): 12.7–14.1 s → 5.0–5.6 s, the following no-op 4.7–5.7 s (a miss) →
  0.38–0.47 s (a hit). ~/code (gazelle 0.51.3, self-referencing `go.env`)
  never takes this path.
* `repository_ctx.path(Label(...))` still names the published tree (pinned,
  the analysis snapshot validates trees): a documented divergence from
  genuine's `<output_base>/external/<name>/...`.
* Links out of the slot keep what they resolved to from there: a relative one
  becomes absolute; one into a sibling repository's slot that links its tree
  is pinned to that tree. Links inside become relative, as before.
* `<cache>/repositories/external/<name>/rel` is the RepoCacheFriendlyPath
  `@@<name>//rel` (toRepoCacheFriendlyPath, :1656-1704).
* A canonical name longer than NAME_MAX (255 bytes; genuine fails with "File
  name too long") gets the slot `#long-<digest>`.

## The bazel_tools slot

Genuine's `<output_base>/external/bazel_tools` is a link to the install
base's `embedded_tools` (no marker; capture
`tests/fixtures/bazel_tools_external/reference_receipt.json`), so a rule's
relative `../bazel_tools/...` names the embedded tools — ~/code's
`build --repo_env=JAVA_HOME=../bazel_tools/jdk` is the bundled JDK for
rules_jvm_external's coursier (`rctx.path(JAVA_HOME + "/bin/java")`,
`java -jar coursier.jar`) and rules_java's `@local_jdk`
(local_java_repository links the JDK's entries; `--java_runtime_version`
still selects the runtime, e.g. `remotejdk_11`). Natively, before every
repository rule runs (built-in profile), the slot `external/bazel_tools` is
bound to the embedded tree: the link `../builtins/trees/<bundle identity>`
(`RepositorySet::bind_builtin_tools_slot`, `repository_slots::bind_to`, the
ordinary binding protocol). A slot already linking the current bundle is left
as is without demanding `@bazel_tools`; otherwise the repository is demanded
(the speculation sentinel propagates; other failures, e.g. no bundle for the
platform, leave the slot as it was). The bundle keeps its modes (the JDK runs
on glibc hosts: `libz.so.1` and the C library come from the host, as for
genuine's). Links into it (`@local_jdk`'s) are pinned to the tree like a
sibling's. The cache collection keeps the link while its tree exists and
sweeps it dangling; the next repository rule binds it again. The execution
root still has no `external/` (docs/INFO.md).

## Locks (all flocks, `direct/locks/`)

* Writers — an execution, a local rule's replay, remaking a missing mutable
  slot — take `slot-<slug>.gate`, then `slot-<slug>` exclusively and release
  the gate; they delete the marker first (RepositoryFetchFunction.java:311)
  and write it last (DigestWriter.java:76-89).
* Readers — another repository's or extension's `execute` — hold, for the life
  of its subprocess only, every self-referencing repository a label of the
  execution resolved into and every real directory among the slots its
  arguments, environment or working directory name (never its own, never one
  this thread writes): gate shared, slot shared, gate released; all or
  nothing in slug order, nothing held while waiting. A waiting writer holds
  the gate, so a stream of readers cannot starve it. A resolved
  self-referencing slot that is missing, a link or an interrupted writer's is
  made again from its tree under the writer lock first.
* Binding on reuse takes no lock: a link renamed into place, which never
  replaces a real directory (a writer's execution or a mutable slot); a
  marker-less leftover directory is replaced only when its locks are free.
* Nothing is held between two `execute`s: holding a slot for a consumer's
  whole execution deadlocks across processes (go_repository reads `go.env`,
  then demands go_repository_tools, which reads `go.env` while another process
  re-executes the cache).

## Interplay

* The analysis snapshot names trees only; a slot is never an input (a hit is
  unaffected when another worktree re-points a slot), slot maintenance uses
  the raw filesystem. The audit (scripts/snapshot_fs_audit.py) classifies a
  replay's slot, locks and marker as `replay-slot`.
* Digest memo and action staging: `repositories/external/...` is no tree store
  (`Area::Transient`, never `REPOSITORY_TREE_STORES`).
* Source resolution never follows a link through a slot.
* Worktrees sharing one cache share the slots: an ordinary slot links whichever
  publication its last binder used (exact for one worktree); a
  self-referencing slot accepts any complete marker (no ping-pong between
  worktrees of different versions).

## Migration

The specification profiles were bumped once (`direct-repository-spec-v2`,
`direct-repository-spec-digest-v2`, `ordinary-repository-spec-digest-v3`):
publications an earlier binary made in a stage may embed the stage's path, so
every non-local generated repository runs once more per store; identical
content keeps its identity (no republication, action keys unchanged); only the
self-referencing ones and the repositories that read them (gazelle's
go_repository repos watch `go.env`) get new identities. The first
materialization of a process removes the earlier binaries' stages
`direct/stages/<slug>` whose stage locks are free (the 2.5 GB legacy GOPATH on
~/code); their lock files stay. Earlier binaries keep working on the same
store with their own pointers and stages.

## Limits

* A tool that writes below another repository's tree through an EXISTING
  path it was given (no missing path named, the slot not yet converted) or
  through a path it learns at run time still writes into the tree; the
  stat fingerprints catch that (the tree is republished). Conversely, a
  command naming a missing path below another repository only to probe it
  converts that repository (a copy of its tree; harmless otherwise).
* A tool that learns a slot's path at run time (not from a label, its
  arguments, environment or working directory) and writes there later is not
  covered: into a mutable slot unlocked, or through an ordinary slot's link
  into its tree (the stat fingerprints catch that: the tree is republished).
* Build actions writing into slots are not covered.
* The cache's garbage collection (docs/CACHE_GC.md) removes unused trees
  with their pointers, slot links, markers and mutable slots; kept trees keep
  the trees their `refs` name.
