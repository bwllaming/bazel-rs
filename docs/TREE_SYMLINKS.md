# Symlinks inside tree artifacts

A tree artifact (`ctx.actions.declare_directory`) may contain symlinks. Bazel
9.3 follows them; rules_oci 2.x depends on it: with
`bazel_features.rules.permits_treeartifact_uplevel_symlinks` (true from Bazel
7.1.0) its `image.sh` links every blob of an image layout relatively instead
of copying it (`blobs/sha256/<digest> -> ../../../<layer>.tar.gz`, and into
the base image tree's blobs), so every `oci_image` produces such a tree.
Natively these trees were rejected (`symlinks inside tree artifacts are not
supported by the native profile`), which failed every `oci_image` of ~/code.
Citations are to the pinned Bazel 9.3.0 sources
(`src/main/java/com/google/devtools/build/lib/`).

## Genuine semantics

* **The tree's value.** ActionOutputMetadataStore
  .constructTreeArtifactValueFromFilesystem (`skyframe/ActionOutputMetadataStore.java:245-316`)
  walks the tree with TreeArtifactValue.visitTree
  (`skyframe/TreeArtifactValue.java:548-592`): a symlink is stat'ed with
  FOLLOW and visited as the file or directory it resolves to, at its own
  (apparent) path; a directory reached through a link is walked through it.
  The regular files reached are the tree's children (TreeFileArtifacts);
  directories are not children. A child's metadata is its resolved file's
  (fileArtifactValueFromArtifact resolves the link and stats the real path,
  `:475-545`): a link and a copy of its target are the same value, the same
  digest, the same consumer key. Links may point anywhere: inside the tree,
  up-level out of it (to inputs, other outputs, sources), to absolute paths.
* **Permissions.** Every entry is chmod'ed to the output mode
  (0555) unless a link was traversed on the way to it
  (`chmod && !traversedSymlink`, `:263-272`): a link's target keeps its own
  mode, and a consumer sees it through the link (a link to a 0644 source file
  is not executable, a link to an output or to a file of the tree is).
* **Failures** (`reportOutputTreeArtifactErrors`,
  `skyframe/SkyframeActionExecutor.java:1554-1567`, then `<mnemonic> <tree>
  failed: not all outputs were created or valid`):
  `error while validating output tree artifact <tree>: child <path> is a
  dangling symbolic link` (the target is missing: `statIfFound` null),
  `child <path> has an unsupported type` (a link to a device, FIFO or
  socket, and such a file itself), and `[unix_jni.cc:382] <absolute path>
  (Too many levels of symbolic links)` for a link loop and for directory
  cycles (`sub/up -> ..`, `self -> .`), which the walk follows until the
  kernel's limit of 40 links per lookup.
* **Sandboxes.** A sandboxed spawn's outputs are moved into the execution root
  and its sandbox is deleted before the outputs are collected
  (AbstractSandboxSpawnRunner.runSpawn, `sandbox/AbstractSandboxSpawnRunner.java:138-178`): a relative link resolves in the
  execution root, an absolute link into the sandbox dangles. An unsandboxed
  spawn runs in the execution root itself, so an absolute link to `$PWD/...`
  resolves; its temporary directory is deleted (LocalSpawnRunner).
* **Consumers.** SpawnInputExpander (`exec/SpawnInputExpander.java:87-132,
  159-190`) expands a tree input into its children at `<tree>/<path>`, an
  empty tree into its directory. The processwrapper-sandbox stages each child
  as a symlink to its execution-root path (so a consumer reads through two
  links), an unsandboxed consumer sees the producer's own links, and
  `bazel-bin` shows them as the action made them.
* **Runfiles.** A runfiles tree links the tree directory itself
  (SymlinkTreeHelper); a spawn's runfiles (a test's) expand the tree into its
  children like any input.
* **Remote cache.** UploadManifest (`remote/UploadManifest.java:205-300,
  470-525`) uploads an output directory's relative links as SymlinkNodes and
  its absolute links as the files or directories they point to; an input
  tree enters the input root as its children's FileNodes (their resolved
  digests). No option of 9.3 changes any of this: `--allow_unresolved_symlinks`
  concerns `declare_symlink` artifacts, and the former
  `--incompatible_remote_symlinks`/`--incompatible_remote_dangling_symlinks`
  no longer exist.

## Native model

* `crate::action::read_tree` reads a tree in one of three modes (`TreeRead`):
  an **output** in its action's execution root right after the action
  (visitTree's semantics: links followed, entries not reached through a link
  made 0555 first, Bazel's three failures with their texts and the tree's
  root-relative path), a **published** tree (only regular files and
  directories, a link there is damage), and any **other tree input** (a
  source or repository directory: links followed). A linked file is read at
  its canonical path, so its content and executable bits are the resolved
  file's; a directory reached through a link is walked through it and
  recorded like a real one (`Tree::links` holds the links themselves).
* A link with an absolute target in the action's working directory dangles
  (genuine's sandbox and temporary directory are gone by then), except into
  the execution root of an unsandboxed action (genuine's own execution root).
* A directory cycle (a link leading into a directory it lies in) fails at
  the link that closes it, with the ELOOP text, where genuine recurses 40
  levels first (and, with two such links, explores exponentially many
  paths).
* All tree outputs are read before anything is published, and every regular
  output is normalized first: links may name inputs and other outputs of
  the action, and a link to another output shows its final mode.
* **Publication materializes links.** A native output lives in its action's
  own cache entry (`<cache>/actions/<key>/outputs/...`), where a relative link
  would resolve against the entry, not against an execution root holding its
  target: the published tree holds what each link resolved to — a file as
  its content-addressed blob (`cas/<digest>-<0444 | x>`, the resolved file's
  executable bits), a directory as a real directory. The tree's value (its
  files, digests and executable bits, its listing) is exactly what genuine's
  TreeArtifactValue holds, only the link-ness is gone.
* **Cache manifests** (`manifest.json`, the existing format) record a
  published file's mode when it is not 0555 (`modes`, an optional field): a
  manifest of a tree without such files is byte for byte what it was, and
  every existing entry stays valid. Reuse checks every file's exact mode
  against it; a link in a published tree, a changed mode or content rebuilds
  the entry.
* **Keys.** A tree's files are keyed by content and executable bits as
  before; link-ness is never keyed (a link and a copy key alike, as in
  Bazel). Keys of trees without links are unchanged.
* **Consumers** stage a tree's files as read-only store hardlinks like every
  native input (docs/HARDLINK_STAGING.md); `Args.add_all(tree)` expands to
  the same children as genuine's. `run` reads the published tree through the
  runfiles tree's link; a test's runfiles expand a tree into its files (an
  empty tree into its directory), like SpawnInputExpander
  (`crate::action::read_content_source`, `crate::test_runner`), which also
  makes trees usable as test data at all (before, a tree in a test's runfiles
  failed with `expected regular file`).
* **Remote cache.** Spawns with tree outputs stay local
  (`output_directories are not modelled`, docs/REMOTE_CACHE.md); a tree
  input's linked files enter the input root as files with their digests, as
  genuine's do.

## Evidence

`crate::action` unit tests cover the three read modes, the rules_oci shapes
and every resolving link, materialization and its manifests, damage found on
reuse, the validation errors (dangling, special, ELOOP, cycles, absolute
targets in the working directory) and links to other outputs of the action;
another reads a test's runfile sources (a file, a tree's files, an empty
tree, a tree through a symlink output).

## Known differences

* Published trees and staged inputs hold no symlinks: `readlink`, `find
  -type l` and `ls -l` see regular files (genuine: the producer's links in
  `bazel-bin`, symlinks to execution-root paths in a sandboxed consumer). The
  receipt keeps both unfollowed views as evidence.
* Declared divergence: a relative link to a file that is not an input or
  output of the action (e.g. an undeclared source) dangles natively — a
  native execution root holds only the action's inputs — while genuine
  resolves it in its execution root, which reaches every source and every
  output built so far. The native error adds the reason to Bazel's text.
* Directory cycles fail earlier, naming a shorter path (above).
* Native trees keep their empty directories (staged and published), as before;
  genuine's consumers see only the children.
* Link targets keep their executable bits but are read-only (0444/0555
  blobs), like every native input; genuine shows the target's own mode (a
  0644 source stays 0644).
