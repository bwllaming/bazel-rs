# Action environments (N42)

Every native process action (genrule, Starlark `ctx.actions.run_shell` and
`ctx.actions.run`) runs with exactly the environment Bazel 9.3 computes for
it, so behavior and cache keys depend on the same variables. Claims below are
checked by Rust tests (`src/action_env.rs`, `src/configuration.rs`,
`src/config_registry.rs`, `src/action.rs`, `tests/action_env.rs`) and by a
paired genuine Bazel 9.3.0 capture
(`artifacts/action_env-capture.py` ->
`tests/fixtures/action_env/reference_receipt.json`, evidence only). Citations
are to the pinned Bazel source (`src/main/java/com/google/devtools/build/lib/`,
release 9.3.0).

## Options

* `--action_env` and `--host_action_env` (`analysis/config/CoreOptions.java:525-564`)
  and `--test_env` (`analysis/test/TestConfiguration.java:90-107`) are
  repeatable `EnvVarsConverter` options
  (`common/options/Converters.java:600-670`): `NAME=VALUE` sets a fixed value
  (`NAME=` an empty one), `NAME` inherits the client's value, `=NAME` unsets.
  `--action_env=` / `--action_env==` fail with Bazel's `While parsing option
  --action_env=: Variable definitions must be in the form of a 'name=value',
  'name', or '=name' assignment`; a `no` prefix is `Illegal use of 'no' prefix
  on non-boolean option`. Entries normalize like
  `FragmentOptions.normalizeEnvVars` (`:119-129`): the last entry per name, at
  the name's first position.
* `--[no]incompatible_strict_action_env` (old name
  `experimental_strict_action_env`, default true,
  `bazel/rules/BazelRuleClassProvider.java:73-87`).
* `query` accepts `--action_env`/`--host_action_env` (it registers
  CoreOptions, `runtime/commands/QueryCommand.java:59-67`) and ignores them;
  `--test_env` and `--incompatible_strict_action_env` are unrecognized there.
* The options are part of the configuration identity only when they differ
  from their defaults, so existing configurations, output roots and action ids
  are unchanged. `--test_env` is part of the untrimmed identity only
  (TestOptions are trimmed from non-test targets); it is accepted and recorded
  (`NativeOptions::test_environment`, BuildConfigurationValue.setupTestEnvironment
  `:180-195`) for a future test command. `select()` on these options fails
  explicitly (not modelled for `config_setting`).
* `--action_env` never reaches repository rules or module extensions:
  `--incompatible_repo_env_ignores_action_env` keeps its Bazel 9.3 default
  true (`runtime/CommonCommandOptions.java:624-634`,
  `runtime/CommandEnvironment.java:342-375`). `--repo_env` (N40,
  docs/RC_OPTIONS.md) feeds only the repository environment, never actions.

## A configuration's shell environment

`BazelRuleClassProvider.SHELL_ACTION_ENV` (`:139-202`) on a non-Windows host:

1. strict (default): `PATH=/bin:/usr/bin:/usr/local/bin` fixed
   (`pathOrDefault`, `:347-361`); non-strict: `PATH` and `LD_LIBRARY_PATH`
   inherited from the client;
2. then the configuration's `--action_env` entries in order: set, inherit
   (replacing a fixed value), unset (removing the strict `PATH` too);
3. `RUNFILES_MANIFEST_ONLY=1` only without runfiles, which are always enabled
   natively.

`ActionEnvironment.split` (`actions/ActionEnvironment.java:62-79`) separates
the fixed and inherited parts (`crate::action_env::ActionEnvironment`). The
exec transition sets `action_env` to the parent's `--host_action_env` and
propagates `--host_action_env` and `--incompatible_strict_action_env`
(`builtin_exec_platforms.bzl:203-207, 221-229, 259`), so genrule tools and
`cfg = "exec"` dependencies run with `--host_action_env`, never
`--action_env`.

`ctx.configuration.default_shell_env` is
`BuildConfigurationValue.getLocalShellEnvironment`, the fixed part
(`analysis/config/BuildConfigurationValue.java:556-558`):
`ActionEnvironment::default_shell_env`. N23c's member reads the configured
target's `Context::action_env.default_shell_env()` (`CtxSurface::action_env`
is that same value, set by `crate::analysis`) instead of a PATH constant (the
former `analysis_runtime::DEFAULT_SHELL_PATH` is gone), so `--action_env`
and the exec configuration's `--host_action_env` are reflected
(`tests/action_env.rs` `default_shell_env_is_the_fixed_part`).

## Per-action environments

* genrule: the configuration's shell environment as is
  (`rules/genrule/GenRuleBase.java:210-219`).
* Starlark `run`/`run_shell` (`SpawnAction.createActionEnvironment`,
  `analysis/actions/SpawnAction.java:584-614`): with
  `use_default_shell_env=False` exactly `env` (fixed); with `True` the
  configuration's shell environment, minus the inherited names `env` sets,
  with `env` overriding fixed values.
* Resolution (`ActionEnvironment.resolve`, `:113-122`): the fixed values,
  then the client's values of the inherited names it sets (an inherited value
  wins over a fixed one of the same name; an unset name keeps the fixed value
  or stays absent). The client environment is the CLI's process environment,
  read once per build when an action first needs it (Bazel reads each name
  from the full client environment, `skyframe/ActionExecutionFunction.java:221-242`).
* `TMPDIR` is always set and always replaces an action `TMPDIR` (fixed or
  inherited), like `PosixLocalEnvProvider.rewriteLocalEnv`
  (`exec/local/PosixLocalEnvProvider.java:41-59`, used by the local and
  sandboxed runners): the client's `TMPDIR`, or `/tmp` when it is unset or
  empty, under every strategy (`crate::action::local_tmpdir`; tests use the
  same rule). Until action-tmpdir it was natively a fresh directory below the
  cache (`<cache>/work/<64-hex key>-<pid>-<n>/tmp`), which made Unix socket
  paths below TMPDIR exceed `sun_path` (~108 bytes; tsx's
  `listen EINVAL ... tsx-<uid>/<pid>.pipe`), the very problem bazel#4376 fixed
  with the short `/tmp` fallback. Pinned against genuine by
  `tests/fixtures/action_tmpdir` (genrule and test, standalone and sandboxed,
  client TMPDIR unset or set: 8/8 EXACT, `tests/sandbox.rs`
  `tmpdir_is_the_client_tmpdir_or_slash_tmp_like_genuine`). Genuine's
  linux-sandbox (unavailable on this host; genuine ran
  processwrapper-sandbox) additionally mounts a private, empty `/tmp`
  (`--incompatible_sandbox_hermetic_tmp`); the Landlock sandbox cannot mount,
  so `/tmp` and TMPDIR stay the shared host directories (docs/SANDBOX.md).
* Nothing else is passed: the former native `PATH=/usr/bin:/bin LANG=C
  LC_ALL=C` defaults are gone. Without a `PATH`, bash defines (but does not
  export) its built-in default, exactly as under Bazel.

## Cache keys

The key (format `native-action-v8` since the N41 merge; N42 introduced the
environment framing as `native-action-v6`) frames the fixed map and the inherited
names (`ActionEnvironment.addTo`, `:124-127`, used by `SpawnAction.computeKey`
`:396-409`) and the resolved client values of the inherited names (Bazel's
action cache digest covers them, `actions/ActionCacheChecker.java:291-301,
571-584`): changing an inherited client value reruns the action, a name the
client does not set is keyed as absent. `TMPDIR` handling is keyed as a
constant. `--test_env` changes no key.

Re-pinned keyed tests (deliberate, all for this change): every key changed
because the native PATH/LANG/LC_ALL constant is no longer framed and the
environment block changed; genrule actions additionally changed environment.
`tests/configured_target_keys.rs` (`action_keys_and_output_roots_match_the_pre_n18a_binary`
fastbuild and dbg sets, `existing_genrule_fixture_keys_are_pinned`) carries
the new values with this explanation.

Integration with N41 (one more deliberate rekey, pinned once for both): the
key and manifest format is `native-action-v8` (the manifests alone are
`native-action-v9` since fix-digest-memo, docs/DIGEST_MEMO.md "Upgrade note"). Reasons: N41's spawn argv
(genrules `bash -c 'source external/bazel_tools/tools/genrule/genrule-setup.sh; CMD'`
with the script as a keyed input instead of `bash -e -o pipefail -c CMD`;
run_shell's `$0` only with arguments; a File executable's execution path as
argv[0]) and the `k8-*` output roots (D1 closed) change the key of every
spawn and every consumer of a generated file, on top of N42's environment
framing. The executed environment and the remote cache key's
`Command.environment_variables` are one map, `crate::action::resolved_environment`
(fixed values, then the resolved inherited values; a declared TMPDIR stays
in the map), with TMPDIR replaced last by the executor
(`tests/remote_cache.rs`
`executed_environment_equals_the_resolved_environment_with_inherited_values`).
The keyed tests (`tests/configured_target_keys.rs`) were re-pinned once for
the merged tree; v7 manifests rebuild once.

## Paired capture

`artifacts/action_env-capture.py` builds one fixture with genuine Bazel 9.3.0
(`--spawn_strategy=local` and the default strategy, which ran
`processwrapper-sandbox` here; one output base per strategy) and natively,
under 12 option sets (default, `--action_env` set/inherit/unset/combined,
`--host_action_env` incl. `==PATH`, `--action_env=TMPDIR=/explicit`
and `EMPTY=`, `-c opt`, `--noincompatible_strict_action_env`, `--test_env`,
a changed inherited client value), and compares the `env | LC_ALL=C sort`
lines of a genrule, an exec-configured genrule tool, Starlark `run_shell` with
`use_default_shell_env` True/False/no env, an `env` override (HOME/FOO/TMPDIR),
a Starlark tool built in the exec configuration, and Starlark `run` True/False.
Final run: 217 EXACT output rows (both strategies), 4 EXACT_MESSAGE option
diagnostics, 48 REFERENCE_ONLY `ctx.configuration.default_shell_env` rows, 0
mismatches (`tests/fixtures/action_env/reference_receipt.json`; run 1 was
invalid evidence for the default strategy because both strategies shared an
output base and the sandboxed builds were action-cache hits; fixed in the
script only). Observed:

* Bazel's TMPDIR is the client's `TMPDIR` under both strategies (natively
  the same since action-tmpdir; normalized in this capture, recorded per row).
* An action `TMPDIR` (`env`, `--action_env=TMPDIR=...`) never reaches the
  process, yet `ctx.configuration.default_shell_env` lists it.
* `default_shell_env` is the fixed part: `{}` under `--action_env==PATH` and
  under `--noincompatible_strict_action_env`, and in the exec configuration it
  follows `--host_action_env` (e.g. `{"HOST":"1","PATH":...}`), never
  `--action_env`.
* A changed inherited client value reruns the dependent actions in Bazel (6
  local processes, the rest action-cache hits) and natively
  (`tests/action_env.rs` `inherited_client_values_are_keyed`).

## Not modelled

`--enable_runfiles=false` (`RUNFILES_MANIFEST_ONLY`), `--shell_executable`
(the shell is `/bin/bash`), Windows, the test action environment (no test
command), `select()` on the environment options; `--repo_env` reaches
repositories only (N40).
