//! L2-W2 CLI coverage (docs/DIGEST_MEMO.md): the persistent file-digest
//! memo `<cache>/digests/v1.dmemo`. A later process takes the digests of
//! unchanged inputs and outputs from it instead of reading them; a no-op
//! build writes nothing; an edited input is read again — also when the edit
//! keeps the size and restores the modification time (the change time
//! differs); a corrupt table is ignored and replaced; with
//! `BAZEL_RS_DIGEST_MEMO=off` the table is neither read nor written.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const BUILD: &str = "\
genrule(name = 'a', srcs = ['in.txt'], outs = ['a.out'], cmd = 'cat $< > $@; echo more >> $@')
genrule(name = 'b', srcs = [':a', 'in.txt'], outs = ['b.out'], cmd = 'cat $(location :a) $(location in.txt) > $@')
";

struct Workspace(tempfile::TempDir);

struct Run {
    stdout: String,
    stderr: String,
}

impl Run {
    /// The `PERF-DMEMO` line starting with `prefix`.
    fn memo(&self, prefix: &str) -> &str {
        self.stderr
            .lines()
            .find(|line| line.starts_with(&format!("PERF-DMEMO {prefix}")))
            .unwrap_or_else(|| panic!("no PERF-DMEMO {prefix} line:\n{}", self.stderr))
    }

    /// The numeric `name=<n>` field of the `PERF-DMEMO load=` line.
    fn field(&self, name: &str) -> u64 {
        let line = self.memo("load=");
        line.split_whitespace()
            .find_map(|word| word.strip_prefix(&format!("{name}=")))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no {name}= in {line}"))
    }

    /// Files and bytes hashed for actions (`hashed: action=<files>/<bytes>B`).
    fn action_hashed(&self) -> (u64, u64) {
        let line = self.memo("hashed:");
        let value = line
            .split_whitespace()
            .find_map(|word| word.strip_prefix("action="))
            .unwrap_or_else(|| panic!("no action= in {line}"));
        let (files, bytes) = value.trim_end_matches('B').split_once('/').unwrap();
        (files.parse().unwrap(), bytes.parse().unwrap())
    }

    /// Assert that stderr contains `needle`.
    fn shows(&self, needle: &str) -> &Self {
        assert!(
            self.stderr.contains(needle),
            "expected {needle:?} in:\n{}",
            self.stderr
        );
        self
    }

    fn output(&self, name: &str) -> PathBuf {
        self.stdout
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(_, path)| PathBuf::from(path))
            .find(|path| path.ends_with(name))
            .unwrap_or_else(|| panic!("no output {name}:\n{}", self.stdout))
    }
}

impl Workspace {
    fn new() -> Self {
        let workspace = Self(
            tempfile::Builder::new()
                .prefix("digest-memo-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        workspace.write("MODULE.bazel", "module(name = 'digest_memo_fixture')\n");
        workspace.write("pkg/BUILD.bazel", BUILD);
        workspace.write("pkg/in.txt", "hello\n");
        workspace
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn table(&self) -> PathBuf {
        self.path().join("cache/digests/v1.dmemo")
    }

    fn build(&self, environment: &[(&str, &str)]) -> Run {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args([
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "build",
                "//pkg:b",
            ])
            .env("BAZEL_RS_TIMING", "1")
            .envs(environment.iter().copied())
            .current_dir(self.path())
            .output()
            .unwrap();
        let run = Run {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        assert!(output.status.success(), "{}", run.stderr);
        run
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(self.path())
            .status();
    }
}

/// The table's identity: inode, modification time and bytes.
fn table_state(path: &Path) -> (u64, i64, i64, Vec<u8>) {
    let metadata = fs::metadata(path).unwrap();
    (
        metadata.ino(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        fs::read(path).unwrap(),
    )
}

/// Sleep past the racy window (2 s) so that files written just before are
/// settled for the next build (only settled digests are recorded).
fn settle() {
    std::thread::sleep(Duration::from_millis(2300));
}

#[test]
fn later_processes_take_unchanged_digests_from_the_table() {
    let workspace = Workspace::new();
    settle();

    // Cold: both actions run; the settled source is recorded and the
    // published outputs ride along.
    let cold = workspace.build(&[]);
    cold.shows("2 actions executed")
        .shows("PERF-DMEMO load=Absent")
        .shows("save: records=");
    assert!(cold.field("remembered") > 0, "{}", cold.stderr);
    let table = fs::read(workspace.table()).unwrap();
    assert_eq!(&table[..8], b"BZRSDMEM");
    assert!((table.len() - 32).is_multiple_of(88));
    assert_eq!(
        fs::read_to_string(cold.output("b.out")).unwrap(),
        "hello\nmore\nhello\n"
    );
    settle();

    // Warm, in a new process: the cache checks take their digests from the
    // table. Files the cold build created itself (store blobs) were racy
    // then; they are settled now and recorded by this build.
    let warm = workspace.build(&[]);
    warm.shows("0 actions executed")
        .shows("PERF-DMEMO load=Valid");
    assert!(warm.field("hits") > 0, "{}", warm.stderr);
    assert!(
        warm.action_hashed().0 < cold.action_hashed().0,
        "{}",
        warm.stderr
    );
    settle();

    // A no-op build reads only the volatile workspace status, which every
    // build writes anew (in its execution root: never recorded), and writes
    // nothing.
    let before = table_state(&workspace.table());
    let noop = workspace.build(&[]);
    noop.shows("0 actions executed").shows("save: nothing");
    assert!(noop.action_hashed().0 <= 1, "{}", noop.stderr);
    assert_eq!(table_state(&workspace.table()), before);

    // A rewrite of the same size that restores the modification time keeps
    // size and mtime: only the change time tells, and it must.
    let input = workspace.path().join("pkg/in.txt");
    let modified = fs::metadata(&input).unwrap().modified().unwrap();
    fs::write(&input, "HELLO\n").unwrap();
    fs::File::options()
        .write(true)
        .open(&input)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let edited = workspace.build(&[]);
    edited.shows("2 actions executed");
    assert_eq!(
        fs::read_to_string(edited.output("b.out")).unwrap(),
        "HELLO\nmore\nHELLO\n"
    );
    settle();

    // A corrupt table reads as empty: the build re-reads what it needs and
    // replaces the table with a valid one.
    let mut corrupt = fs::read(workspace.table()).unwrap();
    corrupt.truncate(40);
    fs::write(workspace.table(), &corrupt).unwrap();
    let repaired = workspace.build(&[]);
    repaired
        .shows("0 actions executed")
        .shows("PERF-DMEMO load=Ignored")
        .shows("save: records=");
    assert!(repaired.action_hashed().0 > 0, "{}", repaired.stderr);
    let table = fs::read(workspace.table()).unwrap();
    assert_eq!(&table[..8], b"BZRSDMEM");
    assert!(table.len() > 32 && (table.len() - 32).is_multiple_of(88));

    // Disabled: the table is neither consulted nor rewritten.
    let before = table_state(&workspace.table());
    let off = workspace.build(&[("BAZEL_RS_DIGEST_MEMO", "off")]);
    off.shows("0 actions executed")
        .shows("PERF-DMEMO table=off");
    assert!(off.action_hashed().0 > 0, "{}", off.stderr);
    assert_eq!(table_state(&workspace.table()), before);
}
