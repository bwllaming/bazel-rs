//! Runfiles trees (N23f, docs/RUNFILES_TREES.md): the runfiles options, the
//! repository mapping manifest (analysis/RepoMappingManifestAction.java) and
//! the native actions RunfilesSupport registers for an executable
//! (analysis/RunfilesSupport.java:280-347, 501-544, 683-717).
//!
//! Paths below are under src/main/java/com/google/devtools/build/lib/ of the
//! pinned Bazel 9.3 sources.

use crate::label::Label;
use crate::runfiles::{Runfiles, WORKSPACE_NAME};
use std::collections::{BTreeMap, BTreeSet};

/// The runfiles options of a build (CoreOptions.java:591-638, 954-961;
/// BuildConfigurationValue.java:689-696, 855-882). They are request options
/// here: they select which runfiles artifacts and actions a build creates and
/// never enter configuration identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunfilesOptions {
    /// `--build_runfile_manifests` (default true).
    pub build_runfile_manifests: bool,
    /// `--build_runfile_links` (default true).
    pub build_runfile_links: bool,
    /// `--enable_runfiles` (tri-state `auto`, which is `yes` on Linux):
    /// RunfileSymlinksMode.CREATE when true, SKIP otherwise.
    pub enable_runfiles: bool,
    /// `--incompatible_compact_repo_mapping_manifest` (default true).
    pub compact_repo_mapping: bool,
}

impl Default for RunfilesOptions {
    fn default() -> Self {
        Self {
            build_runfile_manifests: true,
            build_runfile_links: true,
            enable_runfiles: true,
            compact_repo_mapping: true,
        }
    }
}

impl RunfilesOptions {
    /// BuildConfigurationValue.buildRunfileLinks: links need manifests.
    pub fn links(&self) -> bool {
        self.build_runfile_manifests && self.build_runfile_links
    }

    /// One `--[no]name[=value]` argument of these options, if it is one.
    pub fn parse(&mut self, arg: &str) -> Option<Result<(), String>> {
        let flags: [(&str, &mut bool); 3] = [
            ("build_runfile_manifests", &mut self.build_runfile_manifests),
            ("build_runfile_links", &mut self.build_runfile_links),
            (
                "incompatible_compact_repo_mapping_manifest",
                &mut self.compact_repo_mapping,
            ),
        ];
        for (name, slot) in flags {
            if let Some(value) = crate::configuration::boolean_option(arg, name) {
                return Some(value.map(|value| *slot = value));
            }
        }
        // TriState converter: yes/no/auto and the boolean spellings
        // (common/options/Converters.java TriStateConverter).
        if arg == "--enable_runfiles" {
            self.enable_runfiles = true;
            return Some(Ok(()));
        }
        if arg == "--noenable_runfiles" {
            self.enable_runfiles = false;
            return Some(Ok(()));
        }
        let value = arg.strip_prefix("--enable_runfiles=")?;
        Some(match value.to_ascii_lowercase().as_str() {
            "auto" | "yes" | "true" | "1" => {
                self.enable_runfiles = true;
                Ok(())
            }
            "no" | "false" | "0" => {
                self.enable_runfiles = false;
                Ok(())
            }
            _ => Err(format!(
                "While parsing option --enable_runfiles={value}: '{value}' is not one of {{auto, yes, no}}"
            )),
        })
    }
}

/// The repository mapping (apparent name -> canonical name) of a repository.
pub type MappingLookup<'a> = &'a dyn Fn(&str) -> Result<BTreeMap<String, String>, String>;

/// The repository name a runfiles artifact's owner belongs to (`""` is the
/// main repository; Artifact.getOwner().getRepository().getName()).
fn owner_repository(owner: &str) -> Option<String> {
    Label::parse_canonical(owner)
        .ok()
        .map(|label| label.repository)
}

/// The segment-0 name a canonical repository has in runfiles paths.
fn directory_name(canonical: &str) -> &str {
    if canonical.is_empty() {
        WORKSPACE_NAME
    } else {
        canonical
    }
}

/// `first` and `second` agree up to their last `+`-separated segment
/// (RepoMappingManifestAction.haveSamePrefix, :290-298).
fn same_prefix(first: &str, second: &str) -> bool {
    match (first.rfind('+'), second.rfind('+')) {
        (Some(a), Some(b)) => a == b && first[..a] == second[..b],
        _ => false,
    }
}

/// RepoMappingManifestAction.newDeterministicWriter (:191-278): the
/// `<executable>.repo_mapping` lines `source,apparent,target` for the
/// repositories of `transitive_repositories` (sorted by canonical name),
/// restricted to apparent names (never empty) of repositories that appear as
/// the first segment of a runfiles path (the main repository when there are
/// symlinks, root symlinks' first segments, artifact owners' repositories),
/// sorted by apparent name; the main repository's directory is `_main`. With
/// `compact`, consecutive repositories with the same mapping whose names
/// agree up to their last `+` segment (the repositories of one module
/// extension, whose mappings Bazel shares by reference) become one
/// `prefix+*` source.
pub fn repo_mapping_manifest(
    runfiles: &Runfiles,
    transitive_repositories: &BTreeSet<String>,
    mapping: MappingLookup<'_>,
    compact: bool,
) -> Result<Vec<u8>, String> {
    let mut in_runfiles: BTreeSet<String> = BTreeSet::new();
    if !runfiles.symlink_entries(false).is_empty() {
        in_runfiles.insert(String::new());
    }
    for (path, _) in runfiles.symlink_entries(true) {
        let first = path.split('/').next().unwrap_or_default().to_owned();
        in_runfiles.insert(first);
    }
    for file in runfiles.artifacts() {
        if let Some(repository) = owner_repository(&file.owner) {
            in_runfiles.insert(repository);
        }
    }
    // RepositoryName.getName of the mapping targets: a root symlink below
    // `_main` names no repository (the main repository's name is "").
    let relevant = |canonical: &str| in_runfiles.contains(canonical);
    let mut sorted: Vec<(String, BTreeMap<String, String>)> = Vec::new();
    for repository in transitive_repositories {
        sorted.push((repository.clone(), mapping(repository)?));
    }
    let entries = |mapping: &BTreeMap<String, String>| -> Vec<(String, String)> {
        mapping
            .iter()
            .filter(|(apparent, canonical)| !apparent.is_empty() && relevant(canonical))
            .map(|(apparent, canonical)| (apparent.clone(), canonical.clone()))
            .collect()
    };
    let mut out = Vec::new();
    let mut write = |source: &str, apparent: &str, canonical: &str| {
        out.extend_from_slice(
            format!("{source},{apparent},{}\n", directory_name(canonical)).as_bytes(),
        );
    };
    let mut index = 0;
    while index < sorted.len() {
        let (first, first_mapping) = &sorted[index];
        let mut group = 1;
        if compact {
            while index + group < sorted.len()
                && sorted[index + group].1 == *first_mapping
                && same_prefix(first, &sorted[index + group].0)
            {
                group += 1;
            }
        }
        let source = if group == 1 {
            first.clone()
        } else {
            format!(
                "{}*",
                &first[..=first.rfind('+').expect("same prefix has a +")]
            )
        };
        for (apparent, canonical) in entries(first_mapping) {
            write(&source, &apparent, &canonical);
        }
        index += group;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::starlark_types::File;

    fn runfiles(owners: &[&str]) -> Runfiles {
        let mut builder = crate::runfiles::RunfilesBuilder::default();
        for owner in owners {
            builder.add_artifact(File {
                is_directory: false,
                path: "x".into(),
                short_path: "x".into(),
                owner: (*owner).to_owned(),
                is_source: true,
            });
        }
        builder.build().unwrap()
    }

    #[test]
    fn repo_mapping_manifest_writes_relevant_sorted_and_compact_entries() {
        let mappings: BTreeMap<&str, BTreeMap<String, String>> = BTreeMap::from([
            (
                "",
                BTreeMap::from([
                    ("".to_owned(), "".to_owned()),
                    ("rf".to_owned(), "".to_owned()),
                    ("ext".to_owned(), "ext+".to_owned()),
                    ("other".to_owned(), "other+".to_owned()),
                ]),
            ),
            (
                "ext+",
                BTreeMap::from([("ext".to_owned(), "ext+".to_owned())]),
            ),
            (
                "m+e+a",
                BTreeMap::from([
                    ("ext".to_owned(), "ext+".to_owned()),
                    ("r".to_owned(), "".to_owned()),
                ]),
            ),
            (
                "m+e+b",
                BTreeMap::from([
                    ("ext".to_owned(), "ext+".to_owned()),
                    ("r".to_owned(), "".to_owned()),
                ]),
            ),
        ]);
        let lookup = |repository: &str| Ok(mappings[repository].clone());
        let repositories: BTreeSet<String> = ["", "ext+", "m+e+a", "m+e+b"]
            .map(str::to_owned)
            .into_iter()
            .collect();
        let rf = runfiles(&["//pkg:b", "@@ext+//:e"]);
        let compact = repo_mapping_manifest(&rf, &repositories, &lookup, true).unwrap();
        assert_eq!(
            String::from_utf8(compact).unwrap(),
            ",ext,ext+\n,rf,_main\next+,ext,ext+\nm+e+*,ext,ext+\nm+e+*,r,_main\n"
        );
        let full = repo_mapping_manifest(&rf, &repositories, &lookup, false).unwrap();
        assert_eq!(
            String::from_utf8(full).unwrap(),
            ",ext,ext+\n,rf,_main\next+,ext,ext+\nm+e+a,ext,ext+\nm+e+a,r,_main\nm+e+b,ext,ext+\nm+e+b,r,_main\n"
        );
        // Only the main repository in runfiles: entries pointing elsewhere drop.
        let main_only =
            repo_mapping_manifest(&runfiles(&["//pkg:b"]), &repositories, &lookup, true).unwrap();
        assert_eq!(
            String::from_utf8(main_only).unwrap(),
            ",rf,_main\nm+e+*,r,_main\n"
        );
    }

    #[test]
    fn runfiles_options_parse_like_bazel() {
        let mut options = RunfilesOptions::default();
        assert!(options.parse("--nobuild_runfile_links").unwrap().is_ok());
        assert!(!options.links());
        assert!(options.parse("--enable_runfiles=no").unwrap().is_ok());
        assert!(!options.enable_runfiles);
        assert!(options.parse("--enable_runfiles").unwrap().is_ok());
        assert!(options.enable_runfiles);
        assert!(options.parse("--enable_runfiles=x").unwrap().is_err());
        assert!(
            options
                .parse("--noincompatible_compact_repo_mapping_manifest")
                .unwrap()
                .is_ok()
        );
        assert!(!options.compact_repo_mapping);
        assert!(options.parse("--jobs=1").is_none());
    }
}
