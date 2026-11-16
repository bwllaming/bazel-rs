//! Bazel 9.3.1's progress lines without cursor control (crate::progress
//! plain lines, docs/PROGRESS.md; genuine captures in
//! tests/fixtures/progress_lines): stderr is a pipe, so Bazel prints its
//! short progress bar one line at a time — the phase starts at once, then
//! at most one line per `max(--show_progress_rate_limit, 1s)` when
//! something happened, and the inactivity watchdog's reminder after 11 s
//! without a completed action. The counts and seconds differ natively
//! (normalized); the shapes, their order and the rate are compared.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::io::BufRead;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

include!("support/module_archives.rs");
include!("support/temp_root.rs");

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/progress_lines");

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

/// One stderr line and when it arrived (seconds since the start).
#[derive(Clone, Debug)]
struct Stamped {
    at: f64,
    text: String,
}

/// A genuine capture: its arguments, exit code and stamped stderr.
fn capture(name: &str) -> (Vec<String>, i32, Vec<Stamped>) {
    let text = fs::read_to_string(Path::new(FIXTURE).join("genuine").join(name)).unwrap();
    let mut lines = text.lines();
    let command = lines.next().unwrap().strip_prefix("### ").unwrap();
    let rc = lines
        .next()
        .unwrap()
        .strip_prefix("rc=")
        .unwrap()
        .parse()
        .unwrap();
    let stderr = lines
        .map(|line| {
            let (at, text) = line.split_once(" | ").unwrap();
            Stamped {
                at: at.trim().parse().unwrap(),
                text: text.to_owned(),
            }
        })
        .collect();
    (command.split(' ').map(str::to_owned).collect(), rc, stderr)
}

/// The same command natively in a fresh copy of the workspace, its stderr
/// read as it arrives (a pipe: the plain lines).
fn native(arguments: &[String]) -> (i32, Vec<Stamped>) {
    let temp = tempfile::Builder::new()
        .prefix("progress-lines-")
        .tempdir_in(crate::test_temp::root())
        .unwrap();
    let root = temp.path().join("ws");
    copy_tree(&Path::new(FIXTURE).join("workspace"), &root);
    let cache = temp.path().join("cache");
    seed_module_archives(&cache, TOOLCHAIN_CLOSURE_ARCHIVES);
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!("--cache-dir={}", cache.display()))
        .args(arguments)
        .arg(builtin_registry_flag())
        .arg("--repository_disable_download")
        .current_dir(&root)
        .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
        .env("BAZEL_RS_RESULT_LINES", "0")
        .env_remove("BAZEL_RS_PROGRESS")
        .env_remove("CI")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = Vec::new();
    for line in std::io::BufReader::new(child.stderr.take().unwrap()).lines() {
        lines.push(Stamped {
            at: started.elapsed().as_secs_f64(),
            text: line.unwrap().replace(root.to_str().unwrap(), "<WS>"),
        });
    }
    let status = child.wait().unwrap();
    (status.code().unwrap(), lines)
}

/// Whether `line` is one of Bazel's non-curses progress lines (an empty
/// line right after an `Analyzing:` one is checked by the caller).
fn is_progress(line: &str) -> bool {
    line.starts_with("Computing main repo mapping: ")
        || line.starts_with("Loading: ")
        || line.starts_with("Analyzing: ")
        || execution_counts(line).is_some()
}

/// The `[done / total]` lead of an execution line.
fn execution_counts(line: &str) -> Option<(&str, &str)> {
    let (counts, _) = line.strip_prefix('[')?.split_once("] ")?;
    let (done, total) = counts.split_once(" / ")?;
    [done, total]
        .iter()
        .all(|count| !count.is_empty() && count.chars().all(|c| c.is_ascii_digit() || c == ','))
        .then_some((done, total))
}

/// The progress lines with the landmarks between them (`WARNING:`,
/// `INFO: Analyzed`, `FAIL:`), an `Analyzing:` line's empty follower
/// folded into it (` <NL>`), stamped.
fn progress(lines: &[Stamped]) -> Vec<Stamped> {
    let mut out: Vec<Stamped> = Vec::new();
    let mut after_analysis = false;
    for line in lines {
        let text = &line.text;
        if text.is_empty() && after_analysis {
            out.last_mut().unwrap().text.push_str(" <NL>");
            after_analysis = false;
            continue;
        }
        after_analysis = text.starts_with("Analyzing: ");
        if is_progress(text)
            || text.starts_with("WARNING: ")
            || text.starts_with("INFO: Analyzed ")
            || text.starts_with("FAIL: ")
        {
            out.push(line.clone());
        }
    }
    out
}

/// A progress line without its counts, seconds, strategy and which of the
/// slow genrules or tests it names.
fn shape(line: &str) -> String {
    let mut line = line.to_owned();
    if let Some((done, total)) = execution_counts(&line) {
        let lead = format!("[{done} / {total}]");
        line = line.replacen(&lead, "[N / M]", 1);
    }
    if let Some(open) = line.find(" (")
        && (line.starts_with("Analyzing: ") || line.starts_with("INFO: Analyzed "))
        && let Some(close) = line[open..].find(')')
    {
        line = format!("{}(<counts>{}", &line[..open + 2], &line[open + close..]);
    }
    if line.starts_with("FAIL: ") {
        line = line.split(" (see ").next().unwrap().to_owned();
    }
    let words: Vec<String> = line
        .split(' ')
        .map(|word| {
            let seconds = word
                .strip_suffix('s')
                .filter(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()));
            if seconds.is_some() {
                "<S>s".to_owned()
            } else if ["processwrapper-sandbox", "linux-sandbox", "local"].contains(&word) {
                "<strategy>".to_owned()
            } else if let Some(rest) = word.strip_prefix("//:") {
                let name = rest.trim_end_matches(';');
                let masked = if name == "chain" || name.trim_end_matches(char::is_numeric) == "slow"
                {
                    "<genrule>"
                } else if name.trim_end_matches(char::is_numeric) == "t" {
                    "t<N>"
                } else {
                    name
                };
                format!("//:{masked}{}", &rest[name.len()..])
            } else {
                word.to_owned()
            }
        })
        .collect();
    words.join(" ")
}

/// The shapes of `lines` up to `INFO: Analyzed` (the phase lines), each
/// repetition once.
fn head(lines: &[Stamped]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        let shaped = shape(&line.text);
        if out.last() != Some(&shaped) {
            out.push(shaped);
        }
        if line.text.starts_with("INFO: Analyzed ") {
            break;
        }
    }
    out
}

/// The execution lines (after `INFO: Analyzed`).
fn execution(lines: &[Stamped]) -> Vec<Stamped> {
    lines
        .iter()
        .skip_while(|line| !line.text.starts_with("INFO: Analyzed "))
        .filter(|line| execution_counts(&line.text).is_some())
        .cloned()
        .collect()
}

/// Run capture `name` natively; the genuine and native progress lines.
fn compare(name: &str) -> (Vec<Stamped>, Vec<Stamped>) {
    let (arguments, rc, genuine) = capture(name);
    let (code, native) = native(&arguments);
    let dump = |lines: &[Stamped]| {
        lines
            .iter()
            .map(|line| format!("{:6.2} | {}", line.at, line.text))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(code, rc, "{name}:\n{}", dump(&native));
    let (genuine, native_progress) = (progress(&genuine), progress(&native));
    assert_eq!(
        head(&native_progress),
        head(&genuine),
        "{name} native:\n{}",
        dump(&native)
    );
    // The rate: execution lines at least a second apart (one tick of
    // slack), whatever came before them.
    let rate = arguments
        .iter()
        .find_map(|argument| argument.strip_prefix("--show_progress_rate_limit="))
        .map_or(1.0, |seconds| seconds.parse::<f64>().unwrap().max(1.0));
    let printed: Vec<Stamped> = native_progress
        .iter()
        .filter(|line| is_progress(&line.text))
        .cloned()
        .collect();
    for pair in printed.windows(2) {
        if execution_counts(&pair[1].text).is_some() {
            assert!(
                pair[1].at - pair[0].at >= rate - 0.1,
                "{name}: {pair:#?}\n{}",
                dump(&native)
            );
        }
    }
    (genuine, native_progress)
}

#[test]
fn build_lines_match_bazel() {
    let (genuine, native) = compare("01.txt");
    let (genuine, native) = (execution(&genuine), execution(&native));
    // A second after the analysis the three slow genrules run: Bazel's
    // `[N / M] Executing genrule //:slow1; 1s processwrapper-sandbox ...
    // (3 actions running)`.
    assert!(
        shape(&genuine[0].text).ends_with(" ... (3 actions running)"),
        "{genuine:#?}"
    );
    assert!(!native.is_empty(), "{native:#?}");
    for line in &native {
        let shaped = shape(&line.text);
        assert!(
            shaped.starts_with("[N / M] Executing genrule //:<genrule>; <S>s <strategy>"),
            "{shaped}"
        );
    }
}

#[test]
fn test_lines_match_bazel() {
    let (genuine, native) = compare("02.txt");
    let shapes = |lines: &[Stamped]| -> Vec<String> {
        execution(lines)
            .iter()
            .map(|line| shape(&line.text))
            .collect()
    };
    let (genuine, native) = (shapes(&genuine), shapes(&native));
    // After the failed test, the last one still runs.
    let last = genuine.last().unwrap();
    assert_eq!(
        last,
        "[N / M] 2 / 3 tests, 1 failed; Testing //:t<N>; <S>s <strategy>"
    );
    assert!(
        native
            .iter()
            .any(|line| line.starts_with("[N / M] 2 / 3 tests, 1 failed; Testing //:t<N>")),
        "{native:#?}"
    );
    for line in &native {
        assert!(line.contains("Testing //:t<N>"), "{line}");
    }
}

#[test]
fn the_inactivity_watchdog_reminds_like_bazel() {
    let (genuine, native) = compare("03.txt");
    let shapes = |lines: &[Stamped]| -> Vec<String> {
        execution(lines)
            .iter()
            .map(|line| shape(&line.text))
            .collect()
    };
    assert_eq!(shapes(&native), shapes(&genuine), "{native:#?}");
    // A second into the action, then the reminder at 11 s.
    let seconds: Vec<u64> = execution(&native)
        .iter()
        .map(|line| {
            let (_, rest) = line.text.split_once("; ").unwrap();
            rest.split_once("s ").unwrap().0.parse().unwrap()
        })
        .collect();
    assert!(
        seconds.len() == 2 && seconds[0] <= 2 && (10..=12).contains(&seconds[1]),
        "{native:#?}"
    );
}

#[test]
fn noshow_progress_prints_none() {
    let (genuine, native) = compare("04.txt");
    for lines in [genuine, native] {
        assert!(
            lines.iter().all(|line| !is_progress(&line.text)),
            "{lines:#?}"
        );
    }
}

#[test]
fn the_rate_limit_spaces_the_lines() {
    // Three seconds: the four-second execution shows no line in Bazel's
    // capture; natively none closer than three seconds to the last one
    // (compare).
    let (genuine, _) = compare("05.txt");
    assert!(execution(&genuine).is_empty(), "{genuine:#?}");
}
