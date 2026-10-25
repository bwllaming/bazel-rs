# `bazel-rs info`

`bazel-rs info` implements Bazel 9.3's InfoCommand
(`src/main/java/com/google/devtools/build/lib/runtime/commands/InfoCommand.java`
and the InfoItem classes under `runtime/commands/info/`, pinned source
`/tmp/opencode/bazel-9.3-reference`). Implementation: `src/info.rs` (items,
values, output), `src/main.rs` (`info_command`: options, configuration),
`src/output_tree.rs` (the directories the paths name). Tests:
`tests/info_command.rs` against genuine captures in
`tests/fixtures/info/genuine_capture.json` (re-capture with
`python3 tests/fixtures/info/capture.py`; scratch workspace and
`--output_user_root` under `/tmp/opencode`, `--batch --ignore_all_rc_files`).

## Behaviour (as genuine)

* `bazel-rs info` prints every non-hidden item as `key: value`, sorted by key
  (InfoCommand.getInfoItemMap's TreeMap, :346-352, Java `String` order).
* `bazel-rs info KEY` prints the value alone; several keys print `key: value`
  lines in request order, repeats included (`printKeys = residue.size() != 1`).
* Hidden items (`build-language`, `client-env`, `default-package-path`,
  `defaults-package`, `install_md5`, `peak-heap-size`, `starlark-semantics`,
  `used-heap-size-after-gc`, `worker_metrics`) print only when named.
* Unknown keys: the known keys are printed first, then
  `ERROR: unknown key(s): 'K' (did you mean 'L'?), 'M'` (each unknown key
  once, SpellChecker suggestions over every item name) and exit code 2
  (COMMAND_LINE_ERROR, KEY_NOT_RECOGNIZED).
* `--show_make_env` adds the make variables of the top-level configuration
  (BuildConfigurationValue.getMakeEnvironment): `TARGET_CPU`,
  `COMPILATION_MODE`, `BINDIR`, `GENDIR` and every `--define` (a define
  replaces a global variable of the same name; a variable replaces an item of
  the same name). They sort with the items (upper case first) and can be
  requested by name only with `--show_make_env`.
* `--info_output_type=stdout|response_proto`: `response_proto` values go to a
  server's response extensions; without a server (genuine `--batch`, and
  bazel-rs) nothing is printed.
* `info` inherits the build options (`inheritsOptionsFrom = BuildCommand`):
  rc `build` lines apply (`Inherited 'build' options` with `--announce_rc`),
  then `info` lines; every build option is accepted and validated as for
  `build`.
* Configuration options change the configuration-dependent items like
  genuine: `-c/--compilation_mode` (`k8-opt`), `--cpu` (`foo-fastbuild`),
  `--platform_suffix`; `--platforms` and Starlark build settings do not change
  `bazel-bin` (the top-level configuration is its own output-path baseline,
  crate::config_registry::mnemonic).
* `info` is fast (about 10 ms): no module resolution, loading, analysis or
  fetching, no analysis snapshot, no lockfile write, no memory watchdog; it
  creates nothing, not even the cache directory.

## Item mapping

`<cache>` is the native output base: `--cache-dir=PATH` (absolute, or
relative to the client directory), default `<workspace>/bazel-rs-out`.
Genuine's `<output_base>` is `<output_user_root>/<md5 of the workspace>`.

| item | genuine | native |
|---|---|---|
| `bazel-bin`, `bazel-genfiles` | `<output_base>/execroot/_main/bazel-out/<cfg>/bin` | `<cache>/execroot/_main/bazel-out/<cfg>/bin` |
| `bazel-testlogs` | `<output_base>/execroot/_main/bazel-out/<cfg>/testlogs` | `<cache>/execroot/_main/bazel-out/<cfg>/testlogs` |
| `execution_root` | `<output_base>/execroot/_main` | `<cache>/execroot/_main` |
| `output_path` | `<output_base>/execroot/_main/bazel-out` | `<cache>/execroot/_main/bazel-out` |
| `output_base` | `<output_base>` | `<cache>` |
| `command_log` | `<output_base>/command.log` | `<cache>/command.log` — never written (bazel-rs keeps no command log) |
| `repository_cache` | `<output_user_root>/cache/repos/v1` | `<cache>/repositories/downloads` (the native download cache, `sha256/<hex>` entries) |
| `install_base` | `<output_user_root>/install/<md5>` | the directory of the running bazel-rs binary |
| `install_md5` (hidden) | MD5 of the install | first 16 bytes (32 hex digits) of the binary's BLAKE3, the identity of crate::snapshot::exe_id |
| `release` | `release 9.3.0` | `release bazel-rs <version> (compatibility target Bazel 9.3.0)` |
| `workspace` | workspace root | same |
| `package_path`, `default-package-path` (hidden) | `%workspace%` | same (only the default package path) |
| `server_pid` | the server's (with `--batch`, the batch process's) pid | the bazel-rs process's pid |
| `server_log` | `<output_base>/java.log...` | empty, like ServerLogInfoItem without a log file (bazel-rs writes none) |
| `used-heap-size`, `committed-heap-size`, `used-heap-size-after-gc` (hidden) | JVM heap, `prettyPrintBytes` | resident set size of the process, same format |
| `max-heap-size` | JVM `-Xmx` | the memory ceiling a build would install (`--memory-limit`, `BAZEL_RS_MAX_MEMORY_MB` or the default cap, README "Memory ceiling") |
| `peak-heap-size` (hidden) | `unknown` in a fresh process | `unknown` |
| `gc-count`, `gc-time` | JVM collectors | `0`, `0ms` (no garbage collector) |
| `java-runtime`, `java-vm` | the embedded JDK | `none (bazel-rs is a native binary without a JVM)` |
| `java-home` | `<install_base>/embedded_tools/jdk` | empty (no embedded JDK) |
| `character-encoding` | JVM properties | `file.encoding = UTF-8, defaultCharset = UTF-8, sun.jnu.encoding = UTF-8` (bazel-rs accepts UTF-8 text only) |
| `local_resources` | `RAM=<MemTotal MB>MB, CPU=<processors>.0` | same sources (`/proc/meminfo`, available parallelism) |
| `client-env` (hidden) | `build --action_env=N=V` / `build --test_env=N=V` lines of the inherited names | same |
| `starlark-semantics` (hidden) | `StarlarkSemantics{...}` of the non-default options | same for the semantics options bazel-rs models (`experimental_google_legacy_api`, `incompatible_disable_starlark_host_transitions`, `incompatible_java_info_merge_runtime_module_flags`, `incompatible_disable_non_executable_java_binary`) |
| `defaults-package` (hidden) | empty | same |
| `worker_metrics` (hidden) | `No persistent workers active.` in a fresh process | same (no persistent workers) |
| `build-language` (hidden) | the build language protocol buffer | fails explicitly (needs Bazel's rule class registry) |

The genuine paths below `<output_base>` and the native paths below `<cache>`
are the same, so scripts like `$(bazel-rs info bazel-bin)/pkg/tool` and
`cd $(bazel-rs info execution_root)` keep working.

## The output tree

Native action outputs live in per-action cache entries
(`<cache>/actions/<key>/outputs/<exec path>`, docs/ACTION_OUTPUTS.md). After
every executing `build`, `test` or `run` (not `--nobuild`, not a failed
execution), every output of the request's actions — executed or a cache hit —
is linked at its exec path below `<cache>/execroot/_main`
(crate::output_tree):

* regular files are hardlinks of the read-only (`0555`) published output: a
  binary started as `$(bazel-rs info bazel-bin)/pkg/tool` sees that path as
  `/proc/self/exe`, so runfiles libraries find `tool.runfiles` beside it
  (a symlink is the fallback when a hardlink fails). Do not modify them: they
  share the inode with the action cache (a modified output fails its
  cache check and rebuilds, as for any tampering);
* tree artifacts and output symlinks are symlinks to the published path;
* runfiles trees link the whole `<exe>.runfiles` directory;
* a test's results link `bazel-out/<cfg>/testlogs/<pkg>/<name>` to the
  published testlogs below `<cache>/tests/<key>/` (test.log, test.xml, ...).

An entry that already links the same output costs one `lstat`/`readlink`;
anything else at the path is replaced by renaming a temporary link over it;
a directory link of an earlier build is never written through. Entries of
earlier builds stay until a later build replaces them, like genuine's output
tree. A failure to update the tree prints
`bazel-rs: warning: the output tree below execroot/ is incomplete: ...` and
never fails the command.

## Divergences

* `execution_root` holds `bazel-out/` only: no symlinks to the workspace's
  top-level entries or `external/` (genuine's execroot has them).
* `info --show_make_env` with an explicit `--platforms` fails explicitly:
  `$(TARGET_CPU)` is the target platform's CPU constraint, which needs the
  platform target loaded; for the default host platform it is the host CPU
  (`x86_64`). `info` does not load: an invalid `--platforms` label or
  Starlark build setting is not reported (genuine loads them when a
  configuration-dependent item is printed).
* Unrecognized options fail with the native option errors (exit 1), like the
  other native commands.
* No workspace: the native workspace-discovery error (genuine: `Bazel 'info'
  must be invoked from within a workspace ...`).
