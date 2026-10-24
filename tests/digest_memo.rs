//! L2-W2 CLI coverage (docs/DIGEST_MEMO.md): the persistent file-digest
//! memo `<cache>/digests/v1.dmemo`. A later process takes the digests of
//! unchanged inputs and outputs from it instead of reading them; a no-op
//! build writes nothing; an edited input is read again — also when the edit
//! keeps the size and restores the modification time (the change time
//! differs); a corrupt table is ignored and replaced; with
//! `BAZEL_RS_DIGEST_MEMO=off` the table is neither read nor written. A
//! repository tree file edited that way is caught by the next command's
//! repository verification, which replaces the tree before execution, and
//! by the executor's change-time key in a tree identity that nothing
//! verifies any more (a cached symlink output still points into it).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

include!("support/snapshot_off.rs");

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
        self.hashed("action")
    }

    /// Files and bytes hashed for `user` (`hashed: <user>=<files>/<bytes>B`).
    fn hashed(&self, user: &str) -> (u64, u64) {
        let line = self.memo("hashed:");
        let value = line
            .split_whitespace()
            .find_map(|word| word.strip_prefix(&format!("{user}=")))
            .unwrap_or_else(|| panic!("no {user}= in {line}"));
        let (files, bytes) = value.trim_end_matches('B').split_once('/').unwrap();
        (files.parse().unwrap(), bytes.parse().unwrap())
    }

    /// Files repository_store hashed while verifying direct repositories.
    fn store_hashed(&self) -> u64 {
        let line = self.memo("hashed:");
        line.split_whitespace()
            .find_map(|word| word.strip_prefix("files_hashed_during_verify="))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no files_hashed_during_verify= in {line}"))
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
        self.run(&["build", "//pkg:b"], environment)
    }

    /// One successful command (`args` after the startup options).
    fn run(&self, args: &[&str], environment: &[(&str, &str)]) -> Run {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=cache"])
            .args(args)
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

/// Rewrite `path` in place with other bytes of the same length and restore
/// its modification time: only the change time tells.
fn edit_keeping_size_and_mtime(path: &Path) {
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    let original = fs::read(path).unwrap();
    let edited = original.to_ascii_uppercase();
    assert_ne!(edited, original, "{}", path.display());
    let mut file = fs::File::options().write(true).open(path).unwrap();
    file.write_all(&edited).unwrap();
    file.set_modified(modified).unwrap();
}

/// The published directories in `directory` (a tree store).
fn entries(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.is_dir()
                && !path
                    .file_name()
                    .unwrap()
                    .as_encoded_bytes()
                    .starts_with(b".")
        })
        .collect()
}

/// The only published directory in `directory` (a tree store).
fn only_entry(directory: &Path) -> PathBuf {
    let entries = entries(directory);
    assert_eq!(entries.len(), 1, "{entries:?}");
    entries.into_iter().next().unwrap()
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

/// The executor trusts repository tree files once per command because the
/// command verified their tree before any action ran (docs/DIGEST_MEMO.md,
/// "Trust"). With the table that verification reads only files whose state
/// it has no record of, keyed by the change time: a tree file edited in
/// place between commands — same size, modification time restored — is
/// caught by the next command, which replaces the tree before any action
/// reads it; the executor then hashes the new files (its records, keyed by
/// inode and change time, never match them).
/// Both a repository rule's tree (`repositories/direct/trees`) and the
/// embedded `bazel_tools` (`repositories/builtins/trees`).
#[test]
fn edited_repository_tree_files_are_caught_by_the_next_command() {
    let workspace = Workspace::new();
    workspace.write(
        "MODULE.bazel",
        "module(name = 'tree_edit_fixture')\n\
         maker = use_repo_rule('//:rules.bzl', 'maker')\n\
         maker(name = 'direct_repo')\n",
    );
    workspace.write(
        "rules.bzl",
        "def _make(ctx):\n\
         \x20   ctx.file('BUILD.bazel', \"exports_files(['value'])\")\n\
         \x20   ctx.file('value', 'constant\\n', executable = False)\n\
         \n\
         maker = repository_rule(implementation = _make)\n",
    );
    workspace.write(
        "BUILD.bazel",
        "genrule(\n\
         \x20   name = 'uses',\n\
         \x20   srcs = ['@direct_repo//:value', '@bazel_tools//tools/genrule:genrule-setup.sh'],\n\
         \x20   outs = ['uses.txt'],\n\
         \x20   cmd = 'cat $(SRCS) > $@',\n\
         )\n",
    );
    // The analysis snapshot off (on by default): a hit does not look into
    // a tree's files (docs/ANALYSIS_SNAPSHOT.md, the per-tree trade-off) —
    // the edit below is caught by the full analysis's warm probe, which a
    // hit skips (the hit's healing of the failure it then causes is
    // tests/snapshot.rs's `a_failed_hit_on_a_tampered_store_tree_heals`).
    let build = || {
        workspace.run(
            &["build", "--experimental-builtin-modules", "//:uses"],
            &[SNAPSHOT_OFF],
        )
    };
    let cold = build();
    cold.shows("1 actions executed");
    let expected = fs::read(cold.output("uses.txt")).unwrap();
    // Settled now: the next build records the trees' files, the one after
    // takes them from the table.
    settle();
    build().shows("0 actions executed");
    settle();
    let warm = build();
    warm.shows("0 actions executed");
    assert_eq!(warm.hashed("builtin").0, 0, "{}", warm.stderr);
    assert!(warm.action_hashed().0 <= 1, "{}", warm.stderr);

    let cache = workspace.path().join("cache/repositories");
    let edited: Vec<(PathBuf, Vec<u8>, u64)> = [
        only_entry(&cache.join("direct/trees")).join("value"),
        only_entry(&cache.join("builtins/trees")).join("tools/genrule/genrule-setup.sh"),
    ]
    .into_iter()
    .map(|path| {
        let original = fs::read(&path).unwrap();
        let inode = fs::metadata(&path).unwrap().ino();
        edit_keeping_size_and_mtime(&path);
        (path, original, inode)
    })
    .collect();
    let next = build();
    next.shows("0 actions executed");
    // Caught: the edited files were hashed by the verification (the
    // repository rule's tree by repository_store's fingerprints, bazel_tools
    // through the table) and both trees were replaced.
    assert!(next.store_hashed() >= 1, "{}", next.stderr);
    assert!(next.hashed("builtin").0 >= 1, "{}", next.stderr);
    for (path, original, inode) in &edited {
        assert_eq!(&fs::read(path).unwrap(), original, "{}", path.display());
        assert_ne!(
            fs::metadata(path).unwrap().ino(),
            *inode,
            "{}",
            path.display()
        );
    }
    // The executor read the new files instead of trusting its records.
    let (files, bytes) = next.action_hashed();
    let tree_bytes: u64 = edited
        .iter()
        .map(|(_, original, _)| original.len() as u64)
        .sum();
    assert!(files >= 2 && bytes >= tree_bytes, "{}", next.stderr);
    assert_eq!(fs::read(next.output("uses.txt")).unwrap(), expected);
}

/// A cached `actions.symlink` output keeps pointing into the repository
/// tree identity it was created with: its action key holds the target's
/// digest, not its path, so a new identity with the same file content is a
/// cache hit, and no command verifies the old identity any more. Its cache
/// check reads the old file through the table, which must therefore key
/// repository tree files by their change time: a same-size edit there that
/// restores the modification time is caught (the symlink action runs again
/// and now points into the current identity), as without the table.
#[test]
fn symlinks_into_an_old_tree_identity_see_edits_that_restore_the_mtime() {
    let workspace = Workspace::new();
    workspace.write(
        "MODULE.bazel",
        "module(name = 'stale_identity_fixture')\n\
         maker = use_repo_rule('//:rules.bzl', 'maker')\n\
         maker(name = 'direct_repo')\n",
    );
    // The tree's identity follows version.txt; `value` never changes.
    workspace.write(
        "rules.bzl",
        "def _make(ctx):\n\
         \x20   ctx.file('BUILD.bazel', \"exports_files(['value', 'version'])\")\n\
         \x20   ctx.file('value', 'constant\\n', executable = False)\n\
         \x20   ctx.file('version', ctx.read(Label('//:version.txt')), executable = False)\n\
         \n\
         maker = repository_rule(implementation = _make)\n",
    );
    workspace.write(
        "link.bzl",
        "def _impl(ctx):\n\
         \x20   out = ctx.actions.declare_file(ctx.label.name + '.txt')\n\
         \x20   ctx.actions.symlink(output = out, target_file = ctx.file.src)\n\
         \x20   return [DefaultInfo(files = depset([out]))]\n\
         \n\
         link = rule(implementation = _impl, attrs = {'src': attr.label(allow_single_file = True)})\n",
    );
    workspace.write("version.txt", "v1\n");
    workspace.write(
        "BUILD.bazel",
        "load(':link.bzl', 'link')\n\
         exports_files(['version.txt'])\n\
         link(name = 'linked', src = '@direct_repo//:value')\n\
         genrule(name = 'one', srcs = [':linked'], outs = ['one.txt'], cmd = 'cat $< > $@')\n",
    );
    let build = || workspace.run(&["build", "//:one", "//:linked"], &[]);
    let target = |run: &Run| fs::read_link(run.output("linked.txt")).unwrap();
    let trees = workspace.path().join("cache/repositories/direct/trees");
    let cold = build();
    cold.shows("2 actions executed");
    let old = target(&cold);
    assert_eq!(old, only_entry(&trees).join("value"));
    settle();
    // Settled now: this build records the tree file (by its change time).
    build().shows("0 actions executed");
    settle();
    // A new identity with the same `value`: the symlink action is a cache
    // hit and still points into the old identity.
    workspace.write("version.txt", "v2\n");
    let bumped = build();
    bumped.shows("0 actions executed");
    assert_eq!(target(&bumped), old);
    let current: Vec<PathBuf> = entries(&trees)
        .into_iter()
        .map(|identity| identity.join("value"))
        .filter(|value| *value != old)
        .collect();
    assert_eq!(current.len(), 1, "{current:?}");
    settle();
    edit_keeping_size_and_mtime(&old);
    let next = build();
    // Caught: the symlink action ran again and points into the current
    // identity; the consumer's input digest is unchanged (a cache hit).
    next.shows("1 actions executed");
    assert_eq!(target(&next), current[0]);
    assert_eq!(
        fs::read_to_string(next.output("linked.txt")).unwrap(),
        "constant\n"
    );
    assert_eq!(
        fs::read_to_string(next.output("one.txt")).unwrap(),
        "constant\n"
    );
}

/// Integration-20 (INTEGRATION-20.md F4): the warm revalidation of a
/// repository rule's recorded file inputs takes their digests from the
/// recorded-input memo and the persistent table, so it reads nothing — and
/// the analysis snapshot's record mode still records every answer as the
/// READ of that path, keyed by the stat the lookup used: the file's own,
/// and for a symlink the target's (plus the link's LSTAT), never the
/// link's lstat.
#[test]
fn record_mode_records_reads_the_digest_memo_answered() {
    let workspace = Workspace::new();
    workspace.write(
        "MODULE.bazel",
        "module(name = 'recorded_inputs_fixture')\n\
         gen = use_repo_rule('//:rules.bzl', 'gen')\n\
         gen(name = 'gen')\n",
    );
    workspace.write(
        "rules.bzl",
        "def _gen(ctx):\n\
         \x20   ctx.watch(Label('//:input.txt'))\n\
         \x20   ctx.watch(Label('//:link.txt'))\n\
         \x20   ctx.file('BUILD.bazel', \"filegroup(name = 'all')\")\n\
         \n\
         gen = repository_rule(implementation = _gen)\n",
    );
    workspace.write("BUILD.bazel", "");
    workspace.write("input.txt", "watched contents\n");
    std::os::unix::fs::symlink("input.txt", workspace.path().join("link.txt")).unwrap();
    let analyze = |environment: &[(&str, &str)]| {
        workspace.run(&["build", "--nobuild", "@gen//:all"], environment)
    };
    let recorded = |run: &Run| -> String {
        run.stderr
            .lines()
            .find(|line| line.starts_with("PERF-REPO recorded-input file digests:"))
            .unwrap_or_else(|| panic!("no recorded-input digests line:\n{}", run.stderr))
            .to_owned()
    };
    // Cold: the rule runs; its just-written inputs are racy (never memoized
    // or recorded in the table).
    analyze(&[]);
    settle();
    // Warm: the recorded inputs are revalidated, read once (the link's
    // canonical path and stat are the file's: an in-process hit) and
    // recorded in the table.
    let warm = analyze(&[]);
    assert!(
        recorded(&warm).contains("hashed=1 ") && recorded(&warm).contains("memo_hits=1 "),
        "{}",
        warm.stderr
    );
    // Recording: nothing is read, every answer is a recorded READ.
    let dump = workspace.path().join("snapshot-dump.txt");
    let dump_text = dump.to_str().unwrap();
    let recording = analyze(&[
        ("BAZEL_RS_SNAPSHOT", "record"),
        ("BAZEL_RS_SNAPSHOT_DUMP", dump_text),
    ]);
    assert!(
        recorded(&recording).contains("hashed=0 ") && recorded(&recording).contains("memo_hits=2 "),
        "{}",
        recording.stderr
    );
    let root = fs::canonicalize(workspace.path()).unwrap();
    let input = fs::metadata(root.join("input.txt")).unwrap();
    let key = format!(
        "{}:{}:{}:{}:{}",
        input.dev(),
        input.ino(),
        input.size(),
        input.mtime() * 1_000_000_000 + input.mtime_nsec(),
        input.ctime() * 1_000_000_000 + input.ctime_nsec(),
    );
    let link = fs::symlink_metadata(root.join("link.txt")).unwrap();
    assert_ne!(link.ino(), input.ino());
    let sha256 = {
        use sha2::Digest;
        let digest: [u8; 32] = sha2::Sha256::digest(b"watched contents\n").into();
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let dump = fs::read_to_string(&dump).unwrap();
    for expected in [
        format!(
            "OBS {}/input.txt READ sha256:{sha256} key:{key}",
            root.display()
        ),
        format!("OBS {}/link.txt LSTAT link:input.txt", root.display()),
        format!(
            "OBS {}/link.txt READ sha256:{sha256} key:{key}",
            root.display()
        ),
    ] {
        assert!(
            dump.lines().any(|line| line == expected),
            "missing {expected:?} in the dump:\n{}",
            dump.lines()
                .filter(|line| line.contains("input.txt") || line.contains("link.txt"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
