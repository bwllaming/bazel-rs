// The offline rules_java 9.1.0 module scaffold of the Java action tests
// (include!-ed): the pinned rules_java, rules_cc, platforms, bazel_skylib
// and rules_license archives of the builtin fixture registry, unpacked as
// local overrides with empty BUILD files (their Starlark unchanged), and
// stubs for the modules their MODULE files would otherwise pull in.
// Returns the root MODULE.bazel lines after `module(...)`.

#[allow(dead_code)]
fn java_scaffold_empty_builds(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            java_scaffold_empty_builds(&entry.path());
        } else if matches!(entry.file_name().to_str(), Some("BUILD" | "BUILD.bazel")) {
            std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
            std::fs::write(entry.path(), "").unwrap();
        }
    }
}

fn java_scaffold(root: &std::path::Path) -> String {
    use base64::Engine as _;
    use std::fs;
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let write = |name: &str, text: &str| {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        if path.exists() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        fs::write(path, text).unwrap();
    };
    let mut module = String::new();
    seed_module_archives(
        &root.join("cache"),
        &[
            "rules_java@9.1.0",
            "rules_cc@0.2.17",
            "platforms@1.0.0",
            "bazel_skylib@1.8.2",
            "rules_license@1.0.0",
        ],
    );
    for (name, version) in [
        ("rules_java", "9.1.0"),
        ("rules_cc", "0.2.17"),
        ("platforms", "1.0.0"),
        ("bazel_skylib", "1.8.2"),
        ("rules_license", "1.0.0"),
    ] {
        let pin: serde_json::Value = serde_json::from_slice(
            &fs::read(manifest.join(format!(
                "tests/fixtures/builtin_modules/registry/modules/{name}/{version}/source.json"
            )))
            .unwrap(),
        )
        .unwrap();
        let digest = base64::engine::general_purpose::STANDARD
            .decode(
                pin["integrity"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("sha256-")
                    .unwrap(),
            )
            .unwrap();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        let archive =
            fs::File::open(root.join("cache/repositories/downloads/sha256").join(hex)).unwrap();
        let staging = root.join(format!("extract-{name}"));
        tar::Archive::new(flate2::read::GzDecoder::new(archive))
            .unpack(&staging)
            .unwrap();
        fs::rename(
            staging.join(pin["strip_prefix"].as_str().unwrap_or("")),
            root.join(name),
        )
        .unwrap();
        if name != "platforms" {
            java_scaffold_empty_builds(&root.join(name));
            let deps = match name {
                "rules_java" => {
                    "bazel_dep(name='rules_cc',version='0.2.17')\nbazel_dep(name='bazel_skylib',version='1.8.2')\nbazel_dep(name='platforms',version='1.0.0')\n"
                }
                "rules_cc" => {
                    "bazel_dep(name='bazel_skylib',version='1.8.2')\nbazel_dep(name='cc_compatibility_proxy',version='1')\n"
                }
                _ => "",
            };
            write(
                &format!("{name}/MODULE.bazel"),
                &format!("module(name='{name}',version='{version}')\n{deps}"),
            );
        }
        module += &format!(
            "bazel_dep(name='{name}',version='{version}')\nlocal_path_override(module_name='{name}',path='{name}')\n"
        );
    }
    // Restrict public exports to the actual java_import implementation
    // used by @bazel_tools' otherwise unrelated TestRunner declaration.
    write(
        "rules_java/java/defs.bzl",
        "load('//java/bazel/rules:bazel_java_import.bzl', _import='java_import')\njava_import=_import\n",
    );
    write(
        "rules_java/toolchains/BUILD",
        "alias(name='current_java_toolchain',actual='@@//:compiler',visibility=['//visibility:public'])\n",
    );
    let cc = fs::read_to_string(root.join("rules_cc/cc/extensions.bzl")).unwrap();
    let symbols = cc
        .split_once("\"symbols.bzl\",")
        .unwrap()
        .1
        .split_once("\"\"\"")
        .unwrap()
        .1
        .split_once("\"\"\"")
        .unwrap()
        .0;
    write("cc_proxy/symbols.bzl", symbols);
    write("cc_proxy/BUILD.bazel", "");
    write(
        "cc_proxy/MODULE.bazel",
        "module(name='cc_compatibility_proxy',version='1')\nbazel_dep(name='rules_cc',version='0.2.17')\n",
    );
    module += "local_path_override(module_name='cc_compatibility_proxy',path='cc_proxy')\n";
    for name in [
        "apple_support",
        "buildozer",
        "protobuf",
        "rules_shell",
        "zlib",
    ] {
        module += &format!(
            "local_path_override(module_name='{name}',path='{}')\n",
            manifest
                .join("tests/fixtures/test_flags/stubs")
                .join(name)
                .display()
        );
    }
    copy(
        &manifest.join("tests/fixtures/java_actions/python"),
        &root.join("python_stub/python"),
    );
    write(
        "python_stub/MODULE.bazel",
        "module(name='rules_python',version='0.40.0')\n",
    );
    module += "local_path_override(module_name='rules_python',path='python_stub')\n";
    module
}
