# Workspace status (N63)

The workspace status action of Bazel 9.3
(`src/main/java/com/google/devtools/build/lib/bazel/BazelWorkspaceStatusModule.java`,
`.../analysis/WorkspaceStatusAction.java`; sources cited from the pinned
`bazel-9.3.0`), the `ctx.info_file`/`ctx.version_file` artifacts it produces,
and the C++ build-info files translated from them
(`.../analysis/actions/BuildInfoFileWriteAction.java`). Paired genuine/native
capture: `tests/fixtures/workspace_status2/reference_receipt.json`
(5 byte-exact rows incl. a linked `cc_binary`, 2 exact-text failure rows, 1
declared native gap), written by `artifacts/workspace_status-capture.py`.

## The action

* `--workspace_status_command=<path>` (also an rc option) is a **build-command
  option**: `BazelWorkspaceStatusModule.getCommandOptions:323-328` returns
  `WorkspaceStatusAction.Options` for `build` only, and every command that
  **inherits build options** accepts it — `BlazeCommandUtils.getOptions
  :90-96` recurses `inheritsOptionsFrom` (RunCommand `:114` and TestCommand
  `:69` inherit BuildCommand; CoverageCommand `:108` inherits TestCommand;
  AqueryCommand/CqueryCommand inherit too), and rc `build` lines apply to
  those commands the same way. `query` does **not** inherit build options
  (QueryCommand lists its own options with no `inheritsOptionsFrom`) and
  rejects the flag as unrecognized. The native accepts it for
  `build`/`run`/`test` (its `builds()` set) and rejects it for `query`.
  `--embed_label=<label>` is its companion option; its converter rejects a
  value containing a newline: `Value must not contain multiple lines`
  (`WorkspaceStatusAction.OneLineStringConverter:216-233`). The flag value
  is a `PathFragment` (`OptionsUtils.PathFragmentConverter`), so
  `./status.sh` normalizes to `status.sh` before anything runs — a relative
  script is found through `PATH` from the **workspace directory**, not `./`
  (genuine exits 127 for `./status.sh`; the native reproduces that, receipt
  row `ws/relative_script`).
* The command, when set, runs once per build through `/bin/sh -c <value>`
  with the full **client environment** and the **workspace root** as working
  directory (`BazelWorkspaceStatusActionContext.getCommand:305-320`); its
  stdout is parsed, its stderr passes through to the build's stderr
  (`:113-119`). It is **not sandboxed** and not keyed — matching Bazel, which
  runs it outside any action sandbox as a plain shell command.
* A failing script fails the build as an **ordinary action failure —
  NOT an infrastructure/catastrophe failure**: `createExecutionException`
  builds an `ActionExecutionException` with `catastrophe=false`
  (`BazelWorkspaceStatusAction:122-128` via `WorkspaceStatusAction
  .createExecutionException:205-213`, detailed code
  `WorkspaceStatus.NON_ZERO_EXIT`, exit code 1), with Bazel's text:
  `Failed to determine workspace status: Process exited with status N`,
  or `Process terminated by signal N` when the shell died by a signal
  (`TerminationStatus.toString:230-238`). Genuine's framing is
  `ERROR: <builtin>: BazelWorkspaceStatusAction stable-status.txt failed:
  ...`; the native's executor framing is `action
  'BazelWorkspaceStatusAction#volatile': ...` with the same message text,
  reported **once** (Bazel has one action; receipt rows `ws/script_failed`,
  `ws/no_consumer_fails`).
* The status action runs on **every build that executes**, consumer or
  not: Bazel adds both status artifacts to `artifactsToBuild`
  unconditionally (`BuildView.createResult:647-650,675`) and the action
  `executeUnconditionally`+`isVolatile` (`WorkspaceStatusAction:186-194`),
  so a failing script fails even a build whose targets never touch the
  status (receipt row `ws/no_consumer_fails`). It does **not** run under
  `--nobuild` (no execution phase: `BuildRequestOptions
  .performExecutionPhase:154-160`, `ExecutionTool.executeBuild:364-366`
  — analysis only) nor on `query` (no BuildView; QueryCommand never
  evaluates `WorkspaceStatusValue.BUILD_INFO_KEY`). The native plans the
  status node(s) once per executing `build`/`test`/`run` invocation
  (skipped under `--nobuild`) and computes the status exactly once per
  invocation behind a `OnceLock`, so N62 parallel planning cannot run
  the script twice.
* With no `--workspace_status_command` (the default `""`), no script runs;
  the files still exist with the built-in keys (`getCommand` returns null,
  `:310-311`).

## The files

`bazel-out/stable-status.txt` and `bazel-out/volatile-status.txt`
(`SkyframeExecutor.makeWorkspaceStatusAction:1474-1496`): `key value` lines,
keys sorted (TreeMap), one trailing newline, and a trailing space after a key
with an empty value (`printStatusMap:151-158` — `BUILD_EMBED_LABEL ` with the
default empty label). The built-in keys come from Bazel itself, **not** the
script (`BazelWorkspaceStatusAction.execute:180-198`):

| file | keys |
| --- | --- |
| stable | `BUILD_EMBED_LABEL` (`--embed_label`, default empty), `BUILD_HOST` (the local host name; `NetUtil.computeShortHostName:67-73` — on Linux the JVM reads the container's `/etc/hostname`, which differs from the host's `/proc/sys/kernel/hostname` in a container), `BUILD_USER` (the passwd name of the process uid, Java's `user.name`) |
| volatile | `BUILD_TIMESTAMP` (epoch **seconds**) and `FORMATTED_DATE` (`yyyy MMM dd HH mm ss EEE` in UTC, `:80-85`) |

`SOURCE_DATE_EPOCH` (client environment, seconds) pins the timestamp
(`getCurrentTimeMillis:231-243`); the capture uses
`SOURCE_DATE_EPOCH=1792511811` for byte-deterministic files.

The script's own keys split by `isStableKey` (`:132-137`): `STABLE_*` plus
the three special names go to the stable file, everything else to the
volatile one; a script key **overrides** the built-in value (the parse
results are put after the built-ins). Values are trimmed, inner spacing
survives, lines without a space are ignored, a later duplicate key wins
(`parseWorkspaceStatus:139-149`).

## Caching semantics

The action is volatile and executes unconditionally
(`isVolatile`/`executeUnconditionally:186-194`): the volatile file is
rewritten every build and is a **constant-metadata** artifact — its changing
content never rebuilds a consumer (`:211-215`, `Artifact.isConstantMetadata`;
the volatile build-info outputs are constant metadata too,
`StarlarkActionFactory.transformBuildInfoFile:514-519`). The stable file is
only rewritten when its content changes (`FileSystemUtils.maybeUpdateContent`,
`:205-209`), and it is a regular artifact: a changed stable key rebuilds
consumers. The native models Bazel's ONE action (both outputs,
`BazelWorkspaceStatusModule:87-98`) with two system write nodes
(`BazelWorkspaceStatusAction#stable`/`#volatile`) sharing the one computed
status, planned once per executing invocation (above), and a
`constant_metadata` input list excluded from action keys
(`crate::action::prepare`); a failed computation plans ONE failing node so
the failure line appears once. `tests/workspace_status.rs` and the receipt's
`native_only/warm_rebuild` row pin the rebuild behavior.

The volatile `TranslateBuildInfo` action must NOT re-run when only the
timestamp changes: its key is Bazel's `computeKey:189-197` (GUID
`7e4657a6-…`, isVolatile, the callback's bzl digest) plus the template
input's digest, **excluding the substitutions**, with the volatile status
input constant-metadata (`StarlarkActionFactory:510-519`); the stable
variant's status input remains a regular keyed input (a changed stable key
re-keys it — equivalent). The native keys the resolved action
(`Transform::TemplateKeyed`) by exactly those components plus the
non-timestamp part of the callback result (the volatile SCM keys) and
publishes the cached output on a hit. **Documented divergence:** genuine
re-executes the volatile translate every build, so its `volatile_file.h`
always carries the current timestamp; the native keeps the first-produced
bytes under a changed `SOURCE_DATE_EPOCH` (first-timestamp-wins) — the
receipt's extended `native_only/warm_rebuild` row pins this
(`volatile_header_unchanged`).

## ctx.info_file / ctx.version_file

`StarlarkRuleContext.java:994-1004`: owner-less generated Files (path
`bazel-out/{stable,volatile}-status.txt`, `owner` None, root `bazel-out`).
Since N63 they are real generated artifacts — passing them as action inputs
works (the N54 receipt `tests/fixtures/workspace_status/reference_receipt.json`
recorded the old explicit failure; `tests/fixtures/workspace_status2/` is the
current evidence). The producer is the system workspace-status action, so an
action consuming a status file depends on it in the execution graph.

## C++ build info (`_build_info_files`)

`@bazel_tools//tools/build_defs/build_info:cc_build_info`
(`bazel_cc_build_info.bzl`, embedded in the native's bazel_tools archive)
translates the status files through
`actions.transform_version_file`/`transform_info_file`
(`StarlarkActionFactory.java:475-499`, builtin-restricted to the
`tools/build_defs/build_info` packages). The volatile transform's output
`volatile_file.h` substitutes `{BUILD_SCM_REVISION}`/`{BUILD_SCM_STATUS}`
(defaults `0`/empty) and `{BUILD_TIMESTAMP}`; the stable one's
`non_volatile_file.h` substitutes `{BUILD_EMBED_LABEL}`/`{BUILD_HOST}`/
`{BUILD_USER}` (defaults `""`/`hostname`/`username`). The native keeps the
translation callback (a frozen top-level function) like Bazel keeps its
`StarlarkFunction` (`BuildInfoFileWriteAction.computeKey:189-197` keys GUID +
isVolatile + the callback's bzl digest — never the status content or the
substitutions), evaluates it during planning against the build's status and
executes the action as a keyed template expansion (`Transform::TemplateKeyed`,
`docs/ACTIONS_PARAMS.md`). The stamping
semantics of `cc_binary` (`stamp` 0/1/-1, `cc_helper_internal.bzl`'s
`is_stamping_enabled`/`should_stamp`, `--stamp` via
`ctx.configuration.stamp_binaries()`): with `stamp = 0` (the default) links
use the redacted symlink; `stamp = 1`/`--stamp` select the non-redacted
group. `SKIP_ANALYSIS_TIME_FILETYPE_CHECK` on `cc_binary`'s `deps`
(Attribute.java:2012-2016, used by RuleContext.java:1946-1949) is honored, so
`cc_binary` over a built `cc_library` (and rules_cc's root `BUILD`
`link_extra_lib` implicit dependency) builds and links **byte-identically to
genuine** (receipt row `cc/primary`: `//bin:tool`, both stamped headers).

## Explicit gaps kept

* **cc_library `linkstamp`** — not yet modelled (Short): rules_cc's
  `LinkstampInfo` provider fields are `wrap_with_check_private_api` closures
  (`create_linkstamp.bzl:41-44`); a local closure cannot cross analyses in
  the native depset representation, so a `cc_binary` with a
  linkstamp-carrying dependency fails explicitly (receipt row
  `cc/linkstamp` — genuine builds and embeds the stamps).
* The `BuildInfoEvent` (Bazel's build event of the combined status map) is
  not posted: the native has no build-event stream.
* Genuine reports the failing action as
  `ERROR: <builtin>: BazelWorkspaceStatusAction stable-status.txt failed:`;
  the native's executor framing is `action 'BazelWorkspaceStatusAction#...'`
  with the same message text.
