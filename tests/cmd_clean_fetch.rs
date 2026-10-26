//! `bazel-rs shutdown`, `clean` and `fetch` (docs/COMMANDS.md) against the
//! genuine Bazel 9.3.0 capture tests/fixtures/cmd_clean_fetch/reference_receipt.json
//! (tests/fixtures/cmd_clean_fetch/capture.py): exit codes, the pinned
//! stderr lines, stdout, whether the built output exists in the output tree
//! after each case and how often the http_archive was downloaded. The
//! cases run in order against one cache, like the capture's one output
//! base. Genuine `<OUTPUT_BASE>` is the workspace's state directory.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    manifest().join("tests/fixtures/cmd_clean_fetch")
}

/// A loopback HTTP server for `/remote.tar.gz`, counting its downloads.
struct Server {
    base: String,
    downloads: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn new(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let downloads = Arc::new(AtomicUsize::new(0));
        let (stopped, counted) = (stop.clone(), downloads.clone());
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("listener: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                let text = String::from_utf8_lossy(&request);
                let response = if text.split(' ').nth(1) == Some("/remote.tar.gz") {
                    counted.fetch_add(1, Ordering::SeqCst);
                    let mut response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    response.extend_from_slice(&body);
                    response
                } else {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec()
                };
                let _ = stream.write_all(&response);
            }
        });
        Self {
            base,
            downloads,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

struct Workspace {
    _temp: tempfile::TempDir,
    root: PathBuf,
    cache: PathBuf,
    home: PathBuf,
}

impl Workspace {
    fn new(files: &[(&str, String)]) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let temp = tempfile::Builder::new()
            .prefix("cmd-clean-fetch-")
            .tempdir_in(base)
            .unwrap();
        let top = fs::canonicalize(temp.path()).unwrap();
        let root = top.join("ws");
        for (name, text) in files {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        let home = top.join("home");
        fs::create_dir_all(&home).unwrap();
        Self {
            _temp: temp,
            cache: top.join("cache"),
            root,
            home,
        }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.run_startup(&[], arguments)
    }

    fn run_startup(&self, startup: &[&str], arguments: &[&str]) -> Output {
        let registry = format!(
            "--registry=file://{}/tests/fixtures/builtin_modules/registry",
            manifest().display()
        );
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=5s", "600s", env!("CARGO_BIN_EXE_bazel-rs")])
            .arg("--ignore_all_rc_files")
            .args(startup)
            .arg(format!("--cache-dir={}", self.cache.display()))
            .args(arguments);
        // Repository-using commands resolve the built-in modules from the
        // local fixture registry (no network).
        if matches!(arguments.first(), Some(&"fetch" | &"build")) {
            command.arg(&registry);
        }
        let output = command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("BAZEL_RS_MAX_MEMORY_MB", "4096")
            .current_dir(&self.root)
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out");
        output
    }

    /// The state directory (the native output base) of the workspace:
    /// `<cache>/worktrees/ws-<first 8 bytes of SHA-256 of its path>`
    /// (docs/WORKTREES.md).
    fn state(&self) -> PathBuf {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(self.root.as_os_str().as_encoded_bytes());
        let hash: String = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        self.cache.join("worktrees").join(format!("ws-{hash}"))
    }
}

/// `_tmp_<pid>_<uuid>` as the receipt spells it.
fn normalize_temporary(line: &str) -> String {
    let Some(start) = line.find("_tmp_") else {
        return line.to_owned();
    };
    let end = line[start..]
        .find(" for deletion")
        .map_or(line.len(), |end| start + end);
    format!("{}_tmp_<PID>_<UUID>{}", &line[..start], &line[end..])
}

fn kept(stderr: &str, state: &Path) -> Vec<String> {
    let prefixes = [
        "ERROR: ",
        "INFO: All ",
        "INFO: Starting clean",
        "INFO: Output ",
        "Command '",
    ];
    stderr
        .lines()
        .filter(|line| prefixes.iter().any(|prefix| line.starts_with(prefix)))
        .map(|line| {
            normalize_temporary(&line.replace(&state.display().to_string(), "<OUTPUT_BASE>"))
        })
        .collect()
}

#[test]
fn shutdown_clean_and_fetch_match_genuine_bazel() {
    let receipt: Value = serde_json::from_str(
        &fs::read_to_string(fixture().join("reference_receipt.json")).unwrap(),
    )
    .unwrap();
    let archive = fs::read(fixture().join("remote.tar.gz")).unwrap();
    let server = Server::new(archive);
    let module = receipt["module"]
        .as_str()
        .unwrap()
        .replace("http://127.0.0.1:{PORT}", &server.base);
    let workspace = Workspace::new(&[
        ("MODULE.bazel", module),
        ("pkg/BUILD", receipt["build"].as_str().unwrap().to_owned()),
    ]);
    let mut failures = Vec::new();
    let state = workspace.state();
    for case in receipt["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        // Environmental: `fetch --configure` evaluates every module
        // extension of the built-in modules, which needs every built-in
        // module's source; the offline fixture registry of this test lacks
        // some archives (buildozer's). Covered by fetch_all_configure_and_force.
        if name == "fetch_configure" {
            continue;
        }
        let args: Vec<&str> = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        let startup: Vec<&str> = case["startup"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap())
            .collect();
        let before = server.downloads.load(Ordering::SeqCst);
        let output = workspace.run_startup(&startup, &args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut native = kept(&stderr, &state);
        let mut genuine: Vec<String> = case["stderr"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap().to_owned())
            .collect();
        match name {
            // Declared: the product name in the hint.
            "sync" | "quiet_sync" => {
                for line in &mut genuine {
                    *line = line.replace("'bazel help'", "'bazel-rs help'");
                }
            }
            // Declared: native loading reports its error once, as the
            // failure detail; genuine prints the pattern error events first
            // and a detail without the `no such package` prefix.
            "fetch_missing_package" => {
                let last = native.last().cloned().unwrap_or_default();
                if !last
                    .starts_with("ERROR: Fetching some target dependencies failed with errors: ")
                    || !last.contains("BUILD file not found in any of the following directories")
                {
                    failures.push(format!("{name}: {stderr}"));
                }
                native.clear();
                genuine.clear();
            }
            _ => {}
        }
        let check = |what: &str, genuine: String, native: String, failures: &mut Vec<String>| {
            if genuine != native {
                failures.push(format!(
                    "{name}: {what}: genuine {genuine:?}, native {native:?}\n--- native stderr\n{stderr}"
                ));
            }
        };
        check(
            "exit",
            case["exit"].to_string(),
            output.status.code().unwrap_or(-1).to_string(),
            &mut failures,
        );
        check(
            "stderr",
            format!("{genuine:?}"),
            format!("{native:?}"),
            &mut failures,
        );
        // Declared: native `build` reports its outputs on stdout.
        if !name.contains("build") {
            check(
                "stdout",
                case["stdout"].as_str().unwrap().to_owned(),
                String::from_utf8_lossy(&output.stdout).into_owned(),
                &mut failures,
            );
        }
        let output_exists = state
            .join("execroot/_main/bazel-out/k8-fastbuild/bin/pkg/g.txt")
            .exists();
        check(
            "output_exists",
            case["output_exists"].to_string(),
            output_exists.to_string(),
            &mut failures,
        );
        // Under --quiet nothing but the kept lines reaches stderr (genuine's
        // `quiet_build` prints its `Target ... up-to-date:` result lines on
        // stderr, native its result lines on stdout).
        if name.starts_with("quiet_") && name != "quiet_build" {
            let full: Vec<String> = case["stderr_full"]
                .as_array()
                .unwrap()
                .iter()
                .map(|line| {
                    line.as_str()
                        .unwrap()
                        .replace("'bazel help'", "'bazel-rs help'")
                })
                .collect();
            let native_full: Vec<&str> = stderr.lines().collect();
            check(
                "stderr_full",
                format!("{full:?}"),
                format!("{native_full:?}"),
                &mut failures,
            );
        }
        // The quiet_ cases ran in genuine server mode against a fresh
        // output user root (its own repository cache); natively they share
        // the cache of the cases before them.
        if !name.starts_with("quiet_") {
            check(
                "downloads",
                case["downloads"].to_string(),
                (server.downloads.load(Ordering::SeqCst) - before).to_string(),
                &mut failures,
            );
        }
        // Declared: genuine `clean` also drops the action cache (in its
        // output base), so the next build runs the genrule again; the
        // native action cache is shared by the cache's worktrees and
        // survives `clean`, so it hits.
        if name == "build_after_clean" || name == "build_after_expunge" {
            assert!(
                stderr.contains("0 actions executed, 1 cache hits"),
                "{name}: {stderr}"
            );
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// The shared store survives `clean --expunge` of one worktree; another
/// workspace's state is never touched; a read-only output tree is deleted.
#[test]
fn clean_keeps_the_shared_store_and_other_workspaces() {
    let build = "genrule(name = \"g\", outs = [\"g.txt\"], cmd = \"echo hi > $@\")\n".to_owned();
    let module = "module(name = \"m\")\n".to_owned();
    let workspace = Workspace::new(&[
        ("MODULE.bazel", module.clone()),
        ("pkg/BUILD", build.clone()),
    ]);
    let output = workspace.run(&["build", "--experimental-builtin-modules=false", "//pkg:g"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state = workspace.state();
    let other = workspace.cache.join("worktrees/other-0123456789abcdef");
    fs::create_dir_all(other.join("snapshots")).unwrap();
    fs::write(other.join("snapshots/kept"), "kept").unwrap();
    let actions = |cache: &Path| fs::read_dir(cache.join("actions")).unwrap().count();
    let shared_actions = actions(&workspace.cache);
    assert!(
        state
            .join("execroot/_main/bazel-out/k8-fastbuild/bin/pkg/g.txt")
            .exists()
    );
    // chmod -R a-w of the output tree's directories.
    let mut directories = vec![state.join("execroot")];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                directories.push(entry.path());
            }
        }
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o555)).unwrap();
    }
    let output = workspace.run(&["clean"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_dir(state.join("execroot")).unwrap().count(), 0);
    assert!(!state.join("snapshots").exists());
    let output = workspace.run(&["clean", "--expunge"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!state.exists());
    assert_eq!(actions(&workspace.cache), shared_actions);
    assert_eq!(
        fs::read_to_string(other.join("snapshots/kept")).unwrap(),
        "kept"
    );
    // The next build relinks the output from the shared action cache.
    let output = workspace.run(&["build", "--experimental-builtin-modules=false", "//pkg:g"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("0 actions executed, 1 cache hits"),
        "{stderr}"
    );
    assert!(
        state
            .join("execroot/_main/bazel-out/k8-fastbuild/bin/pkg/g.txt")
            .exists()
    );
}

/// `clean` waits for a command holding the output base lock.
#[test]
fn clean_waits_for_the_output_base_lock() {
    let workspace = Workspace::new(&[("MODULE.bazel", "module(name = \"m\")\n".to_owned())]);
    let output = workspace.run(&["clean"]);
    assert!(output.status.success());
    let state = workspace.state();
    let lock = fs::File::open(state.join("lock")).unwrap();
    fs2::FileExt::lock_shared(&lock).unwrap();
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(1500));
        drop(lock);
    });
    let started = std::time::Instant::now();
    let output = workspace.run(&["clean"]);
    release.join().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(started.elapsed() >= Duration::from_millis(1000), "{stderr}");
    assert!(
        stderr.contains("Another command holds the output base lock: \n")
            && stderr.contains("Waiting for it to complete..."),
        "{stderr}"
    );
}

/// `fetch --configure`, `--all` and `--force` over native repository rules
/// (explicit-module profile: no network).
#[test]
fn fetch_all_configure_and_force() {
    let base = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let log_dir = tempfile::Builder::new()
        .prefix("cmd-fetch-log-")
        .tempdir_in(base)
        .unwrap();
    let log = fs::canonicalize(log_dir.path()).unwrap().join("runs");
    let module = format!(
        "module(name = \"m\")\nr = use_repo_rule(\"//:defs.bzl\", \"r\")\nc = use_repo_rule(\"//:defs.bzl\", \"c\")\nr(name = \"plain\", log = \"{0}\")\nc(name = \"conf\", log = \"{0}\")\n",
        log.display()
    );
    let defs = "def _impl(ctx):\n    ctx.file(\"BUILD\", \"\")\n    ctx.execute([\"/bin/sh\", \"-c\", \"echo \" + ctx.name + \" >> \" + ctx.attr.log])\n\nATTRS = {\"log\": attr.string()}\nr = repository_rule(_impl, attrs = ATTRS)\nc = repository_rule(_impl, attrs = ATTRS, configure = True)\n".to_owned();
    let workspace = Workspace::new(&[
        ("MODULE.bazel", module),
        ("defs.bzl", defs),
        ("BUILD", String::new()),
    ]);
    let runs = || fs::read_to_string(&log).unwrap_or_default();
    let fetch = |args: &[&str]| {
        let mut all = vec!["fetch", "--experimental-builtin-modules=false"];
        all.extend_from_slice(args);
        let output = workspace.run(&all);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{args:?}: {stderr}");
        stderr
    };
    let stderr = fetch(&["--configure"]);
    assert!(
        stderr.contains("INFO: All external dependencies fetched successfully."),
        "{stderr}"
    );
    assert!(
        runs().lines().all(|line| line.ends_with("conf")),
        "{}",
        runs()
    );
    assert_eq!(runs().lines().count(), 1, "{}", runs());
    fetch(&[]);
    assert_eq!(runs().lines().count(), 2, "{}", runs());
    assert!(
        runs().lines().any(|line| line.ends_with("plain")),
        "{}",
        runs()
    );
    // Warm: reused, not executed.
    let stderr = fetch(&["--repo=@plain"]);
    assert!(
        stderr.contains("INFO: All requested repos fetched successfully."),
        "{stderr}"
    );
    assert_eq!(runs().lines().count(), 2, "{}", runs());
    fetch(&["--force", "--repo=@plain"]);
    assert_eq!(runs().lines().count(), 3, "{}", runs());
    // `fetch TARGET` reports no outputs and executes nothing.
    let output = workspace.run(&["fetch", "--experimental-builtin-modules=false", "//:all"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn vendor_fails_explicitly_and_help_lists_commands() {
    let workspace = Workspace::new(&[("MODULE.bazel", "module(name = \"m\")\n".to_owned())]);
    let output = workspace.run(&["vendor", "--vendor_dir=v"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("the vendor command is not implemented")
    );
    let output = workspace.run(&["help", "fetch"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("\nUsage: bazel fetch "));
    let output = workspace.run(&["help"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    for command in ["fetch", "clean", "shutdown"] {
        assert!(stdout.contains(&format!("\n  {command} ")), "{stdout}");
    }
}
