//! Module-command helpers shared with the CLI: repository option syntax,
//! the native built-in profile switch and `mod dump_repo_mapping`'s JSON
//! (the `mod` command itself is crate::mod_command).
use crate::module_graph;
use crate::registry::Registry;
use std::collections::BTreeSet;

/// Bazel 9.3 `mod dump_repo_mapping NAME` (ModCommand.dumpRepoMappings):
/// the repository mapping of the canonical repository `name`, including
/// module aliases and extension imports, as one JSON line. Empty is the
/// canonical root name, not `_main`. JSON is an output format, not a store.
pub fn dump_repo_mapping(
    repositories: &crate::repository::RepositorySet,
    name: &str,
) -> Result<String, String> {
    use serde::ser::{SerializeMap, Serializer as _};
    let mapping = repositories.mapping(name)?;
    let mut bytes = Vec::new();
    let mut serializer = serde_json::Serializer::new(&mut bytes);
    let mut object = serializer
        .serialize_map(Some(mapping.len()))
        .map_err(|e| e.to_string())?;
    let mut emitted = BTreeSet::new();
    // Preserve genuine's insertion order (extension imports, own name,
    // dependencies); the underlying shared lookup map is sorted by name.
    for name in repositories
        .mapping_order(name)
        .iter()
        .chain(mapping.keys())
    {
        if let Some(canonical) = mapping.get(name)
            && emitted.insert(name.clone())
        {
            object
                .serialize_entry(name, canonical)
                .map_err(|e| e.to_string())?;
        }
    }
    object.end().map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

/// Bazel 9.3 RepositoryOptions and BooleanConverter syntax. Boolean flags are
/// nullary unless the value follows '='; negated flags never accept a value.
/// Return the disable setting (not the enabled policy) for either option name.
pub fn repository_disable_download_argument(arg: &str) -> Result<Option<bool>, String> {
    let (flag, value) = arg
        .split_once('=')
        .map_or((arg, None), |(flag, value)| (flag, Some(value)));
    match flag {
        "--norepository_disable_download" | "--noexperimental_repository_disable_download" => {
            if value.is_some() {
                return Err(format!("unexpected value after boolean option: {arg}"));
            }
            Ok(Some(false))
        }
        "--repository_disable_download" | "--experimental_repository_disable_download" => {
            let disabled = match value.map(str::to_ascii_lowercase).as_deref() {
                None | Some("true" | "1" | "yes" | "t" | "y") => true,
                Some("false" | "0" | "no" | "f" | "n") => false,
                _ => return Err(format!("{arg}: expected a boolean")),
            };
            Ok(Some(disabled))
        }
        _ => Ok(None),
    }
}

pub fn registry_path(uri: &str) -> Result<Registry, String> {
    Registry::parse(uri)
}

/// Reject unsupported repository overrides without including their values.
/// Library module-command callers need the same protection as the CLI entry.
pub fn reject_unsupported_repo_env_argument(arg: &std::ffi::OsStr) -> Result<(), String> {
    let bytes = arg.as_encoded_bytes();
    if bytes == b"--repo_env" || bytes.starts_with(b"--repo_env=") {
        Err(
            "unsupported option --repo_env; repository environment overrides are not implemented"
                .to_owned(),
        )
    } else {
        Ok(())
    }
}

/// Native-only switch, independent of graph display/registry policy. The
/// built-in profile (Bazel 9.3's behaviour) is the default of query, build,
/// run and test; `=false` selects the explicit-module profile, which native
/// `mod graph` presents by default.
pub fn builtin_profile_argument(arg: &str) -> Result<Option<module_graph::BuiltinProfile>, String> {
    use module_graph::BuiltinProfile;
    match arg {
        "--experimental-builtin-modules" | "--experimental-builtin-modules=true" => {
            Ok(Some(BuiltinProfile::Bazel93Builtins))
        }
        "--experimental-builtin-modules=false" => Ok(Some(BuiltinProfile::ExplicitModules)),
        _ if arg.starts_with("--experimental-builtin-modules=") => {
            Err("--experimental-builtin-modules expects true or false".to_owned())
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_download_boolean_matches_pinned_bazel_syntax() {
        for name in [
            "repository_disable_download",
            "experimental_repository_disable_download",
        ] {
            let positive = format!("--{name}");
            let negative = format!("--no{name}");
            assert_eq!(
                repository_disable_download_argument(&positive),
                Ok(Some(true))
            );
            assert_eq!(
                repository_disable_download_argument(&negative),
                Ok(Some(false))
            );
            for (values, expected) in [
                (["true", "1", "yes", "t", "y"], true),
                (["false", "0", "no", "f", "n"], false),
            ] {
                for value in values {
                    for value in [value.to_owned(), value.to_ascii_uppercase()] {
                        assert_eq!(
                            repository_disable_download_argument(&format!("{positive}={value}")),
                            Ok(Some(expected))
                        );
                        assert!(
                            repository_disable_download_argument(&format!("{negative}={value}"))
                                .is_err()
                        );
                    }
                }
            }
            for value in [
                "",
                " true",
                "false ",
                "2",
                "on",
                "off",
                "true=false",
                "trüe",
            ] {
                assert!(
                    repository_disable_download_argument(&format!("{positive}={value}")).is_err()
                );
            }
        }
        for unrelated in [
            "--repository_disable_downloads",
            "--repository-disable-download",
            "--nonoexperimental_repository_disable_download",
            "false",
        ] {
            assert_eq!(repository_disable_download_argument(unrelated), Ok(None));
        }
    }

    #[test]
    fn builtin_profile_argument_parses_the_native_switch() {
        use module_graph::BuiltinProfile;
        for (arg, expected) in [
            (
                "--experimental-builtin-modules",
                BuiltinProfile::Bazel93Builtins,
            ),
            (
                "--experimental-builtin-modules=true",
                BuiltinProfile::Bazel93Builtins,
            ),
            (
                "--experimental-builtin-modules=false",
                BuiltinProfile::ExplicitModules,
            ),
        ] {
            assert_eq!(builtin_profile_argument(arg), Ok(Some(expected)));
        }
        assert!(builtin_profile_argument("--experimental-builtin-modules=maybe").is_err());
        assert_eq!(builtin_profile_argument("--include_builtin"), Ok(None));
    }
}
