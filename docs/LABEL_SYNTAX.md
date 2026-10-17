# Label syntax, target patterns and package discovery (N38)

Native labels use Bazel 9.3's complete label grammar, validation and error
texts. The former "stricter subset" (ASCII letters, digits, `_ - . +` and `/`)
is gone. Paired evidence: `tests/fixtures/label_syntax/reference_receipt.json`
(82 rows, genuine Bazel 9.3.0 against native; run 2: 33 same results, 47 shared
error texts, 2 both-fail rows, 0 mismatches; every native error message occurs
verbatim in Bazel's stderr). Pinned source: `/tmp/opencode/bazel-9.3-reference`.

## Labels (`src/bazel_label.rs`, `src/label.rs`)

* `LabelParser.Parts.parse` (`cmdline/LabelParser.java:109-166`), including
  `@repo` = `@repo//:repo` and `@@repo` = `@@repo//:repo` (`:115-126`), `//p` =
  `//p:p`, `/...` handling and the `(perhaps you meant ":x"?)` hint (`:211-213`).
* Package names (`LabelValidator.validatePackageName`, `LabelValidator.java:
  95-139`): any printable ASCII except `:` and `\`, so `//sp ace`, `//(group)`,
  `//tt@x` and `//a+b,c;d=e[f]{g}~h!` are valid. No leading or trailing `/`,
  no `//`, no segment made only of dots. Non-ASCII package names are invalid.
* Target names (`validateTargetName`, `:146-212`): additionally any non-ASCII
  character (`//p:café`); no `:`, `\`, control characters, `..`/`.` segments,
  `//`, trailing `/` or trailing CR. `.` and a trailing `/.` are accepted and a
  trailing `/.` is dropped (`LabelParser.java:227-230`, `Label.create`).
* Repository names (`RepositoryName.validate`, `RepositoryName.java:178-196`).
* `Label::parse_in` is `Label.parseWithPackageContext` (`Label.java:217-245`):
  `... package name cannot contain '...'`, `absolute label must begin with '@'
  or '//'`, `//visibility` and `//conditions` without a repository stay in the
  main repository (`:94-99`, `:177-192`). Every text is
  `StringUtilities.sanitizeControlChars`-escaped (`<?>`, `\r`).
* The same parser serves BUILD attributes, `load()`, `Label()`, select keys,
  visibility and `package_group` specifications, repository-rule and tag
  attributes, `PackageSpecificationInfo.contains` (`Label.parseCanonical`) and
  the command line. An unknown apparent repository still fails eagerly
  (docs/LABEL_PASSTHROUGH.md); that is not a syntax question.

## Target patterns (`src/target_pattern.rs`, `Engine::select_filtered`)

* `TargetPattern.Parser.parse` (`cmdline/TargetPattern.java:814-857`):
  `//...`, `p/...`, `...:all`, `...:*`, `...:all-targets` (below a directory),
  `:all`, `:*`, `:all-targets` (one package), single targets, `@repo` forms,
  and a colon-free relative `p/t` as a path (`InterpretPathAsTarget`,
  `:348-383`: the path as a package's default target, else the longest package
  prefix owning the rest; `couldn't determine target from filename`).
* Relative patterns resolve against the working directory below the workspace
  root, a package or not (`buildtool/AnalysisPhaseRunner.java:554`): `:t` in a
  non-package directory `a/b` is `//a/b:t` (`no such package 'a/b'`).
* `:*`/`:all-targets` list every target: rules, package groups, generated
  files, exported and assumed input files and the BUILD file.
* `//p:all` naming an existing target uses it with Bazel's warning
  (`TargetsInPackage.getWildcardConflict`, `:524-560`).
* `no targets found beneath 'D'` when no package exists below `D`
  (`RecursivePackageProviderBackedTargetPatternResolver.java:326-329`).
* `build` accepts negative patterns (`-//x/...`, after `--`): patterns are
  expanded in order, a negative pattern's targets (unfiltered) removed from
  what was accumulated (`TargetPatternPhaseFunction.mergeAll`); a later
  positive pattern re-adds. Every pattern's packages are loaded, so a broken
  package below an excluded directory still fails the build (paired row
  `B/negative_excluded_broken`). `build` wraps a `:all` package error in
  `while parsing ':all': ...`; `query` does not.
* `query`: the residue is joined by spaces (`QueryOptionHelper.java:41-48`) and
  lexed like Bazel's query `Lexer` (`query2/engine/Lexer.java:119-318`, in
  `src/query_expression.rs`). Spaces and ASCII punctuation other than
  `*/@.-_:$~[]` end an unquoted word, so `query //sp ace:all` is Bazel's
  `Error while parsing '//sp ace:all': unexpected token 'ace:all' after query
  expression '//sp'` and `query //(group)/...` an unknown-function error; quote
  the pattern (`query '"//sp ace:all"'`). Only a single target literal is
  evaluated: functions, operators, `let`, `set` and parentheses are explicit
  native gaps (`query expression '...' is not implemented in the native
  profile`). Syntax errors (`premature end of input`, `unclosed quotation`,
  `syntax error at '...'`, `target literal must not begin with (-)`) use Bazel's
  texts.

## Package discovery and boundaries

* `//...` (`ProcessPackageDirectory`, `ProcessPackageDirectory.java:131-176`)
  looks every directory up as a package; an invalid package name is
  `PackageLookupValue.invalidPackageName` (`PackageLookupFunction.java:91-96`),
  no package and no error. Its subdirectories' paths are invalid too, so the
  subtree is skipped silently: `café/BUILD`, `a:b/BUILD`, `back\slash/BUILD`,
  `.../BUILD` and non-UTF-8 directory names are never packages
  (`venv/.../tt@iqtelif` and `app/(beyond-work)` are valid packages).
  Patterns naming such a package fail with the label error.
* Explicit labels below such a directory do not cross a package boundary
  (`ContainingPackageLookupFunction` walks past invalid names), e.g.
  `srcs = ['café/in.txt']` with `café/BUILD` present.
* Globs are different: `GlobCache.isSubPackage` asks `PathPackageLocator
  .getPackageBuildFileNullable` (`pkgcache/PathPackageLocator.java:83-96`),
  which only looks for a BUILD file, so a directory with a BUILD file is a
  glob boundary whatever its name (paired rows `Q/glob_file_targets`,
  `B/glob_locations_escaped`). Glob results may contain spaces, parentheses
  and non-ASCII names; non-UTF-8 glob entries remain an explicit error.

## Paths

* Package and target names map to execution and output paths unchanged
  (`p/a b (1).pdf`, `bazel-out/<cfg>/bin/outs/o u t.txt`), as in Bazel.
* `$(location)`/`$(locations)` shell-escape each path like
  `ShellEscaper.escapeString` (`analysis/LocationExpander.java:360-362`);
  `$(SRCS)`/`$(OUTS)` join raw paths (`Artifact.joinExecPaths`), exactly like
  Bazel, so a genrule `$(SRCS)` with spaces breaks in both.
* A target name with a lone (non-UTF-8) byte escape (`outs = ['\377.txt']`;
  N26 keeps such bytes as private-use stand-ins U+10FF00 + byte) names the raw
  byte at the file system: action input and output paths go through
  `bazel_bytes::internal_path`, so `ctx.actions.write` creates `0xFF.txt`. As
  in genuine Bazel 9.3.0 (targeted post-capture probe, receipt
  `post_capture_probe`), a genrule writing `$@` (its command is decoded to
  U+FFFD) fails with a missing declared output, and a source `src\377.in`
  is an input in error even when the file exists (Bazel reads it through
  java.io: `error reading file '//:src�.in': <WS>/src?.in`); native reports a
  missing input file. Build results print the raw path bytes.
* Runfiles manifests and trees (whose Bazel format escapes spaces, newlines and
  backslashes) are not generated natively (N23f, docs/RUNFILES.md); nothing
  else is escaped.

## Repository-execution source text

Repository rules and module extensions run `.bzl` files in the Latin-1 char
model (docs/REPOSITORY_RULES.md). Their sources are now decoded as Latin-1
before parsing, exactly like Bazel (`skyframe/StarlarkUtil.java:43-67`,
`ParserInput.fromLatin1`): a raw non-ASCII literal (e.g. the en dash in
rules_python's `normalize_name.bzl` docstring) is one char per UTF-8 byte, so
`len('é') == 2` and `ctx.file` writes the original bytes (paired row
`X/repository_bzl_non_ascii_source`). A non-ASCII `load()` label in that
profile fails explicitly (its Latin-1 decoding would name a different file
natively), and so does any other non-ASCII label string resolved there
(`Label('//:é.txt')`, `attr.label(default = '//:é')`: `non-ASCII label ... is
unsupported in direct repository execution`, `loading::resolve_label`), like
the existing ASCII-label rule for repository attributes; MODULE.bazel literals
keep their UTF-8 model and the existing demanded-usage check.

## Measured on ~/code (native only, read-only checkout)

`artifacts/n38-measure.py` (probe_application_v2 invocation: `--ignore_all_rc_files
--cache-dir=<private> query --experimental-builtin-modules
--registry=https://bcr.bazel.build --lockfile_mode=off`). Before N38 `//...` and
`//src/...` stopped at once on `tt@iqtelif` / `(beyond-work)` and
`//src/beyondwork.ai/...` at rules_oci's `@go_shared`. With N38 all three get
past every label-syntax point and stop in module-extension execution gaps that
are not label related (see the N38 report and artifacts/PROGRESS.md).
