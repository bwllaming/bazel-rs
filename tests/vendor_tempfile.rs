//! The vendored tempfile 3.27.0 (vendor/tempfile-3.27.0.provenance.json):
//! the archive inventory is anchored, only `src/dir/mod.rs` is patched, and
//! a `TempDir` holding sealed (read-only) directories — a repository store's
//! trees, docs/REPOSITORY_SLOTS.md "Sealed trees" — is removed when dropped
//! or closed.

use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

#[test]
fn tempfile_vendor_inventory_is_anchored_and_only_declared_files_are_patched() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let provenance: serde_json::Value = serde_json::from_slice(
        &fs::read(root.join("vendor/tempfile-3.27.0.provenance.json")).unwrap(),
    )
    .unwrap();
    let vendor = root.join("vendor/tempfile-3.27.0");
    let patches = provenance["declared_source_patches"].as_array().unwrap();
    let declared: Vec<&str> = patches
        .iter()
        .map(|p| p["path"].as_str().unwrap())
        .collect();
    assert_eq!(declared, ["src/dir/mod.rs"]);
    fn collect(root: &Path, path: &Path, files: &mut Vec<String>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let metadata = path.symlink_metadata().unwrap();
            if metadata.is_dir() {
                collect(root, &path, files);
            } else {
                assert!(metadata.is_file(), "{}", path.display());
                files.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                );
            }
        }
    }
    let mut files = Vec::new();
    collect(&vendor, &vendor, &mut files);
    files.sort();
    let mut originals = Vec::new();
    for path in files {
        let data = fs::read(vendor.join(&path)).unwrap();
        let current = serde_json::json!({"sha256": format!("{:x}", Sha256::digest(&data)), "size": data.len()});
        if let Some(license) = provenance["license"]["content"].get(path.as_str()) {
            assert_eq!(&current, license, "{path}");
        }
        let original = if let Some(patch) = patches.iter().find(|p| p["path"] == path.as_str()) {
            assert_eq!(current, patch["current"]);
            patch["original"].clone()
        } else {
            current
        };
        // Explicit key order matches the provenance's sorted-key canonical JSON.
        originals.push(serde_json::json!({"path": path, "sha256": original["sha256"], "size": original["size"]}));
    }
    assert_eq!(
        originals.len() as u64,
        provenance["archive"]["regular_members"].as_u64().unwrap()
    );
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&originals).unwrap())
        ),
        provenance["archive"]["archive_member_inventory"]["canonical_json_sha256"]
            .as_str()
            .unwrap()
    );
}

/// A tree like a sealed store tree: read-only directories and files, and a
/// link out of the temporary directory, which is never followed (the
/// outside directory keeps its read-only mode).
fn sealed_tree(root: &Path, outside: &Path) {
    let tree = root.join("cache/repositories/direct/trees/t");
    fs::create_dir_all(tree.join("pkg/sub")).unwrap();
    fs::write(tree.join("pkg/sub/mod.py"), b"").unwrap();
    // Not even readable: the walk must change its mode before listing it.
    fs::create_dir_all(tree.join("pkg/locked/inner")).unwrap();
    fs::set_permissions(
        tree.join("pkg/locked/inner"),
        fs::Permissions::from_mode(0o555),
    )
    .unwrap();
    fs::set_permissions(tree.join("pkg/locked"), fs::Permissions::from_mode(0o000)).unwrap();
    std::os::unix::fs::symlink(outside, tree.join("pkg/outside")).unwrap();
    for (path, mode) in [
        (tree.join("pkg/sub/mod.py"), 0o444),
        (tree.join("pkg/sub"), 0o555),
        (tree.join("pkg"), 0o555),
        (tree.clone(), 0o555),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
}

#[test]
fn temporary_directories_holding_sealed_trees_are_removed() {
    let outside = tempfile::tempdir().unwrap();
    let kept = outside.path().join("kept");
    fs::create_dir(&kept).unwrap();
    fs::set_permissions(&kept, fs::Permissions::from_mode(0o555)).unwrap();
    let dropped = tempfile::tempdir().unwrap();
    sealed_tree(dropped.path(), &kept);
    let path = dropped.path().to_path_buf();
    drop(dropped);
    assert!(!path.exists());
    let closed = tempfile::Builder::new()
        .prefix("sealed-")
        .tempdir()
        .unwrap();
    sealed_tree(closed.path(), &kept);
    let path = closed.path().to_path_buf();
    closed.close().unwrap();
    assert!(!path.exists());
    assert_eq!(
        fs::metadata(&kept).unwrap().permissions().mode() & 0o777,
        0o555
    );
    fs::set_permissions(&kept, fs::Permissions::from_mode(0o755)).unwrap();
}
