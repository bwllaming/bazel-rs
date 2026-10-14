# Module-extension source paths, facts and metadata

This candidate profile lets [ordinary module callbacks](MODULE_EXTENSIONS.md)
resolve and read **source files** named by Labels, observe the (always empty)
`module_ctx.facts`, and return an `extension_metadata` whose direct-dependency
lists drive Bazel's root-module `use_repo` fixup warning. It is aimed at
rules_go-shaped `go_sdk` extensions (`from_file(go_mod = "//:go.mod")` plus
`download(version = ...)`). It does not add SDK download/extraction, `execute`,
toolchains or lockfiles. Semantics follow pinned Bazel 9.3 source
(`e89755babf3e941bc1e9d8120b1cd48854ab9dfa`); native captures against genuine
Bazel for this surface have **not** been run yet.

## `module_ctx.path(path)`

| Argument | Native profile |
|---|---|
| `Label` | Resolved to the source file in its repository; returns a `path` value. |
| `path` | Returned unchanged (`StarlarkBaseExternalContext.getPath`). |
| string | **Unsupported**, explicit error. Bazel resolves it relative to the extension working directory; this profile has no such path view. |
| other | Type error: `in call to path(), parameter 'path' got value of type '<t>', want 'string, Label, or path'`. |

Only the **main repository** and repositories of the **resolved module graph**
(root, registry, `local_path_override`, Git overrides, built-ins) are admitted.
Extension-generated and `use_repo_rule` repositories fail explicitly instead of
evaluating another extension re-entrantly. The label's package must exist
(a regular `BUILD.bazel`/`BUILD`), otherwise
`Unable to load package for <label>: <reason>` (Bazel's prefix; the reason text is
native). As in Bazel, the file itself need not exist for `path()`. With Bazel 9.3's
default `--incompatible_no_implicit_watch_label`, `path()` records no watch and
adds no provenance.

The `path` value is immutable and hashable, compares equal by host path, and
`type()` is `"path"`. `str()` is the absolute host path of the resolved file
(source roots are canonicalized; registry/Git roots are cache paths, not Bazel's
`external/` layout) and `repr()` is its quoted form. **No members** are exposed:
`basename`, `dirname`, `exists`, `is_dir`, `readdir`, `get_child`, `realpath`
are absent, not fabricated. Putting `str(path)` into a repository attribute makes
that host-specific path part of the generated repository's provenance, so cache
keys then depend on the host and `--cache-dir` (a cache miss, not a wrong result).
Source reads open the final component with `O_NOFOLLOW|O_NONBLOCK` on Linux.

## `module_ctx.read(path, *, watch = "auto")`

Strings keep the unchanged [owned scratch-path profile](MODULE_EXTENSION_IO.md),
including its existing native `watch` messages. `Label` and `path` arguments read
the resolved source file in Bazel's order: resolve (package check), validate
`watch`, reject directories, read.

- Bad `watch`: `bad value for 'watch' parameter; want 'yes', 'no', or 'auto', got <v>`.
- Directory: `attempting to read() a directory: <abs path>`.
- Missing file: `cannot read <abs path>: No such file or directory` (Bazel reports a
  Java `FileNotFoundException` instead).
- Any symlink on the repository-relative path, special files: explicit
  unsupported errors (Bazel follows symlinks).
- Each byte maps to U+0000..U+00FF (Latin-1), exactly like owned reads.
- At most 8 MiB per file; every source read counts against the shared 32-call,
  work and deadline budgets of the [I/O profile](MODULE_EXTENSION_IO.md#execution-limits-and-lifetime).
  The opened file's identity and size are re-checked after reading.

Every source read appends an ordered provenance observation
`{"operation": "read_source", "label": "<canonical label>", "watch": "<v>",
"sha256": "<hex>", "size_bytes": <n>}` to `extension_io`. No absolute path enters
provenance. The digest is recorded for **all** watch modes, including `"no"`, so
changing the file's bytes always rekeys the generated repositories even when every
declaration is unchanged; restoring the bytes restores the previous key. This is a
conservative overapproximation of Bazel's watch-based re-evaluation, not watch
reuse: each fresh runtime reevaluates the callback anyway.

## `module_ctx.facts`

Always the empty `Facts` value: there is no lockfile, matching Bazel's
`--lockfile_mode=off` (`SingleExtensionEvalFunction.java:145-150`). `type()` is
`"Facts"`; `str()`/`repr()` are `Facts(<opaque, inspect with print()>)`;
`get(key, default = None)` returns the default; `in` is `False`; equality is by
value (`ctx.facts == ctx.facts`, as in Bazel); hashing fails with
`unhashable type: 'Facts'`; iteration, `len` and `keys` are unsupported.
Divergences: `print(facts)` shows the opaque form (Bazel prints `Facts({})`), and
`facts[key]` fails explicitly (`Facts has no key ...`) where Bazel's lookup yields
an internal null.

## `module_ctx.extension_metadata(*, ...)`

All parameters are keyword-only (`ModuleExtensionContext.java:207-321`). Argument
processing follows `BuiltinFunction.ArgumentProcessor` (`BuiltinFunction.java:310-430`):
named arguments are matched and type-checked **in call order**, so the first bad
argument in the call is reported; unknown names fail with
`extension_metadata() got unexpected keyword argument '<k>'` plus Starlark-Java's
`SpellChecker` suggestion (` (did you mean '<p>'?)`); any positional argument fails
afterwards with `extension_metadata() got unexpected positional argument`. Type errors
are `in call to extension_metadata(), parameter '<p>' got value of type '<t>', want
'<w>'`, with `<w>` `sequence, string, or NoneType` (both dependency parameters),
`bool` (`reproducible`) or `dict` (`facts`). Sequences are lists, tuples and ranges.

- `reproducible`: Boolean, default `False`.
- `facts`: dict, default `{}`, validated like `Facts.validateAndCreate`: string
  keys (`got dict<K, V> for 'facts', want dict<string, unknown>` at the top level,
  `Facts keys must be strings, got '<k>: <v>' (<type>)` nested), values `None`,
  bool, int, float, string, list, tuple or dict
  (`'<repr>' (<type>) is not supported in facts`), nesting at most seven levels
  (`Facts cannot be nested more than 7 levels deep`). Facts are validated
  **before** the dependency parameters, then discarded (no lockfile).
- `root_module_direct_deps`, `root_module_direct_dev_deps`
  (`ModuleExtensionMetadata.create`, `ModuleExtensionMetadata.java:88-174`), in order:
  1. both `None`: no use_repo metadata;
  2. `"all"` with the other exactly an empty **list** (`StarlarkList.equals`; an
     empty tuple or range does not qualify): every generated repository is a
     regular (`root_module_direct_deps = "all"`) or dev direct dependency;
  3. otherwise either `"all"`: `if one of root_module_direct_deps and
     root_module_direct_dev_deps is "all", the other must be an empty list`;
  4. any other string: `root_module_direct_deps and root_module_direct_dev_deps must
     be None, "all", or a list of strings`;
  5. exactly one `None`: `root_module_direct_deps and root_module_direct_dev_deps
     must both be specified or both be unspecified`;
  6. `Sequence.cast` of deps, then of dev deps: `at index <i> of <param>, got element
     of type <t>, want string`;
  7. per deps entry: `in root_module_direct_deps: invalid user-provided repo name
     '<name>': valid names may contain only A-Z, a-z, 0-9, '-', '_', '.', and must
     start with a letter or a number` (control characters sanitized as in
     `StringUtilities.sanitizeControlChars`), then `in root_module_direct_deps:
     duplicate entry '<name>'`;
  8. per dev entry: the same name check prefixed `in root_module_direct_dev_deps: `,
     then `in root_module_direct_dev_deps: entry '<name>' is also in
     root_module_direct_deps`, then `in root_module_direct_dev_deps: duplicate entry
     '<name>'`.

The result has `type()` `"extension_metadata"`, is unhashable and opaque. The
implementation function may return `None` or such a value; anything else fails
with `expected module extension @@<bzl label>%<name> to return None or
extension_metadata, got <type>` (`RegularRunnableExtension.java:298-304`). A
metadata value that is created but not returned has no effect.

### Root-module `use_repo` fixups

After a successful callback (and after every generated repository is declared and
bound), and **only if the root module uses the extension**
(`SingleExtensionEvalFunction.java:455-487`), the returned metadata is checked
against the generated repository names (`LockfileModuleExtensionMetadata.generateFixup`,
lines 79-286). Root proxies are the root module's `use_extension` calls in call
order (several label spellings of one identity merge); `dev_dependency = True`
proxies ignored by `--ignore_dev_dependency` are absent, as in Bazel.

- Explicit lists naming repositories the extension did not create fail with
  `root_module_direct_dev_deps contained the following repositories not generated by
  the extension: <names>` (checked first) or the same for `root_module_direct_deps`;
  names are joined with `, ` in the order given by the extension.
- Non-empty regular (dev) direct dependencies when the root module has no non-dev
  (dev) usage fail with `root_module_direct_deps must be empty if the root module
  contains no usages with dev_dependency = False` (or the `_dev_deps` / `True` form).
- These errors fail the extension evaluation; the native message is
  `<message>\nerror evaluating module extension <name> in <bzl label>` (Bazel reports
  the same two texts as two `ERROR:` events). No repository is published.
- If the expected and actual (regular and dev) imports differ, one warning is written
  to stderr before the import check (`SingleExtensionFunction.java:53`):

```
WARNING: <abs root MODULE.bazel>:<line>:<col>: The module extension <name> defined in @<repo>//<pkg>:<file> reported incorrect imports of repositories via use_repo():

Imported, but not created by the extension (will cause the build to fail):
    <sorted names>

Not imported, but reported as direct dependencies by the extension (may cause the build to fail):
    <sorted names>

Imported as a regular dependency, but reported as a dev dependency by the extension (may cause the build to fail when used by other modules):
    <sorted names>

Imported as a dev dependency, but reported as a regular dependency by the extension (may cause the build to fail when used by other modules):
    <sorted names>

Imported, but reported as indirect dependencies by the extension:
    <sorted names>

Fix the use_repo calls by running 'bazel mod tidy'.
```

  Only non-empty sections appear. The location is the **first** root proxy's
  `use_extension` left parenthesis (`thread.getCallerLocation()`, `Eval.java:714`),
  1-based line and column; the extension file is the root usage's label normalized
  like `ModuleFileGlobals.normalizeLabelString` (`//:ext.bzl` in module `main`
  becomes `@main//:ext.bzl`; an unnamed root module gives `@//:ext.bzl`). An import
  of a repository the extension did not create still fails afterwards with the
  native missing-import error.

Differences and decisions: native columns count characters (Bazel counts the
decoded Latin-1/UTF-8 input; identical for ASCII MODULE files); the warning is a
plain stderr line, not a Bazel event (no colour, no `--ui_event_filters`); `bazel mod
tidy` and its `use_repo_add`/`use_repo_remove` commands are not implemented. Native
re-evaluates the extension in every invocation (only materialized trees are cached),
so the warning is printed by every invocation that demands the extension, as Bazel
prints it whenever `SingleExtension` is evaluated or replayed. Metadata never enters
repository provenance: it cannot change generated repositories, canonical names or
cache keys.

## Exclusions

String `path()`/working-directory paths, `path` members, extension-generated or
`use_repo_rule` source labels, symlinked sources, watch-based reuse, lockfile
facts persistence, `bazel mod tidy`, `download_and_extract`,
`execute`, `file`, `delete`, `which`, `getenv`, SDK extraction and toolchain
registration remain unsupported. Tests: `src/extension_ctx.rs` and
`src/extension_metadata.rs` unit tests, the real-CLI `tests/module_path.rs` (root
and `local_path_override` go.mod labels, generated repository consumed by a
genrule, rekey on byte changes) and `tests/module_extensions.rs` (exact fixup
warnings, metadata errors, skylib `modules.use_all_repos` from a non-root module).

## Genuine reference evidence

The rules_go-shaped CLI fixture in `tests/module_path.rs` (root and
`local_path_override` go.mod reads through `ctx.path`/`ctx.read`, tag reflection,
empty `ctx.facts`, returned `extension_metadata`, genrule consumer) was built with
genuine Bazel 9.3 offline; its output bytes equal the native expectation exactly
(`tests/fixtures/module_path/reference_receipt.json`). Other probes listed in
the review (path/str layout, diagnostics, facts printing) remain labelled native
choices until captured.

Eight paired `extension_metadata` direct-dependency scenarios (explicit and
`"all"` modes, dev modes, every fixup-warning section, invalid-repo, emptiness,
`"all"`+tuple and invalid-name errors) were run under genuine Bazel 9.3 and the
native CLI on identical workspaces: exit classes, path-normalized `WARNING` blocks
and inner error messages are identical in all eight
(`tests/fixtures/module_extension_metadata/reference_receipt.json`); only
diagnostic framing differs. A keyword duplicated through `**kwargs` reports
starlark-rust's duplicate-argument text rather than Java's `got multiple values`.
