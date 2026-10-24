// Warm repository reuse vs. the store's racy window (include!-ed by tests
// that compare a direct publication across processes).
//
// A published direct repository records no stat fingerprint for a file
// younger than src/repository_store.rs RACY_MARGIN (2 s) at publish (review
// R3). The first warm verify that starts after the file settled hashes it,
// finds it intact and persists its fingerprint into the manifest: the one
// write a warm reuse performs. Whether that happens on the first warm
// process depends only on how long after the cold one it starts (on a loaded
// machine, often more than 2 s). These helpers fix the order instead: wait
// until the published tree has settled, let one warm process refresh, and
// require the next one to leave every byte untouched.

/// src/repository_store.rs RACY_MARGIN.
const STORE_RACY_MARGIN: std::time::Duration = std::time::Duration::from_secs(2);

/// Sleep until every entry below `root` is settled for any warm verify that
/// starts afterwards: its mtime and ctime are more than STORE_RACY_MARGIN
/// in the past.
fn wait_until_settled(root: &std::path::Path) {
    use std::os::unix::fs::MetadataExt;
    fn newest(path: &std::path::Path, newest_ns: &mut i128) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            for (seconds, nanoseconds) in [
                (metadata.mtime(), metadata.mtime_nsec()),
                (metadata.ctime(), metadata.ctime_nsec()),
            ] {
                *newest_ns =
                    (*newest_ns).max(i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds));
            }
            if metadata.is_dir() {
                newest(&path, newest_ns);
            }
        }
    }
    let mut newest_ns = 0;
    newest(root, &mut newest_ns);
    let settled = std::time::UNIX_EPOCH
        + std::time::Duration::from_nanos(u64::try_from(newest_ns).unwrap())
        + STORE_RACY_MARGIN
        + std::time::Duration::from_millis(50);
    while let Ok(left) = settled.duration_since(std::time::SystemTime::now()) {
        std::thread::sleep(left);
    }
}

/// A direct-store manifest without its stat fingerprints: what a warm
/// fingerprint refresh must leave unchanged.
fn without_fingerprints(manifest: &[u8]) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_slice(manifest).unwrap();
    for entry in value["files"].as_object_mut().unwrap().values_mut() {
        entry.as_object_mut().unwrap().remove("stat");
    }
    value
}

/// Whether every regular file of a direct-store manifest carries a stat
/// fingerprint (what a settled warm verify persists; directories and
/// symlinks are never fingerprinted by it).
fn files_fingerprinted(manifest: &[u8]) -> bool {
    let value: serde_json::Value = serde_json::from_slice(manifest).unwrap();
    value["files"].as_object().unwrap().values().all(|entry| {
        entry["directory"] == serde_json::Value::Bool(true)
            || !entry["symlink"].is_null()
            || !entry["stat"].is_null()
    })
}
