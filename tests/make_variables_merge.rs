//! integration-15: N53's Make variables never read the action environment
//! (N42): `--action_env` values are not `$(VAR)`s
//! (ConfigurationMakeVariableContext has no action-environment source).
use std::fs;
use std::process::Command;

#[test]
fn action_env_is_not_a_make_variable_source() {
    let dir = tempfile::Builder::new()
        .prefix("make-vars-merge-")
        .tempdir_in("/tmp/opencode")
        .unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::write(ws.join("MODULE.bazel"), "module(name = \"mv\")\n").unwrap();
    fs::create_dir_all(ws.join("p")).unwrap();
    fs::write(ws.join("p/BUILD.bazel"), "platform(name = \"host\")\n").unwrap();
    fs::write(
        ws.join("BUILD.bazel"),
        "genrule(name = \"g\", outs = [\"g.txt\"], cmd = \"echo $(N53_VAR) > $@\")\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!("--cache-dir={}", dir.path().join("cache").display()))
        .args([
            "build",
            "--host_platform=//p:host",
            "--action_env=N53_VAR=x",
            "//:g",
        ])
        .current_dir(&ws)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir.path())
        .env("N53_VAR", "client")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("in cmd attribute of genrule rule //:g: $(N53_VAR) not defined"),
        "{stderr}"
    );
}
