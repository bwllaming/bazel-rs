// Host-aware output directory names for the integration tests whose genuine
// captures come from a Linux x86_64 host (`bazel-out/k8-fastbuild/...`):
// genuine's default `--cpu` (AutoCpuConverter) names the configuration
// output directories, `k8` on Linux x86_64, `aarch64` on Linux arm64,
// `darwin_arm64` on macOS arm64 (docs/MACOS.md); and the environment
// variable genuine's macOS spawns add. Include with
// `include!("support/host_cpu.rs");`.

/// This host's default `--cpu`, the prefix of `bazel-out/<cpu>-fastbuild`.
#[allow(dead_code)]
const HOST_CPU: &str = if cfg!(target_os = "macos") {
    "darwin_arm64"
} else if cfg!(target_arch = "aarch64") {
    "aarch64"
} else {
    "k8"
};

/// The configuration directory suffixes a host's `--cpu` prefixes.
#[allow(dead_code)]
const HOST_CPU_SUFFIXES: &[&str] = &["-fastbuild", "-opt-exec", "-opt", "-dbg"];

/// A Linux x86_64 expectation (`k8-fastbuild`, `k8-opt-exec`, ...) as this
/// host's.
#[allow(dead_code)]
fn host_out(text: &str) -> String {
    if HOST_CPU == "k8" {
        return text.to_owned();
    }
    let mut text = text.to_owned();
    for suffix in HOST_CPU_SUFFIXES {
        text = text.replace(&format!("k8{suffix}"), &format!("{HOST_CPU}{suffix}"));
    }
    text
}

/// This host's output (`darwin_arm64-fastbuild`, ...) in the Linux x86_64
/// captures' spelling; literal `k8` strings of a fixture stay as they are.
#[allow(dead_code)]
fn as_k8(text: &str) -> String {
    if HOST_CPU == "k8" {
        return text.to_owned();
    }
    let mut text = text.to_owned();
    for suffix in HOST_CPU_SUFFIXES {
        text = text.replace(&format!("{HOST_CPU}{suffix}"), &format!("k8{suffix}"));
    }
    text
}

/// genuine's spawns on macOS carry `__CF_USER_TEXT_ENCODING` besides the
/// action's environment (the server's CoreFoundation value; bazel-rs passes
/// its own, docs/MACOS.md): whether an `env` line is that variable there.
#[allow(dead_code)]
fn is_macos_spawn_variable(line: &str) -> bool {
    cfg!(target_os = "macos") && line.starts_with("__CF_USER_TEXT_ENCODING=")
}

/// `text` (an `env` listing) without [`is_macos_spawn_variable`] lines.
#[allow(dead_code)]
fn without_macos_spawn_variables(text: &str) -> String {
    text.split_inclusive('\n')
        .filter(|line| !is_macos_spawn_variable(line))
        .collect()
}
