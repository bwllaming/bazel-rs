// Shared by the library's cfg(test) modules and integration-test harnesses.
// Resolve only the trusted fixture parent, before creating any test data.
// Never canonicalize paths below it: tests deliberately create symlinks,
// dangling links and denied paths which production must continue to reject.
#[allow(dead_code)]
mod test_temp {
    pub fn root() -> std::path::PathBuf {
        std::env::temp_dir()
            .canonicalize()
            .expect("the test temporary parent must exist and be accessible")
    }
}

// GNU timeout(1), which the integration tests wrap the CLI in: `timeout`
// (or its absolute path) on Linux; on macOS, which has none, Homebrew
// coreutils' `gtimeout` (the CI's macOS job installs it; docs/MACOS.md).
#[allow(dead_code)]
const TIMEOUT: &str = if cfg!(target_os = "linux") {
    "timeout"
} else {
    "/opt/homebrew/bin/gtimeout"
};
#[allow(dead_code)]
const USR_BIN_TIMEOUT: &str = if cfg!(target_os = "linux") {
    "/usr/bin/timeout"
} else {
    "/opt/homebrew/bin/gtimeout"
};
