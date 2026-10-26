// Genuine module source archives for the builtin fixture registry
// (include!-ed by tests that build with the built-in modules, the default
// profile, and `--registry=file://<tests/fixtures/builtin_modules/registry>`).
//
// The registry fixture carries MODULE files and source.json, not source
// archives, so every test run that materialized such a module downloaded
// its archive from GitHub. A pre-seeded repository download cache keeps
// that local, exactly like Bazel's repository cache: the blob at
// `<cache dir>/repositories/downloads/sha256/<hex>` that the module's
// source.json integrity names is verified and used before any download
// (src/materializer.rs `cached_download`). The archives are the genuine
// release files (tests/fixtures/module_archives/README.md); each is checked
// against the registry's pinned integrity before it is seeded.

/// The offline genuine registry (tests/fixtures/builtin_modules), as a
/// `--registry` flag.
#[allow(dead_code)] // includers use either helper or both
fn builtin_registry_flag() -> String {
    format!(
        "--registry=file://{}",
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/builtin_modules/registry")
            .display()
    )
}

/// The vendored archives (repository-relative), by `name@version` of the
/// builtin fixture registry.
#[allow(dead_code)]
const MODULE_ARCHIVES: &[(&str, &str)] = &[
    (
        "bazel_features@1.42.1",
        "tests/fixtures/module_archives/bazel_features-v1.42.1.tar.gz",
    ),
    (
        "bazel_skylib@1.8.2",
        "tests/fixtures/build_settings/bazel-skylib-1.8.2.tar.gz",
    ),
    (
        "platforms@1.0.0",
        "tests/fixtures/module_archives/platforms-1.0.0.tar.gz",
    ),
    (
        "rules_license@1.0.0",
        "tests/fixtures/module_archives/rules_license-1.0.0.tar.gz",
    ),
    (
        "rules_python@1.7.0",
        "tests/fixtures/module_archives/rules_python-1.7.0.tar.gz",
    ),
];

/// The source archives a build that resolves every registered toolchain of
/// the builtin closure materializes (the offline registry's pins; the large
/// ones come from the test archive mirror).
#[allow(dead_code)]
const TOOLCHAIN_CLOSURE_ARCHIVES: &[&str] = &[
    "apple_support@2.5.2",
    "bazel_features@1.42.1",
    "bazel_skylib@1.8.2",
    "platforms@1.0.0",
    "protobuf@33.4",
    "rules_cc@0.2.17",
    "rules_java@9.1.0",
    "rules_kotlin@1.9.6",
    "rules_license@1.0.0",
    "rules_python@1.7.0",
    "rules_shell@0.6.1",
    "rules_swift@3.1.2",
];

/// Seed the repository download cache of `cache_dir` (a `--cache-dir`)
/// with the source archives of `modules` (`name@version` of the builtin
/// fixture registry): vendored in [`MODULE_ARCHIVES`], or else from the
/// shared test mirror ([`mirrored_archive`]). Idempotent.
#[allow(dead_code)]
fn seed_module_archives(cache_dir: &std::path::Path, modules: &[&str]) {
    use base64::Engine as _;
    use sha2::Digest as _;
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let downloads = cache_dir.join("repositories/downloads/sha256");
    std::fs::create_dir_all(&downloads).unwrap();
    for module in modules {
        let (name, version) = module.split_once('@').unwrap();
        let source = fixtures
            .join("builtin_modules/registry/modules")
            .join(name)
            .join(version)
            .join("source.json");
        let source: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&source).unwrap()).unwrap();
        let url = source["url"].as_str().unwrap();
        let integrity = source["integrity"]
            .as_str()
            .and_then(|sri| sri.strip_prefix("sha256-"))
            .unwrap();
        let digest = base64::engine::general_purpose::STANDARD
            .decode(integrity)
            .unwrap();
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        let blob = downloads.join(&hex);
        let Some((_, relative)) = MODULE_ARCHIVES
            .iter()
            .find(|(vendored, _)| vendored == module)
        else {
            // Too large to vendor: from the shared test mirror.
            let mirrored = mirrored_archive(module, url, &digest, &hex);
            if !blob.exists() && std::fs::hard_link(&mirrored, &blob).is_err() {
                let mut staged = tempfile::NamedTempFile::new_in(&downloads).unwrap();
                std::io::copy(
                    &mut std::fs::File::open(&mirrored).unwrap(),
                    staged.as_file_mut(),
                )
                .unwrap();
                staged.persist(&blob).unwrap();
            }
            continue;
        };
        let archive = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
        let bytes = std::fs::read(&archive)
            .unwrap_or_else(|error| panic!("{}: {error}", archive.display()));
        assert_eq!(
            sha2::Sha256::digest(&bytes).as_slice(),
            digest.as_slice(),
            "{} is not the archive {module}'s source.json pins",
            archive.display()
        );
        if !blob.exists() {
            // Published like a download: a private temporary file renamed
            // into place.
            let mut staged = tempfile::NamedTempFile::new_in(&downloads).unwrap();
            std::io::Write::write_all(&mut staged, &bytes).unwrap();
            staged.persist(&blob).unwrap();
        }
    }
}

/// The test archive mirror: the module archives too large to vendor (31 MB
/// of rules_kotlin, rules_swift, protobuf, ...; README of
/// tests/fixtures/module_archives), downloaded once per target directory and
/// shared by every test run after it. `BAZEL_RS_TEST_ARCHIVE_MIRROR` names
/// another directory (e.g. one filled on a machine with network access).
#[allow(dead_code)]
fn archive_mirror() -> std::path::PathBuf {
    std::env::var_os("BAZEL_RS_TEST_ARCHIVE_MIRROR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("module-archive-mirror")
        })
}

/// The mirror's blob of `module` (`<mirror>/<sha256 hex>`): downloaded from
/// its source.json `url` with curl when missing, verified against the pinned
/// digest before it is published (renamed into place, so concurrent tests
/// never see a partial blob) and before every use. Offline with an empty
/// mirror this fails with what to do, never with a bazel-rs download error.
#[allow(dead_code)]
fn mirrored_archive(module: &str, url: &str, digest: &[u8], hex: &str) -> std::path::PathBuf {
    use sha2::Digest as _;
    let mirror = archive_mirror();
    std::fs::create_dir_all(&mirror).unwrap();
    let blob = mirror.join(hex);
    let verified = |path: &std::path::Path| {
        std::fs::read(path).is_ok_and(|bytes| sha2::Sha256::digest(&bytes).as_slice() == digest)
    };
    if verified(&blob) {
        return blob;
    }
    let staged = tempfile::NamedTempFile::new_in(&mirror).unwrap();
    let fetched = std::process::Command::new("curl")
        .args(["-fsSL", "--retry", "2", "--connect-timeout", "30", "-o"])
        .arg(staged.path())
        .arg(url)
        .status();
    match fetched {
        Ok(status) if status.success() && verified(staged.path()) => {
            staged.persist(&blob).unwrap();
            blob
        }
        outcome => panic!(
            "the module archive {module} is not in the test archive mirror {} and fetching it once failed \
             ({outcome:?}; {url} must have the sha256 {hex}): run this test once with network access, \
             or put the verified file there as {hex} (BAZEL_RS_TEST_ARCHIVE_MIRROR names another mirror)",
            mirror.display()
        ),
    }
}
