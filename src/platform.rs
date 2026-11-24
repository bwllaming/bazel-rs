//! The host platform layer (docs/MACOS.md): the few operating-system
//! primitives whose Linux implementation (procfs, `O_PATH`, ...) has no
//! direct macOS counterpart, behind one interface with one implementation per
//! supported host — `linux.rs` (the original code, unchanged in behaviour)
//! and `macos.rs` (aarch64-apple-darwin). Subsystems that exist on Linux only
//! (the namespace and Landlock sandboxes, the child subreaper, pidfds) are
//! gated at their own modules; what macOS does instead is documented there
//! and in docs/MACOS.md. Anything not ported fails with
//! [`not_implemented`]'s message instead of silently doing something else.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("bazel-rs supports Linux and macOS hosts only");

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

/// The error text of a feature this host does not implement (yet): names
/// the feature and the host, so that nothing degrades silently.
pub fn not_implemented(feature: &str) -> String {
    format!("{feature} is not implemented on {HOST_NAME}")
}

/// `st_mode` of a rustix stat as `u32` (`mode_t` is 16 bits on macOS).
#[allow(clippy::useless_conversion)]
pub fn stat_mode(stat: &rustix::fs::Stat) -> u32 {
    u32::from(stat.st_mode)
}

/// `st_dev` of a rustix stat as `u64` (std's `MetadataExt::dev` spelling;
/// `dev_t` is a signed 32-bit integer on macOS).
#[allow(clippy::unnecessary_cast)]
pub fn stat_dev(stat: &rustix::fs::Stat) -> u64 {
    stat.st_dev as u64
}

#[cfg(test)]
mod tests {
    #[test]
    fn not_implemented_names_the_feature_and_the_host() {
        let message = super::not_implemented("the linux-sandbox strategy");
        assert!(message.starts_with("the linux-sandbox strategy is not implemented on "));
        assert!(message.ends_with(super::HOST_NAME));
    }

    #[test]
    fn self_exe_is_this_binary() {
        let exe = super::self_exe();
        assert!(std::fs::metadata(exe).is_ok_and(|metadata| metadata.is_file()));
    }

    #[test]
    fn this_process_exists() {
        assert!(super::process_exists(std::process::id()));
    }
}
