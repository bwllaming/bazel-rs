//! Bazel 9.3 MODULE/command-line flag aliases. These are configuration
//! metadata, not repositories or BUILD `alias` targets.
use std::collections::BTreeMap;

use crate::{bazelrc, configuration, label::Label, repository::RepositorySet};

pub type Aliases = BTreeMap<String, Label>;

/// ModuleFileGlobals.normalizeLabelString: preserve apparent repo names,
/// spell relative labels with the module's own repo name, and deliberately
/// turn even @@ labels into apparent labels (the Java method drops one @).
pub(crate) fn normalize_module_label(raw: &str, own: &str, root: bool) -> Result<String, String> {
    let parts = crate::bazel_label::Parts::parse(raw)?;
    let mut mapping = BTreeMap::new();
    if let Some(repo) = parts.repo {
        mapping.insert(repo.to_owned(), repo.to_owned());
    }
    if root {
        mapping.insert(String::new(), own.to_owned());
    }
    let label = Label::parse_in(raw, own, "", &mapping)?;
    Ok(format!(
        "@{}//{}:{}",
        label.repository, label.package, label.name
    ))
}

/// SkyframeExecutor.getFlagAliases: root then selected modules in BFS order;
/// later modules replace earlier definitions. Resolve using each owner's
/// *full* mapping, including use_repo imports, not the root's mapping.
pub fn module_aliases(repositories: &RepositorySet) -> Result<Aliases, String> {
    // The native legacy WORKSPACE/REPO marker profile permits an absent
    // MODULE. It has no module aliases; do not invent a graph demand.
    if !crate::observed_fs::exists(repositories.main_root().join("MODULE.bazel")) {
        return Ok(Aliases::new());
    }
    let graph = repositories.resolve_graph()?;
    let mut result = Aliases::new();
    for (owner, module) in std::iter::once(("", &graph.root)).chain(
        graph
            .modules_in_bfs_order()
            .into_iter()
            .map(|m| (m.canonical_name.as_str(), &m.module)),
    ) {
        for (name, destination) in &module.flag_aliases {
            let label = repositories
                .parse(destination, owner, "")
                // Java falls back to parsing the apparent label with the root
                // mapping if the owner's mapping cannot resolve it.
                .or_else(|_| repositories.parse_deferred(destination, "", ""))?;
            result.insert(name.clone(), label);
        }
    }
    // The dispatcher converts only the effective aliases, not overwritten
    // destinations contributed by earlier modules.
    for (name, label) in &result {
        validate_name(name)?;
        if label.to_string().contains('=') {
            return Err("--flag_alias does not support flag value assignment.".into());
        }
    }
    Ok(result)
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(
            "Flag alias definitions must be in the form of a 'name=label' assignment".into(),
        );
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(format!(
            "{name} should only consist of word characters to be a valid alias name."
        ));
    }
    // Snapshot keys omit these execution/UI options. Until their bootstrap
    // consumers can be reparsed, an unused alias must also fail: a later use
    // could otherwise hit a snapshot before diagnosing the unsupported shadow.
    if crate::snapshot::key::excluded(&format!("--{name}"))
        || known_non_configuration_option(name)
        || matches!(
            name,
            "config" | "enable_platform_specific_config" | "flag_alias"
        )
    {
        return Err(format!(
            "flag alias --{name} shadows a non-configuration or parser-control option; this alias is not implemented in the native profile"
        ));
    }
    Ok(())
}

fn configuration_option(name: &str) -> bool {
    configuration::fragment_option(configuration::canonical_name(name)).is_some()
        // A few native configuration consumers (e.g. strict_action_env) live
        // outside FragmentOptions. Their real parser defines their scope.
        || !matches!(configuration::CommandLineOptions::default().parse(&format!("--{name}"), None),
            Ok(configuration::Parsed::NotModelled))
}

fn known_non_configuration_option(name: &str) -> bool {
    crate::command_options::COMMAND_OPTIONS
        .iter()
        .any(|row| row.0 == name || !row.1.is_empty() && row.1 == name)
        && !configuration_option(name)
}

/// Only unmodelled configuration names with a first-pass converter we can
/// validate may wait for MODULE aliases. Unsupported command/execution options
/// must retain their original parse-order failure (before bootstrap/snapshots).
/// Explicit CLI aliases already replaced the argument before this function.
pub fn defer_unmodelled_option(arg: &str) -> Result<bool, String> {
    let Some(rest) = arg.strip_prefix("--") else {
        return Ok(false);
    };
    let (name, value) = rest
        .split_once('=')
        .map_or((rest, None), |(name, value)| (name, Some(value)));
    if !configuration_option(name) {
        return Ok(false);
    }
    let name = configuration::canonical_name(name);
    let Some(option) = crate::option_metadata::OPTIONS
        .iter()
        .find(|option| option.name == name)
    else {
        return Ok(false);
    };
    match option.converter {
        "Boolean" | "TriState" => {
            if option.converter == "TriState"
                && value.is_some_and(|v| v.eq_ignore_ascii_case("auto"))
            {
                return Ok(true);
            }
            // Use the original (possibly old) spelling for diagnostics.
            let raw_name = rest.split('=').next().unwrap_or(rest);
            configuration::boolean_option(arg, raw_name).transpose()?;
            Ok(true)
        }
        "String" => Ok(true),
        // Do not bypass an unknown native converter just because the alias's
        // eventual Starlark converter might accept its input.
        _ => Ok(false),
    }
}

pub fn definition(
    input: &str,
    labels: &configuration::LabelParser<'_>,
) -> Result<(String, Label), String> {
    let (name, destination) = input.split_once('=').ok_or_else(|| {
        "Flag alias definitions must be in the form of a 'name=label' assignment".to_owned()
    })?;
    validate_name(name)?;
    if destination.contains('=') {
        return Err("--flag_alias does not support flag value assignment.".into());
    }
    if !destination.starts_with("//") && !destination.starts_with('@') {
        return Err("--flag_alias only supports Starlark build settings.".into());
    }
    Ok((name.to_owned(), labels(destination)?))
}

pub fn definition_value(unit: &bazelrc::Unit) -> Option<&str> {
    unit.option.text.strip_prefix("--flag_alias=").or_else(|| {
        (unit.option.text == "--flag_alias")
            .then(|| unit.value.as_ref().map(|a| a.text.as_str()))
            .flatten()
    })
}

/// Swap only an exact long option. --noALIAS is *not* --ALIAS=false;
/// native --nofoo and abbreviations continue to select the native option.
pub fn rewrite(arg: &str, aliases: &Aliases) -> Option<String> {
    let rest = arg.strip_prefix("--")?;
    let (name, suffix) = rest
        .find('=')
        .map_or((rest, ""), |i| (&rest[..i], &rest[i..]));
    let label = aliases.get(name)?;
    Some(format!(
        "--@@{}//{}:{}{suffix}",
        label.repository, label.package, label.name
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_module_labels_keep_declaration_ownership() {
        for raw in [":x", "//:x", "@own//:x", "@@own//:x", "@//:x"] {
            assert_eq!(
                normalize_module_label(raw, "own", true).unwrap(),
                "@own//:x"
            );
        }
        assert_eq!(
            normalize_module_label("@//:x", "own", false).unwrap(),
            "@//:x"
        );
        assert_eq!(
            normalize_module_label("@dep//pkg", "own", false).unwrap(),
            "@dep//pkg:pkg"
        );
    }

    #[test]
    fn definitions_and_exact_long_option_substitution() {
        let labels = configuration::parse_canonical_option_label;
        let (name, label) = definition("foo=//:flag", &labels).unwrap();
        let aliases = Aliases::from([(name, label)]);
        assert_eq!(
            rewrite("--foo=hello=world", &aliases).as_deref(),
            Some("--@@//:flag=hello=world")
        );
        assert_eq!(rewrite("--foo", &aliases).as_deref(), Some("--@@//:flag"));
        assert_eq!(rewrite("--nofoo", &aliases), None);
        assert_eq!(rewrite("-f", &aliases), None);
        for value in [
            "foo",
            "=//:flag",
            "bad-name=//:flag",
            "x=cpu",
            "x=//:flag=1",
            "jobs=//:flag",
            "config=//:flag",
        ] {
            assert!(definition(value, &labels).is_err(), "{value}");
        }
    }

    #[test]
    fn only_configuration_names_with_valid_first_pass_values_are_deferred() {
        for arg in [
            "--disk_cache=cache",
            "--compile_one_dependency",
            "-x",
            "--unknown=value",
        ] {
            assert!(!defer_unmodelled_option(arg).unwrap(), "{arg}");
        }
        for arg in [
            "--build_python_zip",
            "--build_python_zip=true",
            "--build_python_zip=auto",
        ] {
            assert!(defer_unmodelled_option(arg).unwrap(), "{arg}");
        }
        assert!(
            defer_unmodelled_option("--build_python_zip=invalid")
                .unwrap_err()
                .contains("not a boolean")
        );
        for name in ["compile_one_dependency", "disk_cache"] {
            assert!(
                definition(
                    &format!("{name}=//:flag"),
                    &configuration::parse_canonical_option_label
                )
                .unwrap_err()
                .contains("non-configuration")
            );
        }
    }

    #[test]
    fn metadata_changes_configuration_but_resets_on_exec() {
        let plain = configuration::Configuration::default();
        let mut native = plain.native.clone();
        native
            .flag_aliases
            .insert("foo".into(), Label::parse_canonical("//:a").unwrap());
        let first = configuration::Configuration::new(native.clone(), BTreeMap::new());
        native
            .flag_aliases
            .insert("foo".into(), Label::parse_canonical("//:b").unwrap());
        let second = configuration::Configuration::new(native, BTreeMap::new());
        assert_ne!(first.id, plain.id);
        assert_ne!(first.id, second.id);
        let exec = crate::config_registry::exec_transition(
            &first,
            first.native.host_platform.clone(),
            &Default::default(),
        );
        assert!(exec.native.flag_aliases.is_empty());
    }
}
