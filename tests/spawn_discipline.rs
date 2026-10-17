//! The ETXTBSY spawn discipline (src/spawn_lock.rs) is enforced by clippy's
//! disallowed-methods (clippy.toml). This checks what clippy cannot: that the
//! lint stays configured and that no production source opts out of it, so
//! every process start of the crate goes through crate::spawn_lock.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::Path;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

#[test]
fn clippy_disallows_direct_process_starts() {
    let config = fs::read_to_string(Path::new(ROOT).join("clippy.toml")).unwrap();
    for path in [
        "std::process::Command::spawn",
        "std::process::Command::output",
        "std::process::Command::status",
        "std::os::unix::process::CommandExt::exec",
    ] {
        assert!(
            config.contains(&format!("path = \"{path}\"")),
            "{path} not in clippy.toml"
        );
    }
}

#[test]
fn only_spawn_lock_allows_direct_process_starts_in_src() {
    let mut offenders = Vec::new();
    for entry in fs::read_dir(Path::new(ROOT).join("src")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let text = fs::read_to_string(&path).unwrap();
        for (number, line) in text.lines().enumerate() {
            if !line.contains("clippy::disallowed_methods") {
                continue;
            }
            let allowed = match name.as_str() {
                // The process starts themselves.
                "spawn_lock.rs" => line.trim() == "#[allow(clippy::disallowed_methods)]",
                // Test modules only (compiled with cfg(test)).
                "lib.rs" => line.trim() == "#![cfg_attr(test, allow(clippy::disallowed_methods))]",
                _ => false,
            };
            if !allowed {
                offenders.push(format!("src/{name}:{}: {}", number + 1, line.trim()));
            }
        }
    }
    assert!(offenders.is_empty(), "{offenders:#?}");
}
