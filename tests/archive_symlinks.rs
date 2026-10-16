//! Registry archive symlinks and hard links through the Cargo-built CLI.
#![cfg(unix)]

use base64::Engine;
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BUILD: &str = "\
package(default_visibility = ['//visibility:public'])
exports_files(['link.txt', 'abs.txt', 'hard.txt', 'dirlink/real.txt', 'dangling.txt', 'escape.txt'])
filegroup(name = 'data_files', srcs = glob(['data/*.txt']))
genrule(
    name = 'copy',
    srcs = ['link.txt', 'abs.txt', 'hard.txt', 'dirlink/real.txt'],
    outs = ['copy.txt'],
    cmd = 'cat $(SRCS) > $@',
)
genrule(name = 'dangling', srcs = ['dangling.txt'], outs = ['d.txt'], cmd = 'cat $(SRCS) > $@')
genrule(name = 'escape', srcs = ['escape.txt'], outs = ['e.txt'], cmd = 'cat $(SRCS) > $@')
";

// (archive path, tar type, link target, contents)
const ENTRIES: &[(&str, u8, &str, &str)] = &[
    (
        "linked-1/MODULE.bazel",
        b'0',
        "",
        "module(name='archive_copy')\n",
    ),
    ("linked-1/BUILD.bazel", b'0', "", BUILD),
    // Deferred like Bazel's symlink map: a link may precede its referent.
    ("linked-1/link.txt", b'2', "data/real.txt", ""),
    ("linked-1/data/real.txt", b'0', "", "real\n"),
    ("linked-1/abs.txt", b'2', "/linked-1/data/real.txt", ""),
    ("linked-1/hard.txt", b'1', "linked-1/data/real.txt", ""),
    ("linked-1/dirlink", b'2', "data", ""),
    ("linked-1/dangling.txt", b'2', "missing.txt", ""),
    ("linked-1/escape.txt", b'2', "../../outside.txt", ""),
    // The aspect_bazel_lib/bazel_lib/tar.bzl shape: unreferenced e2e links.
    ("linked-1/.bazelversion", b'0', "", "9.3.0\n"),
    (
        "linked-1/e2e/smoke/.bazelversion",
        b'2',
        "../../.bazelversion",
        "",
    ),
    (
        "linked-1/globbed/BUILD.bazel",
        b'0',
        "",
        "filegroup(name = 'g', srcs = glob(['*.txt']))\n",
    ),
    ("linked-1/globbed/l.txt", b'2', "../data/real.txt", ""),
];

fn archive() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, kind, link, data) in ENTRIES {
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o644);
        header.set_size(data.len() as u64);
        header.set_entry_type(tar::EntryType::new(*kind));
        header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        header.as_mut_bytes()[157..157 + link.len()].copy_from_slice(link.as_bytes());
        header.set_cksum();
        builder.append(&header, data.as_bytes()).unwrap();
    }
    let tar = builder.into_inner().unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, &tar).unwrap();
    encoder.finish().unwrap()
}

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("archive-links-")
            .tempdir()
            .unwrap();
        let fixture = Self(temp);
        let archive = archive();
        fs::write(fixture.path().join("linked-1.tar.gz"), &archive).unwrap();
        fs::write(fixture.path().join("outside.txt"), "outside\n").unwrap();
        fixture.write(
            "MODULE.bazel",
            "module(name = 'main')\nbazel_dep(name = 'linked', version = '1')\n",
        );
        fixture.write(
            "BUILD.bazel",
            "genrule(name = 'main_copy', srcs = ['@linked//:dirlink/real.txt'], outs = ['main.txt'], cmd = 'cat $(SRCS) > $@')\n",
        );
        fixture.write(
            "registry/modules/linked/1/MODULE.bazel",
            "module(name = 'linked', version = '1')\n",
        );
        let source = serde_json::json!({
            "url": format!("file://{}", fixture.path().join("linked-1.tar.gz").display()),
            "integrity": format!(
                "sha256-{}",
                base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&archive))
            ),
            "strip_prefix": "linked-1",
        });
        fixture.write("registry/modules/linked/1/source.json", &source.to_string());
        fixture
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg("--cache-dir=cache")
            .arg(args[0])
            .arg(format!(
                "--registry=file://{}",
                self.path().join("registry").display()
            ))
            .args(&args[1..])
            .current_dir(self.path())
            .output()
            .unwrap()
    }

    fn tree(&self) -> PathBuf {
        let trees: Vec<_> = fs::read_dir(self.path().join("cache/repositories/trees"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .collect();
        assert_eq!(trees.len(), 1, "{trees:?}");
        trees[0].clone()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn outputs(output: &Output) -> Vec<(String, PathBuf)> {
    assert!(output.status.success(), "{}", stderr(output));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let (label, path) = line.split_once('\t').unwrap();
            (label.to_owned(), PathBuf::from(path))
        })
        .collect()
}

fn failed(output: &Output, message: &str) {
    assert!(!output.status.success());
    assert!(stderr(output).contains(message), "{}", stderr(output));
}

#[test]
fn registry_archive_links_materialize_and_build_through_links() {
    let fixture = Fixture::new();
    let query = fixture.run(&["query", "@linked//:all"]);
    assert!(query.status.success(), "{}", stderr(&query));
    let listed = String::from_utf8_lossy(&query.stdout).into_owned();
    for name in ["copy", "dangling", "escape", "data_files"] {
        assert!(
            listed.contains(&format!("@@linked+//:{name}\n")),
            "{listed}"
        );
    }

    let root = fixture.tree();
    for (name, target) in [
        ("link.txt", "data/real.txt"),
        ("abs.txt", "data/real.txt"),
        ("dirlink", "data"),
        ("dangling.txt", "missing.txt"),
        ("escape.txt", "../../outside.txt"),
        ("e2e/smoke/.bazelversion", "../../.bazelversion"),
    ] {
        assert_eq!(
            fs::read_link(root.join(name)).unwrap(),
            Path::new(target),
            "{name}"
        );
    }
    assert_eq!(
        fs::metadata(root.join("hard.txt")).unwrap().ino(),
        fs::metadata(root.join("data/real.txt")).unwrap().ino()
    );
    // The authoritative registry MODULE.bazel replaced the archive's copy.
    assert_eq!(
        fs::read_to_string(root.join("MODULE.bazel")).unwrap(),
        "module(name = 'linked', version = '1')\n"
    );
    let manifest = fs::read_to_string(root.with_extension("json")).unwrap();
    assert!(
        manifest.contains(r#""symlink":"../../outside.txt""#),
        "{manifest}"
    );

    let built = fixture.run(&["build", "@linked//:copy", "//:main_copy"]);
    let built = outputs(&built);
    assert_eq!(built.len(), 2, "{built:?}");
    for (label, path) in &built {
        let expected = if label.contains("main_copy") {
            "real\n".to_owned()
        } else {
            "real\n".repeat(4)
        };
        assert_eq!(fs::read_to_string(path).unwrap(), expected, "{label}");
    }
    let warm = fixture.run(&["build", "@linked//:copy", "//:main_copy"]);
    assert_eq!(outputs(&warm), built);
    assert!(
        stderr(&warm).contains("0 actions executed, 2 cache hits"),
        "{}",
        stderr(&warm)
    );
    // The glob that matches no link is unaffected.
    let globbed = fixture.run(&["build", "@linked//:data_files"]);
    assert_eq!(outputs(&globbed).len(), 1);
}

#[test]
fn dangling_and_escaping_archive_links_fail_and_globbed_links_follow() {
    let fixture = Fixture::new();
    // A dangling source symlink is a missing input file, as in Bazel.
    failed(
        &fixture.run(&["build", "@linked//:dangling"]),
        "missing input file '@@linked+//:dangling.txt'",
    );
    // The escaping link's target does not exist: a missing input file, as in
    // Bazel (an existing outside target is the native restriction).
    failed(
        &fixture.run(&["build", "@linked//:escape"]),
        "missing input file '@@linked+//:escape.txt'",
    );
    // glob() follows the in-repository link like Bazel's (N28d).
    let globbed = fixture.run(&["query", "@linked//globbed:all"]);
    assert!(globbed.status.success(), "{}", stderr(&globbed));
    assert_eq!(
        String::from_utf8_lossy(&globbed.stdout),
        "@@linked+//globbed:g\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.path().join("outside.txt")).unwrap(),
        "outside\n"
    );
}
