# macOS (aarch64-apple-darwin)

bazel-rs builds and runs natively on macOS on Apple silicon
(`aarch64-apple-darwin`, macOS 11 or newer). Linux (x86_64, aarch64) stays the
reference host: every macOS difference lives behind `cfg(target_os)` gates,
mostly in the platform layer `src/platform.rs` (`src/platform/linux.rs`, the
former Linux code unchanged; `src/platform/macos.rs`), so Linux builds behave
exactly as before. Intel Macs (`x86_64-apple-darwin`) are not built.

Status: phase 1 (compile, run, CI) and phase 2 (release archive) are done;
phase 3: genuine Bazel 9.3.1 darwin-arm64 runs beside bazel-rs in the macOS
CI job on a mini corpus (see "Genuine parity on macOS"), darwin-sandbox is
the `sandboxed` backend, the integration tests with Linux output paths are
host-aware; what remains is at the end.

## What is ported

| Subsystem | Linux | macOS |
|---|---|---|
| Embedded `@bazel_tools` | `linux-x86_64` / `linux-arm64` bundle | the `darwin-arm64` bundle of the official `bazel-9.3.1-darwin-arm64` (`resources/bazel_tools/darwin-arm64/`, 420 files; same Starlark/BUILD files, darwin JDK and helpers) |
| Host metadata | `repository_ctx.os.name` `linux`, `arch` `amd64`/`aarch64`, extension factor `os:linux` | `mac os x` (the JVM's `os.name`, lower-cased), `aarch64`, factor `os:osx` (OS.java's canonical name); `@platforms//host` resolves to `@platforms//os:osx` + `@platforms//cpu:aarch64` through the platforms module like genuine |
| Default `--cpu`, output roots | `k8`, `bazel-out/k8-fastbuild` | `darwin_arm64`, `bazel-out/darwin_arm64-fastbuild` (AutoCpuConverter) |
| `sandboxed` strategy | linux-sandbox (namespaces) or the Landlock-restricted processwrapper-sandbox | genuine's **darwin-sandbox** (`src/sandbox/darwin.rs`): each spawn in its own sandbox execroot under `/usr/bin/sandbox-exec -f sandbox.sb`, genuine's profile (below); processwrapper-sandbox where `sandbox-exec` does not work (like genuine) |
| Spawn supervision | process-wrapper: child subreaper, pidfd, kill the group and every reparented descendant | process-wrapper without subreaper (like genuine's on macOS): the command runs in its own session/process group, which is SIGKILLed once the command exits and then waited for until it is empty (WaitForProcessGroupToTerminate; signal-0 probes, killed again every millisecond, at most 10 s); the exit is polled (10 ms) |
| Re-executing itself | `/proc/self/exe` | `std::env::current_exe()` (canonicalized once) |
| Process liveness, group | `/proc/<pid>`, `/proc/<pid>/stat` | `kill(pid, 0)`, `getpgid` |
| Resource limits | soft = hard (`GetExplicitSystemLimit` unimplemented) | soft = min(hard, `kern.maxfilesperproc` / `kern.maxprocperuid`), blaze_util_darwin.cc's rule |
| Memory cap watchdog | current RSS from `/proc/self/statm` | peak RSS (`getrusage` `ru_maxrss`, bytes): fires once RSS has ever exceeded the cap; default cap from `hw.memsize` |
| `sysctl` values | – | one `/usr/sbin/sysctl -n` run per process (no unsafe FFI) |
| CPU times (profile, BEP metrics) | `/proc/self/stat` | `getrusage` (self and children) |
| Host name (BEP, workspace status) | `/etc/hostname`, `/proc/sys/kernel/hostname` | `uname` nodename |
| Directory lookups (dir stamps) | `O_PATH` descriptors | `O_RDONLY` directory descriptors (an unreadable directory is inspected by path) |
| Mode changes that never follow links | `O_PATH` + `/proc/self/fd` chmod | `fstatat` + `fchmodat(AT_SYMLINK_NOFOLLOW)` after an inode check |
| Directory stamps | `statx` | `fstatat` |
| Sealed tree publication | rename of the read-only stage | root made owner-writable for the rename (xnu requires write permission on any renamed directory), sealed again at the destination |
| `--cache-dir` | as given | canonicalized (the symlinks /tmp, /var → /private/...), like genuine's output base |
| Digest memo settledness | birth time for owned files | change time only (macOS moves the birth time back with the mtime) |
| Lock files | `O_NOFOLLOW \| O_NONBLOCK` (Linux values) | the same flags with macOS's values |
| Spawn environment | exactly the action's | the action's plus `__CF_USER_TEXT_ENCODING` when the action does not set it: genuine's spawns (local, darwin-sandbox, tests, repository `execute`) all carry the server's CoreFoundation value; bazel-rs passes its own (it links CoreFoundation for the system certificate store, which sets the variable in every process that loads it — also in the re-executed supervisor, so the executor carries the spawn's value to it explicitly) |
| Client environment | the process environment | plus `DARWIN_USER_TEMP_DIR` and `DARWIN_USER_CACHE_DIR` after it, as genuine's client adds them (GetProcessedEnv, confstr; read with `/usr/bin/getconf` once per process) |
| jemalloc | arena binding | the same (tikv-jemalloc works on macOS) |

`unsafe` is used for nothing new: the macOS code uses rustix and nix safe
wrappers (`nix` 0.31.3 is the version `zip` already builds on macOS; rustix's
`system` feature for `uname` on macOS only). Landlock is a Linux-only
dependency.

## darwin-sandbox

`sandboxed` (and `darwin-sandbox`, registered like genuine's when
`sandbox-exec -p "(version 1) (allow default)" /usr/bin/true` succeeds) runs
each spawn in its own sandbox execroot under `/usr/bin/sandbox-exec -f
<sandbox dir>/sandbox.sb`; the sandbox worker supervises sandbox-exec,
which applies the profile and executes the command in place (an argv[0] of
its own, test spawns, goes through the hidden `__exec-arg0` helper). The
profile is DarwinSandboxedSpawnRunner.writeConfig's text: `(allow default)`,
`/bin/ps` executable outside the sandbox, `(deny file-write*)` except below
the always-writable directories (`/dev`, `/private/tmp`, `/private/var/tmp`,
the client's `DARWIN_USER_TEMP_DIR` / `DARWIN_USER_CACHE_DIR`, the user
database home's `Library/{Caches,Logs,Developer}`, each when it exists,
resolved), the sandbox execroot, TEST_TMPDIR, TMPDIR, every
`--sandbox_writable_path` (and its resolved target) and the statistics file
literal; `(deny file-read*)` below each `--sandbox_block_path` (resolved);
for `block-network` or `--nosandbox_default_allow_network` without
`requires-network`, the network denied except localhost and unix sockets.
genuine writes the writable directories from a hash set, bazel-rs sorted.
`--sandbox_debug` keeps the profile in the spawn's sandbox directory
(`<cache>/work/<key>-<pid>-<n>/sandbox.sb`). `BAZEL_RS_DARWIN_SANDBOX=off`
falls back to processwrapper-sandbox. The captured behaviour (host_parity's
darwin_sandbox case: what a spawn may read, write and reach per execution
requirement and flag, and the profiles) matches genuine's on the runner.
Spawn inputs are hardlinks of bazel-rs's content-addressed store where
genuine's sandbox links them (docs/HARDLINK_STAGING.md), as on Linux.

## What fails explicitly

- `linux-sandbox` is not a registered strategy (like genuine on macOS): the
  registry's "no strategy with that identifier was registered" error.
- The namespace sandbox's worker option `--namespaces` is unknown.

## What is weaker than on Linux (documented, not silent)

- No child subreaper: a descendant that leaves the spawn's process group
  (`setsid` daemons, double forks into another group) survives the spawn.
  Genuine's macOS process-wrapper has the same limit
  (process-wrapper-legacy.cc:114-127: it waits for the child, kills its
  process group and waits for the group to empty; nothing tracks processes
  that left it), so this is parity; a tracker would need `proc_listchildpids`
  (FFI, outside the unsafe policy) and would go beyond genuine.
- Where `sandbox-exec` does not work (nested sandboxes), `sandboxed` is
  processwrapper-sandbox and isolates nothing, like genuine's fallback.
- The memory watchdog compares the peak RSS with the cap (no safe API for
  the current RSS).
- Performance-only paths that are off: execution-root reuse (it reads the
  umask from /proc), fd-relative `lstat`s during staging (`O_PATH`), the
  automatic concurrent-revisions choice of `affected` (MemAvailable; it
  plans one revision after the other), `/proc` thread CPU diagnostics
  (`BAZEL_RS_TIMING`).

## Building and checking from Linux

The CI's macOS job builds natively. From Linux, `cargo check` of the macOS
target needs a C compiler for it (ring, jemalloc): zig works, with a stub
`sys/syscall.h` for jemalloc's configure (zig's macOS headers lack it; only
`check` uses it, nothing is linked):

```sh
rustup target add aarch64-apple-darwin
scripts/setup-zig.sh
mkdir -p /tmp/darwin-cc/inc/sys && : > /tmp/darwin-cc/inc/sys/syscall.h
python3 -c 'import sys; sys.path.insert(0, "scripts"); import release
print(release.zig_wrapper(sys.argv[1], "aarch64-macos").replace("\"$@\"", "-idirafter /tmp/darwin-cc/inc \"$@\""))' \
  "$PWD/.direnv/zig/zig" > /tmp/darwin-cc/cc && chmod +x /tmp/darwin-cc/cc
CC_aarch64_apple_darwin=/tmp/darwin-cc/cc cargo clippy --locked --target aarch64-apple-darwin --all-targets -- -D warnings
```

## CI

`.github/workflows/ci.yml` job `macos` on `macos-15` (GitHub's arm64 macOS
runner; `macos-14` is no longer offered): clippy for the target, `cargo
build`, `scripts/smoke.py target/debug/bazel-rs --network` (release.py's
smoke test: `--version`, `version`, `help`, a query and two genrule builds of
an offline workspace, then a built-in-profile build that materializes the
embedded `@bazel_tools` and checks `@platforms//host` =
`@platforms//os:osx,@platforms//cpu:aarch64`; it runs with macOS's default
`TMPDIR` below `/var`), then with `TMPDIR=$RUNNER_TEMP` the library unit tests
and `scripts/ci-integration-tests-macos.txt`: the hermetic programs of the
Linux job (`scripts/ci-integration-tests.txt`) minus those that use GNU `stat` in their
actions (hardlink_staging, output_permissions), non-UTF-8 file names
(bazel_lexer_strings), glibc's error texts (libc_string) or /proc telemetry
(plan_timings). The integration tests wrap the CLI in GNU timeout(1):
`tests/support/temp_root.rs`'s `TIMEOUT` is Homebrew coreutils' `gtimeout`
on macOS, which the job installs. Programs whose genuine captures come from
a Linux x86_64 host (`bazel-out/k8-fastbuild/...`) derive the output
directories from the host's default `--cpu` (`tests/support/host_cpu.rs`:
`host_out` turns an expectation into this host's paths, `as_k8` this host's
output into the capture's spelling, leaving a fixture's literal `k8` alone):
actions_params, args_map_each, artifact_identity, cache_gc,
directory_expander, filegroup_output_group, native, parallel_cli,
py_label_runfiles_path, py_repo_mapping, py_shared_library, runfiles; the
environment listings of action_env and actions_params leave out the
`__CF_USER_TEXT_ENCODING` genuine's macOS spawns carry. py_shared_library
stays Linux-only in CI: its fixture holds `libfoo.so` and `libfoo.SO`,
one file on APFS's default case-insensitive volumes.
Linux-only tests carry `#[cfg(target_os = "linux")]` with the reason:
expectations of the Linux x86_64 host (`k8`), GNU `stat`/`timeout`, `/proc`,
ETXTBSY (macOS executes files open for writing), non-UTF-8 file names (APFS
refuses them), Linux errno numbers.

## Release

`scripts/release.py` target `aarch64-apple-darwin` (docs/RELEASING.md): built
natively on `macos-15` by the release workflow's `macos-arm64` matrix entry
(`cargo build --release --target aarch64-apple-darwin`, Apple clang, ld64's
ad-hoc signature, which arm64 macOS requires), checked as Mach-O (arm64, code
signature present, system dylibs only), smoke-tested with the network smoke
(`@platforms//os:osx`), packaged as
`bazel-rs-<version>-aarch64-apple-darwin.tar.gz` with its `.sha256` and in
`SHA256SUMS`; after publishing, the `verify-macos` job downloads it with curl,
checks the sha256 and the signature (`codesign --verify`) and runs
`bazel-rs --version`. Not notarized: curl downloads run as they are; a
browser download is quarantined (`xattr -d com.apple.quarantine bazel-rs`).

Rehearsed before the first such release (a temporary workflow on the port's
pull request, 2026-11-24): `release.py --ci --dry-run --stage build
--smoke-network --target aarch64-apple-darwin` on `macos-15` built, checked
("macOS ≥ 11.0; system libraries only": libSystem, libiconv, the Security and
CoreFoundation frameworks; `Signature=adhoc`), smoke-tested (query, two
builds, the built-in profile build) and packaged the archive, whose extracted
binary passed `codesign --verify` and ran `--version`.

## Genuine parity on macOS

The macOS CI job downloads the official `bazel-9.3.1-darwin-arm64` release
(`scripts/host_parity.py fetch-genuine`: releases.bazel.build, the published
`.sha256` checked against the pin in the script) and runs
`scripts/host_parity.py run --strict`: genuine (`--batch`, its own output
user root) and bazel-rs (its own cache directory) side by side on copies of
the same fixtures, each with its own HOME and TMPDIR under `env -i`. Per
step it compares the exit code, stdout, selected stderr lines (the
`INFO: N processes` strategies), output files, runfiles trees, test logs
and the darwin-sandbox profiles, after normalizing the tools' own paths
(output base, execution root, cache, workspace, spawn sandboxes),
configuration checksums and label spelling (bazel-rs's are its own by
design, docs/CQUERY.md) and times. A difference fails the step unless it is
listed in the script's KNOWN table with its reason; the JSON report is the
job's `host-parity-macos` artifact. The same script runs on Linux against
the Linux genuine binary (the darwin-only case skipped).

| Case | What it covers | Result on macos-15 |
|---|---|---|
| host_parity (tests/fixtures/host_parity) | `info` paths, query/cquery/aquery, Make variables (`TARGET_CPU`, `BINDIR`, ...), the genrule action environment and shell, selects on `@platforms//os:macos`, `--cpu=darwin_arm64`, `--host_cpu`, `@bazel_tools//src/conditions:darwin_arm64`; an exec tool's runfiles; `sh_test`'s environment, runfiles, `test.xml`; `run`; what rules see (`ctx.var`, `ctx.fragments.apple` / `cpp` / `platform`, the sh toolchain path, the Xcode configuration of `@local_config_xcode` through `xcode_config_label`, `apple_common.apple_host_system_env` / `target_apple_env`); a repository rule's `os` and environment | equal (cquery `deps()` omits toolchain_type targets: known, also on Linux) |
| darwin_sandbox (tests/fixtures/darwin_sandbox) | reads, writes (workspace, execroot, /tmp, TMPDIR, home, `~/Library/Caches`, /Users/Shared), network, `/bin/ps` per `block-network` / `requires-network` / `no-sandbox` / `local`, `--sandbox_default_allow_network`, `--sandbox_writable_path`, `--sandbox_block_path` on a symlink; the profiles | equal (input kind: hardlinks, known) |
| cc_hello | `cc_library` / `cc_binary` / `cc_test` with rules_cc's auto-configured Xcode clang toolchain: cquery, the CppCompile/CppArchive/CppLink command lines, build, run, test | equal |
| py_go_hello | rules_python's hermetic interpreter and rules_go's downloaded SDK: build, run, test, the Go actions | see the report |
| aquery_genrules, runfiles_symlinks, test_args | platform-neutral fixtures of the Linux suite | equal |

Fixed on the way: `apple_common.apple_host_system_env` and
`target_apple_env` (now the verbatim `@_builtins` functions), the default
`sandboxed` strategy (darwin-sandbox), the client environment's
`DARWIN_USER_*` directories, `__CF_USER_TEXT_ENCODING` in spawns.

## Phase 3: what remains

- apple_support's `local_config_apple_cc` (wrapped_clang, `apple_cc_toolchain`,
  `objc_library`) is not exercised: rules_cc's own `cc_configure` (Xcode's
  clang through `cc_wrapper.sh`) is what a workspace without apple_support
  gets, and that matches genuine.
- A descendant tracker beyond genuine's (see above): not pursued.
- Execution-root reuse and fd-relative staging on macOS.
