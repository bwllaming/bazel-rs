# `include()` in MODULE.bazel

Bazel 9.3.1 lets the root module and modules with a non-registry override
split their module file into segments: `include("//pkg:name.MODULE.bazel")`
behaves as if the segment were textually inserted at the call, except that
variable bindings stay in the file that makes them. Implementation:
`src/bzlmod.rs` (`include_statements`, `read_includes`, `include`,
[`IncludeScope`]); pinned source `/tmp/opencode/bazel-9.3.1-reference`,
`bazel/bzlmod/ModuleFileFunction.java`, `CompiledModuleFile.java`,
`ModuleThreadContext.java`. Added on corpus2-quick for the OSS corpus
(B63, aspect-build/bazel-examples).

* **Syntax** (CompiledModuleFile.SyntaxChecker, :99-177): a top-level
  `include(...)` statement with one positional string literal and at most a
  literal `dev_dependency = True|False` is an include statement; any other
  use of the name `include` fails (`the `include` directive MUST be called
  directly at the top-level`, `... MUST be called with one positional string
  literal argument and, optionally, `dev_dependency` as a boolean literal`),
  until a statement assigns to `include`.
* **Reading** (advanceHorizon, :396-543): before the module file runs, its
  segments are read breadth first, each label as written once: the label
  must start with `//`, parse, name a file ending in `.MODULE.bazel` not
  starting with `.`, in a package (a directory with a BUILD file) of the
  module's repository, and be a regular file. The failures are Bazel's
  messages (`bad include label 'L' at LOC: …`, `unable to load package for
  'L' included at LOC: BUILD file not found …`, `error reading 'L' included
  at LOC: not a regular file`), under `Error computing the main repository
  mapping: ` for the root module. Segments are read through observed_fs, so
  the analysis snapshot validates them like MODULE.bazel.
* **Evaluation** (ModuleThreadContext.include, :333-366): each call runs the
  segment's compiled file in its own module (own bindings) with the same
  module context, so its directives, extension usages and tags join the
  module's in place: tags keep one module-wide order, so the lockfile's usages
  digest equals genuine's (fixture case `include_segments`). A segment
  included again runs again; one including itself (directly or not) fails
  with `include() cycle detected: A -> B -> A`. `include()` counts as a
  non-`module()` directive, so a segment calling `module()` fails.
  `dev_dependency = True` includes are skipped under `--ignore_dev_dependency`
  and for non-root modules.
* **Registry modules**: a non-dev include fails (`include() directive found
  at LOC, but it can only be used in the root module or in modules with
  non-registry overrides`). `local_path_override`, `git_override` and
  `archive_override` modules read their segments from their own trees.
* **Not modelled**: `mod tidy` of a root module with segments fails
  explicitly (docs/MOD_COMMAND.md). A MODULE.bazel failure exits 1 natively
  where Bazel exits 48 (all module file failures, B73).

Genuine fixture: tests/fixtures/corpus2_quick (`include_*` cases), replayed
by tests/corpus2_quick.rs; unit test `bzlmod::tests::include_evaluates_segments_in_place`.
