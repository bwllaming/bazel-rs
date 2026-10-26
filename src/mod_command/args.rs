//! ModuleArg and ExtensionArg (bzlmod/modcommand/ModuleArg.java,
//! ExtensionArg.java): command-line references to modules, repositories
//! and module extensions.
use super::{Augmented, Code, ExtensionId, Failure, Inspector, ModuleKey};
use std::collections::BTreeMap;
use std::fmt;

/// A reference to one or more modules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModuleArg {
    /// `<module>@<version>` (`_` the empty version) or `<root>`.
    SpecificVersion { name: String, version: String },
    /// `<module>`.
    AllVersions(String),
    /// `@<apparent repo name>`.
    Apparent(String),
    /// `@@<canonical repo name>`.
    Canonical(String),
}

fn invalid(message: String) -> Failure {
    Failure {
        message,
        code: Code::InvalidArguments,
    }
}

impl ModuleArg {
    pub fn root() -> Self {
        Self::SpecificVersion {
            name: String::new(),
            version: String::new(),
        }
    }

    /// ModuleArgConverter.convert.
    pub fn parse(input: &str) -> Result<Self, String> {
        if input.eq_ignore_ascii_case("<root>") {
            return Ok(Self::root());
        }
        if let Some(name) = input.strip_prefix("@@") {
            crate::label::validate_repository_name(name)
                .map_err(|error| format!("invalid argument '{input}': {error}"))?;
            return Ok(Self::Canonical(name.to_owned()));
        }
        if let Some(name) = input.strip_prefix('@') {
            let valid = name
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric())
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
            if !valid {
                return Err(format!(
                    "invalid argument '{input}': invalid user-provided repo name '{name}': valid names may contain only A-Z, a-z, 0-9, '-', '_', '.', and must start with a letter or a number"
                ));
            }
            return Ok(Self::Apparent(name.to_owned()));
        }
        if let Some((name, version)) = input.split_once('@') {
            if version.is_empty() {
                return Err(format!(
                    "invalid argument '{input}': use _ for the empty version"
                ));
            }
            let version = if version == "_" {
                String::new()
            } else {
                crate::module_graph::parse_version(version).map_err(|_| {
                    format!(
                        "invalid argument '{input}': bad version (does not match regex): {version}"
                    )
                })?
            };
            return Ok(Self::SpecificVersion {
                name: name.to_owned(),
                version,
            });
        }
        Ok(Self::AllVersions(input.to_owned()))
    }

    fn specific_key(&self) -> Option<ModuleKey> {
        match self {
            Self::SpecificVersion { name, version } => Some(ModuleKey {
                name: name.clone(),
                version: version.clone(),
            }),
            _ => None,
        }
    }

    /// The keys of `name` that exist (when unused ones are excluded, the used ones).
    fn existing(inspector: &Inspector, name: &str, include_unused: bool) -> Option<Vec<ModuleKey>> {
        inspector.index.get(name).map(|keys| {
            keys.iter()
                .filter(|key| include_unused || inspector.module(key).is_used())
                .cloned()
                .collect()
        })
    }

    fn list(keys: &[ModuleKey]) -> String {
        let shown: Vec<String> = keys.iter().map(ToString::to_string).collect();
        format!("[{}]", shown.join(", "))
    }

    /// SpecificVersionOfModule.throwIfNonexistent.
    fn check_specific(
        inspector: &Inspector,
        key: &ModuleKey,
        include_unused: bool,
        warn_unused: bool,
    ) -> Result<(), Failure> {
        let module = inspector.graph.get(key);
        if let Some(module) = module
            && !include_unused
            && warn_unused
            && !module.is_used()
        {
            return Err(invalid(format!(
                "Module version {key} is unused as a result of module resolution. Use the --include_unused flag to include it."
            )));
        }
        if module.is_none_or(|module| !include_unused && !module.is_used()) {
            let Some(existing) = Self::existing(inspector, &key.name, include_unused) else {
                return Err(invalid(format!(
                    "Module {} does not exist in the dependency graph.",
                    key.name
                )));
            };
            return Err(invalid(format!(
                "Module version {key} does not exist, available versions: {}.",
                Self::list(&existing)
            )));
        }
        Ok(())
    }

    /// AllVersionsOfModule.resolveInternal.
    fn all_versions(
        inspector: &Inspector,
        name: &str,
        include_unused: bool,
        warn_unused: bool,
    ) -> Result<Vec<ModuleKey>, Failure> {
        let Some(keys) = Self::existing(inspector, name, include_unused) else {
            return Err(invalid(format!(
                "Module {name} does not exist in the dependency graph."
            )));
        };
        if keys.is_empty() {
            if warn_unused {
                return Err(invalid(format!(
                    "Module {name} is unused as a result of module resolution. Use the --include_unused flag to include it."
                )));
            }
            return Err(invalid(format!(
                "Module {name} does not exist in the dependency graph."
            )));
        }
        Ok(keys)
    }

    /// resolveToModuleKeys: `base` is the base module (its deps and unused
    /// deps resolve apparent names).
    pub(crate) fn resolve_to_module_keys(
        &self,
        inspector: &Inspector,
        base: &Augmented,
        include_unused: bool,
        warn_unused: bool,
    ) -> Result<Vec<ModuleKey>, Failure> {
        match self {
            Self::SpecificVersion { .. } => {
                let key = self.specific_key().expect("specific");
                Self::check_specific(inspector, &key, include_unused, warn_unused)?;
                Ok(vec![key])
            }
            Self::AllVersions(name) => {
                Self::all_versions(inspector, name, include_unused, warn_unused)
            }
            Self::Apparent(name) => {
                let mut keys = Vec::new();
                if let Some(dep) = base.dep(name) {
                    keys.push(dep.clone());
                }
                if include_unused
                    && let Some(dep) = base.unused_dep(name)
                    && !keys.contains(dep)
                {
                    keys.push(dep.clone());
                }
                if keys.is_empty() {
                    return Err(invalid(format!(
                        "No module with the apparent repo name @{name} exists in the dependency graph"
                    )));
                }
                Ok(keys)
            }
            Self::Canonical(name) => {
                let mut found = inspector
                    .graph
                    .values()
                    .filter(|module| inspector.canonical.get(&module.key) == Some(name))
                    .map(|module| module.key.clone())
                    .collect::<Vec<_>>();
                found.sort();
                let module = found.first().map(|key| inspector.module(key));
                if let Some(module) = module
                    && !include_unused
                    && warn_unused
                    && !module.is_used()
                {
                    return Err(invalid(format!(
                        "Module version {} is unused as a result of module resolution. Use the --include_unused flag to include it.",
                        module.key
                    )));
                }
                match module {
                    Some(module) if include_unused || module.is_used() => {
                        Ok(vec![module.key.clone()])
                    }
                    _ => Err(invalid(format!(
                        "No module with the canonical repo name @@{name} exists in the dependency graph"
                    ))),
                }
            }
        }
    }

    /// resolveToRepoNames: display key -> canonical repository name, in
    /// `mapping` (the base module's full repository mapping).
    pub(crate) fn resolve_to_repo_names(
        &self,
        inspector: &Inspector,
        mapping: &BTreeMap<String, String>,
        context: &str,
    ) -> Result<Vec<(String, String)>, Failure> {
        let canonical = |key: &ModuleKey| inspector.canonical.get(key).cloned().unwrap_or_default();
        match self {
            Self::SpecificVersion { .. } => {
                let key = self.specific_key().expect("specific");
                Self::check_specific(inspector, &key, false, false)?;
                Ok(vec![(key.to_string(), canonical(&key))])
            }
            Self::AllVersions(name) => Ok(Self::all_versions(inspector, name, false, false)?
                .iter()
                .map(|key| (key.to_string(), canonical(key)))
                .collect()),
            Self::Apparent(name) => match mapping.get(name) {
                Some(repository) => Ok(vec![(self.to_string(), repository.clone())]),
                None => Err(invalid(format!(
                    "No repo visible as {name} from @{}",
                    if context.is_empty() {
                        "main repository".to_owned()
                    } else {
                        format!("repository '@@{context}'")
                    }
                ))),
            },
            Self::Canonical(name) => Ok(vec![(self.to_string(), name.clone())]),
        }
    }
}

impl fmt::Display for ModuleArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpecificVersion { .. } => self.specific_key().expect("specific").fmt(f),
            Self::AllVersions(name) => f.write_str(name),
            Self::Apparent(name) => write!(f, "@{name}"),
            Self::Canonical(name) => write!(f, "@@{name}"),
        }
    }
}

/// `<module><bzl_file_label>%<extension_name>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionArg {
    pub module: ModuleArg,
    pub label: String,
    pub name: String,
}

impl ExtensionArg {
    /// ExtensionArgConverter.convert.
    pub fn parse(input: &str) -> Result<Self, String> {
        let Some(slash) = input.find('/') else {
            return Err(format!("Invalid argument {input}: missing .bzl label"));
        };
        let percent = input.find('%');
        let Some(percent) = percent.filter(|percent| *percent >= slash) else {
            return Err(format!("Invalid argument {input}: missing extension name"));
        };
        Ok(Self {
            module: ModuleArg::parse(&input[..slash])?,
            label: input[slash..percent].to_owned(),
            name: input[percent + 1..].to_owned(),
        })
    }

    /// resolveToExtensionId.
    pub(crate) fn resolve(
        &self,
        inspector: &Inspector,
        base: &Augmented,
    ) -> Result<ExtensionId, Failure> {
        let keys = self
            .module
            .resolve_to_module_keys(inspector, base, false, false)?;
        let [key] = keys.as_slice() else {
            return Err(invalid(format!(
                "Module {}, as part of the extension specifier, should represent exactly one module version. Choose one of: {}.",
                self.module,
                ModuleArg::list(&keys)
            )));
        };
        let canonical = inspector.canonical.get(key).cloned().unwrap_or_default();
        // A repo-relative label: no repository mapping.
        let label = crate::label::Label::parse_in(&self.label, &canonical, "", &BTreeMap::new())
            .map_err(|error| invalid(format!("bad label format in {}: {error}", self.label)))?;
        Ok(ExtensionId {
            label,
            name: self.name.clone(),
            isolation: None,
        })
    }
}

impl fmt::Display for ExtensionArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}%{}", self.module, self.label, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_args_parse_like_the_converter() {
        assert_eq!(ModuleArg::parse("<ROOT>").unwrap(), ModuleArg::root());
        assert_eq!(
            ModuleArg::parse("a@1.0+meta").unwrap(),
            ModuleArg::SpecificVersion {
                name: "a".into(),
                version: "1.0".into()
            }
        );
        assert_eq!(
            ModuleArg::parse("a@_").unwrap().to_string(),
            "a@_".to_owned()
        );
        assert_eq!(
            ModuleArg::parse("a@").unwrap_err(),
            "invalid argument 'a@': use _ for the empty version"
        );
        assert!(ModuleArg::parse("@-x").is_err());
        assert_eq!(
            ModuleArg::parse("@@x+").unwrap(),
            ModuleArg::Canonical("x+".into())
        );
        assert_eq!(
            ModuleArg::parse("rules_go").unwrap(),
            ModuleArg::AllVersions("rules_go".into())
        );
    }

    #[test]
    fn extension_args_split_at_the_first_slash_and_percent() {
        let arg = ExtensionArg::parse("@rules_python//python/extensions:pip.bzl%pip").unwrap();
        assert_eq!(arg.module, ModuleArg::Apparent("rules_python".into()));
        assert_eq!(arg.label, "//python/extensions:pip.bzl");
        assert_eq!(arg.name, "pip");
        assert_eq!(
            arg.to_string(),
            "@rules_python//python/extensions:pip.bzl%pip"
        );
        let innate = ExtensionArg::parse("<root>//:MODULE.bazel%//:defs.bzl r").unwrap();
        assert_eq!(innate.name, "//:defs.bzl r");
        assert!(ExtensionArg::parse("ext.bzl%x").is_err());
        assert!(ExtensionArg::parse("a%b//c").is_err());
    }
}
