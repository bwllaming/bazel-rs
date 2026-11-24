//! The convenience symlinks (crate::convenience_symlinks) against the
//! genuine Bazel 9.3.1 capture tests/fixtures/convenience_symlinks: which
//! links each command creates, replaces or deletes and where they point,
//! the warnings, the build results printed through them, the BEP
//! ConvenienceSymlinksIdentified event, `clean`. Then the native
//! guarantees: copying through the links copies the outputs' content, globs
//! and `//...` ignore the links, and creating or retargeting them never
//! invalidates the analysis snapshot.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

include!("support/module_archives.rs");

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/convenience_symlinks"
);

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A copy of the fixture workspace (named `ws`, like capture.sh's) with its
/// own cache.
struct Scratch {
    _temp: tempfile::TempDir,
    ws: PathBuf,
    cache: PathBuf,
    bep: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let temp = tempfile::Builder::new()
            .prefix("cs-")
            .tempdir_in(base)
            .unwrap();
        let ws = temp.path().join("ws");
        copy(&Path::new(FIXTURE).join("workspace"), &ws);
        let cache = temp.path().join("cache");
        let bep = temp.path().join("bep.json");
        Scratch {
            ws,
            cache,
            bep,
            _temp: temp,
        }
    }

    /// bazel-rs with `args` in the workspace subdirectory `dir`.
    fn run(&self, dir: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.cache.display()))
            .args(with_builtin_registry(args))
            .args(["--curses=no", "--color=no"])
            .current_dir(self.ws.join(dir))
            .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
            .env_remove("BAZEL_RS_RESULT_LINES")
            .stdin(Stdio::null())
            .seeded_archives()
            .output()
            .unwrap()
    }

    /// `<cache>/worktrees/ws-<hash>` as `<OB>`, the workspace as `<WS>`.
    fn normalized(&self, line: &str) -> String {
        let worktrees = format!("{}/worktrees/ws-", self.cache.display());
        let mut line = line.to_owned();
        while let Some(at) = line.find(&worktrees) {
            let end = at + worktrees.len() + 16;
            line.replace_range(at..end, "<OB>");
        }
        line.replace(&self.ws.display().to_string(), "<WS>")
    }

    /// capture.sh's `listing`: the workspace's link-like entries.
    fn listing(&self) -> Vec<String> {
        let mut entries: Vec<(String, PathBuf)> = Vec::new();
        for entry in fs::read_dir(&self.ws).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("bazel-") || name.starts_with("foo-") {
                entries.push((format!("./{name}"), entry.path()));
            } else if name.starts_with("out") {
                entries.push((format!("./{name}"), entry.path()));
                if entry.file_type().unwrap().is_dir() {
                    for inner in fs::read_dir(entry.path()).unwrap() {
                        let inner = inner.unwrap();
                        entries.push((
                            format!("./{name}/{}", inner.file_name().to_string_lossy()),
                            inner.path(),
                        ));
                    }
                }
            }
        }
        entries.sort();
        entries
            .into_iter()
            .map(|(shown, path)| {
                let metadata = fs::symlink_metadata(&path).unwrap();
                let line = if metadata.file_type().is_symlink() {
                    format!("  {shown} -> {}", fs::read_link(&path).unwrap().display())
                } else if metadata.is_dir() {
                    format!("  {shown}/ (dir)")
                } else {
                    format!("  {shown} (file)")
                };
                self.normalized(&line)
            })
            .collect()
    }

    /// The BEP ConvenienceSymlinksIdentified payloads, as capture.sh's
    /// Python prints them (`json.dumps(…, sort_keys=True)`).
    fn bep_events(&self) -> Vec<String> {
        let Ok(text) = fs::read_to_string(&self.bep) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        for line in text.lines() {
            let event: serde_json::Value = serde_json::from_str(line).unwrap();
            if event["id"].get("convenienceSymlinksIdentified").is_none() {
                continue;
            }
            let links = event["convenienceSymlinksIdentified"]["convenienceSymlinks"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if links.is_empty() {
                events.push("  BEP {}".to_owned());
                continue;
            }
            let shown: Vec<String> = links
                .iter()
                .map(|link| {
                    let mut fields = vec![
                        format!("\"action\": {}", link["action"]),
                        format!("\"path\": {}", link["path"]),
                    ];
                    if let Some(target) = link.get("target") {
                        fields.push(format!("\"target\": {target}"));
                    }
                    format!("{{{}}}", fields.join(", "))
                })
                .collect();
            events.push(format!(
                "  BEP {{\"convenienceSymlinks\": [{}]}}",
                shown.join(", ")
            ));
        }
        events
    }
}

/// capture.sh's awk over stderr then stdout.
fn kept(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut paths = false;
    for line in text.lines() {
        if line.starts_with("Target ") || line.starts_with("Aspect ") {
            lines.push(line.to_owned());
            paths = line.ends_with("up-to-date:");
        } else if line.starts_with("WARNING: failed to create")
            || line.starts_with("WARNING: failed to remove")
            || line.starts_with("//:t")
        {
            lines.push(line.to_owned());
            paths = true;
        } else if line.starts_with("  ") {
            if paths {
                lines.push(line.to_owned());
            }
        } else {
            paths = false;
            if line.contains("WARNING")
                || line.contains("convenience")
                || line.starts_with("INFO: Running command line")
                || line.contains("test.log")
                || line.starts_with("ERROR: Build did NOT")
                || line.starts_with("ERROR: Analysis")
            {
                lines.push(line.to_owned());
            }
        }
    }
    lines
}

/// The declared differences (tests/fixtures/convenience_symlinks/README.md),
/// left out on both sides: the native error texts, `INFO: Running command
/// line`, the coverage warnings, the test log paths (`FAIL: …` and the
/// summary lines with their log paths).
fn comparable(lines: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut summary = false;
    for line in lines {
        if line.starts_with("//:") {
            summary = true;
            continue;
        }
        if summary && line.starts_with("  ") && !line.starts_with("  BEP") {
            continue;
        }
        summary = false;
        if line.starts_with("ERROR: ")
            || line.starts_with("WARNING: Target pattern parsing failed.")
            || line.starts_with("WARNING: There was no coverage found.")
            || line.starts_with("INFO: Running command line")
            || line.starts_with("FAIL: ")
        {
            continue;
        }
        out.push(line.clone());
    }
    out
}

/// The genuine capture: each step with its lines.
fn genuine() -> Vec<(String, Vec<String>)> {
    let text = fs::read_to_string(Path::new(FIXTURE).join("genuine.txt")).unwrap();
    let mut steps: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        match line.strip_prefix("### ") {
            Some(step) => steps.push((step.to_owned(), Vec::new())),
            None => steps.last_mut().unwrap().1.push(line.to_owned()),
        }
    }
    steps
}

#[test]
fn convenience_symlinks_match_bazel() {
    let scratch = Scratch::new();
    let mut failures = Vec::new();
    for (step, expected) in genuine() {
        if let Some(command) = step.strip_prefix('!') {
            let status = Command::new("sh")
                .args(["-c", command])
                .current_dir(&scratch.ws)
                .status()
                .unwrap();
            assert!(status.success(), "{step}");
            continue;
        }
        let (dir, args) = match step.strip_prefix('@') {
            Some(rest) => rest.split_once(' ').unwrap(),
            None => (".", step.as_str()),
        };
        let bep = scratch.bep.display().to_string();
        let args: Vec<String> = args
            .split(' ')
            .map(|arg| arg.replace("bep.json", &bep))
            .collect();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = fs::remove_file(&scratch.bep);
        let output = scratch.run(dir, &args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        if step.contains("--print_relative_test_log_paths") {
            // Explicitly unsupported natively (the README).
            assert!(
                !output.status.success() && stderr.contains("print_relative_test_log_paths"),
                "{step}: {stderr}"
            );
            continue;
        }
        let mut observed = vec![format!("rc={}", output.status.code().unwrap())];
        observed.extend(
            kept(&format!("{stderr}{stdout}"))
                .iter()
                .map(|line| scratch.normalized(line)),
        );
        observed.extend(scratch.bep_events());
        observed.push("links:".to_owned());
        observed.extend(scratch.listing());
        let (expected, observed) = (comparable(&expected), comparable(&observed));
        if expected != observed {
            failures.push(format!(
                "### {step}\n--- genuine\n{}\n--- native\n{}\n--- stderr\n{stderr}",
                expected.join("\n"),
                observed.join("\n")
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Copying through the links copies the outputs' content, as with Bazel's
/// output tree: a file, an output symlink, a tree artifact's files, a whole
/// package directory; the copies are writable files, not links into the
/// cache.
#[test]
fn copies_through_the_links_copy_the_outputs() {
    let scratch = Scratch::new();
    let output = scratch.run(".", &["build", "//:a", "//:tree", "//pkg:p"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dest = scratch.ws.join("copied");
    fs::create_dir_all(&dest).unwrap();
    let cp = |args: &[&str]| {
        let status = Command::new("cp")
            .args(args)
            .current_dir(&scratch.ws)
            .status()
            .unwrap();
        assert!(status.success(), "cp {args:?}");
    };
    // The copyfiles idiom: `cp bazel-out/<cfg>/bin/<pkg>/<file> <pkg>/`.
    cp(&["bazel-out/k8-fastbuild/bin/pkg/p.txt", "copied/"]);
    cp(&["bazel-bin/a.txt", "copied/a.txt"]);
    cp(&["bazel-bin/tree_link.txt", "copied/"]);
    cp(&["bazel-bin/tree_dir/sub/t2.txt", "copied/"]);
    cp(&["-R", "bazel-bin/pkg", "copied/pkg"]);
    cp(&["-RL", "bazel-bin/tree_dir", "copied/tree_dir"]);
    for (path, content) in [
        ("p.txt", "p\n"),
        ("a.txt", "a\n"),
        ("tree_link.txt", "src\n"),
        ("t2.txt", "t2\n"),
        ("pkg/p.txt", "p\n"),
        ("tree_dir/t1.txt", "t1\n"),
        ("tree_dir/sub/t2.txt", "t2\n"),
    ] {
        let copied = dest.join(path);
        assert_eq!(fs::read_to_string(&copied).unwrap(), content, "{path}");
        let metadata = fs::symlink_metadata(&copied).unwrap();
        assert!(metadata.is_file(), "{path} is a regular file");
    }
    // `bazel-<workspace>` is the execution root.
    assert_eq!(
        fs::read_to_string(scratch.ws.join("bazel-ws/bazel-out/k8-fastbuild/bin/a.txt")).unwrap(),
        "a\n"
    );
}

fn snapshot_run(scratch: &Scratch, args: &[&str]) -> (Output, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!("--cache-dir={}", scratch.cache.display()))
        .args(with_builtin_registry(args))
        .current_dir(&scratch.ws)
        .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
        .env("BAZEL_RS_SNAPSHOT", "on")
        .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
        .env_remove("BAZEL_RS_RESULT_LINES")
        .stdin(Stdio::null())
        .seeded_archives()
        .output()
        .unwrap();
    let outcome = String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| line.strip_prefix("bazel-rs-snapshot: "))
        .filter(|line| line.starts_with("hit ") || line.starts_with("miss "))
        .map(|line| line.split(' ').next().unwrap().to_owned())
        .collect::<Vec<_>>()
        .join(",");
    (output, outcome)
}

/// Package discovery and globs ignore the links (also Bazel's own, into
/// another output base), so `//...` and a root `glob(["**/*.txt"])` are the
/// same with and without them, and a build that creates or retargets them
/// still hits the analysis snapshot of the next one.
#[test]
fn links_change_no_query_and_keep_the_snapshot() {
    let scratch = Scratch::new();
    let build = scratch.ws.join("BUILD");
    let mut text = fs::read_to_string(&build).unwrap();
    text.push_str("filegroup(name = \"all_txt\", srcs = glob([\"**/*.txt\"]))\n");
    fs::write(&build, text).unwrap();
    // Bazel's links of another output base (an execroot with
    // DO_NOT_BUILD_HERE), as a genuine build leaves them.
    let other = scratch.ws.parent().unwrap().join("genuine-ob/execroot");
    let other_bin = other.join("_main/bazel-out/k8-fastbuild/bin");
    fs::create_dir_all(&other_bin).unwrap();
    fs::write(other_bin.join("stale.txt"), "stale\n").unwrap();
    fs::write(other.join("DO_NOT_BUILD_HERE"), "/elsewhere").unwrap();
    std::os::unix::fs::symlink(&other_bin, scratch.ws.join("bazel-bin")).unwrap();
    std::os::unix::fs::symlink(other.join("_main/bazel-out"), scratch.ws.join("bazel-out"))
        .unwrap();
    let query = |args: &[&str]| {
        let output = scratch.run(".", args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let everything = query(&["query", "//..."]);
    let globbed = query(&["query", "labels(srcs, //:all_txt)"]);
    assert_eq!(globbed, "//:src.txt\n");
    let targets = ["build", "//:all_txt", "//:a"];
    let (cold, outcome) = snapshot_run(&scratch, &targets);
    assert!(cold.status.success());
    assert_eq!(outcome, "miss");
    // The build replaced Bazel's links with its own.
    let own = fs::read_link(scratch.ws.join("bazel-bin")).unwrap();
    assert!(own.starts_with(&scratch.cache), "{}", own.display());
    let (_, outcome) = snapshot_run(&scratch, &targets);
    assert_eq!(outcome, "hit");
    // Another configuration retargets bazel-bin; the first build still hits.
    let (opt, _) = snapshot_run(&scratch, &["build", "//:opt"]);
    assert!(opt.status.success());
    assert!(
        fs::read_link(scratch.ws.join("bazel-bin"))
            .unwrap()
            .ends_with("k8-opt/bin")
    );
    let (again, outcome) = snapshot_run(&scratch, &targets);
    assert!(again.status.success());
    assert_eq!(outcome, "hit");
    assert!(
        String::from_utf8_lossy(&again.stderr)
            .contains("Target //:a up-to-date:\n  bazel-bin/a.txt\n")
    );
    assert_eq!(query(&["query", "//..."]), everything);
    assert_eq!(query(&["query", "labels(srcs, //:all_txt)"]), globbed);
}
