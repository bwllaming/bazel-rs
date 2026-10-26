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

* A tool that learns a slot's path at run time (not from a label, its
  arguments, environment or working directory) and writes there later is not
  covered: into a mutable slot unlocked, or through an ordinary slot's link
  into its tree (the stat fingerprints catch that: the tree is republished).
* Build actions writing into slots are not covered.
* The cache's garbage collection (docs/CACHE_GC.md) removes unused trees
  with their pointers, slot links, markers and mutable slots; kept trees keep
  the trees their `refs` name.
