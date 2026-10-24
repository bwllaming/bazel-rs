// Genuine module source archives for the builtin fixture registry
// (include!-ed by tests that build with `--experimental-builtin-modules`
// and `--registry=file://<tests/fixtures/builtin_modules/registry>`).
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

/// Seed the repository download cache of `cache_dir` (a `--cache-dir`)
/// with the source archives of `modules` (`name@version` of the builtin
/// fixture registry, vendored in [`MODULE_ARCHIVES`]). Idempotent.
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
        let (_, relative) = MODULE_ARCHIVES
            .iter()
            .find(|(vendored, _)| vendored == module)
            .unwrap_or_else(|| panic!("{module} ({url}) is not vendored"));
        let archive = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
        let bytes = std::fs::read(&archive)
            .unwrap_or_else(|error| panic!("{}: {error}", archive.display()));
        assert_eq!(
            sha2::Sha256::digest(&bytes).as_slice(),
            digest.as_slice(),
            "{} is not the archive {module}'s source.json pins",
            archive.display()
        );
        let blob = downloads.join(&hex);
        if !blob.exists() {
            // Published like a download: a private temporary file renamed
            // into place.
            let mut staged = tempfile::NamedTempFile::new_in(&downloads).unwrap();
            std::io::Write::write_all(&mut staged, &bytes).unwrap();
            staged.persist(&blob).unwrap();
        }
    }
}
