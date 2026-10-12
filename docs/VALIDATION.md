# Validation evidence — 2026-10-12

This is the **foundation-milestone snapshot**. The subsequent local-loading and
parallel-execution milestone is documented in `LOADING_VALIDATION.md`.

## Scope

Native subset only; **not full Bazel parity**. No upstream fallback exists in the
Rust binary. `~/code` application builds/tests are not supported yet.

## Toolchain and local checks

- Linux x86_64; Rust/Cargo 1.99.0, rust-analyzer, rustfmt and Clippy installed
  project-locally and available through authorized direnv.
- Official Bazel 9.3.0 downloaded separately, SHA256 verified; its `--version`
  reports `bazel 9.3.0`.
- `cargo test --locked`: 49 unit tests and 11 integration tests passed.
- Python validation-harness suite: 25 unit tests passed.
- `cargo clippy --locked --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check` and release build: passed.

Tests cover label/path validation, package and nested-workspace boundaries,
strict Starlark attributes and loading failures, visibility, generated outputs,
dependency cycles, preflight failures before any action runs, Make expansion,
source-order preservation, fail-fast shell behavior, cache invalidation,
corruption, executable bits, atomic publication, concurrent same-key execution,
bounded log diagnostics and explicit unsupported-command errors.

## Direct main-repo native probe

Executed from `/home/coder/code`, without sourcing its environment or invoking its
wrapper:

```sh
/home/coder/bazel/target/release/bazel-rs --ignore_all_rc_files query //src/docker:config
/home/coder/bazel/target/release/bazel-rs --ignore_all_rc_files \
  --cache-dir=/tmp/opencode/bazel-rs-code-smoke build //src/docker:config
```

Both succeeded. The filegroup resolves to `src/docker/config.json`, with **zero
actions executed**; this is not evidence that Go/Python/Node tests have run.

```sh
/home/coder/bazel/target/release/bazel-rs --ignore_all_rc_files \
  query //src/beyondwork.ai/utils/str:all
```

Failed explicitly with `load() is unsupported; no .bzl loader is installed`.
This is a known compatibility boundary, not a parity pass.

The main repo pins Bazel 9.1.1; that pin and all existing local edits are preserved.
The native probe intentionally opts out of rc evaluation. Results do not imply
equivalence under the repo's normal wrapper/configuration.

The main repo's tracked diff SHA256 before and after the probe matched:
`3755abe6f8dcab5332d2feb50a321fa72a1d872cbc2d1f7b079e2669098a13d3`.
Its porcelain status, including pre-existing untracked paths, was unchanged.

## Independent Bazel 9.3 differential run

`python3 scripts/validate.py --iterations 20 --timeout 90` completed with
**PASS (scoped parity only)**:

- Fixture query labels matched on all invocations.
- Native and reference `//:upper` builds produced exactly
  `HELLO FROM NATIVE RUST` followed by a newline, on first build and unchanged rebuild.
- Main-repo `//src/docker:config` query labels and filegroup contents matched.
- Native Go-package loading failure was recorded separately as an unsupported
  feature, never counted as a parity success or a test execution.
- Private reference servers were shut down and temporary output trees removed.

The reproducible harness stores exact commands, exit statuses, timings and
output evidence in `artifacts/validation.json` (ignored generated artifact).
The test does not use the repo wrapper, does not enable remote cache/execution/BES,
does not update MODULE lockfiles, and does not create convenience symlinks.

## Exploratory query timings

Final release binary; fixture `query //:all`; 20 repeated invocations per engine:

| Engine | Median | p95 |
| --- | ---: | ---: |
| Native Rust process | 5.73 ms | 10.59 ms |
| Official Bazel 9.3, warm server | 34.23 ms | 49.79 ms |

The fixture contains three rules. These numbers **do not establish whole-repo
speedups or feature parity**. Trials were grouped, not interleaved. Native starts
a fresh process each time; reference uses its persistent server, a 512 MiB heap
and two JVM processors. OS page caches were not flushed. The first native query
took 15.29 ms; the first reference query took 22.02 s including fresh installation,
JVM/server startup and repository/module work. That first reference time is not
an isolated client-startup measurement and must not be presented as such.

Reference SHA256:
`d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8`.
Native release SHA256:
`b2e9fdd769082b21cf3e98ccd591f287a0c25b7476abd99e14e76d981d16923b`.
