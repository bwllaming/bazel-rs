//! N36 CLI coverage (docs/HARDLINK_STAGING.md): action inputs are read-only
//! hardlinks to content-addressed store blobs, so one source reachable at two
//! input paths (a header and its actions.symlink `_virtual_includes` alias)
//! is one inode inside the action, as in genuine Bazel 9.3; actions.symlink
//! outputs are real symlinks to the target's path. Paired capture:
//! tests/fixtures/hardlink_staging/reference_receipt.json.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new() -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("hardlink-staging-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='hardlink_staging_fixture')\n");
        ws.file("pkg/BUILD.bazel", BUILD);
        ws.file("pkg/defs.bzl", DEFS);
        ws.file("pkg/hdr.h", "#pragma once\nint from_header = 1;\n");
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, text: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }

    fn build(&self, target: &str) -> (Vec<(String, PathBuf)>, String) {
        let output = self.run(&["build", target]);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stderr}");
        let outputs = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .filter_map(|(_, path)| {
                let relative = path.split_once("/bin/")?.1.to_owned();
                Some((relative, PathBuf::from(path)))
            })
            .collect();
        (outputs, stderr)
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

fn find<'a>(outputs: &'a [(String, PathBuf)], name: &str) -> &'a Path {
    &outputs
        .iter()
        .find(|(relative, _)| relative == name)
        .unwrap_or_else(|| panic!("no output {name} in {outputs:?}"))
        .1
}

/// Shared with artifacts/staging-capture.py.
const DEFS: &str = r##"
REPORT = """
same() { if [ "$(stat -L -c %i "$1")" = "$(stat -L -c %i "$2")" ]; then echo same; else echo different; fi; }
writable() { if [ -w "$1" ]; then echo yes; else echo no; fi; }
{
echo "source_vs_virtual=$(same "$1" "$2")"
echo "generated_vs_virtual=$(same "$3" "$4")"
echo "source_vs_generated=$(same "$1" "$3")"
echo "source_mode=$(stat -L -c %a "$1")"
echo "generated_mode=$(stat -L -c %a "$3")"
echo "source_writable=$(writable "$1")"
echo "generated_writable=$(writable "$3")"
echo "source_lstat=$(stat -c %F "$1")"
echo "virtual_lstat=$(stat -c %F "$2")"
} > "$5"
"""

def _impl(ctx):
    hdr = ctx.file.hdr
    virt = ctx.actions.declare_file("_virtual_includes/lib/hdr.h")
    ctx.actions.symlink(output = virt, target_file = hdr)
    gen = ctx.actions.declare_file("gen.h")
    ctx.actions.write(gen, "#pragma once\nint generated = 1;\n")
    gen_link = ctx.actions.declare_file("_virtual_includes/lib/gen.h")
    ctx.actions.symlink(output = gen_link, target_file = gen)
    report = ctx.actions.declare_file("identity.txt")
    ctx.actions.run_shell(
        outputs = [report],
        inputs = [hdr, virt, gen, gen_link],
        command = REPORT,
        arguments = [hdr.path, virt.path, gen.path, gen_link.path, report.path],
    )
    return [DefaultInfo(files = depset([virt, gen, gen_link, report]))]

identity = rule(implementation = _impl, attrs = {"hdr": attr.label(allow_single_file = True)})
"##;

const BUILD: &str = r#"load(":defs.bzl", "identity")
identity(name = "identity", hdr = "hdr.h")
"#;

fn is_root() -> bool {
    fs::metadata("/proc/self").is_ok_and(|metadata| metadata.uid() == 0)
}

#[test]
fn header_alias_is_one_inode_inside_the_action_and_symlink_outputs_are_real() {
    let ws = Workspace::new();
    let (outputs, stderr) = ws.build("//pkg:identity");
    assert!(
        stderr.contains("4 actions executed, 0 cache hits"),
        "{stderr}"
    );
    let writable = if is_root() { "yes" } else { "no" };
    assert_eq!(
        fs::read_to_string(find(&outputs, "pkg/identity.txt")).unwrap(),
        format!(
            "source_vs_virtual=same\ngenerated_vs_virtual=same\nsource_vs_generated=different\nsource_mode=444\ngenerated_mode=555\nsource_writable={writable}\ngenerated_writable={writable}\nsource_lstat=regular file\nvirtual_lstat=regular file\n"
        )
    );
    // The published aliases are symlinks resolving to their targets' inodes.
    let header = fs::canonicalize(ws.path().join("pkg/hdr.h")).unwrap();
    let virt = find(&outputs, "pkg/_virtual_includes/lib/hdr.h");
    assert!(fs::symlink_metadata(virt).unwrap().file_type().is_symlink());
    assert_eq!(fs::canonicalize(virt).unwrap(), header);
    assert_eq!(
        fs::metadata(virt).unwrap().ino(),
        fs::metadata(&header).unwrap().ino()
    );
    let gen_link = find(&outputs, "pkg/_virtual_includes/lib/gen.h");
    assert!(
        fs::symlink_metadata(gen_link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_link(gen_link).unwrap(),
        find(&outputs, "pkg/gen.h")
    );
    assert_eq!(
        fs::metadata(gen_link).unwrap().ino(),
        fs::metadata(find(&outputs, "pkg/gen.h")).unwrap().ino()
    );
    // Store blobs are shared, read-only inodes.
    let store = ws.path().join("private-cache/cas");
    for entry in fs::read_dir(&store).unwrap() {
        let entry = entry.unwrap();
        let metadata = fs::symlink_metadata(entry.path()).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.mode() & 0o222, 0, "{:?}", entry.path());
    }
    let (_, again) = ws.build("//pkg:identity");
    assert!(
        again.contains("0 actions executed, 4 cache hits"),
        "{again}"
    );
    // A changed header re-runs its alias and the consumer only.
    ws.file("pkg/hdr.h", "#pragma once\nint from_header = 2;\n");
    let (outputs, changed) = ws.build("//pkg:identity");
    assert!(
        changed.contains("2 actions executed, 2 cache hits"),
        "{changed}"
    );
    assert!(
        fs::read_to_string(find(&outputs, "pkg/identity.txt"))
            .unwrap()
            .starts_with("source_vs_virtual=same\n")
    );
    assert_eq!(
        fs::read_to_string(find(&outputs, "pkg/_virtual_includes/lib/hdr.h")).unwrap(),
        "#pragma once\nint from_header = 2;\n"
    );
}
