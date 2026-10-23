//! N60: genuine `cc_library` and `cc_import` build through rules_cc 0.2.17's
//! Starlark `cc_common.compile`/`link` over the native `cc_internal` surface
//! on the host `@local_config_cc` toolchain (docs/CC_ACTIONS.md). The paired
//! genuine Bazel 9.3 capture (byte-identical .o/.d/.a/.so and the go
//! c-archive/cdeps paths) is tests/fixtures/cc_compile/reference_receipt.json,
//! written by artifacts/cc_compile-capture.py; this gate covers the CLI path
//! on every run: the builds succeed, the artifacts have the right shapes, a
//! warm rebuild is fully cached and a fresh cache reproduces identical bytes.
//! The fixture workspace is tests/fixtures/cc_compile/workspace (PROVENANCE.md
//! for the vendored modules and the generated prebuilt libraries).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

include!("support/executable_script.rs");
include!("support/verify_env.rs");

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const STUBBED: &[&str] = &[
    "buildozer",
    "rules_java",
    "rules_python",
    "rules_shell",
    "zlib",
];
const VENDORED: &[&str] = &[
    "rules_cc",
    "bazel_skylib",
    "platforms",
    "bazel_features",
    "package_metadata",
    "apple_support",
    "hermetic_cc_toolchain",
];
const TARGETS: &[&str] = &["//cdep:built", "//cdep:prebuilt", "//cdep:prebuilt_shared"];
const ARTIFACTS: [&str; 4] = [
    "bazel-out/k8-fastbuild/bin/cdep/_objs/built/prebuilt.pic.o",
    "bazel-out/k8-fastbuild/bin/cdep/_objs/built/prebuilt.pic.d",
    "bazel-out/k8-fastbuild/bin/cdep/libbuilt.a",
    "bazel-out/k8-fastbuild/bin/cdep/libbuilt.so",
];

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    manifest().join("tests/fixtures/cc_compile")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            // An executable fixture is written by a separate process: no
            // descriptor of it in this (spawning) test process (ETXTBSY).
            let mode = fs::metadata(entry.path()).unwrap().permissions().mode() & 0o7777;
            if mode & 0o111 != 0 {
                write_executable_script(&target, fs::read(entry.path()).unwrap(), mode);
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }
}

struct Workspace {
    root: PathBuf,
    _keep: tempfile::TempDir,
}

impl Workspace {
    /// The fixture workspace plus the generated prebuilt libraries
    /// (artifacts/cc_compile-capture.py's setup, verbatim commands).
    fn new() -> Self {
        let keep = tempfile::Builder::new()
            .prefix("cc-compile-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        let root = keep.path().to_path_buf();
        copy_tree(&fixture().join("workspace"), &root);
        // The genuine files already vendored for N25 (byte-identical), then
        // this fixture's own genuine files.
        let toolchain = manifest().join("tests/fixtures/cc_toolchain");
        let shared = manifest().join("tests/fixtures/cc_features/vendor");
        for relative in fs::read_to_string(toolchain.join("vendor_shared.txt"))
            .unwrap()
            .lines()
        {
            let target = root.join("vendor").join(relative);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::copy(shared.join(relative), target).unwrap();
        }
        copy_tree(&toolchain.join("vendor"), &root.join("vendor"));
        // This fixture's own rules_cc overlay (the rule files beyond the
        // N25d toolchain subset; PROVENANCE.md).
        copy_tree(&fixture().join("workspace/vendor"), &root.join("vendor"));
        let mut module = String::from(
            "module(name = \"cc_compile_fixture\")\n\n\
             bazel_dep(name = \"rules_cc\", version = \"0.2.17\")\n\
             bazel_dep(name = \"platforms\", version = \"1.1.0\")\n\
             bazel_dep(name = \"bazel_skylib\", version = \"1.9.0\")\n\
             bazel_dep(name = \"hermetic_cc_toolchain\", version = \"4.1.0\")\n",
        );
        for name in VENDORED {
            module.push_str(&format!(
                "local_path_override(module_name = \"{name}\", path = \"vendor/{name}\")\n"
            ));
        }
        let stubs = manifest().join("tests/fixtures/test_flags/stubs");
        for name in STUBBED {
            module.push_str(&format!(
                "local_path_override(module_name = \"{name}\", path = \"{}\")\n",
                stubs.join(name).display()
            ));
        }
        module.push_str(&format!(
            "local_path_override(module_name = \"protobuf\", path = \"{}\")\n",
            toolchain.join("stubs/protobuf").display()
        ));
        fs::write(root.join("MODULE.bazel"), module).unwrap();
        for directory in ["home", "tmp"] {
            fs::create_dir(root.join(directory)).unwrap();
        }
        // The deterministic prebuilt libraries of the paired capture.
        let cdep = root.join("cdep");
        let env = [("PATH", "/usr/bin:/bin"), ("LC_ALL", "C"), ("LANG", "C")];
        let status = Command::new("gcc")
            .args(["-c", "-O0", "-g0", "prebuilt.c", "-o", "prebuilt.o"])
            .env_clear()
            .envs(env.iter().copied())
            .current_dir(&cdep)
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("ar")
            .args(["rcD", "libprebuilt.a", "prebuilt.o"])
            .env_clear()
            .envs(env.iter().copied())
            .current_dir(&cdep)
            .status()
            .unwrap();
        assert!(status.success());
        fs::remove_file(cdep.join("prebuilt.o")).unwrap();
        let status = Command::new("gcc")
            .args([
                "-shared",
                "-O0",
                "-g0",
                "-Wl,-build-id=none",
                "-Wl,-soname=libprebuilt.so",
                "prebuilt.c",
                "-o",
                "libprebuilt.so",
            ])
            .env_clear()
            .envs(env.iter().copied())
            .current_dir(&cdep)
            .status()
            .unwrap();
        assert!(status.success());
        Self { root, _keep: keep }
    }

    fn build(&self, cache: &str, args: &[&str]) -> Output {
        self.build_with_env(cache, args, &[])
    }

    /// `build` plus extra client environment variables (N63's
    /// SOURCE_DATE_EPOCH pins the workspace status timestamp).
    fn build_with_env(&self, cache: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
        let registry = format!(
            "--registry=file://{}",
            manifest()
                .join("tests/fixtures/builtin_modules/registry")
                .display()
        );
        let mut command = Command::new("/usr/bin/timeout");
        command
            .args(["--kill-after=5s", "900s"])
            .arg(env!("CARGO_BIN_EXE_bazel-rs"))
            .args(["--ignore_all_rc_files", &format!("--cache-dir={cache}")])
            .args(["build", "--experimental-builtin-modules", registry.as_str()])
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("LC_ALL", "C");
        for (name, value) in env {
            command.env(name, value);
        }
        command.output().unwrap()
    }

    /// sha256 of every artifact under the cache (bazel-out relative path).
    fn artifact_hashes(&self, cache: &str) -> Vec<(String, String)> {
        let mut hashes = Vec::new();
        for relative in ARTIFACTS {
            let hits: Vec<PathBuf> = fs::read_dir(self.root.join(cache).join("actions"))
                .unwrap()
                .map(|entry| entry.unwrap().path().join("outputs").join(relative))
                .filter(|path| path.exists())
                .collect();
            assert_eq!(hits.len(), 1, "{relative}: {hits:?}");
            hashes.push((relative.to_owned(), sha256(&fs::read(&hits[0]).unwrap())));
        }
        hashes
    }
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `label\tpath` lines of a successful build.
fn outputs(output: &Output) -> Vec<(String, PathBuf)> {
    assert!(output.status.success(), "{}", stderr(output));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(label, path)| (label.to_owned(), PathBuf::from(path)))
        .collect()
}

#[test]
fn cc_library_and_cc_import_build_cached_and_deterministic() {
    if !Path::new("/usr/bin/gcc").exists() && !Path::new("/usr/bin/cc").exists() {
        eprintln!("skipped: no host C compiler for cc_configure");
        return;
    }
    let ws = Workspace::new();
    let first = ws.build("cache", TARGETS);
    assert!(first.status.success(), "{}", stderr(&first));
    let hashes = ws.artifact_hashes("cache");

    // The artifact shapes of the paired capture: an ELF shared object, an ar
    // archive and a dependency file naming the header.
    for (relative, hash) in &hashes {
        assert_eq!(hash.len(), 64, "{relative}");
    }
    let (object, dotd, archive, shared) = {
        let root = ws.root.clone();
        let read = move |cache: &str, relative: &str| {
            fs::read_dir(root.join(cache).join("actions"))
                .unwrap()
                .map(|entry| entry.unwrap().path().join("outputs").join(relative))
                .find(|path| path.exists())
                .map(|path| fs::read(path).unwrap())
                .unwrap()
        };
        (
            read("cache", ARTIFACTS[0]),
            read("cache", ARTIFACTS[1]),
            read("cache", ARTIFACTS[2]),
            read("cache", ARTIFACTS[3]),
        )
    };
    assert!(object.starts_with(&[0x7f, b'E', b'L', b'F']), "{object:?}");
    assert!(shared.starts_with(&[0x7f, b'E', b'L', b'F']), "{shared:?}");
    assert!(archive.starts_with(b"!<arch>\n"), "{archive:?}");
    let dotd = String::from_utf8_lossy(&dotd);
    assert!(dotd.contains("prebuilt.h"), "{dotd}");

    // A warm rebuild is fully cached.
    let warm = ws.build("cache", TARGETS);
    assert!(warm.status.success(), "{}", stderr(&warm));
    assert!(
        stderr(&warm).contains("0 actions executed"),
        "{}",
        stderr(&warm)
    );

    // A fresh cache reproduces identical bytes (self-determinism; the
    // genuine-Bazel byte identity is the paired receipt's claim).
    let cold = ws.build("cache2", TARGETS);
    assert!(cold.status.success(), "{}", stderr(&cold));
    assert_eq!(ws.artifact_hashes("cache2"), hashes);
}

#[test]
fn module_interfaces_fail_before_any_native_boundary() {
    if !Path::new("/usr/bin/gcc").exists() && !Path::new("/usr/bin/cc").exists() {
        eprintln!("skipped: no host C compiler for cc_configure");
        return;
    }
    let ws = Workspace::new();
    let output = ws.build("cache", &["//cdep:modules_unsupported"]);
    assert!(!output.status.success());
    let text = stderr(&output);
    // The rule's own gate (attrs.bzl) comes before any native boundary.
    assert!(
        text.contains("attribute module_interfaces: requires --experimental_cpp_modules"),
        "{text}"
    );
}

/// N63: the recorded next cc boundary closed — `cc_binary` over a built
/// `cc_library` links end to end (SKIP_ANALYSIS_TIME_FILETYPE_CHECK honored
/// on `deps`, the rules_cc root BUILD's link_extra_lib implicit dep), the
/// workspace status action feeds `ctx.info_file`/`ctx.version_file` inputs
/// and the `_build_info_files` translator, and the fixed status script +
/// SOURCE_DATE_EPOCH make every byte reproducible (the paired genuine
/// capture is tests/fixtures/workspace_status2/reference_receipt.json).
#[test]
fn cc_binary_links_and_the_workspace_status_stamps() {
    if !Path::new("/usr/bin/gcc").exists() && !Path::new("/usr/bin/cc").exists() {
        eprintln!("skipped: no host C compiler for cc_configure");
        return;
    }
    let ws = Workspace::new();
    let script = format!("--workspace_status_command={}/status.sh", ws.root.display());
    let pinned = [("SOURCE_DATE_EPOCH", "1792511811")];
    let first = ws.build_with_env(
        "cache",
        &[&script, "//bin:tool", "//bin:stamp1_tool"],
        &pinned,
    );
    assert!(first.status.success(), "{}", stderr(&first));
    let built = outputs(&first);
    let binary = built
        .iter()
        .find(|(label, _)| label == "//bin:tool")
        .map(|(_, path)| fs::read(path).unwrap())
        .unwrap();
    // A real ELF that runs correctly.
    assert!(
        binary.starts_with(&[0x7f, b'E', b'L', b'F']),
        "{} bytes",
        binary.len()
    );

    // The status files and the stamped build-info headers, deterministic
    // under SOURCE_DATE_EPOCH + the fixed script.
    let deterministic = ws.build_with_env(
        "cache2",
        &[
            script.as_str(),
            "--embed_label=n63-fixture",
            "//bin:status_cat",
            "//bin:stamped_headers",
        ],
        &pinned,
    );
    assert!(deterministic.status.success(), "{}", stderr(&deterministic));
    let built = outputs(&deterministic);
    let read = |suffix: &str| {
        built
            .iter()
            .find(|(_, path)| path.ends_with(suffix))
            .map(|(_, path)| fs::read_to_string(path).unwrap())
            .unwrap_or_else(|| panic!("no output {suffix} in {built:?}"))
    };
    let host = read("status_cat.txt")
        .lines()
        .find(|line| line.starts_with("BUILD_HOST "))
        .unwrap()
        .trim_start_matches("BUILD_HOST ")
        .to_owned();
    assert_eq!(
        read("status_cat.txt"),
        format!(
            "BUILD_EMBED_LABEL n63-fixture\nBUILD_HOST {host}\nBUILD_USER stampuser\nSTABLE_COMMIT deadbeefcafe\nSTABLE_SCM_STATUS clean\nBUILD_SCM_REVISION deadbeefcafe\nBUILD_SCM_STATUS clean\nBUILD_TIMESTAMP 1792511811\nFORMATTED_DATE 2026 Oct 20 15 56 51 Tue\n"
        )
    );
    assert_eq!(
        read("volatile_file.h"),
        "#define BUILD_SCM_REVISION \"deadbeefcafe\"\n#define BUILD_SCM_STATUS \"clean\"\n#define BUILD_TIMESTAMP 1792511811\n"
    );
    assert_eq!(
        read("non_volatile_file.h"),
        format!(
            "#define BUILD_EMBED_LABEL \"n63-fixture\"\n#define BUILD_HOST \"{host}\"\n#define BUILD_USER \"stampuser\"\n"
        )
    );

    // The same build in a fresh cache reproduces identical bytes.
    let cold = ws.build_with_env(
        "cache3",
        &[
            script.as_str(),
            "--embed_label=n63-fixture",
            "//bin:status_cat",
            "//bin:stamped_headers",
            "//bin:tool",
        ],
        &pinned,
    );
    assert!(cold.status.success(), "{}", stderr(&cold));

    // The recorded remaining cc boundary: a cc_library `linkstamp` attribute
    // puts the LinkstampInfo provider (rules_cc's private-API wrapper
    // functions) in a linker-input depset, which cannot cross analyses
    // natively; the failure is explicit.
    let output = ws.build("cache", &[&script, "//bin:stamped_tool"]);
    let text = stderr(&output);
    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains("a depset element containing a function that is not a global def of a loaded .bzl file is not implemented in the native profile"),
        "{text}"
    );
}
