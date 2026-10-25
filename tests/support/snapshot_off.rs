// Shared by integration tests (`include!("support/snapshot_off.rs")`) whose
// subject is something the analysis snapshot legitimately changes. The
// snapshot is on by default (L2-W7, docs/ANALYSIS_SNAPSHOT.md): a `build`,
// `test` or `run` sets up its store `<cache>/worktrees/<name>-<hash>/snapshots/`
// (the workspace's state directory, docs/WORKTREES.md) at the check point,
// before loading and analysis (also when the analysis then fails), saves a
// snapshot there when it succeeds (also under `--nobuild`), and an unchanged
// re-run is a hit that skips loading and analysis altogether. So a test that
// asserts
// * the cache directory's contents (`no cache directory after a failed
//   analysis`, `no file in the cache under --nobuild`),
// * the full analysis's own reports on a re-run (PERF-STAGE rows, the
//   planned request's encoding written by `BAZEL_RS_SNAPSHOT_PLAN_OUT`), or
// * what the warm probe of a re-run's analysis does (repairing an edited
//   repository tree, which a hit does not look into: the documented per-tree
//   trade-off)
// runs those commands with the snapshot off, `.envs([SNAPSHOT_OFF])`, and says
// why at the call site. Everything else runs with the default; the
// snapshot's own suite (tests/snapshot.rs) compares `on` with `off` step by
// step.

/// `BAZEL_RS_SNAPSHOT=off` for one bazel-rs command (see above).
const SNAPSHOT_OFF: (&str, &str) = ("BAZEL_RS_SNAPSHOT", "off");
