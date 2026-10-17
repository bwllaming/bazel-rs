//! Value-safe handling of `--repo_env` on the command line (N40: the option
//! is modelled, crate::repo_env), and the library `mod` entry's rejection.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::process::Command;

const SECRET: &str = "SYNTHETIC_SECRET_42";
const ERROR: &str =
    "unsupported option --repo_env; repository environment overrides are not implemented";

#[test]
fn override_forms_never_echo_values() {
    let workspace = tempfile::Builder::new()
        .prefix("repo-env-options-")
        .tempdir_in("/tmp/opencode")
        .unwrap();
    // Variable names are not secret (`--repo_env=NAME` inherits NAME); values
    // never appear in any diagnostic, whatever the outcome.
    let forms = [
        vec![format!("--repo_env=BAZEL_RS_ENV_FIXTURE={SECRET}")],
        vec![
            "--repo_env".to_owned(),
            format!("BAZEL_RS_ENV_FIXTURE={SECRET}"),
        ],
        vec![format!("--repo_env=A={SECRET}=")],
        vec!["--repo_env".to_owned()],
    ];
    for command in [
        vec![],
        vec!["version"],
        vec!["query"],
        vec!["query", "//:target"],
        vec!["build"],
        vec!["build", "//:target"],
        vec!["mod", "graph", "--output=json"],
        vec!["mod", "unknown"],
        vec!["build", "--jobs"],
        vec!["build", "--lockfile_mode=invalid"],
    ] {
        for form in &forms {
            let output = Command::new("timeout")
                .args(["--kill-after=5", "15", env!("CARGO_BIN_EXE_bazel-rs")])
                .arg("--ignore_all_rc_files")
                .args(&command)
                .args(form)
                .current_dir(workspace.path())
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1), "{command:?} {form:?}");
            assert!(output.stdout.is_empty());
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(!stderr.contains(SECRET), "{stderr}");
            assert!(!stderr.contains("panicked"), "{stderr}");
        }
    }
    assert!(
        std::fs::read_dir(workspace.path())
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn module_library_entry_redacts_before_graph_or_argument_errors() {
    let workspace = tempfile::Builder::new()
        .prefix("repo-env-module-options-")
        .tempdir_in("/tmp/opencode")
        .unwrap();
    for args in [
        vec![format!("--repo_env=A={SECRET}")],
        vec!["graph".into(), format!("--repo_env=A={SECRET}")],
        vec!["unknown".into(), "--repo_env".into(), SECRET.into()],
        vec![
            "graph".into(),
            "--output".into(),
            format!("--repo_env=A={SECRET}"),
        ],
    ] {
        let error = bazel_rs::module_cli::run(workspace.path(), &args).unwrap_err();
        assert_eq!(error, ERROR);
        assert!(!error.contains(SECRET));
    }
    assert!(
        std::fs::read_dir(workspace.path())
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn override_rejection_matches_only_the_exact_option() {
    for arg in [
        "--repo_environment",
        "--repo_environment=A=value",
        "--other=--repo_env=x",
        "x--repo_env",
    ] {
        assert!(
            bazel_rs::module_cli::reject_unsupported_repo_env_argument(std::ffi::OsStr::new(arg))
                .is_ok()
        );
    }
    for arg in [
        "--repo_env",
        "--repo_env=",
        "--repo_env==NAME",
        "--repo_env=NAME=value",
    ] {
        assert_eq!(
            bazel_rs::module_cli::reject_unsupported_repo_env_argument(std::ffi::OsStr::new(arg))
                .unwrap_err(),
            ERROR
        );
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_arguments_cannot_bypass_override_redaction_or_panic() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let workspace = tempfile::Builder::new()
        .prefix("repo-env-byte-options-")
        .tempdir_in("/tmp/opencode")
        .unwrap();
    let mut bad_bytes = SECRET.as_bytes().to_vec();
    bad_bytes.push(0xff);
    let bad_value = OsString::from_vec(bad_bytes);
    let bad_inline =
        OsString::from_vec([b"--repo_env=A=".as_slice(), bad_value.as_encoded_bytes()].concat());
    for args in [
        vec![bad_inline],
        vec!["build".into(), "--repo_env".into(), bad_value.clone()],
        vec![bad_value.clone(), "--repo_env=A=marker".into()],
        vec![
            "mod".into(),
            "graph".into(),
            bad_value.clone(),
            "--repo_env".into(),
        ],
    ] {
        let output = Command::new("timeout")
            .args(["--kill-after=5", "15", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(args)
            .current_dir(workspace.path())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            error,
            "bazel-rs: command-line arguments must be valid UTF-8\n"
        );
        assert!(!error.contains(SECRET));
        assert!(!error.contains("panicked"));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg(bad_value)
        .current_dir(workspace.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "bazel-rs: command-line arguments must be valid UTF-8\n"
    );
    assert!(
        std::fs::read_dir(workspace.path())
            .unwrap()
            .next()
            .is_none()
    );
}
