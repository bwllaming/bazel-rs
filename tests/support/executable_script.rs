// Shared by unit tests (`include!("../tests/support/executable_script.rs")`)
// and integration tests (`include!("support/executable_script.rs")`).
//
// Writes `contents` to `path` with permission bits `mode` WITHOUT this test
// process ever holding a writable descriptor to the resulting inode. A test
// binary runs many threads that spawn processes all the time; a child spawned
// while `fs::write` has the script open inherits that descriptor until its own
// exec closes it (O_CLOEXEC), and for that moment exec of the script fails
// with ETXTBSY ("Text file busy"). Like Bazel's process-wrapper,
// `repository_ctx.execute` reports that as a failed command with empty
// stdout, so a script written by the test itself turned into an intermittent
// empty result. Here a separate `/bin/sh` writes, chmods and renames a fresh
// inode into place, so no descriptor to it exists in this process to leak.
fn write_executable_script(path: &std::path::Path, contents: impl AsRef<[u8]>, mode: u32) {
    use std::os::unix::ffi::OsStrExt;
    let status = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"tmp="$1.write-executable.$$" && printf '%s' "$3" > "$tmp" && chmod "$2" "$tmp" && mv -f "$tmp" "$1""#)
        .arg("write-executable")
        .arg(path)
        .arg(format!("{mode:o}"))
        .arg(std::ffi::OsStr::from_bytes(contents.as_ref()))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(
        status.success(),
        "cannot write executable {}: {status}",
        path.display()
    );
}
