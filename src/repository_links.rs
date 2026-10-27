//! Validation for a preserved, relative, regular-file-only symlink profile.
//!
//! Inputs must describe the complete namespace that will be materialized (after
//! any export/strip-prefix selection). No filesystem access or mutation occurs.
//! Call `validate` before writing anything; `target_path` alone is only a lexical
//! helper and cannot detect missing directories or traversal through files/links.
//! Materializers must preserve the original target string, not replace it with
//! the normalized path returned by `target_path`.
//!
//! Names are UTF-8, normal relative slash-separated paths. Spaces and hidden
//! names are allowed, as are `#`, `%`, `?`, `:` and `\\` (ordinary Linux
//! filename bytes; Bazel imposes no such exclusion). Targets additionally allow
//! `.` and `..`, but not absolute paths or empty components (including trailing
//! slashes). Directory links, external links, dangling links, and cycles are not
//! supported. Limits are 4096 bytes per name/target (PATH_MAX), 255 bytes per
//! component (NAME_MAX), 256 raw target components, and 40 symlink hops; there
//! is no component-count (depth) cap. Native platforms may impose a lower
//! symlink-hop limit; validation does not override that platform limit.

use std::collections::{BTreeMap, BTreeSet};

const MAX_PATH_BYTES: usize = 4096;
const MAX_NAME_BYTES: usize = 255;
const MAX_TARGET_COMPONENTS: usize = 256;
const MAX_LINK_STEPS: usize = 40;

fn check_characters(value: &str, kind: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("empty {kind}"));
    }
    if value.len() > MAX_PATH_BYTES {
        return Err(format!("{kind} exceeds {MAX_PATH_BYTES} bytes"));
    }
    if value.contains('\0') {
        return Err(format!("{kind} contains NUL: {value:?}"));
    }
    Ok(())
}

fn check_name(path: &str) -> Result<(), String> {
    check_characters(path, "entry path")?;
    for component in path.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.len() > MAX_NAME_BYTES
        {
            return Err(format!(
                "entry path is not a normal relative path: {path:?}"
            ));
        }
    }
    Ok(())
}

fn parent_components(path: &str) -> Vec<&str> {
    let mut components: Vec<_> = path.split('/').collect();
    components.pop();
    components
}

/// Return the lexical root-relative destination of a relative symlink target.
///
/// This only validates syntax and bounds: for example, `missing/../file` can
/// normalize to `file` here but is rejected by `validate` if `missing` is absent.
/// Preserve `raw_target` verbatim when creating the symlink.
pub fn target_path(link_path: &str, raw_target: &str) -> Result<String, String> {
    check_name(link_path)?;
    check_characters(raw_target, "symlink target")?;
    if raw_target.starts_with('/') {
        return Err(format!(
            "absolute symlink target is unsupported: {raw_target:?}"
        ));
    }
    let mut components = parent_components(link_path);
    for (index, component) in raw_target.split('/').enumerate() {
        if index >= MAX_TARGET_COMPONENTS {
            return Err(format!(
                "symlink target exceeds {MAX_TARGET_COMPONENTS} raw components"
            ));
        }
        match component {
            "" => return Err(format!("empty symlink target component: {raw_target:?}")),
            "." => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(format!(
                        "symlink target escapes the repository root: {raw_target:?}"
                    ));
                }
            }
            normal => components.push(normal),
        }
        // Also bound intermediate paths, not just the final normalized result.
        let bytes = components.iter().map(|part| part.len()).sum::<usize>()
            + components.len().saturating_sub(1);
        if bytes > MAX_PATH_BYTES {
            return Err(format!(
                "resolved symlink target exceeds {MAX_PATH_BYTES} bytes"
            ));
        }
    }
    if components.is_empty() {
        return Err("directory target is unsupported: repository root".to_owned());
    }
    Ok(components.join("/"))
}

/// Validate every entry and every link in a complete, selected namespace.
///
/// Successful links terminate at an existing regular file, possibly through
/// other links. Only implicit directories (ancestors of input entries) exist.
/// Intermediate raw components must traverse those directories, even when a
/// later `..` would lexically cancel them. Files and links may never be parents.
/// The function neither creates files nor reads the host filesystem.
pub fn validate(
    regular_files: &BTreeSet<String>,
    links: &BTreeMap<String, String>,
) -> Result<(), String> {
    for path in regular_files.iter().chain(links.keys()) {
        check_name(path)?;
    }
    for path in links.keys() {
        if regular_files.contains(path) {
            return Err(format!(
                "regular file and symlink share the same path: {path:?}"
            ));
        }
    }

    let mut directories = BTreeSet::new();
    directories.insert(String::new());
    for path in regular_files.iter().chain(links.keys()) {
        for (index, _) in path.match_indices('/') {
            let parent = &path[..index];
            if regular_files.contains(parent) || links.contains_key(parent) {
                return Err(format!(
                    "file-parent conflict: {parent:?} is an ancestor of {path:?}"
                ));
            }
            directories.insert(parent.to_owned());
        }
    }

    let mut destinations = BTreeMap::new();
    for (link, raw_target) in links {
        let destination =
            target_path(link, raw_target).map_err(|error| format!("symlink {link:?}: {error}"))?;
        let mut walked = parent_components(link);
        let mut components = raw_target.split('/').peekable();
        while let Some(component) = components.next() {
            match component {
                "." => {}
                ".." => {
                    // target_path already checked root confinement and syntax.
                    walked.pop();
                }
                normal => {
                    walked.push(normal);
                    if components.peek().is_some() {
                        let intermediate = walked.join("/");
                        if regular_files.contains(&intermediate)
                            || links.contains_key(&intermediate)
                        {
                            return Err(format!(
                                "symlink {link:?}: intermediate file or symlink is unsupported: {intermediate:?}"
                            ));
                        }
                        if !directories.contains(&intermediate) {
                            return Err(format!(
                                "symlink {link:?}: missing intermediate directory: {intermediate:?}"
                            ));
                        }
                    }
                }
            }
        }
        if directories.contains(&destination) {
            return Err(format!(
                "symlink {link:?}: directory target is unsupported: {destination:?}"
            ));
        }
        if !regular_files.contains(&destination) && !links.contains_key(&destination) {
            return Err(format!(
                "symlink {link:?}: dangling target: {destination:?}"
            ));
        }
        destinations.insert(link.as_str(), destination);
    }

    for source in links.keys() {
        let mut current = source.as_str();
        let mut visited = BTreeSet::new();
        let mut steps = 0;
        loop {
            if !visited.insert(current) {
                return Err(format!("symlink {source:?}: cycle at {current:?}"));
            }
            if steps >= MAX_LINK_STEPS {
                return Err(format!(
                    "symlink {source:?}: chain exceeds {MAX_LINK_STEPS} steps"
                ));
            }
            steps += 1;
            // Every link was checked and entered in destinations above.
            let destination = destinations[current].as_str();
            if regular_files.contains(destination) {
                break;
            }
            current = destination;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    fn links(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(path, target)| ((*path).to_owned(), (*target).to_owned()))
            .collect()
    }

    fn rejected(files: &[&str], entries: &[(&str, &str)], message: &str) {
        let error = validate(&self::files(files), &links(entries)).unwrap_err();
        assert!(
            error.contains(message),
            "expected {message:?}, got {error:?}"
        );
    }

    #[test]
    fn exact_rules_distroless_e2e_target() {
        let target = "../../.bazelversion";
        assert_eq!(target.len(), 19);
        assert_eq!(
            target_path("e2e/smoke/.bazelversion", target).unwrap(),
            ".bazelversion"
        );
        validate(
            &files(&[".bazelversion"]),
            &links(&[("e2e/smoke/.bazelversion", target)]),
        )
        .unwrap();
    }

    #[test]
    fn relative_leaf_hidden_names_and_spaces() {
        validate(
            &files(&["a dir/.hidden"]),
            &links(&[("a dir/a link", "./.hidden")]),
        )
        .unwrap();
        assert_eq!(
            target_path("a dir/a link", "./.hidden").unwrap(),
            "a dir/.hidden"
        );
    }

    #[test]
    fn chain_resolves_each_targets_own_parent() {
        validate(
            &files(&["data/file"]),
            &links(&[("a/first", "../b/second"), ("b/second", "../data/file")]),
        )
        .unwrap();
    }

    #[test]
    fn self_and_multiple_link_cycles() {
        rejected(&[], &[("self", "self")], "cycle");
        rejected(&[], &[("a", "b"), ("b", "a")], "cycle");
        rejected(&[], &[("a", "b"), ("b", "c"), ("c", "b")], "cycle");
    }

    #[test]
    fn unsafe_targets_are_rejected() {
        // Only structural escapes reject: empty/absolute components and
        // root escape. Controls, drive letters, backslashes, `?#%` are
        // ordinary Linux filename bytes in a target.
        for target in [
            "",
            "/file",
            "//file",
            "../file",
            "x/../../file",
            "file/",
            "a//file",
        ] {
            assert!(target_path("link", target).is_err(), "accepted {target:?}");
        }
    }

    #[test]
    fn all_entry_names_must_be_safe_normal_relative_paths() {
        for name in [
            "", "/file", "file/", "a//b", ".", "..", "./file", "a/../b", "a/./b", "a\0b",
        ] {
            assert!(
                validate(&files(&[name]), &BTreeMap::new()).is_err(),
                "file {name:?}"
            );
            assert!(
                validate(&files(&["file"]), &links(&[(name, "file")])).is_err(),
                "link {name:?}"
            );
            assert!(target_path(name, "file").is_err(), "helper {name:?}");
        }
    }

    #[test]
    fn dangling_targets_are_distinct_from_directories() {
        rejected(&["dir/file"], &[("link", "absent")], "dangling target");
        rejected(&["dir/file"], &[("link", "dir")], "directory target");
        rejected(&["file"], &[("link", ".")], "directory target");
        rejected(&["file"], &[("dir/link", "..")], "directory target");
        // A directory implied only by another link also counts as a directory.
        rejected(
            &["file"],
            &[("dir/leaf", "../file"), ("link", "dir")],
            "directory target",
        );
    }

    #[test]
    fn symlinks_cannot_be_intermediate_components() {
        rejected(
            &["file"],
            &[("a", "b/../file"), ("b", "file")],
            "intermediate file or symlink",
        );
        rejected(
            &["file"],
            &[("a", "b/./../file"), ("b", "file")],
            "intermediate file or symlink",
        );
        rejected(
            &["file"],
            &[("a", "b/child"), ("b", "file")],
            "intermediate file or symlink",
        );
        rejected(
            &["dir/child", "file"],
            &[("a", "dirlink/../file"), ("dirlink", "dir")],
            "intermediate file or symlink",
        );
    }

    #[test]
    fn missing_directory_cannot_be_lexically_cancelled() {
        assert_eq!(target_path("link", "missing/../file").unwrap(), "file");
        rejected(
            &["file"],
            &[("link", "missing/../file")],
            "missing intermediate directory",
        );
        rejected(
            &["file"],
            &[("link", "missing/./../file")],
            "missing intermediate directory",
        );
    }

    #[test]
    fn regular_file_cannot_be_lexically_cancelled() {
        for target in [
            "file/../file",
            "file/./../file",
            "file/child",
            "file/.",
            "file/..",
        ] {
            // Root normalization of file/.. is rejected even before walking.
            assert!(validate(&files(&["file"]), &links(&[("link", target)])).is_err());
        }
        rejected(
            &["file"],
            &[("link", "file/../file")],
            "intermediate file or symlink",
        );
    }

    #[test]
    fn existing_directories_may_be_cancelled() {
        for target in ["dir/../file", "./dir/./.././file", "dir/nested/../../file"] {
            validate(
                &files(&["dir/nested/child", "file"]),
                &links(&[("link", target)]),
            )
            .unwrap();
            assert_eq!(target_path("link", target).unwrap(), "file");
        }
    }

    #[test]
    fn file_parent_conflicts_cover_every_entry_kind() {
        rejected(&["a", "a/b"], &[], "file-parent conflict");
        rejected(&["a"], &[("a/b", "../a")], "file-parent conflict");
        rejected(&["a/b", "file"], &[("a", "file")], "file-parent conflict");
        rejected(
            &["file"],
            &[("a", "file"), ("a/b", "../file")],
            "file-parent conflict",
        );
    }

    #[test]
    fn same_path_file_and_link_is_a_conflict() {
        rejected(&["file"], &[("file", "file")], "same path");
    }

    #[test]
    fn validates_unreferenced_links_too() {
        rejected(
            &["file"],
            &[("good", "file"), ("unused", "absent")],
            "dangling target",
        );
    }

    #[test]
    fn strip_prefix_cannot_reach_unexported_namespace() {
        validate(
            &files(&["outside", "export/local"]),
            &links(&[("export/link", "../outside")]),
        )
        .unwrap();
        // After stripping export/, the same literal target escapes the namespace.
        rejected(&["local"], &[("link", "../outside")], "escapes");
        rejected(&["local"], &[("link", "outside")], "dangling target");
    }

    #[test]
    fn path_and_target_byte_bounds() {
        // Entry names: NAME_MAX per component caps the reachable total
        // below PATH_MAX (16x255 + 15 separators = 4095); a 4096-byte
        // target string (no per-component cap on raw targets) still fits.
        let component = "a".repeat(MAX_NAME_BYTES);
        let maximum_entry = vec![component.as_str(); 16].join("/");
        assert_eq!(maximum_entry.len(), MAX_PATH_BYTES - 1);
        assert_eq!(target_path("link", &maximum_entry).unwrap(), maximum_entry);
        validate(&files(&[&maximum_entry]), &BTreeMap::new()).unwrap();
        let maximum = "a".repeat(MAX_PATH_BYTES);
        let too_long = "a".repeat(MAX_PATH_BYTES + 1);
        assert_eq!(target_path("link", &maximum).unwrap(), maximum);
        assert!(validate(&files(&[&maximum]), &BTreeMap::new()).is_err());
        // A single component over NAME_MAX rejects even under PATH_MAX.
        assert!(
            validate(
                &files(&["a".repeat(MAX_NAME_BYTES + 1).as_str()]),
                &BTreeMap::new()
            )
            .is_err()
        );
        assert!(target_path("link", &too_long).is_err());
        assert!(target_path(&too_long, "file").is_err());
        assert!(validate(&files(&[&too_long]), &BTreeMap::new()).is_err());
        // Both inputs individually fit, but their resolved concatenation does not.
        assert!(
            target_path("parent/link", &maximum)
                .unwrap_err()
                .contains("resolved")
        );
        // Bytes, not Unicode scalar values, are bounded.
        assert!(target_path("link", &"é".repeat(2049)).is_err());
    }

    #[test]
    fn path_depth_is_bounded_only_by_total_bytes() {
        // No component-count cap (Bazel/Linux): 64- and 65-component paths
        // both validate; only the 4096-byte total bounds depth.
        let deep64 = vec!["a"; 64].join("/");
        let deep65 = format!("{deep64}/a");
        assert_eq!(target_path("link", &deep64).unwrap(), deep64);
        validate(&files(&[&deep64]), &BTreeMap::new()).unwrap();
        assert_eq!(target_path("link", &deep65).unwrap(), deep65);
        validate(&files(&[&deep65]), &BTreeMap::new()).unwrap();
        // Bazel-legal filename bytes in names and targets.
        validate(
            &files(&["a?b", "a%b", "C:file", "a\\b"]),
            &links(&[("a#b", "a%b")]),
        )
        .unwrap();
    }

    #[test]
    fn raw_walk_component_bound_includes_dots() {
        let maximum = format!("{}file", "./".repeat(MAX_TARGET_COMPONENTS - 1));
        validate(&files(&["file"]), &links(&[("link", &maximum)])).unwrap();
        let too_many = format!("./{maximum}");
        assert!(
            target_path("link", &too_many)
                .unwrap_err()
                .contains("raw components")
        );
    }

    #[test]
    fn iterative_chain_length_is_bounded_at_40_links() {
        fn chain(length: usize) -> BTreeMap<String, String> {
            (0..length)
                .map(|i| {
                    let target = if i + 1 == length {
                        "file".to_owned()
                    } else {
                        format!("link{:03}", i + 1)
                    };
                    (format!("link{i:03}"), target)
                })
                .collect()
        }
        validate(&files(&["file"]), &chain(MAX_LINK_STEPS)).unwrap();
        assert!(
            validate(&files(&["file"]), &chain(MAX_LINK_STEPS + 1))
                .unwrap_err()
                .contains("chain exceeds 40 steps")
        );
    }

    #[test]
    fn source_inputs_are_unchanged_and_no_files_are_synthesized() {
        let regular_files = files(&["dir/child", "file"]);
        let link_entries = links(&[("link", "./dir/../file")]);
        let original_files = regular_files.clone();
        let original_links = link_entries.clone();
        validate(&regular_files, &link_entries).unwrap();
        assert_eq!(regular_files, original_files);
        assert_eq!(link_entries, original_links);
        assert_eq!(link_entries["link"], "./dir/../file");
        assert_eq!(regular_files.len(), 2);
        assert_eq!(link_entries.len(), 1);
        validate(&BTreeSet::new(), &BTreeMap::new()).unwrap();
    }

    #[cfg(unix)]
    mod native {
        use super::*;
        use std::fs;
        use std::os::unix::fs::symlink;
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct TempDir(PathBuf);

        impl TempDir {
            fn new() -> Self {
                static NEXT: AtomicUsize = AtomicUsize::new(0);
                for _ in 0..100 {
                    let path = crate::test_temp::root().join(format!(
                        "repository-links-test-{}-{}",
                        std::process::id(),
                        NEXT.fetch_add(1, Ordering::Relaxed)
                    ));
                    match fs::create_dir(&path) {
                        Ok(()) => return Self(path),
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                        Err(error) => panic!("create private test directory: {error}"),
                    }
                }
                panic!("unable to allocate private test directory");
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                fs::remove_dir_all(&self.0).expect("remove private test directory");
            }
        }

        #[test]
        fn actual_e2e_symlink_preserves_literal_and_reads_root_file() {
            let temp = TempDir::new();
            let regular_files = files(&[".bazelversion"]);
            let link_entries = links(&[("e2e/smoke/.bazelversion", "../../.bazelversion")]);
            validate(&regular_files, &link_entries).unwrap();
            fs::create_dir_all(temp.0.join("e2e/smoke")).unwrap();
            fs::write(temp.0.join(".bazelversion"), b"test-version\n").unwrap();
            let link = temp.0.join("e2e/smoke/.bazelversion");
            symlink(&link_entries["e2e/smoke/.bazelversion"], &link).unwrap();
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert!(
                fs::symlink_metadata(temp.0.join(".bazelversion"))
                    .unwrap()
                    .is_file()
            );
            assert_eq!(
                fs::read_link(&link).unwrap(),
                PathBuf::from("../../.bazelversion")
            );
            assert_eq!(fs::read(&link).unwrap(), b"test-version\n");
        }

        #[cfg(target_os = "linux")]
        include!("../tests/support/forty_link_read.rs");

        #[cfg(target_os = "linux")]
        #[test]
        fn accepted_chain_limit_matches_linux_file_resolution() {
            let temp = TempDir::new();
            let regular = files(&["file"]);
            fs::write(temp.0.join("file"), b"payload").unwrap();
            let mut entries = BTreeMap::new();
            for index in 0..40 {
                let name = format!("link{index}");
                let target = if index == 39 {
                    "file".to_owned()
                } else {
                    format!("link{}", index + 1)
                };
                symlink(&target, temp.0.join(&name)).unwrap();
                entries.insert(name, target);
            }
            validate(&regular, &entries).unwrap();
            assert_eq!(read_through_forty_links(&temp.0.join("link0")), b"payload");
            entries.insert("extra".into(), "link0".into());
            symlink("link0", temp.0.join("extra")).unwrap();
            assert_eq!(
                fs::read(temp.0.join("extra")).unwrap_err().raw_os_error(),
                Some(40)
            );
            assert!(
                validate(&regular, &entries).is_err(),
                "do not accept a chain that the native filesystem cannot resolve"
            );
        }

        #[test]
        fn actual_chain_and_directory_cancellation_read_the_regular_file() {
            let temp = TempDir::new();
            let regular_files = files(&["dir/child", "file"]);
            let link_entries = links(&[("first", "./dir/../second"), ("second", "file")]);
            validate(&regular_files, &link_entries).unwrap();
            fs::create_dir(temp.0.join("dir")).unwrap();
            fs::write(temp.0.join("dir/child"), b"child").unwrap();
            fs::write(temp.0.join("file"), b"payload").unwrap();
            for (path, target) in &link_entries {
                symlink(target, temp.0.join(path)).unwrap();
            }
            assert_eq!(fs::read(temp.0.join("first")).unwrap(), b"payload");
        }

        #[test]
        fn actual_kernel_rejects_lexical_cancellation_traps() {
            let temp = TempDir::new();
            fs::write(temp.0.join("file"), b"payload").unwrap();
            symlink("file", temp.0.join("filelink")).unwrap();
            for (index, target) in [
                "missing/../file",
                "file/../file",
                "filelink/../file",
                "filelink/./../file",
            ]
            .iter()
            .enumerate()
            {
                let name = format!("trap{index}");
                assert_eq!(target_path(&name, target).unwrap(), "file");
                assert!(
                    validate(
                        &files(&["file"]),
                        &links(&[("filelink", "file"), (&name, target)])
                    )
                    .is_err()
                );
                symlink(target, temp.0.join(&name)).unwrap();
                let error = fs::read(temp.0.join(&name)).unwrap_err();
                #[cfg(target_os = "linux")]
                assert_eq!(error.raw_os_error(), Some(if index == 0 { 2 } else { 20 }));
                #[cfg(not(target_os = "linux"))]
                let _ = error;
            }
        }
    }
}
