// Host-aware output directory names for the integration tests whose genuine
// captures come from a Linux x86_64 host (`bazel-out/k8-fastbuild/...`):
// genuine's default `--cpu` (AutoCpuConverter) names the configuration
// output directories, `k8` on Linux x86_64, `aarch64` on Linux arm64,
// `darwin_arm64` on macOS arm64 (docs/MACOS.md). Include with
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
