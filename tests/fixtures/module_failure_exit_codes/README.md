# module_failure_exit_codes

Bazel 9.3.1's exit codes when module resolution or a module extension fails
(B73 of docs/OSS_CORPUS.md), pinned by tests/exit_codes.rs
`module_failures_match_bazel` (crate::exit_code::module_failure).

`cases/<case>/` are the workspaces; `genuine.tsv` has one line per command:
`CASE<TAB>ARGS<TAB>EXIT CODE<TAB>first ERROR line`, captured with the genuine
binary (`--batch --ignore_all_rc_files --output_user_root=<fresh per case>
ARGS --repository_cache=<shared> --curses=no --color=no`, the default
registry, stdin from /dev/null, a fresh copy of the case per command).
A case's `extra_args` file holds options appended to every command of the
case (and is removed from the copy). `<WS>` is the workspace,
`<OUTPUT_BASE>` the output base. Only the exit codes are pinned (the native
messages differ).

- A failure of module resolution proper — the root or a dependency's
  MODULE.bazel failing (syntax, an undefined name, `fail()`, a duplicate
  `bazel_dep`, a missing `include()`), a module no registry has — is an
  ExternalDepsException (BAD_MODULE, MODULE_NOT_FOUND): exit 48
  (EXTERNAL_DEPS_ERROR) in every command that computes the main repository
  mapping (`build`, `test`, `run`, `query`, `cquery`, `aquery`, `fetch`;
  `fetch` reports it as is, not as `Fetching some target dependencies
  failed`), 37 in `mod` (MOD_COMMAND_UNKNOWN), 0 in `info`.
- A `local_path_override` whose directory does not exist fails the
  override's repository fetch: 37 (the repository's failure has no
  ExternalDeps code), also in `mod`.
- A failing module extension, an extension `.bzl` that does not exist, an
  extension that does not generate an imported repository and a failing
  `use_repo_rule` repository fail only what needs the repository: `build`
  1, `fetch` 2, `query` 0 (no repository is loaded for `//:t`).
- MODULE.bazel.lock: a lockfile of the current version (28) that does not
  parse, and with `--lockfile_mode=error` an old version, fail as
  BAD_LOCKFILE: 48 (37 in `mod`). A file without a version is an old one,
  ignored in the default `--lockfile_mode=update`.
- `mod graph` evaluates every extension (BazelModuleInspectorFunction) and
  fails with `Results may be incomplete as N extension(s) failed.`, exit 2;
  bazel-rs does not evaluate extensions for `mod graph` (B80): those five
  lines are not pinned.
