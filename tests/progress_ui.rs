//! The interactive progress display (crate::progress, docs/PROGRESS.md):
//! Bazel's curses-mode status area when stderr and stdout are a terminal,
//! and exactly the plain output otherwise.
//!
//! The terminal runs are on a pseudo-terminal (support/pty_terminal.rs);
//! the plain run writes stdout and stderr into one pipe, so its bytes are
//! in the order a terminal shows them.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::Path;
use std::process::Command;

include!("support/pty_terminal.rs");

const DEFS: &str = r#"
print("hello from loading")
def nothing():
    pass
"#;

const BUILD: &str = r#"
load(":defs.bzl", "nothing")
genrule(name = "slow", outs = ["slow.txt"], cmd = "sleep 2; echo slow > $@")
genrule(name = "top", srcs = [":slow"], outs = ["top.txt"], cmd = "cat $< > $@")
genrule(name = "endless", outs = ["endless.txt"], cmd = "sleep 60; touch $@")
"#;

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("progress-ui-")
                .tempdir_in(base)
                .unwrap(),
        );
        fs::write(ws.path().join("MODULE.bazel"), "module(name = \"ui\")\n").unwrap();
        fs::write(ws.path().join("BUILD"), BUILD).unwrap();
        fs::write(ws.path().join("defs.bzl"), DEFS).unwrap();
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    /// A bazel-rs command in this workspace with its own cache, the
    /// environment of an interactive terminal.
    fn command(&self, cache: &str, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={cache}"))
            .args(arguments)
            .arg("--experimental-builtin-modules=false")
            .current_dir(self.path())
            .env("TERM", "xterm-256color")
            .env_remove("CI")
            .env_remove("NO_COLOR")
            .env_remove("BAZEL_RS_PROGRESS")
            .env_remove("EMACS")
            .env_remove("INSIDE_EMACS");
        command
    }

    /// `command` with stdout and stderr into one pipe (no terminal).
    fn plain(&self, mut command: Command) -> (Vec<u8>, i32) {
        let (mut reader, writer) = std::io::pipe().unwrap();
        command
            .stdout(writer.try_clone().unwrap())
            .stderr(writer)
            .stdin(std::process::Stdio::null());
        let mut child = command.spawn().unwrap();
        drop(command);
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut bytes).unwrap();
        let status = child.wait().unwrap();
        (bytes, status.code().unwrap())
    }

    /// The lines with this workspace's path replaced (each run has its own
    /// cache below it).
    fn normalized(&self, text: &str, cache: &str) -> Vec<String> {
        let root = self.path().to_str().unwrap();
        text.replace(&format!("{root}/{cache}"), "<cache>")
            .replace(root, "<workspace>")
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn the_status_area_shows_on_a_terminal_and_nowhere_else() {
    let ws = Workspace::new();
    let build = ["build", "//:top"];

    // No terminal: no escape sequence, no interactive line.
    let (plain, code) = ws.plain(ws.command("plain", &build));
    assert_eq!(code, 0, "{}", text(&plain));
    assert!(!plain.contains(&0x1b), "{}", text(&plain));
    assert!(!text(&plain).contains("INFO: "), "{}", text(&plain));
    let expected = ws.normalized(&text(&plain), "plain");
    assert!(
        expected
            .iter()
            .any(|line| line.contains("hello from loading"))
    );
    assert!(expected.iter().any(|line| line.starts_with("//:top\t")));

    // A terminal with --curses=no: the same bytes (the terminal turns each
    // \n into \r\n).
    let off = on_pty(
        &ws.command("off", &[&build[..], &["--curses=no"]].concat()),
        100,
        None,
    );
    assert_eq!(off.status, 0, "{}", text(&off.bytes));
    assert!(!off.bytes.contains(&0x1b), "{}", text(&off.bytes));
    assert_eq!(
        ws.normalized(&text(&off.bytes).replace("\r\n", "\n"), "off"),
        expected
    );
    // --noshow_progress and CI likewise.
    let quiet = on_pty(
        &ws.command("quiet", &[&build[..], &["--noshow_progress"]].concat()),
        100,
        None,
    );
    assert!(!quiet.bytes.contains(&0x1b), "{}", text(&quiet.bytes));
    let mut ci = ws.command("ci", &build);
    ci.env("CI", "true");
    let ci = on_pty(&ci, 100, None);
    assert!(!ci.bytes.contains(&0x1b), "{}", text(&ci.bytes));
    assert_eq!(
        ws.normalized(&text(&ci.bytes).replace("\r\n", "\n"), "ci"),
        expected
    );
    // The startup option --quiet (crate::ui): no status area, and of the
    // plain output only the result lines.
    let startup_quiet = on_pty(
        &ws.command("startup-quiet", &[&["--quiet"][..], &build[..]].concat()),
        100,
        None,
    );
    assert_eq!(startup_quiet.status, 0, "{}", text(&startup_quiet.bytes));
    assert!(
        !startup_quiet.bytes.contains(&0x1b),
        "{}",
        text(&startup_quiet.bytes)
    );
    assert_eq!(
        ws.normalized(
            &text(&startup_quiet.bytes).replace("\r\n", "\n"),
            "startup-quiet"
        ),
        expected
            .iter()
            .filter(|line| line.starts_with("//:"))
            .cloned()
            .collect::<Vec<_>>()
    );

    // A terminal: the status area while the slow action runs...
    let on = on_pty(&ws.command("on", &build), 100, None);
    assert_eq!(on.status, 0, "{}", text(&on.bytes));
    let raw = text(&on.bytes);
    assert!(raw.contains("\x1b[J"), "{raw:?}");
    assert!(
        strip_escapes(&raw).contains("[2 / 4] Genrule slow.txt; "),
        "{raw:?}"
    );
    assert!(
        raw.contains("s linux-sandbox") || raw.contains("s local"),
        "{raw:?}"
    );
    // ...every status line fits the 100 columns (one left free)...
    for frame in raw.split(['\r', '\n']) {
        let visible: String = strip_escapes(frame);
        if visible.starts_with('[') {
            assert!(visible.chars().count() <= 99, "{visible:?}");
        }
    }
    // ...and what is left on the screen is the plain output, in order, with
    // the interactive lines: the analysis summary before the results, the
    // elapsed time and the outcome last. No status line is left.
    let screen = ws.normalized(&Screen::render(&on.bytes).join("\n"), "on");
    let (info, rest): (Vec<&String>, Vec<&String>) =
        screen.iter().partition(|line| line.starts_with("INFO: "));
    assert_eq!(rest.into_iter().cloned().collect::<Vec<_>>(), expected);
    assert_eq!(info.len(), 3, "{screen:#?}");
    assert_eq!(
        info[0],
        "INFO: Analyzed 1 target (1 package loaded, 2 targets configured)."
    );
    assert!(info[1].starts_with("INFO: Elapsed time: "), "{screen:#?}");
    assert!(
        info[2].starts_with("INFO: Build completed successfully, ")
            && info[2].ends_with(" total actions"),
        "{screen:#?}"
    );
    let analyzed = screen.iter().position(|line| line == info[0]).unwrap();
    let result = screen
        .iter()
        .position(|line| line.starts_with("//:top\t"))
        .unwrap();
    assert!(analyzed < result, "{screen:#?}");
    assert_eq!(
        &screen[screen.len() - 2..],
        [info[1].clone(), info[2].clone()]
    );

    // The same build again: an analysis snapshot hit, said once.
    let hit = on_pty(&ws.command("on", &build), 100, None);
    assert_eq!(hit.status, 0, "{}", text(&hit.bytes));
    let screen = ws.normalized(&Screen::render(&hit.bytes).join("\n"), "on");
    assert!(
        screen.contains(&"INFO: Analysis snapshot hit: loading and analysis skipped.".to_owned()),
        "{screen:#?}"
    );
    assert!(
        screen
            .last()
            .unwrap()
            .starts_with("INFO: Build completed successfully")
    );
}

#[test]
fn an_interrupt_clears_the_status_area_and_still_ends_by_the_signal() {
    let ws = Workspace::new();
    let run = on_pty(
        &ws.command("cache", &["build", "//:endless"]),
        100,
        Some("Genrule endless.txt"),
    );
    // Ended by SIGINT, as without the display.
    assert_eq!(run.status, -2, "{}", text(&run.bytes));
    let screen = Screen::render(&run.bytes);
    assert!(
        !screen.iter().any(|line| line.starts_with('[')),
        "{screen:#?}"
    );
}

/// `frame` without its escape sequences.
fn strip_escapes(frame: &str) -> String {
    let mut out = String::new();
    let mut chars = frame.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
