# rc-file evaluation (N31)

`bazel-rs` evaluates rc files like Bazel 9.3.0 for `build`, `query` and `mod`.
`version`, `help` and `capabilities` are native-only and read no rc files.
Implementation: `src/bazelrc.rs` (client and server halves), the call sites in
`src/main.rs`, and the per-command option data in `src/command_options.rs`.
Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b). Client paths below
are relative to `src/main/cpp/`, server paths to
`src/main/java/com/google/devtools/build/lib/`.

## Client: startup options and rc files

Native-only `--native-lockfile=workspace|readonly|/absolute/path` is also accepted
before the command, like the native `--cache-dir=PATH` control. It remains
CLI-only, not a genuine Bazel startup option and never allowed in rc files.
Command-level values override the startup default; see
[lockfile isolation](REGISTRY_LOCKFILE.md#native-only---native-lockfileworkspacereadonlyabsolutepath).

* **Command line split** (`option_processor.cc:80-172`). The leading `-`
  arguments, up to the command, are startup options. Bazel's nullary and unary
  startup flags (`startup_options.cc:121-166`, `bazel_startup_options.cc:29-39`)
  are recognized; `--bazelrc VALUE` becomes `--bazelrc=VALUE`. Unknown flags,
  values given to nullary flags and missing unary values fail with Bazel's texts.
  The native-only `--cache-dir=PATH` and the native `--version` command are
  still accepted.
* **Modelled startup options**: `--[no]system_rc`, `--[no]workspace_rc`,
  `--[no]home_rc`, `--[no]ignore_all_rc_files`, `--[no]quiet`
  ([`STARTUP_QUIET.md`](STARTUP_QUIET.md)) and `--bazelrc`. Any other valid
  startup option fails explicitly, also when it comes from a `startup` or
  `startup:linux` rc line (the error names the rc file:line).
* **rc file order** (`option_processor.cc:374-490`): `/etc/bazel.bazelrc`,
  `%workspace%/.bazelrc`, `$HOME/.bazelrc`, the comma-separated `$BAZELRC` files,
  then the `--bazelrc` values in order. Values after `--bazelrc=/dev/null` are
  ignored (`blaze_util.cc:82-114`). `$BAZELRC` and `--bazelrc` files must be
  readable (`Error: Unable to read .bazelrc file '…'.`). Files are deduplicated
  by canonical path (nonexistent ones drop out), and Bazel's
  `Duplicate rc file: …` warnings (`:307-340`) and the
  `The following rc files are no longer being read…` warning (`:203-243`,
  `:342-354`, `:467-487`) are printed.
* **Parsing** (`rc_file.cc:85-249`). `\` line continuations are removed first
  (also inside comments). Then each line is whitespace-stripped and tokenized
  with quotes, `\` escapes and `#` comments (`util/strings.cc:181-242`). The
  first word is the command key (`build`, `build:NAME`, `common`, `always`,
  `startup`…). `import`, `try-import` and `try-import-if-bazel-version OP VERSION`
  (evaluated against 9.3.0 with SemVer semantics, `sem_ver.cc`) handle
  `%workspace%/` paths, missing optional imports, loop detection and the
  invalid-declaration or condition errors, all with Bazel's texts. A line without
  options defines nothing, so `build:x` alone leaves config `x` undefined. Every
  option keeps the physical line of its first byte.
* `--ignore_all_rc_files` warns about ignored `--bazelrc`/explicit `--*_rc`
  values (`bazel_startup_options.cc:56-77`).

## Server: groups, `--config` and notes

* **Structuring** (`runtime/BlazeOptionHandler.java:663-742`). Options are
  chunked per rc file. Unknown command names produce
  `while reading option defaults file '…': invalid command name '…'.` warnings.
  `--flag_alias` is restricted to `build`/`common`/`always`.
* **Groups** (`:199-251`, `:607-621`): `always`, `common`, then the command.
  `build`, `query` and `mod` inherit from no other command; `run` (N23f)
  inherits `build` (groups `always`, `common`, `build`, `run`;
  RunCommand's `inheritsOptionsFrom`). For `common` filtering `run` uses the
  option data's "other commands" bit, which also covers other non-build
  commands' options (an over-approximation). `common` options that
  the current command does not have, but some other Bazel 9.3 command does, are
  ignored and reported in the `Ignored as unsupported by '…'` note. Options
  unknown to every command are errors. The per-command option data comes from
  genuine Bazel's `help flags-as-proto`
  (`scripts/extract_command_options.py`). `always` options are never ignored.
* **`--config`** (`runtime/ConfigExpander.java:90-303`). Definitions are read from
  `always:NAME`, `common:NAME` and `<command>:NAME` in that order, rc order within
  each. Recursive configs expand in place. The cycle,
  `--config value`-in-a-definition and
  `Config value 'NAME' is not defined in any .rc file` errors, and the
  repeated-config and long-chain warnings, are Bazel's.
  `--enable_platform_specific_config` expands `linux`. Expanded options take the
  priority of their `--config` (`common/options/OptionsParserImpl.java:611-620`),
  so a later command-line option overrides an earlier `--config` and vice versa.
* **Order**. Errors are reported in Bazel's parse order: rc chunks, then the
  command line, then each expansion. Values are applied in priority order.
  Residue (targets, `mod` subcommands) is collected in parse-call order, so rc
  files can name targets. Starlark flags (`--//pkg:flag`, `--@repo//…`) from rc
  files reach the build-setting parser like command-line ones.
* **Notes**. `--announce_rc` prints the rc notes after the stored warnings. On an
  option error with an invalid argument (`ARG :: …`), the notes containing `ARG`
  are printed first (`runtime/BlazeCommandDispatcher.java:923-948`). `build` in a
  `MODULE.bazel` workspace defers `Unrecognized option` errors, without notes, to
  its second parsing round (`:738-755`, `:1022`).

## Native boundary

After expansion every option goes through the existing native parsers
(`src/main.rs` `NativeOptions`, `crate::configuration`, `crate::module_cli`).
Options those parsers do not model fail explicitly, for example:

    unsupported option "--distdir=…" from rc file /ws/.bazelrc:3 ('build' options); options are never silently ignored
    unsupported option --sandbox_add_mount_pair from rc file /ws/.bazelrc:1 (config definition 'build:cross'): only identity mounts of an absolute path (PATH or PATH:PATH) are modelled …

The options of real rc files (`--repo_env`, `--attempt_to_print_relative_paths`,
`--incompatible_strict_action_env`, the Apple SDK versions, test, remote and
sandbox options, …) are modelled or fail explicitly as described in
[RC_OPTIONS.md](RC_OPTIONS.md) (N40). The value of `--repo_env` is never echoed
in errors.

Deliberate differences from Bazel:

* **Native-only options in rc files.** `--experimental-builtin-modules[=BOOL]`
  and `--cache-dir=PATH` (also as a `startup` line) are not Bazel options.
  Genuine Bazel reports them as unrecognized in an rc file, so native rejects
  them there too: `native-only option NAME from rc file FILE:LINE … is not a
  Bazel option and is rejected in rc files …; pass it on the command line`.
  Command-line use is unchanged. (The built-in modules are the default, so
  the switch only matters as `--experimental-builtin-modules=false`;
  docs/BUILTIN_MODULES.md.)
* **Redacted environment values in rc notes.** Bazel echoes rc arguments
  verbatim in `--announce_rc` and early-exit notes. Native shows the values of
  `--repo_env`, `--action_env`, `--host_action_env` and `--test_env` as
  `--repo_env=KEY=<redacted>`, also in the split `--repo_env KEY=VALUE` form.
  A bare `KEY`, which inherits from the environment, is shown as is. This keeps
  the native rule that such values are never printed.

Other known divergences:

* Exit codes are always 1, not Bazel's 2/7/37.
* Error lines are prefixed `bazel-rs: ` instead of `ERROR: ` or the client's log
  format.
* `--terminal_columns` in the client note comes from `$COLUMNS` or 80, because
  this crate forbids the `TIOCGWINSZ` ioctl.
* When an rc file is read twice, Bazel's chunk merging depends on its hash-map
  order. Native keeps the per-command order, which is the only order that
  affects option values.

The system rc path `/etc/bazel.bazelrc` is read but cannot be exercised in the
paired capture without root.

## Evidence

* Rust tests: `src/bazelrc.rs` unit tests and `tests/bazelrc.rs`.
* Per-command option data. `src/command_options.rs` records the genuine
  binary's SHA-256 (`d302d22e…f80eb8`) and the SHA-256 of its
  `help flags-as-proto` output (`42691ca1…fdfe5c0`) in its header. Regenerate
  it with:

      python3 scripts/extract_command_options.py \
        --bazel /home/coder/bazel/.direnv/reference/bazel-9.3.0-linux-x86_64 \
        --scratch /tmp/opencode > src/command_options.rs

  The script runs the binary with `--batch`, `--ignore_all_rc_files`, and a
  private HOME and output user root, all removed afterwards. It refuses to
  generate if the binary's or the output's SHA-256 differs from the pinned one.
  `--from-base64 FILE` regenerates from a saved output with the same check.
* Paired capture: `artifacts/bazelrc-capture.py`, which compares native and genuine
  Bazel 9.3.0 in fresh scratch workspaces with a private HOME. Its tracked
  receipt is `tests/fixtures/bazelrc/reference_receipt.json`, which no test
  reads. The receipt also records a native-only probe of the real `~/code` rc
  files. That probe ran without `BAZELRC`. Per the comment in
  `~/code/.bazelrc.coder`, `~/code`'s `.envrc` points `BAZELRC` at
  `.bazelrc.coder` when `CODER=true`; this work never sources that `.envrc`.
  Under direnv, Bazel and native therefore also read `.bazelrc.coder` (its
  `common --repo_env=…` lines) after the workspace rc. The first failure is
  unchanged, because the workspace rc's `common` options come first. It stopped at the first unmodelled option:
  `--credential_helper=…` from `/home/coder/code/.bazelrc:2` (`'common'`
  options), for both `build` and `query`. With N40 ([RC_OPTIONS.md](RC_OPTIONS.md)),
  N41 ([REMOTE_CACHE.md](REMOTE_CACHE.md): `--remote_cache`, `--credential_helper`)
  and N42 ([ACTION_ENV.md](ACTION_ENV.md): `--incompatible_strict_action_env`)
  those lines are modelled; for the remaining stops see [RC_OPTIONS.md](RC_OPTIONS.md).
