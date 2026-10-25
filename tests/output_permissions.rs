//! Real CLI regression for executable generated outputs observed in Bazel 9.3.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
include!("support/explicit_modules.rs");

#[test]
fn genrule_outputs_are_executable_on_cold_and_warm_builds() {
    let root = tempfile::Builder::new()
        .prefix("output-mode-cli-")
        .tempdir_in("/tmp/opencode")
        .unwrap();
    fs::write(
        root.path().join("MODULE.bazel"),
        "module(name='output_mode_test')\n",
    )
    .unwrap();
    fs::write(root.path().join("BUILD.bazel"), "genrule(name='copy',srcs=['input'],outs=['output'],cmd='cat $(SRCS) > $@; chmod 0644 $@')\n").unwrap();
    fs::write(root.path().join("input"), "payload\n").unwrap();
    let mut previous: Option<PathBuf> = None;
    for _ in 0..2 {
        let result = Command::new("timeout")
            .args([
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "build",
                "//:copy",
            ])
            .current_dir(root.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let text = String::from_utf8(result.stdout).unwrap();
        let (_, path) = text.lines().next().unwrap().split_once('\t').unwrap();
        let path = PathBuf::from(path);
        assert_eq!(fs::read(&path).unwrap(), b"payload\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o555
        );
        if let Some(previous) = &previous {
            assert_eq!(&path, previous);
        }
        previous = Some(path);
    }
    assert_eq!(
        fs::metadata(root.path().join("input"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
}

#[test]
fn writes_and_shell_outputs_normalize_only_after_execution() {
    let root = tempfile::Builder::new()
        .prefix("all-output-modes-")
        .tempdir_in("/tmp/opencode")
        .unwrap();
    fs::write(
        root.path().join("MODULE.bazel"),
        "module(name='output_modes')\n",
    )
    .unwrap();
    fs::write(
        root.path().join("BUILD.bazel"),
        "load(':defs.bzl','outputs')\noutputs(name='probe')\n",
    )
    .unwrap();
    fs::write(root.path().join("defs.bzl"), r#"
def _impl(ctx):
    data = ctx.actions.declare_file("write-data")
    executable = ctx.actions.declare_file("write-executable")
    ctx.actions.write(data, "payload", is_executable=False)
    ctx.actions.write(executable, "payload", is_executable=True)
    files = [data, executable]
    for mode in ["000", "644", "755"]:
        output = ctx.actions.declare_file("shell-" + mode)
        observed = ctx.actions.declare_file("observed-" + mode)
        ctx.actions.run_shell(outputs=[output, observed], arguments=[output.path, observed.path, mode], command="printf payload > \"$1\"; chmod \"$3\" \"$1\"; stat -c %a \"$1\" > \"$2\"")
        files.extend([output, observed])
    return [DefaultInfo(files=depset(files))]
outputs = rule(implementation=_impl)
"#).unwrap();
    for _ in 0..2 {
        let result = Command::new("timeout")
            .args([
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "build",
                "//:probe",
            ])
            .current_dir(root.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let text = String::from_utf8(result.stdout).unwrap();
        assert_eq!(text.lines().count(), 8);
        for line in text.lines() {
            let (_, path) = line.split_once('\t').unwrap();
            let path = PathBuf::from(path);
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o555
            );
            let expected = match path.file_name().unwrap().to_str().unwrap() {
                "observed-000" => b"0\n".as_slice(),
                "observed-644" => b"644\n".as_slice(),
                "observed-755" => b"755\n".as_slice(),
                _ => b"payload".as_slice(),
            };
            assert_eq!(fs::read(path).unwrap(), expected);
        }
    }
}
