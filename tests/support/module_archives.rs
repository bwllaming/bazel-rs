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
// Registry patches/overlays are verified and seeded too: disabling repository
// downloads also disallows copying a missing blob from a file:// registry.

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

/// The commands that take `--registry` and resolve the built-in modules:
/// [`with_builtin_registry`] adds the offline registry only to them (`info`
/// and `clean` take it too but resolve nothing; `info` must not even create
/// the cache directory that [`SeededArchives`] would fill).
#[allow(dead_code)]
const REGISTRY_COMMANDS: &[&str] = &[
    "build", "test", "run", "query", "cquery", "aquery", "mod", "fetch", "coverage",
];

/// `args` (startup options, the command word, its arguments) with the
/// offline built-in registry ([`builtin_registry_flag`]) right after the
/// command word, unless they select a registry already or the command does
/// not take one (`help`, `version`, an unknown command: their exit codes and
/// texts stay the same). Without it the built-in closure of the default
/// module profile resolves through https://bcr.bazel.build/ on every run.
#[allow(dead_code)]
fn with_builtin_registry<S: AsRef<str>>(args: &[S]) -> Vec<String> {
    let mut args: Vec<String> = args.iter().map(|arg| arg.as_ref().to_owned()).collect();
    if args
        .iter()
        .any(|arg| arg == "--registry" || arg.starts_with("--registry="))
    {
        return args;
    }
    if let Some(command) = args.iter().position(|arg| !arg.starts_with('-'))
        && REGISTRY_COMMANDS.contains(&args[command].as_str())
    {
        args.insert(command + 1, builtin_registry_flag());
    }
    args
}

/// Whether `arg` (with `previous`, for the two-word spelling) selects the
/// offline built-in registry fixture.
#[allow(dead_code)]
fn selects_builtin_registry(previous: Option<&str>, arg: &str) -> bool {
    let value = match arg.strip_prefix("--registry=") {
        Some(value) => value,
        None if previous == Some("--registry") => arg,
        None => return false,
    };
    value.starts_with("file://")
        && value
            .trim_end_matches('/')
            .ends_with("tests/fixtures/builtin_modules/registry")
}

#[allow(dead_code)]
trait SeededArchives {
    /// When this bazel-rs command line selects the offline built-in registry
    /// (`--registry=file://…/tests/fixtures/builtin_modules/registry`) and
    /// names a `--cache-dir` (relative ones against the command's current
    /// directory, so chain it after `current_dir`), seed that cache with
    /// every archive of the built-in closure ([`TOOLCHAIN_CLOSURE_ARCHIVES`],
    /// vendored or from the test archive mirror): the build then never
    /// downloads a module source archive. Chain it right before
    /// `.output()`/`.status()`/`.spawn()`.
    fn seeded_archives(&mut self) -> &mut Self;
}

impl SeededArchives for std::process::Command {
    fn seeded_archives(&mut self) -> &mut Self {
        let args: Vec<String> = self
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let builtin = args.iter().enumerate().any(|(index, arg)| {
            selects_builtin_registry(index.checked_sub(1).map(|at| args[at].as_str()), arg)
        });
        let cache = args
            .iter()
            .find_map(|arg| arg.strip_prefix("--cache-dir="));
        if let (true, Some(cache)) = (builtin, cache) {
            let mut cache = std::path::PathBuf::from(cache);
            if cache.is_relative() {
                let base = self
                    .get_current_dir()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| std::env::current_dir().unwrap());
                cache = base.join(cache);
            }
            seed_module_archives(&cache, TOOLCHAIN_CLOSURE_ARCHIVES);
        }
        self
    }
}

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
        let module_dir = fixtures
            .join("builtin_modules/registry/modules")
            .join(name)
            .join(version);
        let source: serde_json::Value =
            serde_json::from_slice(&std::fs::read(module_dir.join("source.json")).unwrap())
                .unwrap();
        for kind in ["patches", "overlay"] {
            if let Some(files) = source.get(kind).and_then(serde_json::Value::as_object) {
                for (relative, integrity) in files {
                    seed_verified_module_file(
                        &downloads,
                        &module_dir.join(kind).join(relative),
                        integrity.as_str().unwrap(),
                    );
                }
            }
        }
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
        if blob.exists() {
            // Seeded (and verified) by an earlier command of this cache.
            continue;
        }
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

/// A local registry patch/overlay, verified against its captured source.json
/// pin before publishing it just like a downloaded content-addressed blob.
#[allow(dead_code)]
fn seed_verified_module_file(
    downloads: &std::path::Path,
    input: &std::path::Path,
    integrity: &str,
) {
    use base64::Engine as _;
    use sha2::Digest as _;
    let digest = base64::engine::general_purpose::STANDARD
        .decode(integrity.strip_prefix("sha256-").unwrap())
        .unwrap();
    let bytes = std::fs::read(input).unwrap_or_else(|error| panic!("{}: {error}", input.display()));
    assert_eq!(
        sha2::Sha256::digest(&bytes).as_slice(),
        digest.as_slice(),
        "{} does not match its source.json integrity",
        input.display()
    );
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let blob = downloads.join(hex);
    if !blob.exists() {
        let mut staged = tempfile::NamedTempFile::new_in(downloads).unwrap();
        std::io::Write::write_all(&mut staged, &bytes).unwrap();
        staged.persist(&blob).unwrap();
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

/// The public mirrors of the Bazel Central Registry
/// (`https://bcr.bazel.build/bazel_registry.json` once listed them; Bazel
/// tries `<mirror><url without scheme>`): the fill falls back to them when
/// the source.json URL fails (GitHub release downloads answer 504 at times).
/// Every candidate's bytes are verified against the pinned sha256.
#[allow(dead_code)]
const BCR_MIRRORS: &[&str] = &[
    "https://bcr.cloudflaremirrors.com/",
    "https://mirror.bazel.build/",
];

/// The URLs the one-time fill tries for an archive: its source.json URL,
/// then each [`BCR_MIRRORS`] spelling of it.
#[allow(dead_code)]
fn archive_candidates(url: &str) -> Vec<String> {
    let mut urls = vec![url.to_owned()];
    if let Some((_, rest)) = url.split_once("://") {
        urls.extend(BCR_MIRRORS.iter().map(|mirror| format!("{mirror}{rest}")));
    }
    urls
}

/// The mirror's blob of `module` (`<mirror>/<sha256 hex>`): downloaded once
/// with curl when missing (its source.json `url`, then the BCR mirrors;
/// three rounds with backoff, one test at a time per blob under a lock
/// file), verified against the pinned digest before it is published
/// (renamed into place, so concurrent tests never see a partial blob) and
/// before every use. A filled mirror never touches the network. Offline
/// with an empty mirror this fails with what to do, never with a bazel-rs
/// download error.
#[allow(dead_code)]
fn mirrored_archive(module: &str, url: &str, digest: &[u8], hex: &str) -> std::path::PathBuf {
    use fs2::FileExt as _;
    // One test at a time per process: the blobs are verified once per
    // process (hashing the 31 MB on every seeded command would dominate the
    // harnesses that run many commands), filled once per mirror.
    static FILL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static VERIFIED: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());
    // A fill that failed already in this test process fails at once (one
    // program's tests would otherwise each retry it).
    static FAILED: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());
    let _fill = FILL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mirror = archive_mirror();
    std::fs::create_dir_all(&mirror).unwrap();
    let blob = mirror.join(hex);
    if VERIFIED.lock().unwrap().contains(&blob) {
        return blob;
    }
    if let Some((_, message)) = FAILED.lock().unwrap().iter().find(|(failed, _)| failed == hex) {
        panic!("{message}");
    }
    let verified = |path: &std::path::Path| file_has_sha256(path, digest, hex);
    let remember = |blob: std::path::PathBuf| {
        VERIFIED.lock().unwrap().push(blob.clone());
        blob
    };
    if verified(&blob) {
        return remember(blob);
    }
    // One download per blob: concurrent tests (and test programs) wait for
    // the first one's instead of fetching the same archive in parallel.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(mirror.join(format!("{hex}.lock")))
        .unwrap();
    lock.lock_exclusive().unwrap();
    if verified(&blob) {
        return remember(blob);
    }
    let candidates = archive_candidates(url);
    let mut failures = Vec::new();
    // Each round tries every candidate once (a failing origin falls through
    // to the mirrors at once), the next one after a backoff.
    for round in 0..3u64 {
        if round > 0 {
            std::thread::sleep(std::time::Duration::from_secs(5 * round));
        }
        for candidate in &candidates {
            let staged = tempfile::NamedTempFile::new_in(&mirror).unwrap();
            let fetched = std::process::Command::new("curl")
                // A stalled origin (GitHub's release CDN answers 504 after
                // a minute at times) gives way to the next candidate.
                .args(["-fsSL", "--connect-timeout", "30", "--max-time", "600"])
                .args(["--speed-limit", "1024", "--speed-time", "15", "-o"])
                .arg(staged.path())
                .arg(candidate)
                .stdin(std::process::Stdio::null())
                .output();
            match fetched {
                Ok(output) if output.status.success() && verified(staged.path()) => {
                    staged.persist(&blob).unwrap();
                    return remember(blob);
                }
                Ok(output) if output.status.success() => {
                    failures.push(format!("{candidate}: not the pinned sha256"));
                }
                Ok(output) => failures.push(format!(
                    "{candidate}: curl {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
                Err(error) => failures.push(format!("{candidate}: curl: {error}")),
            }
        }
    }
    let message = format!(
        "the module archive {module} is not in the test archive mirror {} and the one-time fill \
         failed (sha256 {hex}):\n  {}\nrun the tests once with network access, or put the verified \
         file there as {hex} (BAZEL_RS_TEST_ARCHIVE_MIRROR names another mirror; \
         tests/fixtures/module_archives/README.md)",
        mirror.display(),
        failures.join("\n  ")
    );
    FAILED.lock().unwrap().push((hex.to_owned(), message.clone()));
    panic!("{message}")
}

/// Whether the file at `path` has the SHA-256 `digest` (`hex`): coreutils'
/// `sha256sum` when present (the test profile's sha2 is unoptimized, ~20x
/// slower on the 23 MB rules_kotlin archive), else sha2.
#[allow(dead_code)]
fn file_has_sha256(path: &std::path::Path, digest: &[u8], hex: &str) -> bool {
    use sha2::Digest as _;
    if !path.is_file() {
        return false;
    }
    match std::process::Command::new("sha256sum")
        .arg("-b")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).split_whitespace().next() == Some(hex)
        }
        _ => std::fs::read(path)
            .is_ok_and(|bytes| sha2::Sha256::digest(&bytes).as_slice() == digest),
    }
}
