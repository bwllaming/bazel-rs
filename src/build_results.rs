//! The build results of `build`, `test` and `run` on stderr, like Bazel's
//! BuildResultPrinter (buildtool/BuildResultPrinter.java; genuine captures
//! in tests/fixtures/build_results):
//!
//! ```text
//! Target //pkg:gen up-to-date:
//!   <output tree>/bazel-out/k8-fastbuild/bin/pkg/out.txt
//! Target //pkg:files up-to-date (nothing to build)
//! Target //pkg:broken failed to build
//! Aspect //pkg:defs.bzl%asp of //pkg:gen up-to-date:
//!   <output tree>/bazel-out/k8-fastbuild/bin/pkg/gen.asp.txt
//! Use --verbose_failures to see the command lines of failed build steps.
//! ```
//!
//! * `--show_result=N` (default 1) budgets the non-empty and failed results;
//!   past it nothing prints. When the targets and aspects outnumber N, the
//!   `(nothing to build)` lines are left out.
//! * The paths are the artifacts' paths in the output tree
//!   (crate::output_tree, `$(bazel-rs info execution_root)/bazel-out/…`):
//!   what Bazel prints without convenience symlinks (`--symlink_prefix=/`;
//!   bazel-rs creates none, so no `bazel-bin/…` relative form).
//! * Only after an execution phase (not under `--nobuild`).
//!
//! `BAZEL_RS_RESULT_LINES=1` (for scripts and the test suite) also prints
//! the earlier native result lines, `LABEL<TAB>PATH` per artifact at its
//! executed path (in the action cache), on stdout for `build` and on stderr
//! for `test` and `run` (whose stdout is the tests' and the program's).

use crate::engine::BuildSummary;
use std::io::Write;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static SHOW_RESULT: AtomicUsize = AtomicUsize::new(1);
static VERBOSE_FAILURES: AtomicBool = AtomicBool::new(false);

/// `--show_result` and `--verbose_failures` of this command.
pub fn set_options(show_result: usize, verbose_failures: bool) {
    SHOW_RESULT.store(show_result, Ordering::Relaxed);
    VERBOSE_FAILURES.store(verbose_failures, Ordering::Relaxed);
}

/// `--verbose_failures` of this command (crate::action_console's failure
/// messages print the complete command).
pub fn verbose_failures() -> bool {
    VERBOSE_FAILURES.load(Ordering::Relaxed)
}

/// The native `LABEL<TAB>PATH` result lines are requested
/// (`BAZEL_RS_RESULT_LINES=1`).
pub fn native_lines() -> bool {
    // snapshot-unrecorded: a console format switch read after execution,
    // never an input of analysis or planning.
    std::env::var_os("BAZEL_RS_RESULT_LINES").is_some_and(|value| value == "1")
}

/// Where the native result lines go.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NativeLines {
    Stdout,
    Stderr,
}

/// Print the results of `summary` (and the native lines when requested).
pub fn print(summary: &BuildSummary, native: NativeLines) -> Result<(), String> {
    if native_lines() {
        let mut out: Box<dyn Write> = match native {
            NativeLines::Stdout => Box::new(std::io::stdout().lock()),
            NativeLines::Stderr => Box::new(std::io::stderr().lock()),
        };
        for (label, output) in &summary.outputs {
            // The path's raw bytes: a lone-byte target name names the byte
            // itself (bazel_bytes::internal_path), not U+FFFD.
            out.write_all(format!("{label}\t").as_bytes())
                .and_then(|()| out.write_all(output.as_os_str().as_bytes()))
                .and_then(|()| out.write_all(b"\n"))
                .and_then(|()| out.flush())
                .map_err(|error| error.to_string())?;
        }
    }
    let text = render(
        summary,
        SHOW_RESULT.load(Ordering::Relaxed),
        VERBOSE_FAILURES.load(Ordering::Relaxed),
    );
    if !text.is_empty() {
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(&text);
        let _ = stderr.flush();
    }
    Ok(())
}

fn push_paths(text: &mut Vec<u8>, paths: &[PathBuf]) {
    for path in paths {
        text.extend_from_slice(b"  ");
        text.extend_from_slice(path.as_os_str().as_bytes());
        text.push(b'\n');
    }
}

/// BuildResultPrinter.showBuildResult's text.
pub fn render(summary: &BuildSummary, show_result: usize, verbose_failures: bool) -> Vec<u8> {
    let mut text = Vec::new();
    let ok = outputs(summary, show_result, &mut text);
    if !ok && !verbose_failures {
        text.extend_from_slice(
            b"Use --verbose_failures to see the command lines of failed build steps.\n",
        );
    }
    text
}

/// BuildResultPrinter.outputTargets: whether no failure was seen.
fn outputs(summary: &BuildSummary, show_result: usize, text: &mut Vec<u8>) -> bool {
    let failed = summary.failed_targets.is_empty();
    // `essentialBudget`: the non-empty and failed results that may print.
    let mut budget = show_result as i128;
    for (_, paths) in &summary.results {
        if !paths.is_empty() {
            budget -= 1;
            if budget < 0 {
                return failed;
            }
        }
    }
    for _ in summary
        .failed_targets
        .iter()
        .chain(&summary.skipped_targets)
    {
        budget -= 1;
        if budget < 0 {
            return failed;
        }
    }
    for (_, _, paths) in &summary.aspects {
        if !paths.is_empty() {
            budget -= 1;
            if budget < 0 {
                return failed;
            }
        }
    }
    let omit_nothing_to_build = summary.results.len()
        + summary.failed_targets.len()
        + summary.skipped_targets.len()
        + summary.aspects.len()
        > show_result;
    for label in &summary.skipped_targets {
        text.extend_from_slice(format!("Target {label} was skipped\n").as_bytes());
    }
    for (label, paths) in &summary.results {
        if paths.is_empty() {
            if !omit_nothing_to_build {
                text.extend_from_slice(
                    format!("Target {label} up-to-date (nothing to build)\n").as_bytes(),
                );
            }
            continue;
        }
        text.extend_from_slice(format!("Target {label} up-to-date:\n").as_bytes());
        push_paths(text, paths);
    }
    for label in &summary.failed_targets {
        text.extend_from_slice(format!("Target {label} failed to build\n").as_bytes());
    }
    for (aspect, label, paths) in &summary.aspects {
        if paths.is_empty() {
            if !omit_nothing_to_build {
                text.extend_from_slice(
                    format!("Aspect {aspect} of {label} up-to-date (nothing to build)\n")
                        .as_bytes(),
                );
            }
            continue;
        }
        text.extend_from_slice(format!("Aspect {aspect} of {label} up-to-date:\n").as_bytes());
        push_paths(text, paths);
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::Label;

    fn label(text: &str) -> Label {
        Label::parse_canonical(text).unwrap()
    }

    fn rendered(summary: &BuildSummary, show_result: usize, verbose: bool) -> String {
        String::from_utf8(render(summary, show_result, verbose)).unwrap()
    }

    /// The genuine captures of tests/fixtures/build_results, with the
    /// output tree for `bazel-bin`.
    #[test]
    fn budget_nothing_to_build_and_failures() {
        let mut summary = BuildSummary {
            results: vec![(
                label("//:a"),
                vec!["/x/bin/a.txt".into(), "/x/bin/a2.txt".into()],
            )],
            ..BuildSummary::default()
        };
        assert_eq!(
            rendered(&summary, 1, false),
            "Target //:a up-to-date:\n  /x/bin/a.txt\n  /x/bin/a2.txt\n"
        );
        assert_eq!(rendered(&summary, 0, false), "");
        summary.results.push((label("//:empty"), Vec::new()));
        // Two targets over a budget of one: the empty one is left out.
        assert_eq!(
            rendered(&summary, 1, false),
            "Target //:a up-to-date:\n  /x/bin/a.txt\n  /x/bin/a2.txt\n"
        );
        assert_eq!(
            rendered(&summary, 2, false),
            "Target //:a up-to-date:\n  /x/bin/a.txt\n  /x/bin/a2.txt\nTarget //:empty up-to-date (nothing to build)\n"
        );
        summary
            .results
            .push((label("//:b"), vec!["/x/bin/b.txt".into()]));
        assert_eq!(rendered(&summary, 1, false), "");
        // --keep_going: one failure, one success over a budget of one.
        let failed = BuildSummary {
            results: vec![(label("//:a"), vec!["/x/bin/a.txt".into()])],
            failed_targets: vec![label("//:fail")],
            ..BuildSummary::default()
        };
        assert_eq!(
            rendered(&failed, 1, false),
            "Use --verbose_failures to see the command lines of failed build steps.\n"
        );
        assert_eq!(rendered(&failed, 1, true), "");
        assert_eq!(
            rendered(&failed, 5, false),
            "Target //:a up-to-date:\n  /x/bin/a.txt\nTarget //:fail failed to build\nUse --verbose_failures to see the command lines of failed build steps.\n"
        );
        let aspect = BuildSummary {
            results: vec![(label("//:a"), Vec::new())],
            aspects: vec![(
                "//:defs.bzl%asp".into(),
                label("//:a"),
                vec!["/x/bin/a.asp.txt".into()],
            )],
            ..BuildSummary::default()
        };
        assert_eq!(
            rendered(&aspect, 5, false),
            "Target //:a up-to-date (nothing to build)\nAspect //:defs.bzl%asp of //:a up-to-date:\n  /x/bin/a.asp.txt\n"
        );
    }
}
