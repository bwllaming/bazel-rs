//! genuine's darwin-sandbox on macOS (DarwinSandboxedSpawnRunner,
//! sandbox/DarwinSandboxedSpawnRunner.java; docs/MACOS.md): the spawn runs in
//! its own sandbox execroot under `/usr/bin/sandbox-exec -f sandbox.sb`, a
//! Seatbelt profile that allows everything except
//!
//! - file writes outside the writable directories: the always-writable set
//!   (`/dev`, `/tmp`, `/private/tmp`, `/private/var/tmp`, the client's
//!   `DARWIN_USER_TEMP_DIR` / `DARWIN_USER_CACHE_DIR`, the user's
//!   `~/Library/{Caches,Logs,Developer}`, each when it exists, symlinks
//!   resolved: getAlwaysWritableDirs:118-135), the sandbox execroot, the
//!   spawn's TEST_TMPDIR and TMPDIR and every `--sandbox_writable_path`
//!   (AbstractSandboxSpawnRunner.getWritableDirs:352-405);
//! - reads below `--sandbox_block_path` entries (SandboxOptions
//!   .getInaccessiblePaths:213-226);
//! - the network except localhost and unix sockets when the spawn must be
//!   denied it (`block-network`, `--nosandbox_default_allow_network` without
//!   `requires-network`: Spawns.requiresNetwork).
//!
//! The profile text is writeConfig's (:268-311), the writable directories
//! sorted (genuine writes them from a hash set). Genuine starts its
//! process-wrapper inside sandbox-exec; here the sandbox worker supervises
//! `sandbox-exec` itself (crate::process_wrapper), which applies the profile
//! and executes the command in place, so the command's process is the one
//! the worker supervises. The statistics file genuine's process-wrapper
//! writes stays writable as a literal like genuine's.
//!
//! The backend is used when `sandbox-exec` works on the host
//! (DarwinSandboxedSpawnRunner.computeIsSupported:52-70: `sandbox-exec -p
//! "(version 1) (allow default)" /usr/bin/true` in /usr/bin with an empty
//! environment); otherwise `sandboxed` is the processwrapper-sandbox like
//! genuine's (SandboxModule registers darwin-sandbox only when supported).
//! [`KNOB`] `off` skips it (measurements, tests).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// DarwinSandboxedSpawnRunner.sandboxExecBinary.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// `off` disables the darwin-sandbox backend (the processwrapper-sandbox
/// runs `sandboxed` spawns instead).
pub const KNOB: &str = "BAZEL_RS_DARWIN_SANDBOX";

/// Whether `sandbox-exec` can apply a profile on this host (cached per
/// process like genuine's static isSupported).
pub fn supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        // snapshot-unrecorded: an execution-phase test knob.
        if std::env::var(KNOB).is_ok_and(|value| value == "off") {
            return false;
        }
        let mut command = std::process::Command::new(SANDBOX_EXEC);
        command
            .args(["-p", "(version 1) (allow default)", "/usr/bin/true"])
            .current_dir("/usr/bin")
            .env_clear()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::spawn_lock::spawn(&mut command)
            .and_then(|mut child| child.wait())
            .is_ok_and(|status| status.success())
    })
}

/// The inputs of one spawn's profile.
pub struct Profile<'a> {
    /// The spawn's sandbox execroot (writable).
    pub execroot: &'a Path,
    /// The spawn's TMPDIR.
    pub tmpdir: &'a Path,
    /// The spawn's TEST_TMPDIR, if any.
    pub test_tmpdir: Option<&'a Path>,
    /// `--sandbox_writable_path` entries.
    pub writable_paths: &'a [PathBuf],
    /// `--sandbox_block_path` entries.
    pub block_paths: &'a [PathBuf],
    /// Whether the spawn must be denied the network.
    pub deny_network: bool,
    /// The statistics file genuine's process-wrapper writes (writable).
    pub statistics: &'a Path,
}

/// `path` with its symlinks resolved when it exists (Path.resolveSymbolicLinks).
fn resolved(path: &Path) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

/// getAlwaysWritableDirs (:118-135): each path that exists, resolved.
fn always_writable() -> &'static BTreeSet<PathBuf> {
    static DIRS: OnceLock<BTreeSet<PathBuf>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut dirs = BTreeSet::new();
        let mut candidates: Vec<PathBuf> = ["/dev", "/tmp", "/private/tmp", "/private/var/tmp"]
            .iter()
            .map(PathBuf::from)
            .collect();
        for name in ["DARWIN_USER_TEMP_DIR", "DARWIN_USER_CACHE_DIR"] {
            if let Some(value) = crate::client_env::var_os(name) {
                candidates.push(PathBuf::from(value));
            }
        }
        // The JVM's user.home: the user database's home directory, not $HOME.
        #[cfg(target_os = "macos")]
        let home = crate::platform::user_home();
        #[cfg(not(target_os = "macos"))]
        let home: Option<PathBuf> = None;
        if let Some(home) = home {
            for sub in ["Library/Caches", "Library/Logs", "Library/Developer"] {
                candidates.push(home.join(sub));
            }
        }
        for candidate in candidates {
            if let Some(path) = resolved(&candidate) {
                dirs.insert(path);
            }
        }
        dirs
    })
}

/// addWritablePath (:407-434): a path below the execroot as it is, an
/// existing one resolved, a missing one the spawn's I/O error.
fn add_writable(
    writable: &mut BTreeSet<PathBuf>,
    execroot: &Path,
    path: &Path,
    what: &str,
) -> Result<(), String> {
    let path = execroot.join(path);
    if path.starts_with(execroot) {
        writable.insert(path);
    } else if let Some(real) = resolved(&path) {
        writable.insert(real);
    } else {
        return Err(format!(
            "Cannot resolve symlinks in {what} because it doesn't exist: \"{}\"",
            path.display()
        ));
    }
    Ok(())
}

/// Escapes quotes and backslashes for SBPL string literals
/// (escapeSchemeString:254-260).
fn escape(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// The profile text of one spawn (writeConfig:262-311).
pub fn profile(input: &Profile<'_>) -> Result<String, String> {
    let mut writable = always_writable().clone();
    writable.insert(input.execroot.to_path_buf());
    if let Some(test_tmpdir) = input.test_tmpdir {
        add_writable(&mut writable, input.execroot, test_tmpdir, "TEST_TMPDIR")?;
    }
    add_writable(&mut writable, input.execroot, input.tmpdir, "TMPDIR")?;
    for path in input.writable_paths {
        writable.insert(path.clone());
        if let Some(real) = resolved(path) {
            writable.insert(real);
        }
    }
    let blocked: BTreeSet<PathBuf> = input
        .block_paths
        .iter()
        .map(|path| resolved(path).unwrap_or_else(|| path.clone()))
        .collect();
    let mut text = String::from(
        "(version 1)\n(debug deny)\n(allow default)\n(allow process-exec (with no-sandbox) (literal \"/bin/ps\"))\n",
    );
    if input.deny_network {
        text.push_str("(deny network*)\n");
        text.push_str("(allow network-inbound (local ip \"localhost:*\"))\n");
        text.push_str("(allow network* (remote ip \"localhost:*\"))\n");
        text.push_str("(allow network* (remote unix-socket))\n");
    }
    text.push_str("(deny file-write*)\n(allow file-write*\n");
    for path in &writable {
        text.push_str(&format!("    (subpath \"{}\")\n", escape(path)));
    }
    text.push_str(&format!(
        "    (literal \"{}\")\n)\n",
        escape(input.statistics)
    ));
    if !blocked.is_empty() {
        text.push_str("(deny file-read*\n");
        for path in &blocked {
            text.push_str(&format!("    (subpath \"{}\")\n", escape(path)));
        }
        text.push_str(")\n");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_follows_write_config() {
        let root = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(root.path()).unwrap();
        let execroot = root.join("sandbox/1/execroot/_main");
        fs::create_dir_all(&execroot).unwrap();
        let tmp = root.join("tmp");
        fs::create_dir_all(&tmp).unwrap();
        let text = profile(&Profile {
            execroot: &execroot,
            tmpdir: &tmp,
            test_tmpdir: Some(Path::new("_tmp/t")),
            writable_paths: &[root.join("w\"q")],
            block_paths: &[root.join("blocked")],
            deny_network: true,
            statistics: &root.join("sandbox/1/stats.out"),
        })
        .unwrap();
        assert!(text.starts_with(
            "(version 1)\n(debug deny)\n(allow default)\n(allow process-exec (with no-sandbox) (literal \"/bin/ps\"))\n(deny network*)\n(allow network-inbound (local ip \"localhost:*\"))\n(allow network* (remote ip \"localhost:*\"))\n(allow network* (remote unix-socket))\n(deny file-write*)\n(allow file-write*\n"
        ), "{text}");
        for line in [
            format!("    (subpath \"{}\")\n", execroot.display()),
            format!("    (subpath \"{}\")\n", execroot.join("_tmp/t").display()),
            format!("    (subpath \"{}\")\n", tmp.display()),
            format!("    (subpath \"{}/w\\\"q\")\n", root.display()),
            format!(
                "    (literal \"{}/sandbox/1/stats.out\")\n)\n",
                root.display()
            ),
            format!(
                "(deny file-read*\n    (subpath \"{}/blocked\")\n)\n",
                root.display()
            ),
        ] {
            assert!(text.contains(&line), "{line}\n{text}");
        }
        let open = profile(&Profile {
            execroot: &execroot,
            tmpdir: &tmp,
            test_tmpdir: None,
            writable_paths: &[],
            block_paths: &[],
            deny_network: false,
            statistics: &root.join("stats.out"),
        })
        .unwrap();
        assert!(!open.contains("network"), "{open}");
        assert!(!open.contains("file-read"), "{open}");
        let missing = profile(&Profile {
            execroot: &execroot,
            tmpdir: &root.join("missing"),
            test_tmpdir: None,
            writable_paths: &[],
            block_paths: &[],
            deny_network: false,
            statistics: &root.join("stats.out"),
        });
        assert_eq!(
            missing.unwrap_err(),
            format!(
                "Cannot resolve symlinks in TMPDIR because it doesn't exist: \"{}/missing\"",
                root.display()
            )
        );
    }
}
