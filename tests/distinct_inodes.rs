//! Distinct artifacts with identical content are distinct files wherever a
//! process sees them, as in Bazel (docs/HARDLINK_STAGING.md, "Distinct
//! artifacts, distinct inodes"): an action's inputs, an action tool's
//! runfiles and a test's runfiles. The store keeps one blob per content, and
//! staging used to hardlink every input to it: two sources, two generated
//! files, a copy of a source and a tree artifact's file holding the same
//! bytes were one inode inside the action, so `tar`, `bsdtar`/mtree, `cp -a`
//! and `rsync -H` recorded hard links between them (rules_js's
//! `js_image_layer` stored a copied `package.json` as a hard link to the
//! identical file of a Next.js standalone tree; a test extracting only the
//! former lost it). An `actions.symlink` alias stays the inode of the
//! artifact it names, and one artifact at several paths one inode.
//!
//! Pinned by a genuine capture (tests/fixtures/distinct_inodes: capture.py,
//! reference_receipt.json): every probe line names the first listed path with
//! the same device:inode. Whether a path is a symlink is not compared:
//! native stages regular files where Bazel's sandboxes and runfiles trees
//! show symlinks (a declared difference since N36). Genuine's `sandboxed`
//! ran as processwrapper-sandbox on this host. The output tree (`bazel-bin`)
//! is the declared difference: native's published outputs with one content
//! share their blob's inode (genuine: separate files).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
include!("support/temp_root.rs");
use std::path::Path;
use std::process::{Command, Output};

include!("support/module_archives.rs");

struct Workspace(tempfile::TempDir);

impl Drop for Workspace {
    fn drop(&mut self) {
        // Published outputs may be read-only.
        let _ = Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(self.0.path())
            .status();
    }
}

impl Workspace {
    fn path(&self) -> &Path {
        self.0.path()
    }

    fn run(&self, cache: &str, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", &format!("--cache-dir={cache}")])
            .args(args)
            // Verify that the fixture's seeded closure suffices even on a
            // cold materialization, with no fallback archive downloads.
            .arg("--repository_disable_download")
            .current_dir(self.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }
}

/// `line` without the trailing ` symlink` / ` file` kind.
fn identity(line: &str) -> &str {
    line.strip_suffix(" symlink")
        .or_else(|| line.strip_suffix(" file"))
        .unwrap_or(line)
}

#[test]
fn distinct_artifacts_with_one_content_are_distinct_inodes() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/distinct_inodes");
    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(fixture.join("reference_receipt.json")).unwrap())
            .unwrap();
    let ws = Workspace(
        tempfile::Builder::new()
            .prefix("distinct-inodes-cli-")
            .tempdir_in(crate::test_temp::root())
            .unwrap(),
    );
    for name in [
        "MODULE.bazel",
        "BUILD.bazel",
        "defs.bzl",
        "src1.txt",
        "src2.txt",
    ] {
        fs::copy(fixture.join("workspace").join(name), ws.path().join(name)).unwrap();
    }
    for (index, strategy) in ["standalone", "sandboxed"].into_iter().enumerate() {
        // Test-action toolchain discovery visits the whole builtin closure
        // (including rules_shell), not only the platform definitions.
        seed_module_archives(&ws.path().join(strategy), TOOLCHAIN_CLOSURE_ARCHIVES);
        let registry = builtin_registry_flag();
        let output = ws.run(
            strategy,
            &[
                "test",
                &registry,
                &format!("--spawn_strategy={strategy}"),
                "--test_output=all",
                "//:all",
            ],
        );
        let mut text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{strategy}: {text}");
        let info = ws.run(strategy, &["info", &registry, "bazel-bin"]);
        assert!(info.status.success(), "{info:?}");
        let bin = Path::new(String::from_utf8(info.stdout).unwrap().trim()).to_path_buf();
        for name in ["input_probe.txt", "tool_probe.txt"] {
            text.push_str(&fs::read_to_string(bin.join(name)).unwrap());
        }
        let mut lines: Vec<String> = text
            .lines()
            .filter(|line| {
                ["test_runfile_", "input_", "tool_runfile_"]
                    .iter()
                    .any(|key| line.starts_with(key))
            })
            .map(|line| identity(line).to_owned())
            .collect();
        // The output tree's files, like capture.py's output_tree_lines.
        let mut first: Vec<((u64, u64), &str)> = Vec::new();
        for name in ["a.out", "b.out", "c.out", "dir/sub/f.txt", "link.txt"] {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = fs::metadata(bin.join(name)).unwrap();
            let key = (metadata.dev(), metadata.ino());
            let owner = match first.iter().find(|(seen, _)| *seen == key) {
                Some((_, owner)) => owner,
                None => {
                    first.push((key, name));
                    name
                }
            };
            lines.push(format!("output_tree_{name}={owner}"));
        }
        let mut expected: Vec<&str> = receipt["rows"][index]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| identity(line.as_str().unwrap()))
            .collect();
        assert_eq!(receipt["rows"][index]["strategy"], strategy);
        // Genuine's output tree: five separate files. Native's: the three
        // generated files and the tree's file are their blob's inode (the
        // store keeps one inode per published content; separate files
        // would cost a copy of every duplicate output, measured +4.5 GB on
        // a Next.js app's 8 GB cache).
        let tree_lines = expected.split_off(expected.len() - 5);
        assert_eq!(
            tree_lines,
            [
                "output_tree_a.out=a.out",
                "output_tree_b.out=b.out",
                "output_tree_c.out=c.out",
                "output_tree_dir/sub/f.txt=dir/sub/f.txt",
                "output_tree_link.txt=link.txt",
            ]
        );
        expected.extend([
            "output_tree_a.out=a.out",
            "output_tree_b.out=a.out",
            "output_tree_c.out=a.out",
            "output_tree_dir/sub/f.txt=a.out",
            "output_tree_link.txt=link.txt",
        ]);
        assert_eq!(lines, expected, "{strategy}");
    }
}
