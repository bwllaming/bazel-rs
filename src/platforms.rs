//! Native platform providers and platform construction: `constraint_setting`,
//! `constraint_value` and `platform` analysis (rules/platform/*.java) with
//! ConstraintSettingInfo, ConstraintValueInfo, ConstraintCollection and
//! PlatformInfo (analysis/platform/*.java), and the constraint matching of
//! `select()` keys and `config_setting(constraint_values = ...)`.
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::configured_attributes::{ConfigMatching, Diff, MatchResult};
use crate::label::Label;

fn shown(label: &Label) -> String {
    crate::rule_declarations::canonical_form(label)
}

/// ConstraintSettingInfo (analysis/platform/ConstraintSettingInfo.java):
/// equality and hashing use the label only (:101-112).
#[derive(Clone, Debug)]
pub struct ConstraintSettingInfo {
    pub label: Label,
    /// `default_constraint_value` (a NODEP label in the setting's package).
    pub default_value: Option<Label>,
    /// `refines_constraint_value`, resolved through aliases.
    pub refined: Option<Box<ConstraintValueInfo>>,
}

impl PartialEq for ConstraintSettingInfo {
    fn eq(&self, other: &Self) -> bool {
        self.label == other.label
    }
}

impl Eq for ConstraintSettingInfo {}

impl ConstraintSettingInfo {
    /// ConstraintSettingInfo.defaultConstraintValue (:68-73): a value of this
    /// setting with the default's label.
    pub fn default_constraint_value(&self) -> Option<ConstraintValueInfo> {
        self.default_value
            .as_ref()
            .map(|label| ConstraintValueInfo {
                setting: Arc::new(self.clone()),
                label: label.clone(),
            })
    }

    /// ConstraintSettingInfo.refinementChain (:85-93): the labels of the
    /// refined values, transitively.
    pub fn refinement_chain(&self) -> Vec<Label> {
        let mut chain = Vec::new();
        let mut current = self.refined.as_deref();
        while let Some(value) = current {
            if chain.contains(&value.label) {
                break;
            }
            chain.push(value.label.clone());
            current = value.setting.refined.as_deref();
        }
        chain
    }
}

/// ConstraintValueInfo (analysis/platform/ConstraintValueInfo.java): equal
/// when both the setting (by label) and the label are equal (:113-118).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstraintValueInfo {
    pub setting: Arc<ConstraintSettingInfo>,
    pub label: Label,
}

impl ConstraintValueInfo {
    /// ConstraintValueInfo.configMatchingProvider (:76-94): a `constraint_value`
    /// used directly as a `select()` key matches when the target platform has
    /// this value for its setting; it specifies no settings, flags or
    /// constraint values (so it refines nothing).
    pub fn config_matching(&self, platform: &PlatformInfo) -> ConfigMatching {
        let platform_value = platform.constraints.get(&self.setting);
        let result = if platform_value.as_ref() == Some(self) {
            MatchResult::Match
        } else {
            MatchResult::NoMatch(vec![Diff {
                what: self.setting.label.clone(),
                got: platform_value.map_or_else(|| "<unset>".to_owned(), |value| value.label.name),
                want: self.label.name.clone(),
            }])
        };
        ConfigMatching {
            label: self.label.clone(),
            settings: Vec::new(),
            flags: BTreeMap::new(),
            constraints: Default::default(),
            result,
        }
    }
}

/// ConstraintCollection (analysis/platform/ConstraintCollection.java).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConstraintCollection {
    pub parent: Option<Arc<ConstraintCollection>>,
    /// `constraints()`: one value per setting, the first one winning
    /// (toImmutableMap with `(first, second) -> first`, :83-97), in order.
    pub values: Vec<ConstraintValueInfo>,
}

impl ConstraintCollection {
    /// ConstraintCollection.Builder.build (:83-97) after validateConstraints.
    pub fn build(
        parent: Option<Arc<ConstraintCollection>>,
        values: &[ConstraintValueInfo],
    ) -> Result<Self, String> {
        validate_constraints(values)?;
        let mut kept: Vec<ConstraintValueInfo> = Vec::new();
        for value in values {
            if !kept.iter().any(|seen| seen.setting == value.setting) {
                kept.push(value.clone());
            }
        }
        Ok(Self {
            parent,
            values: kept,
        })
    }

    /// ConstraintCollection.get (:208-217): own value, else the parent's, else
    /// the setting's default value.
    pub fn get(&self, setting: &ConstraintSettingInfo) -> Option<ConstraintValueInfo> {
        if let Some(value) = self.values.iter().find(|value| *value.setting == *setting) {
            return Some(value.clone());
        }
        match &self.parent {
            Some(parent) => parent.get(setting),
            None => setting.default_constraint_value(),
        }
    }

    /// ConstraintCollection.has (:168-177).
    pub fn has(&self, setting: &ConstraintSettingInfo) -> bool {
        self.values.iter().any(|value| *value.setting == *setting)
            || match &self.parent {
                Some(parent) => parent.has(setting),
                None => setting.default_value.is_some(),
            }
    }

    /// ConstraintCollection.containsAll / findMissing (:125-145).
    pub fn contains_all(&self, expected: &[&ConstraintValueInfo]) -> bool {
        expected
            .iter()
            .all(|value| self.get(&value.setting).as_ref() == Some(*value))
    }

    /// ConstraintCollection.allConstraintSettings (:232-238): the settings
    /// set here or by a parent, nearest first, without duplicates.
    fn all_settings(&self) -> Vec<Arc<ConstraintSettingInfo>> {
        let mut settings: Vec<Arc<ConstraintSettingInfo>> = Vec::new();
        let mut current = Some(self);
        while let Some(collection) = current {
            for value in &collection.values {
                if !settings.iter().any(|seen| **seen == *value.setting) {
                    settings.push(value.setting.clone());
                }
            }
            current = collection.parent.as_deref();
        }
        settings
    }

    /// ConstraintCollection.diff (:151-155): the settings (of either
    /// collection) whose values differ, defaults included.
    pub fn diff(&self, other: &ConstraintCollection) -> Vec<Arc<ConstraintSettingInfo>> {
        let mut settings = self.all_settings();
        for setting in other.all_settings() {
            if !settings.iter().any(|seen| **seen == *setting) {
                settings.push(setting);
            }
        }
        settings.retain(|setting| self.get(setting) != other.get(setting));
        settings
    }

    /// ConstraintCollection.hasWithoutDefault (:182-194): set here or by a
    /// parent, ignoring the setting's default value.
    pub fn has_without_default(&self, setting: &ConstraintSettingInfo) -> bool {
        self.values.iter().any(|value| *value.setting == *setting)
            || self
                .parent
                .as_ref()
                .is_some_and(|parent| parent.has_without_default(setting))
    }
}

/// ConstraintCollection.validateConstraints (:305-325) with
/// DuplicateConstraintException.formatError (:346-364): settings with more
/// than one distinct value, in first-seen order.
pub fn validate_constraints(values: &[ConstraintValueInfo]) -> Result<(), String> {
    let mut grouped: Vec<(&ConstraintSettingInfo, Vec<&ConstraintValueInfo>)> = Vec::new();
    for value in values {
        match grouped
            .iter_mut()
            .find(|(setting, _)| **setting == *value.setting)
        {
            Some((_, seen)) => {
                if !seen.contains(&value) {
                    seen.push(value);
                }
            }
            None => grouped.push((&value.setting, vec![value])),
        }
    }
    let duplicates = grouped
        .iter()
        .filter(|(_, values)| values.len() > 1)
        .map(|(setting, values)| {
            format!(
                "constraint_setting {} has [{}]",
                shown(&setting.label),
                values
                    .iter()
                    .map(|value| shown(&value.label))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect::<Vec<_>>();
    if duplicates.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Duplicate constraint values detected: {}",
            duplicates.join(", ")
        ))
    }
}

/// PlatformInfo (analysis/platform/PlatformInfo.java).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlatformInfo {
    pub label: Label,
    pub constraints: Arc<ConstraintCollection>,
    pub remote_execution_properties: String,
    /// PlatformProperties (analysis/platform/PlatformProperties.java), in
    /// insertion order.
    pub exec_properties: Vec<(String, String)>,
    /// Inherited parent flags followed by this platform's flags.
    pub flags: Vec<String>,
    /// `required_settings` (the config_setting labels; their matching is
    /// only used by execution-platform toolchain resolution, N17).
    pub required_settings: Vec<Label>,
    pub check_toolchain_types: bool,
    pub allowed_toolchain_types: Vec<Label>,
    pub missing_toolchain_error: Option<String>,
}

/// PlatformInfo.PARENT_REMOTE_EXECUTION_KEY (analysis/platform/PlatformInfo.java).
const PARENT_REMOTE_EXECUTION_KEY: &str = "{PARENT_REMOTE_EXECUTION_PROPERTIES}";

/// PlatformRule.DEFAULT_MISSING_TOOLCHAIN_ERROR (rules/platform/PlatformRule.java:36-38).
pub const DEFAULT_MISSING_TOOLCHAIN_ERROR: &str = "For more information on platforms or toolchains see https://bazel.build/concepts/platforms-intro.";

/// The values Platform.create (rules/platform/Platform.java:44-120) reads.
#[derive(Clone, Debug, Default)]
pub struct PlatformAttributes {
    pub parents: Vec<Arc<PlatformInfo>>,
    pub constraint_values: Vec<ConstraintValueInfo>,
    pub remote_execution_properties: String,
    pub exec_properties: Vec<(String, String)>,
    pub flags: Vec<String>,
    pub required_settings: Vec<Label>,
    pub check_toolchain_types: bool,
    pub allowed_toolchain_types: Vec<Label>,
    pub missing_toolchain_error: String,
}

/// An attribute error of a rule (`in ATTR attribute of KIND rule L: MESSAGE`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributeError {
    pub attribute: &'static str,
    pub message: String,
}

/// Platform.create (rules/platform/Platform.java:44-120) and
/// PlatformInfo.Builder.build (analysis/platform/PlatformInfo.java:407-437).
/// `display` is Label.getShorthandDisplayForm with the main repository
/// mapping (for the refinement errors, :130-183).
pub fn create_platform(
    label: &Label,
    attributes: PlatformAttributes,
    display: &dyn Fn(&Label) -> String,
) -> Result<PlatformInfo, Vec<AttributeError>> {
    let error = |attribute, message: String| vec![AttributeError { attribute, message }];
    if attributes.parents.len() > 1 {
        return Err(error(
            "parents",
            "parents attribute must have a single value".to_owned(),
        ));
    }
    let parent = attributes.parents.first().cloned();
    // checkRemoteExecutionProperties (PlatformInfo.java:372-399).
    let remote = &attributes.remote_execution_properties;
    if !attributes.exec_properties.is_empty() && !remote.is_empty() {
        return Err(error(
            "exec_properties",
            "Platform contains both remote_execution_properties and exec_properties. Prefer exec_properties over the deprecated remote_execution_properties.".to_owned(),
        ));
    }
    if let Some(parent) = &parent {
        if !attributes.exec_properties.is_empty() && !parent.remote_execution_properties.is_empty()
        {
            return Err(error(
                "exec_properties",
                format!(
                    "Platform specifies exec_properties but its parent {} specifies remote_execution_properties. Prefer exec_properties over the deprecated remote_execution_properties.",
                    shown(&parent.label)
                ),
            ));
        }
        if !remote.is_empty() && !parent.exec_properties.is_empty() {
            return Err(error(
                "exec_properties",
                "Platform specifies remote_execution_properties but its parent specifies exec_properties. Prefer exec_properties over the deprecated remote_execution_properties.".to_owned(),
            ));
        }
    }
    // mergeRemoteExecutionProperties (:439-452): the attribute value is never
    // null (its default is ""), so the parent's value only survives through
    // the {PARENT_REMOTE_EXECUTION_PROPERTIES} placeholder.
    let parent_remote = parent
        .as_ref()
        .map_or("", |parent| parent.remote_execution_properties.as_str());
    let remote_execution_properties = remote.replace(PARENT_REMOTE_EXECUTION_KEY, parent_remote);
    let mut flags = parent
        .as_ref()
        .map_or_else(Vec::new, |parent| parent.flags.clone());
    flags.extend(attributes.flags);
    let constraints = ConstraintCollection::build(
        parent.as_ref().map(|parent| parent.constraints.clone()),
        &attributes.constraint_values,
    )
    .map_err(|message| error("constraint_values", message))?;
    let platform = PlatformInfo {
        label: label.clone(),
        constraints: Arc::new(constraints),
        remote_execution_properties,
        exec_properties: merge_exec_properties(
            parent
                .as_ref()
                .map(|parent| parent.exec_properties.as_slice()),
            attributes.exec_properties,
        ),
        flags,
        required_settings: attributes.required_settings,
        check_toolchain_types: attributes.check_toolchain_types,
        allowed_toolchain_types: if attributes.check_toolchain_types {
            attributes.allowed_toolchain_types
        } else {
            Vec::new()
        },
        // setMissingToolchainErrorMessage (:363-370): empty means none.
        missing_toolchain_error: (!attributes.missing_toolchain_error.is_empty())
            .then_some(attributes.missing_toolchain_error),
    };
    let errors =
        refined_constraint_errors(label, &attributes.constraint_values, &platform, display);
    if errors.is_empty() {
        Ok(platform)
    } else {
        Err(errors)
    }
}

/// PlatformProperties.Builder.build / mergeParent
/// (analysis/platform/PlatformProperties.java:61-90): without a (non-empty)
/// parent the properties are kept as given; otherwise the parent's entries
/// are overridden, and an empty value removes the key.
fn merge_exec_properties(
    parent: Option<&[(String, String)]>,
    properties: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let Some(parent) = parent.filter(|parent| !parent.is_empty()) else {
        return properties;
    };
    let mut merged = parent.to_vec();
    for (key, value) in properties {
        if value.is_empty() {
            merged.retain(|(seen, _)| *seen != key);
        } else {
            match merged.iter_mut().find(|(seen, _)| *seen == key) {
                Some(entry) => entry.1 = value,
                None => merged.push((key, value)),
            }
        }
    }
    merged
}

/// Platform.validateRefinedConstraintValues (rules/platform/Platform.java:130-183).
fn refined_constraint_errors(
    label: &Label,
    direct: &[ConstraintValueInfo],
    platform: &PlatformInfo,
    display: &dyn Fn(&Label) -> String,
) -> Vec<AttributeError> {
    let mut errors = Vec::new();
    for value in direct {
        let setting = &value.setting;
        if setting.default_constraint_value().as_ref() == Some(value) {
            continue;
        }
        let Some(refined) = setting.refined.as_deref() else {
            continue;
        };
        let actual = platform.constraints.get(&refined.setting);
        if actual.as_ref() == Some(refined) {
            continue;
        }
        let message = match actual {
            None => format!(
                "constraint_value {} refines {}, but platform {} does not set the latter. Fix with:\n  buildozer 'add constraint_values {}' {}",
                display(&value.label),
                display(&refined.label),
                display(label),
                display(&refined.label),
                display(label)
            ),
            Some(actual) => format!(
                "constraint_value {} refines {}, but platform {} sets the conflicting constraint_value {} for {}",
                display(&value.label),
                display(&refined.label),
                display(label),
                display(&actual.label),
                display(&refined.setting.label)
            ),
        };
        errors.push(AttributeError {
            attribute: "constraint_values",
            message,
        });
    }
    errors
}

/// Label.getShorthandDisplayForm (cmdline/Label.java:527-539) with the main
/// repository mapping (`mapping`: apparent name -> canonical name):
/// RepositoryName.getDisplayForm is empty for the main repository,
/// `@apparent` when the main repository sees it, `@@canonical` otherwise;
/// `//pkg:pkg` shortens to `//pkg` and `@r//:r` to `@r`.
pub fn shorthand_display_form(label: &Label, mapping: &BTreeMap<String, String>) -> String {
    let repository = if label.repository.is_empty() {
        String::new()
    } else {
        match mapping
            .iter()
            .find(|(apparent, canonical)| !apparent.is_empty() && **canonical == label.repository)
        {
            Some((apparent, _)) => format!("@{apparent}"),
            None => format!("@@{}", label.repository),
        }
    };
    let base = label.package.rsplit('/').next().unwrap_or_default();
    if !label.package.is_empty() && base == label.name {
        return format!("{repository}//{}", label.package);
    }
    if label.package.is_empty()
        && (repository == format!("@{}", label.name) || repository == format!("@@{}", label.name))
    {
        return repository;
    }
    format!("{repository}//{}:{}", label.package, label.name)
}

/// The target platform a rule implementation sees.
#[derive(Clone, Debug, Default)]
pub enum TargetPlatform {
    /// The rule does not use toolchain resolution (no toolchain context:
    /// RuleContext.targetPlatformHasConstraint returns false,
    /// analysis/RuleContext.java:1144-1150).
    #[default]
    Absent,
    /// Not resolved yet: crate::engine resolves it when the implementation
    /// asks ([`NEEDS_TARGET_PLATFORM`]) and analyzes the rule again.
    Deferred,
    Resolved(Arc<PlatformInfo>),
}

/// The analysis error of a rule implementation that needs a deferred target
/// platform (crate::analysis_ctx).
pub const NEEDS_TARGET_PLATFORM: &str = "\u{0}rule implementation needs the target platform";

/// InvalidPlatformException's default text
/// (skyframe/toolchains/PlatformLookupUtil.java:175-205).
pub fn not_a_platform(label: &Label) -> String {
    format!(
        "Target {} was referenced as a platform, but does not provide PlatformInfo",
        shown(label)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(text: &str) -> Label {
        Label::parse_canonical(text).unwrap()
    }

    fn setting(text: &str, default: Option<&str>) -> Arc<ConstraintSettingInfo> {
        Arc::new(ConstraintSettingInfo {
            label: label(text),
            default_value: default.map(label),
            refined: None,
        })
    }

    fn value(setting: &Arc<ConstraintSettingInfo>, text: &str) -> ConstraintValueInfo {
        ConstraintValueInfo {
            setting: setting.clone(),
            label: label(text),
        }
    }

    fn platform(
        name: &str,
        parent: Option<Arc<PlatformInfo>>,
        values: Vec<ConstraintValueInfo>,
    ) -> Result<PlatformInfo, Vec<AttributeError>> {
        create_platform(
            &label(name),
            PlatformAttributes {
                parents: parent.into_iter().collect(),
                constraint_values: values,
                ..Default::default()
            },
            &|label| shorthand_display_form(label, &BTreeMap::new()),
        )
    }

    #[test]
    fn inheritance_overrides_and_defaults_follow_constraint_collection() {
        let os = setting("//os:os", None);
        let cpu = setting("//cpu:cpu", None);
        let libc = setting("//libc:libc", Some("//libc:glibc"));
        let parent = Arc::new(
            platform(
                "//p:parent",
                None,
                vec![value(&os, "//os:linux"), value(&cpu, "//cpu:x86_64")],
            )
            .unwrap(),
        );
        let child = platform("//p:child", Some(parent), vec![value(&cpu, "//cpu:arm64")]).unwrap();
        assert_eq!(
            child.constraints.get(&os).unwrap().label,
            label("//os:linux")
        );
        assert_eq!(
            child.constraints.get(&cpu).unwrap().label,
            label("//cpu:arm64")
        );
        // A setting with a default value is always "set" (ConstraintCollection.has).
        assert_eq!(
            child.constraints.get(&libc).unwrap().label,
            label("//libc:glibc")
        );
        assert!(child.constraints.has(&libc));
        let other = setting("//x:x", None);
        assert!(child.constraints.get(&other).is_none() && !child.constraints.has(&other));
        assert!(
            child
                .constraints
                .contains_all(&[&value(&os, "//os:linux"), &value(&cpu, "//cpu:arm64")])
        );
        assert!(
            !child
                .constraints
                .contains_all(&[&value(&cpu, "//cpu:x86_64")])
        );
        // Equal settings compare by label only.
        let same = Arc::new(ConstraintSettingInfo {
            default_value: Some(label("//os:linux")),
            ..(*os).clone()
        });
        assert_eq!(value(&same, "//os:linux"), value(&os, "//os:linux"));
    }

    #[test]
    fn duplicates_parents_and_exec_properties_are_bazel_errors() {
        let os = setting("//os:os", None);
        let cpu = setting("//cpu:cpu", None);
        assert_eq!(
            platform(
                "//p:p",
                None,
                vec![
                    value(&os, "//os:linux"),
                    value(&cpu, "//cpu:x86_64"),
                    value(&os, "//os:linux"),
                    value(&os, "@@r+//os:osx"),
                    value(&cpu, "//cpu:arm64"),
                ]
            )
            .unwrap_err(),
            vec![AttributeError {
                attribute: "constraint_values",
                message: "Duplicate constraint values detected: constraint_setting //os:os has [//os:linux, @@r+//os:osx], constraint_setting //cpu:cpu has [//cpu:x86_64, //cpu:arm64]".into()
            }]
        );
        // The same value twice is not a duplicate; the first value wins.
        let ok = platform(
            "//p:p",
            None,
            vec![value(&os, "//os:linux"), value(&os, "//os:linux")],
        )
        .unwrap();
        assert_eq!(ok.constraints.values.len(), 1);
        let parent = Arc::new(platform("//p:a", None, vec![]).unwrap());
        assert_eq!(
            create_platform(
                &label("//p:p"),
                PlatformAttributes {
                    parents: vec![parent.clone(), parent.clone()],
                    ..Default::default()
                },
                &|label| shown(label),
            )
            .unwrap_err()[0]
                .message,
            "parents attribute must have a single value"
        );
        let with = |parent: Option<Arc<PlatformInfo>>, remote: &str, exec: &[(&str, &str)]| {
            create_platform(
                &label("//p:c"),
                PlatformAttributes {
                    parents: parent.into_iter().collect(),
                    remote_execution_properties: remote.into(),
                    exec_properties: exec
                        .iter()
                        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                        .collect(),
                    ..Default::default()
                },
                &|label| shown(label),
            )
        };
        assert_eq!(
            with(None, "r", &[("a", "1")]).unwrap_err()[0].message,
            "Platform contains both remote_execution_properties and exec_properties. Prefer exec_properties over the deprecated remote_execution_properties."
        );
        let exec_parent = Arc::new(with(None, "", &[("a", "1"), ("b", "2"), ("c", "")]).unwrap());
        assert_eq!(
            exec_parent.exec_properties,
            [
                ("a".into(), "1".into()),
                ("b".into(), "2".into()),
                ("c".into(), String::new())
            ]
        );
        let child = with(
            Some(exec_parent.clone()),
            "",
            &[("b", ""), ("d", "4"), ("a", "9")],
        )
        .unwrap();
        assert_eq!(
            child.exec_properties,
            [
                ("a".to_owned(), "9".to_owned()),
                ("c".into(), String::new()),
                ("d".into(), "4".into())
            ]
        );
        assert_eq!(
            with(Some(exec_parent), "x", &[]).unwrap_err()[0].message,
            "Platform specifies remote_execution_properties but its parent specifies exec_properties. Prefer exec_properties over the deprecated remote_execution_properties."
        );
        let remote_parent = Arc::new(with(None, "base", &[]).unwrap());
        assert_eq!(
            with(Some(remote_parent.clone()), "", &[("a", "1")]).unwrap_err()[0].message,
            "Platform specifies exec_properties but its parent //p:c specifies remote_execution_properties. Prefer exec_properties over the deprecated remote_execution_properties."
        );
        assert_eq!(
            with(
                Some(remote_parent.clone()),
                "{PARENT_REMOTE_EXECUTION_PROPERTIES} more",
                &[]
            )
            .unwrap()
            .remote_execution_properties,
            "base more"
        );
        assert_eq!(
            with(Some(remote_parent), "", &[])
                .unwrap()
                .remote_execution_properties,
            ""
        );
    }

    #[test]
    fn refinement_and_direct_key_matching() {
        let os = setting("//os:os", None);
        let linux = value(&os, "//os:linux");
        let distro = Arc::new(ConstraintSettingInfo {
            label: label("//os:distro"),
            default_value: Some(label("//os:unknown")),
            refined: Some(Box::new(linux.clone())),
        });
        assert_eq!(distro.refinement_chain(), [label("//os:linux")]);
        let errors = platform("//p:p", None, vec![value(&distro, "//os:debian")]).unwrap_err();
        assert_eq!(
            errors[0].message,
            "constraint_value //os:debian refines //os:linux, but platform //p does not set the latter. Fix with:\n  buildozer 'add constraint_values //os:linux' //p"
        );
        let errors = platform(
            "//p:p",
            None,
            vec![value(&distro, "//os:debian"), value(&os, "//os:osx")],
        )
        .unwrap_err();
        assert_eq!(
            errors[0].message,
            "constraint_value //os:debian refines //os:linux, but platform //p sets the conflicting constraint_value //os:osx for //os"
        );
        // The default value carries no refinement implication.
        assert!(platform("//p:p", None, vec![value(&distro, "//os:unknown")]).is_ok());
        let host = platform("//p:host", None, vec![linux.clone()]).unwrap();
        assert_eq!(linux.config_matching(&host).result, MatchResult::Match);
        assert_eq!(
            value(&os, "//os:osx").config_matching(&host).result,
            MatchResult::NoMatch(vec![Diff {
                what: label("//os:os"),
                got: "linux".into(),
                want: "osx".into()
            }])
        );
        let empty = platform("//p:empty", None, vec![]).unwrap();
        assert_eq!(
            linux.config_matching(&empty).result,
            MatchResult::NoMatch(vec![Diff {
                what: label("//os:os"),
                got: "<unset>".into(),
                want: "linux".into()
            }])
        );
        assert!(linux.config_matching(&host).constraints.is_empty());
    }

    #[test]
    fn shorthand_display_forms_follow_label() {
        let mapping = BTreeMap::from([
            (String::new(), String::new()),
            ("platforms".to_owned(), "platforms+".to_owned()),
        ]);
        for (text, expected) in [
            ("//p:p", "//p"),
            ("//a/b:b", "//a/b"),
            ("//p:x", "//p:x"),
            ("//:x", "//:x"),
            ("@@platforms+//os:linux", "@platforms//os:linux"),
            ("@@platforms+//os:os", "@platforms//os"),
            ("@@other+//:other+", "@@other+"),
            ("@@other+//:x", "@@other+//:x"),
        ] {
            assert_eq!(shorthand_display_form(&label(text), &mapping), expected);
        }
        assert_eq!(
            not_a_platform(&label("//:fg")),
            "Target //:fg was referenced as a platform, but does not provide PlatformInfo"
        );
    }
}
