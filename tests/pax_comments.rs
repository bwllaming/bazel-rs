//! Complete extraction of an unchanged genuine Git-generated release archive.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use bazel_rs::archive::extract;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

#[test]
fn genuine_archive_preserves_every_member_without_a_metadata_pseudo_file() {
    let bytes = include_bytes!("fixtures/pax_comments/bazel_features-1.42.1.tar.gz");
    assert_eq!(
        format!("{:x}", Sha256::digest(bytes)),
        "8189bac9a6bf9cc155a854c4cbebfebf58b9ca7a2d0a67645f7d0c1f83c523ac"
    );
    let source: Value =
        serde_json::from_slice(include_bytes!("fixtures/pax_comments/source.json")).unwrap();
    let expected: Value = serde_json::from_slice(include_bytes!(
        "fixtures/pax_comments/expected_manifest.json"
    ))
    .unwrap();
    let root = tempfile::Builder::new()
        .prefix("genuine-pax-")
        .tempdir_in("/tmp/opencode")
        .unwrap();
    extract(
        bytes,
        "tar.gz",
        source["strip_prefix"].as_str().unwrap(),
        root.path(),
    )
    .unwrap();
    let mut files = BTreeMap::new();
    let mut directories = BTreeSet::new();
    fn walk(
        root: &Path,
        path: &Path,
        files: &mut BTreeMap<String, Value>,
        directories: &mut BTreeSet<String>,
    ) {
        let name = path
            .strip_prefix(root)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let metadata = fs::symlink_metadata(path).unwrap();
        assert!(!metadata.file_type().is_symlink(), "unexpected link {name}");
        if metadata.is_dir() {
            directories.insert(name);
            for entry in fs::read_dir(path).unwrap() {
                walk(root, &entry.unwrap().path(), files, directories);
            }
        } else {
            assert!(metadata.is_file(), "unexpected special member {name}");
            let bytes = fs::read(path).unwrap();
            files.insert(name, json!({"size":bytes.len(),"sha256":format!("{:x}",Sha256::digest(&bytes)),"mode":metadata.permissions().mode() & 0o7777}));
        }
    }
    walk(root.path(), root.path(), &mut files, &mut directories);
    assert_eq!(files.len(), 41);
    assert_eq!(directories.len(), 7);
    assert_eq!(json!(files), expected["files"]);
    assert_eq!(json!(directories), expected["directories"]);
    let path = root.path().to_owned();
    root.close().unwrap();
    assert!(!path.exists());
}
