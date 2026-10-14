# Module-extension source paths, facts and metadata

This candidate profile lets [ordinary module callbacks](MODULE_EXTENSIONS.md)
resolve and read **source files** named by Labels, observe the (always empty)
`module_ctx.facts`, and return an inert `extension_metadata`. It is aimed at
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

All parameters are keyword-only (`ModuleExtensionContext.java:207-321`):

- `reproducible`: Boolean, default `False`.
- `facts`: dict, default `{}`, validated like `Facts.validateAndCreate`: string
  keys (`got dict<K, V> for 'facts', want dict<string, unknown>` at the top level,
  `Facts keys must be strings, got '<k>: <v>' (<type>)` nested), values `None`,
  bool, int, float, string, list, tuple or dict
  (`'<repr>' (<type>) is not supported in facts`), nesting at most seven levels
  (`Facts cannot be nested more than 7 levels deep`).
- `root_module_direct_deps`, `root_module_direct_dev_deps`: **only `None`** is
  supported. Lists or `"all"` fail explicitly after facts validation; there are no
  `use_repo` fixup warnings or `bazel mod tidy` metadata.

The result has `type()` `"extension_metadata"`, is unhashable, and is inert.
The implementation function may return `None` or such a value; anything else fails
with `expected module extension @@<bzl label>%<name> to return None or
extension_metadata, got <type>` (`RegularRunnableExtension.java:298-304`). The
returned metadata, `reproducible` and facts are **discarded**: nothing is written,
persisted or fed back into `module_ctx.facts`, and they do not affect provenance.

## Exclusions

String `path()`/working-directory paths, `path` members, extension-generated or
`use_repo_rule` source labels, symlinked sources, watch-based reuse, lockfile
facts persistence, `root_module_direct_deps` fixups, `download_and_extract`,
`execute`, `file`, `delete`, `which`, `getenv`, SDK extraction and toolchain
registration remain unsupported. Tests: `src/extension_ctx.rs` unit tests and the
real-CLI `tests/module_path.rs` (root and `local_path_override` go.mod labels,
generated repository consumed by a genrule, rekey on byte changes).

## Genuine reference evidence

The rules_go-shaped CLI fixture in `tests/module_path.rs` (root and
`local_path_override` go.mod reads through `ctx.path`/`ctx.read`, tag reflection,
empty `ctx.facts`, returned `extension_metadata`, genrule consumer) was built with
genuine Bazel 9.3 offline; its output bytes equal the native expectation exactly
(`tests/fixtures/module_path/reference_receipt.json`). Other probes listed in
the review (path/str layout, diagnostics, facts printing) remain labelled native
choices until captured.
