# `.bzl` load visibility

Native emulation of Bazel 9.3 `visibility()` for `.bzl` files and the load-time
checks that use it. The sources below are from the pinned Bazel reference
`/tmp/opencode/bazel-9.3-reference` (e89755...).

## Where `visibility()` is available

- It is a `.bzl` global (`StarlarkBuildApiGlobals` is `@GlobalMethods(environment
  = Environment.BZL)`). Native installs it in `crate::package::bzl_globals_for`.
  Every native `.bzl` loader shares those globals: `.bzl` files loaded from BUILD,
  and the MODULE-time module-extension and repository-rule definition loads
  (including provenance-capturing loads).
- BUILD files and `MODULE.bazel` do not predeclare it. In a BUILD file
  `visibility(...)` is an unknown name (see Divergences for the wording).
- It has one positional-only parameter, `value`
  (`StarlarkBuildApiGlobals.java:47-50`).

## `visibility()` semantics (`BazelBuildApiGlobals.java:43-94`)

The checks run in Bazel's order, and each error message is Bazel's exact text:

1. `--experimental_bzl_visibility` defaults to true
   (`BuildLanguageOptions.java:202-203`). Native always behaves as if it is set,
   so the `requires --experimental_bzl_visibility` error cannot happen.
2. The call must come from the top-level initialization of a `.bzl` file.
   Otherwise the error is `visibility() can only be used during .bzl
   initialization (top-level evaluation)` (`BzlInitThreadContext.fromOrFail`,
   `BzlInitThreadContext.java:84-92`). Bazel keeps this state on the thread.
   Native keeps an initialization frame on a thread-local stack, keyed by the
   identity of that evaluation's `ExtensionContext`. The frame exists only while
   `loading::Loader` runs one file's top level, and it is popped on success or
   error. Because the key is the context's identity, a frozen function called
   later (for example from a BUILD file or during analysis) is rejected, and so
   is any other evaluator that reuses the context type.
3. The call stack must be exactly `[<toplevel>, visibility]`. Otherwise the error
   is `load visibility may only be set at the top level, not inside a function`.
   Native requires `call_stack_count() == 2`. A top-level comprehension adds no
   frame and is accepted. A helper `def`, a lambda, or a native higher-order
   function such as `sorted(key=visibility)` adds a frame and is rejected.
4. A second call fails with `load visibility may not be set more than once`.
5. A string is parsed as a single specification. A list (`StarlarkList` only) is
   checked with `Sequence.cast(value, String.class, "visibility list")` first, so
   a non-string element gives `at index N of visibility list, got element of type
   T, want string` before any element is parsed. Any other type, including a
   tuple, gives `Invalid visibility: got 'T', want string or list of strings`.
6. The result is `BzlVisibility.of(specs)`. An empty list, or a lone `"private"`,
   is PRIVATE. A list containing `"public"` is PUBLIC. Anything else is the union
   of the specifications.

If a file never calls `visibility()`, it is PUBLIC
(`BzlLoadFunction.java:923-926`). The visibility is stored with the frozen
module in the loader's cache (`Loader::visibilities`). The check runs separately
for each requesting file.

## Specifications (`PackageSpecification.fromStringForBzlVisibility`, 233-252)

This is `fromString` with `allowPublicPrivate=true` and
`repoRootMeansCurrentRepo=true`, followed by a rejection of negative patterns:

- `"public"` and `"private"`.
- `"//pkg"` and `"//pkg:__pkg__"` select one package. `"//pkg/..."` and
  `"//pkg:__subpackages__"` select the package and all its subpackages.
- `"//..."` selects every package in the declaring `.bzl` file's repository.
- Repository-qualified forms are accepted, as in Bazel's code (the user
  documentation says `@` is not allowed, but the code accepts it):
  - `@@canonical//...` is used as written.
  - `@apparent//...` is resolved through the declaring repository's mapping.
  - An unknown apparent repository is not an error. It matches no package,
    because Bazel creates an invisible `RepositoryName`.
- Exact error texts:
  - `Cannot negate "public" package specification` (the same for `"private"`).
  - `Cannot use negative package patterns here`.
  - `invalid package name 'X': must start with '//', '@', or be 'public' or
    'private'`.
  - For any other malformed label: `invalid package name 'X': <detail>`, where
    `<detail>` is Bazel's `LabelSyntaxException` text for `X` (or for the
    `X:__pkg__` fallback form). Captured examples:
    - `invalid package name '//b:foo': invalid target name 'foo:__pkg__': target
      names may not contain ':'`
    - `invalid package name '//b/../c': invalid package name 'b/../c': package
      name component contains only '.' characters`
    - `invalid package name '@package_metadata': invalid repository name
      'package_metadata:__pkg__': repo names may contain only A-Z, a-z, 0-9, '-',
      '_', '.' and '+'`
- This path uses `src/bazel_label.rs`, a byte-level port of Bazel's label
  grammar and its messages: `LabelParser.Parts.parse`/`validateAndCreate`,
  `LabelValidator.validatePackageName`/`validateTargetName`,
  `RepositoryName.validate`, the `Label.parseWithRepoContext` checks for `...`
  and absolute labels, and `sanitizeControlChars`. Bytes are treated like
  Bazel's Latin-1 internal strings. The ported grammar is broader than the
  native label profile, so a spec such as `//foo bar` is accepted, as in Bazel;
  it can never match a native package. Repository resolution:
  - Unqualified `conditions`/`visibility` packages go to the main repository.
  - Unqualified packages otherwise use the declaring repository.
  - `@@name` is used as written.
  - `@name` goes through the native mapping of the declaring repository.

## Load checks (`BzlLoadFunction.checkLoadVisibilities`, 1282-1318)

A load is allowed if the requesting file is in the same package as the loaded
file, or if the loaded file's visibility allows the requesting package. Each
violating load statement is reported with Bazel's event location for the load:
the requesting file's absolute filesystem path, then the 1-based line and column
where the module string starts. The column counts bytes, because Bazel parses
files as Latin-1 (`StarlarkUtil.createParserInput`). There is no end column.

    <abs path>:<line>:<col>: Starlark file <Label.toString> is not visible for loading from package <PackageIdentifier.getCanonicalForm>. Check the file's `visibility()` declaration.

The label is in canonical form: `//a:x.bzl` for the main repository and
`@@repo+//pkg:x.bzl` otherwise. The package is `//pkg` or `@@repo+//pkg`.

After all violations, the error ends with `visibilityViolation`'s line:
`<description> contains .bzl load visibility violations`. The description is
`module <canonical .bzl label>` for a `.bzl` file (`BzlLoadFunction.java:850`) or
`file <canonical BUILD label>` for a BUILD file (`PackageFunction.java:1146-1149`).
Native joins all of these lines into one error string, while Bazel reports
separate events followed by an exception. As in Bazel, the check runs after the
direct loads have been evaluated and before the requesting file executes or its
symbols are bound.

Coverage of the checks:

- BUILD loads and every `.bzl`-to-`.bzl` load in any loader are checked. This
  includes the transitive loads of MODULE-time extension and repository-rule
  `.bzl` files.
- The entry `.bzl` named by `use_extension` or `use_repo_rule` is **not** checked.
  `RegularRunnableExtension.java:156-163` and
  `InnateRunnableExtension.java:119-134` load it with
  `BzlLoadValue.keyForBzlmod` and never call `checkLoadVisibilities` (there is a
  TODO in `RegularRunnableExtension`). Native `load_definition_in` and
  `load_module_extension_in` match this.
- `@_builtins` and the prelude are not implemented natively, so their exemptions
  do not apply. Native has no WORKSPACE loading.

## Genuine Bazel 9.3 capture and divergences

A paired capture against Bazel 9.3 (`bazel-9.3.0-linux-x86_64`, sha256
`d302d22e...`) is recorded in `tests/fixtures/bzl_visibility/reference_receipt.json`.
It was produced by the ignored script `artifacts/n8-capture.py`, which mirrors
every step of `tests/bzl_visibility.rs` and adds extra edge cases. The first run
found three mismatches, which are now fixed:

- the violation location (formerly `<file or label>:L:C-ENDCOL`);
- the three label-syntax detail texts above;
- the test asserts that depended on those forms.

In the rerun, 46 steps compare as follows: 16 EXACT, 28 KNOWN_DIVERGENCE and 2
MISMATCH. Both mismatches are the BUILD-file undefined-name text below. The
remaining divergences:

- **Undefined name in a BUILD file:** native printed the general starlark-rust
  interpreter text ``Variable `visibility` not found`` when this capture ran;
  Bazel prints `name 'visibility' is not defined`. Since N33 the native text is
  Bazel's for every undefined name (docs/STARLARK_VENDOR.md, `tests/fixtures/interpreter_wording/reference_receipt.json`).
- **Exit code:** a failing `query` exits with 1 natively and 7 in Bazel.
- **Framing:**
  - Native joins all violations and the summary into one error with no `ERROR:`
    prefixes. Bazel wraps the summary as `error loading package '<pkg>':
    <summary>`, or for a module extension as `Target parsing failed due to
    unexpected exception: Error loading '//ext:ext.bzl' for module extensions,
    requested by <MODULE.bazel location>: ...`.
  - `.bzl` initialization failures are wrapped as `cannot evaluate <label>:`
    with a starlark-rust traceback. Bazel prints a Java traceback with `Error in
    visibility: <msg>`, then `initialization of module '<path>' failed`.
- **Build success output:** native prints `label<TAB>path` on stdout; Bazel
  prints `Target ... up-to-date` on stderr. The artifact bytes are equal.
- **External repository paths:** a requesting file in an external repository is
  located under the native repository cache path, not under Bazel's
  `<output_base>/external/...`.

Apparent-repo specs resolve through the native repository mapping, so a native
mapping failure (for example an isolated extension, unsupported natively, or a
name native rejects) is reported as an error where Bazel would map to a repo that
simply grants nothing; access is never wrongly granted.

## Flags

`--check_bzl_visibility` defaults to true (`BuildLanguageOptions.java:233-234`).
Only that default is emulated: violations are errors.
`--nocheck_bzl_visibility`, which turns violations into warnings ending in
`Continuing because --nocheck_bzl_visibility is active`, is not accepted.

## Tests

- Unit tests: the `tests` modules in `src/bzl_visibility.rs` and `src/bazel_label.rs`.
- CLI tests: `tests/bzl_visibility.rs`.
- The fixture `tests/fixtures/bzl_visibility/package_metadata` follows the
  `package_metadata_info.bzl` pattern: a top-level `visibility("public")`
  followed by provider and function definitions, plus a private helper.
