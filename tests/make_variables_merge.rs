//! integration-15: N53's Make variables never read the action environment
//! (N42): `--action_env` values are not `$(VAR)`s
//! (ConfigurationMakeVariableContext has no action-environment source).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
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
        .arg(format!(
            "--cache-dir={}",
            dir.path().join("cache").display()
        ))
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

/// N54 makes files carried only in a dependency's provider payload action
/// inputs of dependents (Bazel imposes no input restriction); N53's
/// $(location) map stays the declared attributes' files
/// (LocationExpander.buildLocationMap), so such a file is not a location
/// target.
#[test]
fn provider_payload_files_are_inputs_but_not_location_targets() {
    let dir = tempfile::Builder::new()
        .prefix("payload-location-merge-")
        .tempdir_in("/tmp/opencode")
        .unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(ws.join("p")).unwrap();
    fs::write(ws.join("MODULE.bazel"), "module(name = \"pl\")\n").unwrap();
    fs::write(ws.join("p/BUILD.bazel"), "platform(name = \"host\")\n").unwrap();
    fs::write(
        ws.join("defs.bzl"),
        r#"MyInfo = provider(fields = ["f"])
def _prov(ctx):
    f = ctx.actions.declare_file("payload")
    ctx.actions.write(f, "payload\n")
    return [DefaultInfo(files = depset()), MyInfo(f = f)]
prov = rule(_prov)
def _use(ctx):
    f = ctx.attr.deps[0][MyInfo].f
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    text = ctx.expand_location(ctx.attr.text) if ctx.attr.text else ""
    ctx.actions.run_shell(inputs = [f], outputs = [out], command = "cat %s > %s; echo '%s' >> %s" % (f.path, out.path, text, out.path))
    return [DefaultInfo(files = depset([out]))]
use = rule(_use, attrs = {"deps": attr.label_list(), "text": attr.string()})
"#,
    )
    .unwrap();
    fs::write(
        ws.join("BUILD.bazel"),
        "load(\":defs.bzl\", \"prov\", \"use\")\nprov(name = \"prov\")\nuse(name = \"input\", deps = [\":prov\"])\nuse(name = \"location\", deps = [\":prov\"], text = \"$(location :payload)\")\n",
    )
    .unwrap();
    let run = |target: &str| {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                dir.path().join("cache").display()
            ))
            .args(["build", "--host_platform=//p:host", target])
            .current_dir(&ws)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", dir.path())
            .output()
            .unwrap()
    };
    let input = run("//:input");
    assert!(
        input.status.success(),
        "{}",
        String::from_utf8_lossy(&input.stderr)
    );
    let location = run("//:location");
    let stderr = String::from_utf8_lossy(&location.stderr);
    assert!(!location.status.success(), "{stderr}");
    assert!(
        stderr.contains("in use rule //:location: label '//:payload' in $(location) expression is not a declared prerequisite of this rule"),
        "{stderr}"
    );
}
