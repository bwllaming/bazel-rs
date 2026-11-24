# macOS (aarch64-apple-darwin)

bazel-rs builds and runs natively on macOS on Apple silicon
(`aarch64-apple-darwin`, macOS 11 or newer). Linux (x86_64, aarch64) stays the
reference host: every macOS difference lives behind `cfg(target_os)` gates,
mostly in the platform layer `src/platform.rs` (`src/platform/linux.rs`, the
former Linux code unchanged; `src/platform/macos.rs`), so Linux builds behave
exactly as before. Intel Macs (`x86_64-apple-darwin`) are not built.

Status: phase 1 (compile, run, CI) and phase 2 (release archive) are done;
phase 3 (genuine-captured parity on a Mac, the Xcode C++ toolchain,
darwin-sandbox) is open — see the end.

## What is ported

| Subsystem | Linux | macOS |
|---|---|---|
| Embedded `@bazel_tools` | `linux-x86_64` / `linux-arm64` bundle | the `darwin-arm64` bundle of the official `bazel-9.3.1-darwin-arm64` (`resources/bazel_tools/darwin-arm64/`, 420 files; same Starlark/BUILD files, darwin JDK and helpers) |
| Host metadata | `repository_ctx.os.name` `linux`, `arch` `amd64`/`aarch64`, extension factor `os:linux` | `mac os x` (the JVM's `os.name`, lower-cased), `aarch64`, factor `os:osx` (OS.java's canonical name); `@platforms//host` resolves to `@platforms//os:osx` + `@platforms//cpu:aarch64` through the platforms module like genuine |
| Default `--cpu`, output roots | `k8`, `bazel-out/k8-fastbuild` | `darwin_arm64`, `bazel-out/darwin_arm64-fastbuild` (AutoCpuConverter) |
| `sandboxed` strategy | linux-sandbox (namespaces) or the Landlock-restricted processwrapper-sandbox | genuine's **processwrapper-sandbox**: each spawn in its own sandbox execroot, no isolation (`Backend::NoIsolation`; runner name `processwrapper-sandbox`) |
| Spawn supervision | process-wrapper: child subreaper, pidfd, kill the group and every reparented descendant | process-wrapper without subreaper (like genuine's on macOS): the command runs in its own session/process group, which is SIGKILLed once the command exits; the exit is polled (10 ms) |
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
| Spawn environment | exactly the action's | the same: CoreFoundation (linked for the system certificate store) adds `__CF_USER_TEXT_ENCODING` to every process that loads it, so the executor carries the spawn's own value (or its absence) to the re-executed supervisor, which restores it before starting the command |
| jemalloc | arena binding | the same (tikv-jemalloc works on macOS) |

`unsafe` is used for nothing new: the macOS code uses rustix and nix safe
wrappers (`nix` 0.31.3 is the version `zip` already builds on macOS; rustix's
`system` feature for `uname` on macOS only). Landlock is a Linux-only
dependency.

## What fails explicitly

- `--spawn_strategy=darwin-sandbox` (and `--strategy=M=darwin-sandbox`):
  "the darwin-sandbox strategy is not implemented on macOS (requested for …)".
- `linux-sandbox` is not a registered strategy (like genuine on macOS): the
  registry's "no strategy with that identifier was registered" error.
- The namespace sandbox's worker option `--namespaces` is unknown.

## What is weaker than on Linux (documented, not silent)

- No child subreaper: a descendant that leaves the spawn's process group
  (`setsid` daemons, double forks into another group) survives the spawn
  (genuine's macOS process-wrapper has the same limit).
- `sandboxed` isolates nothing (genuine's processwrapper-sandbox); genuine on
  macOS would use darwin-sandbox, so the summary says
  `processwrapper-sandbox` where genuine says `darwin-sandbox`.
  `--sandbox_block_path`, `--sandbox_writable_path` and `block-network`
  are ignored like processwrapper-sandbox does.
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
Linux job (`scripts/ci-integration-tests.txt`) minus those whose expectations
are the Linux x86_64 host's (`bazel-out/k8-*` paths: actions_params,
args_map_each, artifact_identity, cache_gc, directory_expander,
filegroup_output_group, native, parallel_cli, py_label_runfiles_path,
py_repo_mapping, py_shared_library, runfiles), that use GNU `stat` in their
actions (hardlink_staging, output_permissions), non-UTF-8 file names
(bazel_lexer_strings), glibc's error texts (libc_string) or /proc telemetry
(plan_timings). The integration tests wrap the CLI in GNU timeout(1):
`tests/support/temp_root.rs`'s `TIMEOUT` is Homebrew coreutils' `gtimeout`
on macOS, which the job installs.
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

## Phase 3

- Genuine-captured fixtures on a Mac runner (bazelisk / `bazel-9.3.1-darwin-arm64`)
  and a corpus subset on macOS.
- The C/C++ toolchain: genuine's `cc_configure` on macOS (Xcode /
  apple_support's `local_config_apple_cc`, `xcode_locator`, wrapped_clang);
  today the repository rules run as written, unmodelled native parts fail.
- darwin-sandbox (`sandbox-exec` profiles, DarwinSandboxedSpawnRunner) as the
  default `sandboxed` backend.
- A descendant tracker for the process-wrapper (kqueue `EVFILT_PROC` /
  `NOTE_TRACK` is gone on modern macOS; `proc_listchildpids` needs FFI).
- Execution-root reuse and fd-relative staging on macOS.
