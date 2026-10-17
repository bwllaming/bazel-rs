//! Immutable, root-owned patch inputs shared by discovery and materialization.
//!
//! Bazel 9.3 contract: ModuleFileFunction.java:875-955 projects main-repository
//! patches onto MODULE.bazel, ignoring external patches and patch commands;
//! ModuleFileGlobals.java:522-545 records the single-version override options.
//! Our strict local profile requires real package markers and rejects symlinks.
//! No BUILD evaluation, fetching, generated targets, or command execution occurs.

use crate::bzlmod::{ModuleFile, PatchOptions};
use crate::label::Label;
use crate::patching::{self, Patch};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Input {
    Main {
        label: Label,
        /// The patch file read from the root module's checkout.
        path: std::path::PathBuf,
        bytes: Vec<u8>,
        patch: Patch,
    },
    External(Label),
}

/// Snapshot once, then reuse the same bytes for discovery, cache keys and trees.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalPatchPlan {
    inputs: Vec<Input>,
    commands: Vec<String>,
    strip: usize,
}

/// Resolve patch labels in the root MODULE's package and repository context.
/// Selected dependencies and extension imports deliberately do not enter this
/// mapping. Canonical external labels are validated and recorded, never read.
pub fn snapshot(
    root: &Path,
    root_module: &ModuleFile,
    options: &PatchOptions,
) -> Result<LocalPatchPlan, String> {
    let strip = usize::try_from(options.patch_strip)
        .map_err(|_| "patch_strip must be nonnegative".to_owned())?;
    let mut mapping = BTreeMap::from([(String::new(), String::new())]);
    if let Some(info) = &root_module.module {
        let own = info.effective_repo_name();
        if !own.is_empty() {
            mapping.insert(own.to_owned(), String::new());
        }
    }
    // Validate all labels before converting any of them to filesystem paths.
    let labels = options
        .patches
        .iter()
        .map(|text| {
            Label::parse_in(text, "", "", &mapping)
                .map_err(|e| format!("invalid patch label {text:?}: {e}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut plan = LocalPatchPlan {
        inputs: Vec::new(),
        commands: options.patch_cmds.clone(),
        strip,
    };
    let mut checked_root = false;
    for label in labels {
        if !label.repository.is_empty() {
            plan.inputs.push(Input::External(label));
            continue;
        }
        if !checked_root {
            check_root(root)?;
            checked_root = true;
        }
        let package = root.join(&label.package);
        check_descendant_directories(root, &label.package, "patch package")?;
        check_package_marker(&package)?;
        let relative = Path::new(&label.name);
        if let Some(parent) = relative.parent() {
            // Label::parse_in already guarantees safe ASCII normal components.
            check_descendant_directories(
                &package,
                parent.to_str().expect("validated label is ASCII"),
                "patch file parent",
            )?;
        }
        let path = package.join(relative);
        let bytes = read_patch(&path).map_err(|e| format!("patch {label}: {e}"))?;
        let patch = patching::parse(&bytes, strip)
            .map_err(|e| format!("cannot parse patch {label}: {e}"))?;
        plan.inputs.push(Input::Main {
            label,
            path,
            bytes,
            patch,
        });
    }
    Ok(plan)
}

impl LocalPatchPlan {
    /// Only main-repository patch projections participate in discovery.
    /// Commands are never executed and never modify the discovered MODULE.
    pub fn apply_module(&self, original: &[u8]) -> Result<Vec<u8>, String> {
        let mut bytes = original.to_vec();
        for input in &self.inputs {
            if let Input::Main { path, patch, .. } = input {
                // ModuleFileFunction.java:946-951.
                bytes = patch.apply_module(&bytes).map_err(|e| {
                    format!(
                        "error applying single_version_override patch {} to module file: {e}",
                        path.display()
                    )
                })?;
            }
        }
        Ok(bytes)
    }

    /// Call before fetching or staging a materialized repository.
    pub fn validate_materialization(&self) -> Result<(), String> {
        if !self.commands.is_empty() {
            return Err(
                "patch_cmds are unsupported for materialization; no commands are executed"
                    .to_owned(),
            );
        }
        for input in &self.inputs {
            if let Input::External(label) = input {
                return Err(format!(
                    "canonical external patch {label} is unsupported for materialization; no external patch repository is fetched"
                ));
            }
        }
        Ok(())
    }

    pub fn apply_tree(&self, root: &Path) -> Result<(), String> {
        self.validate_materialization()?;
        for input in &self.inputs {
            if let Input::Main { label, patch, .. } = input {
                patch
                    .apply_tree(root)
                    .map_err(|e| format!("cannot apply patch {label}: {e}"))?;
            }
        }
        Ok(())
    }

    pub fn has_transformations(&self) -> bool {
        !self.inputs.is_empty() || !self.commands.is_empty()
    }

    /// Content-based and ordered; deliberately excludes parser internals and
    /// checkout locations. Canonicalized aliases for the same input share a key.
    pub fn cache_identity(&self) -> serde_json::Value {
        let inputs: Vec<_> = self
            .inputs
            .iter()
            .map(|input| match input {
                Input::Main { label, bytes, .. } => serde_json::json!({
                    "kind": "main", "label": label.canonical_starlark(),
                    "sha256": format!("{:x}", Sha256::digest(bytes)),
                }),
                Input::External(label) => serde_json::json!({
                    "kind": "external", "label": label.canonical_starlark(),
                }),
            })
            .collect();
        let mut identity = serde_json::json!({
            "inputs": inputs, "commands": self.commands, "strip": self.strip,
        });
        if !self.inputs.is_empty() {
            // Patched trees never reuse the earlier engine's caches.
            identity["engine"] = serde_json::Value::from(crate::patching::ENGINE);
        }
        identity
    }
}

fn check_directory(path: &Path, kind: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("cannot access {kind} directory {}: {e}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "symlink {kind} directory is unsupported: {}",
            path.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(format!(
            "{kind} path is not a directory: {}",
            path.display()
        ));
    }
    Ok(())
}

fn check_root(root: &Path) -> Result<(), String> {
    // Inspect ancestors too, without canonicalizing away evidence of symlinks.
    // The checkout is trusted against concurrent replacement of checked paths.
    let mut path = std::path::PathBuf::new();
    for component in root.components() {
        if component == Component::ParentDir {
            return Err("patch root must not contain '..' components".to_owned());
        }
        path.push(component.as_os_str());
        check_directory(&path, "patch root")?;
    }
    if path.as_os_str().is_empty() {
        check_directory(Path::new("."), "patch root")?;
    }
    Ok(())
}

fn check_descendant_directories(root: &Path, relative: &str, kind: &str) -> Result<(), String> {
    let mut path = root.to_path_buf();
    for component in Path::new(relative).components() {
        path.push(component.as_os_str());
        check_directory(&path, kind)?;
    }
    Ok(())
}

fn check_package_marker(package: &Path) -> Result<(), String> {
    for name in ["BUILD.bazel", "BUILD"] {
        let path = package.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(format!(
                        "symlink patch package marker is unsupported: {}",
                        path.display()
                    ));
                }
                if !metadata.is_file() {
                    return Err(format!(
                        "patch package marker is not a regular file: {}",
                        path.display()
                    ));
                }
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "cannot inspect patch package marker {}: {e}",
                    path.display()
                ));
            }
        }
    }
    Err(format!(
        "patch package {} is missing a regular BUILD.bazel or BUILD file",
        package.display()
    ))
}

/// Read a whole patch file (PatchUtil reads patches into memory); there is no
/// artificial per-file or total size cap.
fn read_patch(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("cannot access patch file {}: {e}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "symlink patch file is unsupported: {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "patch file is not a regular file: {}",
            path.display()
        ));
    }
    let mut file =
        File::open(path).map_err(|e| format!("cannot open patch file {}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read patch file {}: {e}", path.display()))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bzlmod::{Dependency, ModuleInfo};
    use tempfile::TempDir;

    fn change(path: &str, before: &str, after: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-{before}\n+{after}\n"
        )
    }

    fn options(labels: &[&str]) -> PatchOptions {
        PatchOptions {
            patches: labels.iter().map(|s| (*s).to_owned()).collect(),
            patch_strip: 1,
            ..Default::default()
        }
    }

    fn workspace() -> TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("BUILD.bazel"), "not evaluated!").unwrap();
        fs::write(
            root.path().join("fix.patch"),
            change("MODULE.bazel", "old", "new"),
        )
        .unwrap();
        root
    }

    fn plan(root: &Path, labels: &[&str]) -> Result<LocalPatchPlan, String> {
        snapshot(root, &ModuleFile::default(), &options(labels))
    }

    #[test]
    fn anonymous_root_and_main_label_spellings() {
        let root = workspace();
        let expected = plan(root.path(), &[":fix.patch"]).unwrap();
        for label in [
            "fix.patch",
            "//:fix.patch",
            "@//:fix.patch",
            "@@//:fix.patch",
        ] {
            let actual = plan(root.path(), &[label]).unwrap();
            assert_eq!(actual, expected);
            assert_eq!(actual.cache_identity(), expected.cache_identity());
        }
        assert_eq!(expected.apply_module(b"old\n").unwrap(), b"new\n");
        assert!(
            plan(root.path(), &["@self//:fix.patch"])
                .unwrap_err()
                .contains("unknown apparent")
        );
    }

    #[test]
    fn only_effective_owner_alias_is_visible() {
        let root = workspace();
        fs::create_dir(root.path().join("pkg")).unwrap();
        fs::write(root.path().join("pkg/BUILD"), "").unwrap();
        fs::copy(
            root.path().join("fix.patch"),
            root.path().join("pkg/fix.patch"),
        )
        .unwrap();
        let mut module = ModuleFile {
            module: Some(ModuleInfo {
                name: "owner".into(),
                repo_name: Some("self".into()),
                ..Default::default()
            }),
            dependencies: vec![Dependency {
                name: "dep".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(snapshot(root.path(), &module, &options(&["@self//pkg:fix.patch"])).is_ok());
        for label in [
            "@owner//:fix.patch",
            "@dep//:fix.patch",
            "@unknown//:fix.patch",
        ] {
            assert!(
                snapshot(root.path(), &module, &options(&[label]))
                    .unwrap_err()
                    .contains("unknown apparent")
            );
        }
        for repo_name in [None, Some(String::new())] {
            module.module.as_mut().unwrap().repo_name = repo_name;
            assert!(snapshot(root.path(), &module, &options(&["@owner//:fix.patch"])).is_ok());
        }
    }

    #[test]
    fn canonical_external_is_recorded_without_filesystem_access() {
        let missing = Path::new("/nonexistent-patch-inputs-root");
        let p = plan(missing, &["@@external+//missing:fix.patch"]).unwrap();
        assert!(p.has_transformations());
        assert_eq!(
            p.apply_module(b"unaltered\0bytes").unwrap(),
            b"unaltered\0bytes"
        );
        assert!(
            p.validate_materialization()
                .unwrap_err()
                .contains("canonical external patch")
        );
        assert!(
            p.apply_tree(missing)
                .unwrap_err()
                .contains("canonical external patch")
        );
        assert_eq!(
            p.cache_identity()["inputs"][0]["label"],
            "@@external+//missing:fix.patch"
        );
        for invalid in [
            "@@external+//../bad:x",
            "@@external+//p:../x",
            "@@../p:x",
            "@unknown//p:x",
        ] {
            assert!(plan(missing, &[invalid]).is_err());
        }
    }

    #[test]
    fn missing_package_marker_file_and_directory_have_distinct_errors() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            plan(root.path(), &["//absent:fix.patch"])
                .unwrap_err()
                .contains("patch package directory")
        );
        fs::write(
            root.path().join("fix.patch"),
            change("MODULE.bazel", "old", "new"),
        )
        .unwrap();
        assert!(
            plan(root.path(), &[":fix.patch"])
                .unwrap_err()
                .contains("missing a regular BUILD.bazel or BUILD")
        );
        fs::write(root.path().join("BUILD"), "not evaluated").unwrap();
        assert!(plan(root.path(), &[":fix.patch"]).is_ok());
        assert!(
            plan(root.path(), &[":absent.patch"])
                .unwrap_err()
                .contains("cannot access patch file")
        );
        fs::create_dir(root.path().join("directory.patch")).unwrap();
        assert!(
            plan(root.path(), &[":directory.patch"])
                .unwrap_err()
                .contains("patch file is not a regular file")
        );
        fs::create_dir(root.path().join("BUILD.bazel")).unwrap();
        assert!(
            plan(root.path(), &[":fix.patch"])
                .unwrap_err()
                .contains("package marker is not a regular file")
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_file_package_marker_and_path_components() {
        use std::os::unix::fs::symlink;
        let root = workspace();
        symlink("fix.patch", root.path().join("link.patch")).unwrap();
        assert!(
            plan(root.path(), &[":link.patch"])
                .unwrap_err()
                .contains("symlink patch file")
        );
        fs::create_dir(root.path().join("real")).unwrap();
        fs::write(root.path().join("real/BUILD"), "").unwrap();
        fs::copy(
            root.path().join("fix.patch"),
            root.path().join("real/fix.patch"),
        )
        .unwrap();
        symlink("real", root.path().join("link")).unwrap();
        assert!(
            plan(root.path(), &["//link:fix.patch"])
                .unwrap_err()
                .contains("symlink patch package")
        );
        assert!(
            plan(root.path(), &[":link/fix.patch"])
                .unwrap_err()
                .contains("symlink patch file parent")
        );
        assert!(
            plan(&root.path().join("link"), &[":fix.patch"])
                .unwrap_err()
                .contains("symlink patch root")
        );
        fs::remove_file(root.path().join("BUILD.bazel")).unwrap();
        symlink("real/BUILD", root.path().join("BUILD.bazel")).unwrap();
        assert!(
            plan(root.path(), &[":fix.patch"])
                .unwrap_err()
                .contains("symlink patch package marker")
        );
    }

    #[test]
    fn projection_edits_dependency_but_not_original_input() {
        let root = workspace();
        let before = "bazel_dep(name = \"dep\", version = \"1.0\")";
        let after = "bazel_dep(name = \"dep\", version = \"2.0\")";
        fs::write(
            root.path().join("fix.patch"),
            change("MODULE.bazel", before, after),
        )
        .unwrap();
        let original = format!("{before}\n").into_bytes();
        let p = plan(root.path(), &[":fix.patch"]).unwrap();
        assert_eq!(
            p.apply_module(&original).unwrap(),
            format!("{after}\n").as_bytes()
        );
        assert_eq!(original, format!("{before}\n").as_bytes());
    }

    #[test]
    fn non_module_patch_preserves_module_and_applies_to_tree() {
        let root = workspace();
        fs::write(
            root.path().join("fix.patch"),
            change("src/file.txt", "old", "new"),
        )
        .unwrap();
        let p = plan(root.path(), &[":fix.patch"]).unwrap();
        assert_eq!(
            p.apply_module(b"MODULE stays\0exact").unwrap(),
            b"MODULE stays\0exact"
        );
        let tree = tempfile::tempdir().unwrap();
        fs::create_dir(tree.path().join("src")).unwrap();
        fs::write(tree.path().join("src/file.txt"), "old\n").unwrap();
        p.apply_tree(tree.path()).unwrap();
        assert_eq!(
            fs::read(tree.path().join("src/file.txt")).unwrap(),
            b"new\n"
        );
    }

    #[test]
    fn commands_are_not_executed_or_projected_and_reject_before_tree_writes() {
        let root = workspace();
        let marker = root.path().join("command-ran");
        let mut opts = options(&[]);
        opts.patch_cmds = vec![format!("touch {}", marker.display())];
        let p = snapshot(root.path(), &ModuleFile::default(), &opts).unwrap();
        assert!(p.has_transformations());
        assert_eq!(p.apply_module(b"old\n").unwrap(), b"old\n");
        assert!(
            p.validate_materialization()
                .unwrap_err()
                .contains("patch_cmds")
        );
        assert!(p.apply_tree(root.path()).is_err());
        assert!(!marker.exists());
        opts.patches.push(":fix.patch".into());
        let p = snapshot(root.path(), &ModuleFile::default(), &opts).unwrap();
        assert_eq!(p.apply_module(b"old\n").unwrap(), b"new\n");
        fs::write(root.path().join("MODULE.bazel"), "old\n").unwrap();
        assert!(p.apply_tree(root.path()).is_err());
        assert_eq!(
            fs::read(root.path().join("MODULE.bazel")).unwrap(),
            b"old\n"
        );
        assert!(!marker.exists());
    }

    #[test]
    fn external_rejection_precedes_any_local_tree_writes() {
        let root = workspace();
        fs::write(root.path().join("MODULE.bazel"), "old\n").unwrap();
        let p = plan(root.path(), &[":fix.patch", "@@external+//p:missing.patch"]).unwrap();
        assert_eq!(p.apply_module(b"old\n").unwrap(), b"new\n");
        assert!(p.apply_tree(root.path()).is_err());
        assert_eq!(
            fs::read(root.path().join("MODULE.bazel")).unwrap(),
            b"old\n"
        );
    }

    #[test]
    fn ordered_snapshot_is_immutable_and_new_content_changes_identity() {
        let root = workspace();
        fs::write(
            root.path().join("next.patch"),
            change("MODULE.bazel", "new", "last"),
        )
        .unwrap();
        let labels = [":fix.patch", ":next.patch"];
        let p = plan(root.path(), &labels).unwrap();
        let identity = p.cache_identity();
        assert_eq!(p.apply_module(b"old\n").unwrap(), b"last\n");
        fs::write(
            root.path().join("next.patch"),
            change("MODULE.bazel", "new", "changed"),
        )
        .unwrap();
        assert_eq!(p.apply_module(b"old\n").unwrap(), b"last\n");
        assert_eq!(p.cache_identity(), identity);
        let updated = plan(root.path(), &labels).unwrap();
        assert_ne!(updated.cache_identity(), identity);
        assert_eq!(updated.apply_module(b"old\n").unwrap(), b"changed\n");
        fs::remove_file(root.path().join("fix.patch")).unwrap();
        fs::remove_file(root.path().join("next.patch")).unwrap();
        let tree = tempfile::tempdir().unwrap();
        fs::write(tree.path().join("MODULE.bazel"), "old\n").unwrap();
        p.apply_tree(tree.path()).unwrap();
        assert_eq!(
            fs::read(tree.path().join("MODULE.bazel")).unwrap(),
            b"last\n"
        );
    }

    #[test]
    fn identity_includes_order_strip_commands_and_labels() {
        let root = workspace();
        fs::copy(
            root.path().join("fix.patch"),
            root.path().join("same.patch"),
        )
        .unwrap();
        let p = plan(root.path(), &[":fix.patch", ":same.patch"]).unwrap();
        assert_ne!(
            p.cache_identity(),
            plan(root.path(), &[":same.patch", ":fix.patch"])
                .unwrap()
                .cache_identity()
        );
        assert_ne!(
            plan(root.path(), &[":fix.patch"]).unwrap().cache_identity(),
            plan(root.path(), &[":same.patch"])
                .unwrap()
                .cache_identity()
        );
        let mut opts = options(&[":fix.patch", ":same.patch"]);
        opts.patch_strip = 0;
        assert_ne!(
            p.cache_identity(),
            snapshot(root.path(), &ModuleFile::default(), &opts)
                .unwrap()
                .cache_identity()
        );
        opts.patch_strip = 1;
        opts.patch_cmds = vec!["first".into(), "second".into()];
        let commands = snapshot(root.path(), &ModuleFile::default(), &opts).unwrap();
        assert_ne!(p.cache_identity(), commands.cache_identity());
        opts.patch_cmds.reverse();
        assert_ne!(
            commands.cache_identity(),
            snapshot(root.path(), &ModuleFile::default(), &opts)
                .unwrap()
                .cache_identity()
        );
    }

    #[test]
    fn empty_and_strip_only_plans_do_not_touch_filesystem() {
        let missing = Path::new("/nonexistent-patch-inputs-root");
        let p = snapshot(missing, &ModuleFile::default(), &PatchOptions::default()).unwrap();
        assert_eq!(p, LocalPatchPlan::default());
        assert!(!p.has_transformations());
        assert_eq!(p.apply_module(b"exact").unwrap(), b"exact");
        p.apply_tree(missing).unwrap();
        let strip_only = snapshot(missing, &ModuleFile::default(), &options(&[])).unwrap();
        assert!(!strip_only.has_transformations());
        assert_ne!(strip_only.cache_identity(), p.cache_identity());
        let opts = PatchOptions {
            patch_strip: -1,
            ..Default::default()
        };
        assert!(
            snapshot(missing, &ModuleFile::default(), &opts)
                .unwrap_err()
                .contains("nonnegative")
        );
    }

    #[test]
    fn rejects_invalid_paths_before_filesystem_lookup() {
        let root = Path::new("/nonexistent-patch-inputs-root");
        for label in [
            "../outside",
            "//p:../outside",
            "//../p:x",
            "//p:/absolute",
            "//p:a\\b",
        ] {
            assert!(
                plan(root, &[label])
                    .unwrap_err()
                    .contains("invalid patch label")
            );
        }
        assert!(
            plan(root, &[":missing.patch", "../outside"])
                .unwrap_err()
                .contains("invalid patch label")
        );
    }

    #[test]
    fn patch_reads_have_no_native_size_cap() {
        // Above the former 16 MiB per-file / 64 MiB total native caps.
        let root = workspace();
        let path = root.path().join("huge.patch");
        File::create(&path)
            .unwrap()
            .set_len(17 * 1024 * 1024)
            .unwrap();
        assert_eq!(read_patch(&path).unwrap().len(), 17 * 1024 * 1024);
        let path = root.path().join("small.patch");
        fs::write(&path, b"12345").unwrap();
        assert_eq!(read_patch(&path).unwrap(), b"12345");
    }

    #[test]
    fn patch_engine_marker_only_for_nonempty_inputs() {
        // An empty plan keeps the exact earlier identity (unpatched trees).
        assert_eq!(
            LocalPatchPlan::default().cache_identity(),
            serde_json::json!({"inputs": [], "commands": [], "strip": 0})
        );
        let plan = LocalPatchPlan {
            inputs: vec![Input::External(
                Label::parse_canonical("@@other+//:fix.patch").unwrap(),
            )],
            commands: Vec::new(),
            strip: 1,
        };
        let identity = plan.cache_identity();
        assert_eq!(identity["engine"], crate::patching::ENGINE);
        let mut legacy = identity.clone();
        legacy.as_object_mut().unwrap().remove("engine");
        assert_ne!(identity, legacy);
    }
}
