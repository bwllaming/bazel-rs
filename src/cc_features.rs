//! The C++ toolchain feature engine (Bazel 9.3 `rules/cpp`):
//!
//! - parsing a Starlark `CcToolchainConfigInfo` (rules_cc
//!   `create_cc_toolchain_config_info`) into features, action configs, tools
//!   and artifact name patterns: `CcToolchainFeaturesLib.java:55-553` and
//!   `CcToolchainConfigInfo.java:56-92`;
//! - the `CcToolchainFeatures` graph, its validation and per-request cache:
//!   `CcToolchainFeatures.java:1099-1328`;
//! - feature selection: `FeatureSelection.java:112-300`;
//! - `FeatureConfiguration` (`CcToolchainFeatures.java:817-1032`): enabled
//!   features, command lines, environment and tool selection;
//! - the Starlark surface: `cc_common` feature functions
//!   (`cc_common_bazel.bzl:21-37` delegating to `CcModule.java:61-138`),
//!   `cc_internal.cc_toolchain_features` / `cc_toolchain_variables` /
//!   `combine_cc_toolchain_variables` / `intern_string_sequence_variable_value` /
//!   `exec_os` (`CcStarlarkInternal.java`), `CcToolchainFeatures.configure_features`
//!   / `default_features_and_action_configs` and `FeatureConfiguration.is_enabled`
//!   / `is_requested` (`FeatureConfigurationForStarlark.java:72-91`).
//!
//! Variables and flag expansion live in `crate::cc_variables`. See
//! docs/CC_FEATURES.md for the profile and its divergences.

use crate::cc_variables::{
    EnvEntry, Expandable, FlagGroup, Scope, expand_env_entry, expand_flag_group,
    normalize_unix_path, parse_chunks, relative_path, safe_path_string,
};
use crate::language_modules::{Style, bind, bind_styled, failure, param, type_error};
use crate::repository::RepositorySet;
use allocative::Allocative;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::{AllocList, ListRef};
use starlark::values::tuple::TupleRef;
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value,
    starlark_value,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};

/// Caffeine `maximumSize(10000)` of the per-toolchain configuration cache
/// (CcToolchainFeatures.java:1222-1227). Natively, configurations beyond the
/// bound are computed but not cached instead of being evicted.
const CONFIGURATION_CACHE_LIMIT: usize = 10_000;

/// `infoError`'s location for values without a recorded creation call site
/// (plain `struct()` values; see `creation_location`).
const UNKNOWN_LOCATION: &str = "<unknown>";

/// Appended to messages where Bazel 9.3 throws an unchecked Java exception.
fn bazel_crash(exception: &str) -> String {
    format!("(Bazel 9.3 fails with an internal {exception})")
}

// ---------------------------------------------------------------------------
// The data model (CcToolchainFeatures.java:300-760).

/// WithFeatureSet (CcToolchainFeatures.java:355).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct WithFeatureSet {
    pub features: Vec<String>,
    pub not_features: Vec<String>,
}

/// `isWithFeaturesSatisfied` (CcToolchainFeatures.java:300-312).
fn with_features_satisfied(sets: &[WithFeatureSet], enabled: &HashSet<String>) -> bool {
    sets.is_empty()
        || sets.iter().any(|set| {
            set.features.iter().all(|feature| enabled.contains(feature))
                && !set
                    .not_features
                    .iter()
                    .any(|feature| enabled.contains(feature))
        })
}

/// FlagSet (CcToolchainFeatures.java:317-347). `expandIfAllAvailable` is
/// always empty for Starlark-defined flag sets (CcToolchainFeaturesLib.java:358).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FlagSet {
    pub actions: Vec<String>,
    pub with_features: Vec<WithFeatureSet>,
    pub flag_groups: Vec<FlagGroup>,
}

/// EnvSet (CcToolchainFeatures.java:360-386).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct EnvSet {
    pub actions: Vec<String>,
    pub entries: Vec<EnvEntry>,
    pub with_features: Vec<WithFeatureSet>,
}

/// Feature (CcToolchainFeatures.java:404-516).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Feature {
    pub name: String,
    pub flag_sets: Vec<FlagSet>,
    pub env_sets: Vec<EnvSet>,
    pub enabled: bool,
    pub requires: Vec<Vec<String>>,
    pub implies: Vec<String>,
    pub provides: Vec<String>,
}

/// Tool.PathOrigin (CcToolchainFeatures.java:524-528).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PathOrigin {
    CrosstoolPackage,
    FilesystemRoot,
    WorkspaceRoot,
}

/// Tool (CcToolchainFeatures.java:523-620). `path` is a normalized fragment.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Tool {
    pub path: String,
    pub origin: PathOrigin,
    pub execution_requirements: Vec<String>,
    pub with_features: Vec<WithFeatureSet>,
}

impl Tool {
    /// `Tool.getToolPathString` (CcToolchainFeatures.java:595-606).
    fn path_string(&self, tools_directory: &str) -> String {
        match self.origin {
            PathOrigin::CrosstoolPackage => {
                safe_path_string(&relative_path(tools_directory, &self.path))
            }
            PathOrigin::FilesystemRoot | PathOrigin::WorkspaceRoot => safe_path_string(&self.path),
        }
    }
}

/// ActionConfig (CcToolchainFeatures.java:643-761); its config name is its
/// action name (CcToolchainFeaturesLib.java:450-451).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ActionConfig {
    pub action_name: String,
    pub tools: Vec<Tool>,
    pub flag_sets: Vec<FlagSet>,
    pub enabled: bool,
    pub implies: Vec<String>,
}

impl ActionConfig {
    /// `ActionConfig.getTool` (CcToolchainFeatures.java:694-703).
    fn tool(&self, enabled: &HashSet<String>) -> Result<&Tool, String> {
        self.tools
            .iter()
            .find(|tool| with_features_satisfied(&tool.with_features, enabled))
            .ok_or_else(|| {
                format!(
                    "Matching tool for action {} not found for given feature configuration",
                    self.action_name
                )
            })
    }
}

/// ArtifactCategory (ArtifactCategory.java:24-49): name, default prefix,
/// default extension, extra allowed extensions.
const ARTIFACT_CATEGORIES: &[(&str, &str, &[&str])] = &[
    ("static_library", "lib", &[".a", ".lib"]),
    ("alwayslink_static_library", "lib", &[".lo", ".lo.lib"]),
    (
        "dynamic_library",
        "lib",
        &[".so", ".dylib", ".dll", ".pyd", ".wasm"],
    ),
    ("executable", "", &["", ".exe", ".wasm"]),
    (
        "interface_library",
        "lib",
        &[".ifso", ".tbd", ".if.lib", ".lib"],
    ),
    ("pic_file", "", &[".pic"]),
    ("included_file_list", "", &[".d"]),
    ("serialized_diagnostics_file", "", &[".dia"]),
    ("object_file", "", &[".o", ".obj"]),
    ("pic_object_file", "", &[".pic.o"]),
    ("cpp_module", "", &[".pcm", ".gcm", ".ifc"]),
    ("cpp_modules_info", "", &[".CXXModules.json"]),
    ("cpp_modules_ddi", "", &[".ddi"]),
    ("cpp_modules_modmap", "", &[".modmap"]),
    ("cpp_modules_modmap_input", "", &[".modmap.input"]),
    ("generated_assembly", "", &[".s", ".asm"]),
    ("processed_header", "", &[".processed"]),
    ("generated_header", "", &[".h"]),
    ("preprocessed_c_source", "", &[".i"]),
    ("preprocessed_cpp_source", "", &[".ii"]),
    ("coverage_data_file", "", &[".gcno"]),
    ("clif_output_proto", "", &[".opb"]),
];

/// `StringUtil.joinEnglishListSingleQuoted` (util/StringUtil.java:54-88).
fn join_english_list_single_quoted(choices: &[&str]) -> String {
    let mut buffer = String::new();
    for (index, choice) in choices.iter().enumerate() {
        if !buffer.is_empty() {
            let has_next = index + 1 < choices.len();
            if has_next || index >= 2 {
                buffer.push(',');
            }
            if !has_next {
                buffer.push_str(" or");
            }
            buffer.push(' ');
        }
        buffer.push('\'');
        buffer.push_str(choice);
        buffer.push('\'');
    }
    if buffer.is_empty() {
        "nothing".to_owned()
    } else {
        buffer
    }
}

// ---------------------------------------------------------------------------
// CcToolchainFeaturesLib: Starlark structs to the data model.

fn is_starlark_info(value: Value<'_>) -> bool {
    value.get_type() == "struct"
}

/// `Provider.getPrintableName()` of a struct (`struct`) or provider instance.
fn printable_name(value: Value<'_>) -> String {
    crate::providers::instance_identity(value)
        .and_then(|identity| identity.rsplit_once('%').map(|(_, name)| name.to_owned()))
        .unwrap_or_else(|| "struct".to_owned())
}

/// `Info.getCreationLocation()` as Bazel prints it: `<file path>:<line>:<col>`
/// of the provider call's opening parenthesis (the call expression's
/// Location), 1-based with a byte column (Bazel reads `.bzl` files as
/// Latin-1). Plain `struct()` values and values without a Starlark call site
/// have no recorded location.
fn creation_location(value: Value<'_>, repositories: Option<&RepositorySet>) -> String {
    let Some(span) = crate::providers::instance_creation_location(value) else {
        return UNKNOWN_LOCATION.to_owned();
    };
    let file = span.file.filename();
    let path = crate::label::Label::parse_canonical(file)
        .ok()
        .and_then(|label| {
            let root = repositories?.root(&label.repository).ok()?;
            Some(root.join(&label.package).join(&label.name))
        })
        .map_or_else(|| file.to_owned(), |path| path.display().to_string());
    let source = span.file.source();
    let begin = span.span.begin().get() as usize;
    let paren = source
        .get(begin..)
        .and_then(|rest| rest.find('('))
        .map_or(begin, |offset| begin + offset);
    let line = span
        .file
        .find_line(starlark::codemap::Pos::new(paren as u32));
    let column = paren - span.file.line_span(line).begin().get() as usize;
    format!("{path}:{}:{}", line + 1, column + 1)
}

/// Java class names in `Field '%s' is not of '%s' type.`.
const JAVA_STRING: &str = "java.lang.String";
const JAVA_BOOLEAN: &str = "java.lang.Boolean";
const JAVA_STARLARK_INFO: &str = "com.google.devtools.build.lib.packages.StarlarkInfo";
const JAVA_ARTIFACT: &str = "com.google.devtools.build.lib.actions.Artifact";

struct Parser<'v> {
    heap: Heap<'v>,
    repositories: Option<Arc<RepositorySet>>,
}

impl<'v> Parser<'v> {
    /// `infoError` (CcToolchainFeaturesLib.java:55-62).
    fn info_error(&self, info: Value<'v>, message: impl fmt::Display) -> starlark::Error {
        failure(format!(
            "in {} instantiated at {}: {message}",
            printable_name(info),
            creation_location(info, self.repositories.as_deref())
        ))
    }

    /// `StarlarkInfo.getValue`: None (Java null) for an absent field.
    fn field(&self, info: Value<'v>, name: &str) -> starlark::Result<Option<Value<'v>>> {
        info.get_attr(name, self.heap)
    }

    /// `getFieldFromStarlarkProvider` (CcToolchainFeaturesLib.java:509-526):
    /// Ok(None) for a None value; `matches` checks `clazz.isInstance`.
    fn typed_field(
        &self,
        info: Value<'v>,
        name: &str,
        java_class: &str,
        mandatory: bool,
        matches: impl Fn(Value<'v>) -> bool,
    ) -> starlark::Result<Option<Value<'v>>> {
        let Some(value) = self.field(info, name)? else {
            if mandatory {
                return Err(self.info_error(info, format!("Missing mandatory field '{name}'")));
            }
            return Ok(None);
        };
        if matches(value) {
            return Ok(Some(value));
        }
        if value.is_none() {
            return Ok(None);
        }
        Err(self.info_error(
            info,
            format!("Field '{name}' is not of '{java_class}' type."),
        ))
    }

    fn string_field(
        &self,
        info: Value<'v>,
        name: &str,
        mandatory: bool,
    ) -> starlark::Result<Option<String>> {
        Ok(self
            .typed_field(info, name, JAVA_STRING, mandatory, |v| {
                v.unpack_str().is_some()
            })?
            .map(|v| v.unpack_str().expect("checked").to_owned()))
    }

    fn bool_field(&self, info: Value<'v>, name: &str) -> starlark::Result<Option<bool>> {
        Ok(self
            .typed_field(info, name, JAVA_BOOLEAN, true, |v| {
                v.unpack_bool().is_some()
            })?
            .map(|v| v.unpack_bool().expect("checked")))
    }

    /// `Sequence.noneableCast(v, T, field)` elements of a list-valued field;
    /// an absent field is empty (`getValueOrNull`).
    fn sequence_field(&self, info: Value<'v>, name: &str) -> starlark::Result<Vec<Value<'v>>> {
        match self.field(info, name)? {
            None => Ok(Vec::new()),
            Some(value) if value.is_none() => Ok(Vec::new()),
            Some(value) => sequence_items(value, name, self.heap),
        }
    }

    fn string_list(&self, info: Value<'v>, name: &str) -> starlark::Result<Vec<String>> {
        let items = self.sequence_field(info, name)?;
        cast_strings(&items, name)
    }

    /// `ImmutableSet.copyOf`: first occurrences in order.
    fn string_set(&self, info: Value<'v>, name: &str) -> starlark::Result<Vec<String>> {
        Ok(dedup(self.string_list(info, name)?))
    }

    fn info_list(&self, info: Value<'v>, name: &str) -> starlark::Result<Vec<Value<'v>>> {
        let items = self.sequence_field(info, name)?;
        cast_infos(&items, name)?;
        Ok(items)
    }

    /// `checkRightProviderType` (CcToolchainFeaturesLib.java:65-74).
    fn check_type(&self, info: Value<'v>, expected: &str) -> starlark::Result<()> {
        let type_name = self.field(info, "type_name")?;
        let received = match type_name {
            None => printable_name(info),
            Some(value) => match value.unpack_str() {
                Some(text) => text.to_owned(),
                None => {
                    return Err(failure(format!(
                        "type_name of a {} is a {}, not a string {}",
                        printable_name(info),
                        value.get_type(),
                        bazel_crash("ClassCastException")
                    )));
                }
            },
        };
        if type_name.and_then(|v| v.unpack_str()) != Some(expected) {
            return Err(self.info_error(
                info,
                format!("Expected object of type '{expected}', received '{received}'."),
            ));
        }
        Ok(())
    }

    /// `featureFromStarlark` (CcToolchainFeaturesLib.java:87-152).
    fn feature(&self, info: Value<'v>) -> starlark::Result<Feature> {
        self.check_type(info, "feature")?;
        let name = self.string_field(info, "name", true)?;
        let enabled = self.bool_field(info, "enabled")?;
        let Some(name) = name.filter(|name| !(name.is_empty() && enabled == Some(false))) else {
            return Err(self.info_error(
                info,
                "A feature must either have a nonempty 'name' field or be enabled.",
            ));
        };
        if name.is_empty() && enabled.is_none() {
            return Err(failure(format!(
                "feature with an empty name has enabled = None {}",
                bazel_crash("NullPointerException")
            )));
        }
        if !valid_selectable_name(&name) {
            return Err(self.info_error(
                info,
                format!(
                    "A feature's name must consist solely of lowercase ASCII letters, digits, '.', '_', '+', and '-', got '{name}'"
                ),
            ));
        }
        let mut flag_sets = Vec::new();
        for flag_set in self.info_list(info, "flag_sets")? {
            let parsed = self.flag_set(flag_set, None)?;
            if parsed.actions.is_empty() {
                return Err(self.info_error(
                    flag_set,
                    "A flag_set that belongs to a feature must have nonempty 'actions' parameter.",
                ));
            }
            flag_sets.push(parsed);
        }
        let mut env_sets = Vec::new();
        for env_set in self.info_list(info, "env_sets")? {
            env_sets.push(self.env_set(env_set)?);
        }
        let mut requires = Vec::new();
        for feature_set in self.info_list(info, "requires")? {
            let type_name = self.field(feature_set, "type_name")?;
            if type_name.and_then(|v| v.unpack_str()) != Some("feature_set") {
                return Err(self.info_error(info, "expected object of type 'feature_set'."));
            }
            requires.push(self.string_set(feature_set, "features")?);
        }
        let implies = self.string_list(info, "implies")?;
        let provides = self.string_list(info, "provides")?;
        let Some(enabled) = enabled else {
            // `new Feature(..., enabled, ...)` unboxes the null Boolean.
            return Err(failure(format!(
                "feature '{name}' has enabled = None {}",
                bazel_crash("NullPointerException")
            )));
        };
        Ok(Feature {
            name,
            flag_sets,
            env_sets,
            enabled,
            requires,
            implies,
            provides,
        })
    }

    /// `variableWithValueFromStarlark` (CcToolchainFeaturesLib.java:198-216).
    fn variable_with_value(&self, info: Value<'v>) -> starlark::Result<(String, String)> {
        self.check_type(info, "variable_with_value")?;
        let name = self.string_field(info, "name", true)?;
        let value = self.string_field(info, "value", true)?;
        let Some(name) = name.filter(|name| !name.is_empty()) else {
            return Err(self.info_error(
                info,
                "'name' parameter of variable_with_value must be a nonempty string.",
            ));
        };
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            return Err(self.info_error(
                info,
                "'value' parameter of variable_with_value must be a nonempty string.",
            ));
        };
        Ok((name, value))
    }

    /// `envEntryFromStarlark` (CcToolchainFeaturesLib.java:220-237).
    fn env_entry(&self, info: Value<'v>) -> starlark::Result<EnvEntry> {
        self.check_type(info, "env_entry")?;
        let key = self.string_field(info, "key", true)?;
        let value = self.string_field(info, "value", true)?;
        let Some(key) = key.filter(|key| !key.is_empty()) else {
            return Err(self.info_error(
                info,
                "'key' parameter of env_entry must be a nonempty string.",
            ));
        };
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            return Err(self.info_error(
                info,
                "'value' parameter of env_entry must be a nonempty string.",
            ));
        };
        let expand_if_available = self.string_field(info, "expand_if_available", false)?;
        Ok(EnvEntry {
            key,
            value: parse_chunks(&value).map_err(failure)?,
            expand_if_all_available: expand_if_available.into_iter().collect(),
        })
    }

    /// `withFeatureSetFromStarlark` (CcToolchainFeaturesLib.java:241-249).
    fn with_feature_set(&self, info: Value<'v>) -> starlark::Result<WithFeatureSet> {
        self.check_type(info, "with_feature_set")?;
        Ok(WithFeatureSet {
            features: self.string_set(info, "features")?,
            not_features: self.string_set(info, "not_features")?,
        })
    }

    fn with_features(&self, info: Value<'v>) -> starlark::Result<Vec<WithFeatureSet>> {
        let mut sets: Vec<WithFeatureSet> = Vec::new();
        for set in self.info_list(info, "with_features")? {
            let parsed = self.with_feature_set(set)?;
            if !sets.contains(&parsed) {
                sets.push(parsed);
            }
        }
        Ok(sets)
    }

    /// `envSetFromStarlark` (CcToolchainFeaturesLib.java:253-273).
    fn env_set(&self, info: Value<'v>) -> starlark::Result<EnvSet> {
        self.check_type(info, "env_set")?;
        let actions = self.string_set(info, "actions")?;
        if actions.is_empty() {
            return Err(self.info_error(
                info,
                "actions parameter of env_set must be a nonempty list.",
            ));
        }
        let mut entries = Vec::new();
        for entry in self.info_list(info, "env_entries")? {
            entries.push(self.env_entry(entry)?);
        }
        Ok(EnvSet {
            actions,
            entries,
            with_features: self.with_features(info)?,
        })
    }

    /// `flagGroupFromStarlark` (CcToolchainFeaturesLib.java:277-328).
    fn flag_group(&self, info: Value<'v>) -> starlark::Result<FlagGroup> {
        self.check_type(info, "flag_group")?;
        let mut expandables = Vec::new();
        let flags = self.string_list(info, "flags")?;
        for flag in &flags {
            expandables.push(Expandable::Flag(parse_chunks(flag).map_err(failure)?));
        }
        let groups = self.info_list(info, "flag_groups")?;
        for group in &groups {
            expandables.push(Expandable::Group(self.flag_group(*group)?));
        }
        if !groups.is_empty() && !flags.is_empty() {
            return Err(self.info_error(
                info,
                "flag_group must contain either a list of flags or a list of flag_groups.",
            ));
        }
        if groups.is_empty() && flags.is_empty() {
            return Err(self.info_error(info, "Both 'flags' and 'flag_groups' are empty."));
        }
        let iterate_over = self.string_field(info, "iterate_over", true)?;
        let expand_if_available = self.string_field(info, "expand_if_available", true)?;
        let expand_if_not_available = self.string_field(info, "expand_if_not_available", true)?;
        let expand_if_true = self.string_field(info, "expand_if_true", true)?;
        let expand_if_false = self.string_field(info, "expand_if_false", true)?;
        let expand_if_equal = match self.typed_field(
            info,
            "expand_if_equal",
            JAVA_STARLARK_INFO,
            true,
            is_starlark_info,
        )? {
            Some(value) => Some(self.variable_with_value(value)?),
            None => None,
        };
        Ok(FlagGroup {
            expandables,
            iterate_over,
            expand_if_all_available: expand_if_available.into_iter().collect(),
            expand_if_none_available: expand_if_not_available.into_iter().collect(),
            expand_if_true,
            expand_if_false,
            expand_if_equal,
        })
    }

    /// `flagSetFromStarlark` (CcToolchainFeaturesLib.java:332-360).
    fn flag_set(&self, info: Value<'v>, action_name: Option<&str>) -> starlark::Result<FlagSet> {
        self.check_type(info, "flag_set")?;
        let mut actions = self.string_set(info, "actions")?;
        if let Some(action_name) = action_name {
            if !actions.is_empty() {
                // ActionConfig.FLAG_SET_WITH_ACTION_ERROR (CcToolchainFeatures.java:644-647).
                return Err(failure(format!(
                    "action_config {action_name} specifies actions.  An action_config's flag sets automatically apply to the configured action.  Thus, you must not specify action lists in an action_config's flag set."
                )));
            }
            actions = vec![action_name.to_owned()];
        }
        let mut flag_groups = Vec::new();
        for group in self.info_list(info, "flag_groups")? {
            flag_groups.push(self.flag_group(group)?);
        }
        Ok(FlagSet {
            actions,
            with_features: self.with_features(info)?,
            flag_groups,
        })
    }

    /// `toolFromStarlark` (CcToolchainFeaturesLib.java:364-408) and the Tool
    /// constructor's `checkToolPath` (CcToolchainFeatures.java:566-592).
    fn tool(&self, info: Value<'v>) -> starlark::Result<Tool> {
        self.check_type(info, "tool")?;
        let path = self.string_field(info, "path", false)?;
        let artifact = self.typed_field(info, "tool", JAVA_ARTIFACT, false, |v| {
            v.get_type() == "File"
        })?;
        let (path, origin) = if let Some(path) = path {
            if artifact.is_some() {
                return Err(self.info_error(
                    info,
                    "\"tool\" and \"path\" cannot be set at the same time.",
                ));
            }
            let normalized = normalize_unix_path(&path);
            if normalized.is_empty() {
                return Err(
                    self.info_error(info, "The 'path' field of tool must be a nonempty string.")
                );
            }
            let origin = if normalized.starts_with('/') {
                PathOrigin::FilesystemRoot
            } else {
                PathOrigin::CrosstoolPackage
            };
            (normalized, origin)
        } else if let Some(artifact) = artifact {
            let file =
                crate::starlark_types::unpack_file(artifact).map_err(starlark::Error::new_other)?;
            let exec_path = normalize_unix_path(&file.path);
            if exec_path.starts_with('/') {
                return Err(failure(format!(
                    "Tool-path with origin WORKSPACE_ROOT must be relative, got '{exec_path}'."
                )));
            }
            (exec_path, PathOrigin::WorkspaceRoot)
        } else {
            return Err(failure(
                "Exactly one of \"tool\" and \"path\" must be set.".to_owned(),
            ));
        };
        let with_features = self.with_features(info)?;
        let execution_requirements = self.string_set(info, "execution_requirements")?;
        Ok(Tool {
            path,
            origin,
            execution_requirements,
            with_features,
        })
    }

    /// `actionConfigFromStarlark` (CcToolchainFeaturesLib.java:412-452).
    fn action_config(&self, info: Value<'v>) -> starlark::Result<ActionConfig> {
        self.check_type(info, "action_config")?;
        let action_name = self.string_field(info, "action_name", true)?;
        let Some(action_name) = action_name.filter(|name| !name.is_empty()) else {
            return Err(self.info_error(
                info,
                "The 'action_name' field of action_config must be a nonempty string.",
            ));
        };
        if !valid_selectable_name(&action_name) {
            return Err(self.info_error(
                info,
                format!(
                    "An action_config's name must consist solely of lowercase ASCII letters, digits, '.', '_', '+', and '-', got '{action_name}'"
                ),
            ));
        }
        let enabled = self.bool_field(info, "enabled")?;
        let mut tools = Vec::new();
        for tool in self.info_list(info, "tools")? {
            tools.push(self.tool(tool)?);
        }
        let mut flag_sets = Vec::new();
        for flag_set in self.info_list(info, "flag_sets")? {
            flag_sets.push(self.flag_set(flag_set, Some(&action_name))?);
        }
        let implies = self.string_list(info, "implies")?;
        let Some(enabled) = enabled else {
            return Err(failure(format!(
                "action_config '{action_name}' has enabled = None {}",
                bazel_crash("NullPointerException")
            )));
        };
        Ok(ActionConfig {
            action_name,
            tools,
            flag_sets,
            enabled,
            implies,
        })
    }

    /// `artifactNamePatternFromStarlark` (CcToolchainFeaturesLib.java:454-497).
    fn artifact_name_pattern(&self, info: Value<'v>) -> starlark::Result<()> {
        self.check_type(info, "artifact_name_pattern")?;
        let category = self.string_field(info, "category_name", true)?;
        let Some(category) = category.filter(|name| !name.is_empty()) else {
            return Err(self.info_error(
                info,
                "The 'category_name' field of artifact_name_pattern must be a nonempty string.",
            ));
        };
        let Some((name, _, allowed)) = ARTIFACT_CATEGORIES
            .iter()
            .find(|(name, _, _)| *name == category)
        else {
            return Err(self.info_error(
                info,
                format!("Artifact category {category} not recognized."),
            ));
        };
        let extension = self
            .string_field(info, "extension", true)?
            .unwrap_or_default();
        if !allowed.contains(&extension.as_str()) {
            return Err(self.info_error(
                info,
                format!(
                    "Unrecognized file extension '{extension}', allowed extensions are {}, please check artifact_name_pattern configuration for {name} in your rule.",
                    join_english_list_single_quoted(allowed)
                ),
            ));
        }
        self.string_field(info, "prefix", true)?;
        Ok(())
    }
}

/// `^[_a-z0-9+\-\.]*$` (CcToolchainFeaturesLib.java:97, 422).
fn valid_selectable_name(name: &str) -> bool {
    name.bytes()
        .all(|b| matches!(b, b'_' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'-' | b'.'))
}

fn dedup(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect()
}

/// The elements of a Starlark `Sequence` (list, tuple or range), or
/// `Sequence.cast`'s `for %s, got %s, want sequence`.
fn sequence_items<'v>(
    value: Value<'v>,
    what: &str,
    heap: Heap<'v>,
) -> starlark::Result<Vec<Value<'v>>> {
    if let Some(list) = ListRef::from_value(value) {
        return Ok(list.iter().collect());
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        return Ok(tuple.iter().collect());
    }
    if value.get_type() == "range" {
        return Ok(value.iterate(heap)?.collect());
    }
    Err(failure(format!(
        "for {what}, got {}, want sequence",
        value.get_type()
    )))
}

fn element_error(index: usize, what: &str, value: Value<'_>, want: &str) -> starlark::Error {
    failure(format!(
        "at index {index} of {what}, got element of type {}, want {want}",
        value.get_type()
    ))
}

fn cast_strings(items: &[Value<'_>], what: &str) -> starlark::Result<Vec<String>> {
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            item.unpack_str()
                .map(str::to_owned)
                .ok_or_else(|| element_error(index, what, *item, "string"))
        })
        .collect()
}

fn cast_infos(items: &[Value<'_>], what: &str) -> starlark::Result<()> {
    for (index, item) in items.iter().enumerate() {
        if !is_starlark_info(*item) {
            return Err(element_error(index, what, *item, "struct"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// CcToolchainFeatures (CcToolchainFeatures.java:1034-1328).

/// The parsed, validated toolchain graph. Selectables are the features in
/// declaration order followed by the action configs.
#[derive(Debug)]
pub(crate) struct ToolchainData {
    features: Vec<Feature>,
    action_configs: Vec<ActionConfig>,
    names: Vec<String>,
    by_name: HashMap<String, usize>,
    action_config_by_action: HashMap<String, usize>,
    implies: Vec<Vec<usize>>,
    implied_by: Vec<Vec<usize>>,
    requires: Vec<Vec<Vec<usize>>>,
    required_by: Vec<Vec<usize>>,
    /// `provides.build().inverse()`: strings in first-appearance order with
    /// their providers in declaration order (duplicates kept).
    provides: Vec<(String, Vec<usize>)>,
    default_selectables: Vec<String>,
    tools_directory: String,
}

/// `CcToolchainConfigInfo.getFeatures/getActionConfigs/getArtifactNamePatterns`
/// (CcToolchainConfigInfo.java:56-92) plus the CcToolchainFeatures constructor.
pub(crate) fn toolchain_from_config_info<'v>(
    config: Value<'v>,
    tools_directory: &str,
    heap: Heap<'v>,
    repositories: Option<Arc<RepositorySet>>,
) -> starlark::Result<ToolchainData> {
    let parser = Parser { heap, repositories };
    let required = |name: &str, what: &str| -> starlark::Result<Vec<Value<'v>>> {
        let value = parser.field(config, name)?.ok_or_else(|| {
            failure(format!(
                "CcToolchainConfigInfo has no field '{name}' {}",
                bazel_crash("NullPointerException")
            ))
        })?;
        let items = sequence_items(value, what, heap)?;
        cast_infos(&items, what)?;
        Ok(items)
    };
    let mut features = Vec::new();
    for feature in required("_features_DO_NOT_USE", "_features")? {
        features.push(parser.feature(feature)?);
    }
    // `OS.valueOf(actual.getValue("_exec_os_DO_NOT_USE", String.class))`.
    match parser.field(config, "_exec_os_DO_NOT_USE")? {
        None => {
            return Err(failure(format!(
                "CcToolchainConfigInfo has no field '_exec_os_DO_NOT_USE' {}",
                bazel_crash("NullPointerException")
            )));
        }
        Some(value) => match value.unpack_str() {
            Some("DARWIN" | "FREEBSD" | "OPENBSD" | "LINUX" | "UNKNOWN") => {}
            Some("WINDOWS") => {
                return Err(failure(format!(
                    "Windows execution platform tool paths (_exec_os_DO_NOT_USE = \"WINDOWS\") {} (C++ toolchain features)",
                    crate::language_modules::NOT_IMPLEMENTED
                )));
            }
            Some(other) => {
                return Err(failure(format!(
                    "No enum constant com.google.devtools.build.lib.util.OS.{other} {}",
                    bazel_crash("IllegalArgumentException")
                )));
            }
            None => {
                return Err(failure(format!(
                    "for _exec_os_DO_NOT_USE field, got {}, want string",
                    value.get_type()
                )));
            }
        },
    }
    let mut action_configs = Vec::new();
    for action_config in required("_action_configs_DO_NOT_USE", "_action_configs")? {
        action_configs.push(parser.action_config(action_config)?);
    }

    build_graph(features, action_configs, tools_directory, || {
        for pattern in required(
            "_artifact_name_patterns_DO_NOT_USE",
            "_artifact_name_patterns",
        )? {
            parser.artifact_name_pattern(pattern)?;
        }
        Ok(())
    })
}

/// The CcToolchainFeatures constructor after parsing
/// (CcToolchainFeatures.java:1099-1192): duplicate checks, then
/// `artifact_name_patterns` (parsed between them and the graph), then the
/// implies/requires/provides graph.
pub(crate) fn build_graph(
    features: Vec<Feature>,
    action_configs: Vec<ActionConfig>,
    tools_directory: &str,
    artifact_name_patterns: impl FnOnce() -> starlark::Result<()>,
) -> starlark::Result<ToolchainData> {
    // checkForActionNameDups (CcToolchainFeatures.java:1207-1217).
    let mut action_names = HashSet::new();
    for action_config in &action_configs {
        if !action_names.insert(action_config.action_name.as_str()) {
            return Err(failure(format!(
                "Invalid toolchain configuration: multiple action configs for action '{}'",
                action_config.action_name
            )));
        }
    }
    let names: Vec<String> = features
        .iter()
        .map(|feature| feature.name.clone())
        .chain(action_configs.iter().map(|ac| ac.action_name.clone()))
        .collect();
    // checkForActivatableDups (CcToolchainFeatures.java:1194-1205).
    let mut by_name = HashMap::new();
    for (index, name) in names.iter().enumerate() {
        if by_name.insert(name.clone(), index).is_some() {
            return Err(failure(format!(
                "Invalid toolchain configuration: feature or action config '{name}' was specified multiple times."
            )));
        }
    }
    artifact_name_patterns()?;

    let count = names.len();
    let resolve = |name: &str, reference: &str| -> starlark::Result<usize> {
        by_name.get(name).copied().ok_or_else(|| {
            failure(format!(
                "Invalid toolchain configuration: feature '{name}', which is referenced from feature '{reference}', is not defined."
            ))
        })
    };
    let mut implies = vec![Vec::new(); count];
    let mut implied_by = vec![Vec::new(); count];
    let mut requires = vec![Vec::new(); count];
    let mut required_by = vec![Vec::new(); count];
    let mut provides: Vec<(String, Vec<usize>)> = Vec::new();
    for (index, feature) in features.iter().enumerate() {
        for required_set in &feature.requires {
            let mut all_of: Vec<usize> = Vec::new();
            for required_name in required_set {
                let required = resolve(required_name, &feature.name)?;
                if !all_of.contains(&required) {
                    all_of.push(required);
                }
                required_by[required].push(index);
            }
            requires[index].push(all_of);
        }
        for implied_name in &feature.implies {
            let implied = resolve(implied_name, &feature.name)?;
            implied_by[implied].push(index);
            implies[index].push(implied);
        }
        for provided in &feature.provides {
            match provides.iter_mut().find(|(name, _)| name == provided) {
                Some((_, providers)) => providers.push(index),
                None => provides.push((provided.clone(), vec![index])),
            }
        }
    }
    for (offset, action_config) in action_configs.iter().enumerate() {
        let index = features.len() + offset;
        for implied_name in &action_config.implies {
            let implied = resolve(implied_name, &action_config.action_name)?;
            implied_by[implied].push(index);
            implies[index].push(implied);
        }
    }
    let default_selectables = features
        .iter()
        .filter(|feature| feature.enabled)
        .map(|feature| feature.name.clone())
        .chain(
            action_configs
                .iter()
                .filter(|ac| ac.enabled)
                .map(|ac| ac.action_name.clone()),
        )
        .collect();
    let action_config_by_action = action_configs
        .iter()
        .enumerate()
        .map(|(index, ac)| (ac.action_name.clone(), index))
        .collect();
    Ok(ToolchainData {
        features,
        action_configs,
        names,
        by_name,
        action_config_by_action,
        implies,
        implied_by,
        requires,
        required_by,
        provides,
        default_selectables,
        tools_directory: normalize_unix_path(tools_directory),
    })
}

/// The result of feature selection (CcToolchainFeatures.FeatureConfiguration).
#[derive(Debug)]
pub(crate) struct FeatureConfigurationData {
    toolchain: Arc<ToolchainData>,
    requested: Vec<String>,
    enabled_features: Vec<usize>,
    enabled_feature_names: Vec<String>,
    enabled_set: HashSet<String>,
    enabled_actions: HashSet<String>,
}

impl ToolchainData {
    fn is_satisfied(&self, index: usize, requested: &HashSet<usize>, enabled: &[bool]) -> bool {
        (requested.contains(&index) || self.implied_by[index].iter().any(|i| enabled[*i]))
            && self.implies[index].iter().all(|i| enabled[*i])
            && (self.requires[index].is_empty()
                || self.requires[index]
                    .iter()
                    .any(|all_of| all_of.iter().all(|i| enabled[*i])))
    }

    /// `FeatureSelection.run` (FeatureSelection.java:145-198).
    pub(crate) fn select(
        self: &Arc<Self>,
        requested_features: Vec<String>,
    ) -> Result<FeatureConfigurationData, String> {
        let requested: Vec<usize> = requested_features
            .iter()
            .filter_map(|name| self.by_name.get(name).copied())
            .collect();
        let requested_set: HashSet<usize> = requested.iter().copied().collect();
        let count = self.names.len();
        let mut enabled = vec![false; count];
        // enableAllImpliedBy (FeatureSelection.java:204-212).
        let mut stack = requested.clone();
        while let Some(index) = stack.pop() {
            if !enabled[index] {
                enabled[index] = true;
                stack.extend(self.implies[index].iter().copied());
            }
        }
        // disableUnsupportedActivatables / checkActivatable
        // (FeatureSelection.java:215-251). Every satisfaction condition is
        // monotone in the enabled set, so this worklist reaches the same
        // greatest fixpoint as Bazel's recursive re-checks.
        let mut work: Vec<usize> = (0..count).filter(|i| enabled[*i]).collect();
        while let Some(index) = work.pop() {
            if !enabled[index] || self.is_satisfied(index, &requested_set, &enabled) {
                continue;
            }
            enabled[index] = false;
            work.extend(self.implied_by[index].iter().copied());
            work.extend(self.required_by[index].iter().copied());
            work.extend(self.implies[index].iter().copied());
        }
        // COLLIDING_PROVIDES_ERROR (FeatureSelection.java:170-185).
        for (provided, providers) in &self.provides {
            let conflicts: Vec<&str> = providers
                .iter()
                .filter(|i| enabled[**i])
                .map(|i| self.names[*i].as_str())
                .collect();
            if conflicts.len() > 1 {
                return Err(format!(
                    "Symbol {provided} is provided by all of the following features: {}",
                    conflicts.join(" ")
                ));
            }
        }
        let feature_count = self.features.len();
        let enabled_features: Vec<usize> = (0..feature_count).filter(|i| enabled[*i]).collect();
        let enabled_feature_names: Vec<String> = enabled_features
            .iter()
            .map(|i| self.features[*i].name.clone())
            .collect();
        let enabled_actions = (feature_count..count)
            .filter(|i| enabled[*i])
            .map(|i| self.action_configs[i - feature_count].action_name.clone())
            .collect();
        Ok(FeatureConfigurationData {
            toolchain: Arc::clone(self),
            requested: dedup(requested_features),
            enabled_set: enabled_feature_names.iter().cloned().collect(),
            enabled_feature_names,
            enabled_features,
            enabled_actions,
        })
    }

    fn action_config(&self, action: &str) -> Result<&ActionConfig, String> {
        self.action_config_by_action
            .get(action)
            .map(|index| &self.action_configs[*index])
            .ok_or_else(|| {
                format!("Action {action} does not have an enabled configuration in the toolchain.")
            })
    }
}

impl FeatureConfigurationData {
    fn is_enabled(&self, feature: &str) -> bool {
        self.enabled_set.contains(feature)
    }

    fn action_is_configured(&self, action: &str) -> bool {
        self.enabled_actions.contains(action)
    }

    fn expand_flag_sets<'v>(
        &self,
        flag_sets: &[FlagSet],
        action: &str,
        scope: &Scope<'_, 'v>,
        eval: &mut Evaluator<'v, '_, '_>,
        command_line: &mut Vec<String>,
    ) -> starlark::Result<()> {
        for flag_set in flag_sets {
            if !with_features_satisfied(&flag_set.with_features, &self.enabled_set)
                || !flag_set.actions.iter().any(|a| a == action)
            {
                continue;
            }
            for group in &flag_set.flag_groups {
                expand_flag_group(group, scope, eval, command_line)?;
            }
        }
        Ok(())
    }

    /// `getCommandLine` (CcToolchainFeatures.java:916-936).
    fn command_line<'v>(
        &self,
        action: &str,
        variables: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Vec<String>> {
        let scope = Scope::Root(variables);
        let mut command_line = Vec::new();
        let toolchain = &self.toolchain;
        if self.action_is_configured(action) {
            let action_config = toolchain.action_config(action).map_err(failure)?;
            self.expand_flag_sets(
                &action_config.flag_sets,
                &action_config.action_name,
                &scope,
                eval,
                &mut command_line,
            )?;
        }
        for index in &self.enabled_features {
            self.expand_flag_sets(
                &toolchain.features[*index].flag_sets,
                action,
                &scope,
                eval,
                &mut command_line,
            )?;
        }
        Ok(command_line)
    }

    /// `getEnvironmentVariables` (CcToolchainFeatures.java:976-984).
    fn environment<'v>(
        &self,
        action: &str,
        variables: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Vec<(String, String)>> {
        let scope = Scope::Root(variables);
        let mut environment: Vec<(String, String)> = Vec::new();
        for index in &self.enabled_features {
            for env_set in &self.toolchain.features[*index].env_sets {
                if !env_set.actions.iter().any(|a| a == action)
                    || !with_features_satisfied(&env_set.with_features, &self.enabled_set)
                {
                    continue;
                }
                for entry in &env_set.entries {
                    if let Some(value) = expand_env_entry(entry, &scope, eval)? {
                        environment.push((entry.key.clone(), value));
                    }
                }
            }
        }
        // ImmutableMap.Builder.buildOrThrow runs after every expansion, so an
        // expansion error wins over a duplicate key; Guava reports the later
        // entry first (`Multiple entries with same key: K=2 and K=1`).
        for (index, (key, value)) in environment.iter().enumerate() {
            if let Some((_, existing)) = environment[..index].iter().find(|(k, _)| k == key) {
                return Err(failure(format!(
                    "Multiple entries with same key: {key}={value} and {key}={existing} {}",
                    bazel_crash("IllegalArgumentException")
                )));
            }
        }
        Ok(environment)
    }

    /// `getToolPathForAction` (CcToolchainFeatures.java:986-993).
    fn tool_path(&self, action: &str) -> Result<String, String> {
        let tool = self
            .toolchain
            .action_config(action)?
            .tool(&self.enabled_set)?;
        Ok(tool.path_string(&self.toolchain.tools_directory))
    }

    /// `getToolRequirementsForAction` (CcToolchainFeatures.java:995-1002).
    fn tool_requirements(&self, action: &str) -> Result<Vec<String>, String> {
        Ok(self
            .toolchain
            .action_config(action)?
            .tool(&self.enabled_set)?
            .execution_requirements
            .clone())
    }
}

// ---------------------------------------------------------------------------
// Starlark values.

#[derive(Debug)]
pub(crate) struct FeaturesInner {
    data: Arc<ToolchainData>,
    /// Keyed by the requested *set* (ImmutableSet equality); the cached value
    /// keeps the first request's order, as Bazel's LoadingCache does.
    cache: Mutex<HashMap<BTreeSet<String>, Arc<FeatureConfigurationData>>>,
}

impl FeaturesInner {
    /// `getFeatureConfiguration` (CcToolchainFeatures.java:1260-1271).
    fn configuration(
        &self,
        requested: Vec<String>,
    ) -> Result<Arc<FeatureConfigurationData>, String> {
        let key: BTreeSet<String> = requested.iter().cloned().collect();
        if let Some(cached) = self.cache.lock().expect("cache lock").get(&key) {
            return Ok(Arc::clone(cached));
        }
        let computed = Arc::new(self.data.select(requested)?);
        let mut cache = self.cache.lock().expect("cache lock");
        if let Some(cached) = cache.get(&key) {
            return Ok(Arc::clone(cached));
        }
        if cache.len() < CONFIGURATION_CACHE_LIMIT {
            cache.insert(key, Arc::clone(&computed));
        }
        Ok(computed)
    }
}

#[derive(Clone, Debug)]
pub(crate) enum MethodTarget {
    Features(Arc<FeaturesInner>),
    Configuration(Arc<FeatureConfigurationData>),
}

// Only derive-generated marker implementations need the exception. These
// simple values own all their data (no Starlark heap references).
#[allow(unsafe_code)]
mod owned {
    use super::*;

    /// CcToolchainFeatures (unannotated: type is the Java simple name).
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct CcToolchainFeatures {
        #[allocative(skip)]
        pub inner: Arc<FeaturesInner>,
    }

    /// FeatureConfigurationForStarlark (`@StarlarkBuiltin(name = "FeatureConfiguration")`).
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct FeatureConfiguration {
        #[allocative(skip)]
        pub data: Arc<FeatureConfigurationData>,
    }

    /// A bound `@StarlarkMethod` of either value.
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct FeatureMethod {
        #[allocative(skip)]
        pub target: MethodTarget,
        #[allocative(skip)]
        pub name: &'static str,
    }
}
pub(crate) use owned::{CcToolchainFeatures, FeatureConfiguration, FeatureMethod};
starlark_simple_value!(CcToolchainFeatures);
starlark_simple_value!(FeatureConfiguration);
starlark_simple_value!(FeatureMethod);

const FEATURES_METHODS: [&str; 2] = ["configure_features", "default_features_and_action_configs"];
const CONFIGURATION_METHODS: [&str; 2] = ["is_enabled", "is_requested"];

impl fmt::Display for CcToolchainFeatures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<unknown object com.google.devtools.build.lib.rules.cpp.CcToolchainFeatures>")
    }
}

#[starlark_value(type = "CcToolchainFeatures")]
impl<'v> StarlarkValue<'v> for CcToolchainFeatures {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let name = FEATURES_METHODS.iter().find(|m| **m == name)?;
        Some(heap.alloc(FeatureMethod {
            target: MethodTarget::Features(Arc::clone(&self.inner)),
            name,
        }))
    }

    fn dir_attr(&self) -> Vec<String> {
        FEATURES_METHODS.iter().map(|m| (*m).to_owned()).collect()
    }
}

/// `FeatureConfigurationForStarlark.str` (FeatureConfigurationForStarlark.java:52-60).
impl fmt::Display for FeatureConfiguration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "<FeatureConfiguration(ENABLED:{};REQUESTED:{})>",
            self.data.enabled_feature_names.join(", "),
            self.data.requested.join(", ")
        )
    }
}

#[starlark_value(type = "FeatureConfiguration")]
impl<'v> StarlarkValue<'v> for FeatureConfiguration {
    fn collect_repr(&self, collector: &mut String) {
        // repr is not overridden: StarlarkValue's `<unknown object CLASS>`.
        collector.push_str(
            "<unknown object com.google.devtools.build.lib.rules.cpp.FeatureConfigurationForStarlark>",
        );
    }

    fn collect_str(&self, collector: &mut String) {
        collector.push_str(&self.to_string());
    }

    /// `debugPrint` (FeatureConfigurationForStarlark.java:62-67), used by
    /// `print()` and `fail()`.
    fn collect_debug_print(&self, collector: &mut String, _eval: &Evaluator<'v, '_, '_>) {
        collector.push_str("<FeatureConfiguration(");
        collector.push_str(&self.data.enabled_feature_names.join(", "));
        collector.push_str(")>");
    }

    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let name = CONFIGURATION_METHODS.iter().find(|m| **m == name)?;
        Some(heap.alloc(FeatureMethod {
            target: MethodTarget::Configuration(Arc::clone(&self.data)),
            name,
        }))
    }

    fn dir_attr(&self) -> Vec<String> {
        CONFIGURATION_METHODS
            .iter()
            .map(|m| (*m).to_owned())
            .collect()
    }
}

impl fmt::Display for FeatureMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let receiver = match self.target {
            MethodTarget::Features(_) => "CcToolchainFeatures",
            MethodTarget::Configuration(_) => "FeatureConfiguration",
        };
        write!(f, "<built-in method {} of {receiver} value>", self.name)
    }
}

#[starlark_value(type = "builtin_function_or_method")]
impl<'v> StarlarkValue<'v> for FeatureMethod {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        match (&self.target, self.name) {
            (MethodTarget::Features(inner), "configure_features") => {
                const F: &str = "configure_features";
                let bound = bind(
                    F,
                    &[param("requested_features", true, true, true)],
                    args,
                    heap,
                )?;
                let requested = bound[0].expect("required");
                let Some(list) = ListRef::from_value(requested) else {
                    return Err(type_error(F, "requested_features", requested, "list"));
                };
                let items: Vec<Value<'v>> = list.iter().collect();
                default_allowlist(eval)?;
                let requested = cast_strings(&items, "requested_features")?;
                let data = inner.configuration(requested).map_err(failure)?;
                Ok(heap.alloc(FeatureConfiguration { data }))
            }
            (MethodTarget::Features(inner), _) => {
                bind("default_features_and_action_configs", &[], args, heap)?;
                default_allowlist(eval)?;
                Ok(frozen_string_list(eval, &inner.data.default_selectables))
            }
            (MethodTarget::Configuration(data), name) => {
                let bound = bind(name, &[param("feature", true, false, true)], args, heap)?;
                let feature = bound[0].expect("required");
                let feature = feature
                    .unpack_str()
                    .ok_or_else(|| type_error(name, "feature", feature, "string"))?;
                default_allowlist(eval)?;
                Ok(Value::new_bool(if name == "is_enabled" {
                    data.is_enabled(feature)
                } else {
                    data.requested.iter().any(|r| r == feature)
                }))
            }
        }
    }
}

/// `CcModule.checkPrivateStarlarkificationAllowlist` (CcModule.java:189-192):
/// `BuiltinRestriction.failIfCalledOutsideDefaultAllowlist` for the caller.
fn default_allowlist(eval: &Evaluator<'_, '_, '_>) -> starlark::Result<()> {
    let allowlist: Vec<(String, String)> =
        crate::subrule_fragments::INTERNAL_STARLARK_API_ALLOWLIST
            .iter()
            .map(|(repo, prefix)| ((*repo).to_owned(), (*prefix).to_owned()))
            .collect();
    crate::cc_common::check_private_api(eval, 0, &allowlist)
}

/// `PRIVATE_STARLARKIFICATION_ALLOWLIST` of `@_builtins`
/// (builtins_bzl/common/cc/cc_helper_internal.bzl:21-64, including
/// `CREATE_COMPILE_ACTION_API_ALLOWLISTED_PACKAGES`).
const BUILTINS_PRIVATE_STARLARKIFICATION_ALLOWLIST: [(&str, &str); 28] = [
    ("_builtins", ""),
    ("", "tools/build_defs/android"),
    ("", "third_party/bazel_rules/rules_android"),
    ("build_bazel_rules_android", ""),
    ("rules_android", ""),
    ("", "third_party/bazel_rules/rules_apple"),
    ("apple_support", ""),
    ("rules_apple", ""),
    ("", "bazel_internal/test_rules/cc"),
    ("", "third_party/bazel_rules/rules_cc"),
    ("", "tools/build_defs/cc"),
    ("rules_cc", ""),
    ("", "third_party/gpus/cuda"),
    ("", "tools/build_defs/go"),
    ("", "third_party/bazel_rules/rules_java"),
    ("rules_java", ""),
    ("", "tools/build_defs/objc"),
    ("", "third_party/protobuf"),
    ("protobuf", ""),
    ("com_google_protobuf", ""),
    ("", "rust/private"),
    ("rules_rust", "rust/private"),
    ("", "third_party/bazel_rules/rules_python"),
    ("", "research/colab"),
    ("", "devtools/rust/cc_interop"),
    ("", "third_party/crubit"),
    ("", "tools/build_defs/clif"),
    ("", "third_party/bazel_rules/rules_cc"),
];

/// `StarlarkList.immutableCopyOf`: allocated frozen, so mutation fails.
fn frozen_string_list<'v>(eval: &Evaluator<'v, '_, '_>, items: &[String]) -> Value<'v> {
    eval.frozen_heap()
        .alloc(AllocList(items.iter().map(String::as_str)))
        .to_value()
}

// ---------------------------------------------------------------------------
// cc_common / cc_internal entry points.

/// The native implementations behind `Behavior::CcFeatures`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Allocative)]
pub(crate) enum Op {
    /// cc_common_bazel.bzl `_get_tool_for_action` -> CcModule.getToolForAction.
    GetToolForAction,
    GetExecutionRequirements,
    ActionIsEnabled,
    GetMemoryInefficientCommandLine,
    GetEnvironmentVariables,
    EmptyVariables,
    GetToolRequirementForAction,
    /// cc_internal (CcStarlarkInternal) methods.
    CcToolchainFeatures,
    CcToolchainVariables,
    CombineCcToolchainVariables,
    InternStringSequenceVariableValue,
    ExecOs,
}

impl Op {
    /// The `cc_common` field implemented by this op, if any.
    pub(crate) fn for_cc_common(field: &str) -> Option<Op> {
        Some(match field {
            "get_tool_for_action" => Op::GetToolForAction,
            "get_execution_requirements" => Op::GetExecutionRequirements,
            "action_is_enabled" => Op::ActionIsEnabled,
            "get_memory_inefficient_command_line" => Op::GetMemoryInefficientCommandLine,
            "get_environment_variables" => Op::GetEnvironmentVariables,
            "empty_variables" => Op::EmptyVariables,
            "get_tool_requirement_for_action" => Op::GetToolRequirementForAction,
            _ => return None,
        })
    }

    /// The `cc_internal` method implemented by this op, if any.
    pub(crate) fn for_cc_internal(method: &str) -> Option<Op> {
        Some(match method {
            "cc_toolchain_features" => Op::CcToolchainFeatures,
            "cc_toolchain_variables" => Op::CcToolchainVariables,
            "combine_cc_toolchain_variables" => Op::CombineCcToolchainVariables,
            "intern_string_sequence_variable_value" => Op::InternStringSequenceVariableValue,
            "exec_os" => Op::ExecOs,
            _ => return None,
        })
    }
}

fn feature_configuration<'v>(
    function: &str,
    value: Value<'v>,
) -> starlark::Result<&'v FeatureConfiguration> {
    FeatureConfiguration::from_value(value).ok_or_else(|| {
        type_error(
            function,
            "feature_configuration",
            value,
            "FeatureConfiguration",
        )
    })
}

fn string<'v>(function: &str, param: &str, value: Value<'v>) -> starlark::Result<&'v str> {
    value
        .unpack_str()
        .ok_or_else(|| type_error(function, param, value, "string"))
}

fn variables<'v>(function: &str, value: Value<'v>) -> starlark::Result<Value<'v>> {
    if crate::cc_variables::is_variables(value) {
        Ok(value)
    } else {
        Err(type_error(function, "variables", value, "Variables"))
    }
}

/// `Behavior::CcFeatures` dispatch. `name` and `style` identify the Bazel
/// callable (an `@_builtins` def for cc_common, a Java method for cc_internal).
pub(crate) fn call<'v>(
    op: Op,
    style: Style,
    name: &str,
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let heap = eval.heap();
    let keyword = |names: &[&'static str]| -> Vec<crate::language_modules::Param> {
        names.iter().map(|n| param(n, false, true, true)).collect()
    };
    match op {
        Op::GetToolForAction
        | Op::GetExecutionRequirements
        | Op::ActionIsEnabled
        | Op::GetToolRequirementForAction => {
            let bound = bind_styled(
                style,
                name,
                &keyword(&["feature_configuration", "action_name"]),
                args,
                heap,
            )?;
            let internal = name.trim_start_matches('_');
            if op == Op::GetToolRequirementForAction {
                let allowlist: Vec<(String, String)> = BUILTINS_PRIVATE_STARLARKIFICATION_ALLOWLIST
                    .iter()
                    .map(|(repo, prefix)| ((*repo).to_owned(), (*prefix).to_owned()))
                    .collect();
                crate::cc_common::check_private_api(eval, 0, &allowlist)?;
            }
            let configuration = feature_configuration(internal, bound[0].expect("required"))?;
            let action = string(internal, "action_name", bound[1].expect("required"))?;
            let data = &configuration.data;
            match op {
                Op::GetToolForAction => {
                    // CcModule.getToolForAction wraps the IllegalArgumentException.
                    let path = data.tool_path(action).map_err(failure)?;
                    Ok(heap.alloc(path))
                }
                Op::ActionIsEnabled => Ok(Value::new_bool(data.action_is_configured(action))),
                _ => {
                    let requirements = data.tool_requirements(action).map_err(|message| {
                        failure(format!(
                            "{message} {}",
                            bazel_crash("IllegalArgumentException")
                        ))
                    })?;
                    Ok(frozen_string_list(eval, &requirements))
                }
            }
        }
        Op::GetMemoryInefficientCommandLine | Op::GetEnvironmentVariables => {
            let bound = bind_styled(
                style,
                name,
                &keyword(&["feature_configuration", "action_name", "variables"]),
                args,
                heap,
            )?;
            let internal = name.trim_start_matches('_');
            let configuration = feature_configuration(internal, bound[0].expect("required"))?;
            let action = string(internal, "action_name", bound[1].expect("required"))?;
            let variables = variables(internal, bound[2].expect("required"))?;
            let data = Arc::clone(&configuration.data);
            if op == Op::GetMemoryInefficientCommandLine {
                let command_line = data.command_line(action, variables, eval)?;
                Ok(frozen_string_list(eval, &command_line))
            } else {
                let environment = data.environment(action, variables, eval)?;
                Ok(eval
                    .frozen_heap()
                    .alloc(AllocDict(
                        environment
                            .iter()
                            .map(|(key, value)| (key.as_str(), value.as_str())),
                    ))
                    .to_value())
            }
        }
        Op::EmptyVariables => {
            bind_styled(style, name, &[], args, heap)?;
            Ok(crate::cc_variables::empty(eval))
        }
        Op::CcToolchainFeatures => {
            const F: &str = "cc_toolchain_features";
            let bound = bind(
                F,
                &keyword(&["toolchain_config_info", "tools_directory"]),
                args,
                heap,
            )?;
            let config = bound[0].expect("required");
            if !is_starlark_info(config) {
                return Err(type_error(F, "toolchain_config_info", config, "struct"));
            }
            let tools_directory = string(F, "tools_directory", bound[1].expect("required"))?;
            let data = toolchain_from_config_info(
                config,
                tools_directory,
                heap,
                crate::loading::repositories(eval),
            )?;
            Ok(heap.alloc(CcToolchainFeatures {
                inner: Arc::new(FeaturesInner {
                    data: Arc::new(data),
                    cache: Mutex::new(HashMap::new()),
                }),
            }))
        }
        Op::CcToolchainVariables => {
            const F: &str = "cc_toolchain_variables";
            let bound = bind(F, &keyword(&["vars"]), args, heap)?;
            let vars = bound[0].expect("required");
            let Some(dict) = DictRef::from_value(vars) else {
                return Err(type_error(F, "vars", vars, "dict"));
            };
            let mut entries = Vec::with_capacity(dict.len());
            for (key, value) in dict.iter() {
                let Some(key) = key.unpack_str() else {
                    // Dict.cast (net/starlark/java/eval/Dict.java:659-684) with
                    // Starlark.classType(Object.class) = "unknown" (Starlark.java:386-391).
                    return Err(failure(format!(
                        "got dict<{}, {}> for 'vars', want dict<string, unknown>",
                        key.get_type(),
                        value.get_type()
                    )));
                };
                entries.push((key.to_owned(), value));
            }
            drop(dict);
            Ok(crate::cc_variables::alloc_map(eval, None, entries))
        }
        Op::CombineCcToolchainVariables => {
            const F: &str = "combine_cc_toolchain_variables";
            if let Some((key, _)) = args.names_map()?.iter().next() {
                return Err(failure(format!(
                    "{F}() got unexpected keyword argument '{}'",
                    key.as_str()
                )));
            }
            let positions: Vec<Value<'v>> = args.positions(heap)?.collect();
            let Some((parent, rest)) = positions.split_first() else {
                return Err(failure(format!(
                    "{F}() missing 1 required positional argument: parent"
                )));
            };
            if !crate::cc_variables::is_variables(*parent) {
                return Err(type_error(F, "parent", *parent, "Variables"));
            }
            for (index, value) in rest.iter().enumerate() {
                if !crate::cc_variables::is_variables(*value) {
                    return Err(element_error(index, "variables", *value, "Variables"));
                }
            }
            crate::cc_variables::combine(eval, *parent, rest)
        }
        Op::InternStringSequenceVariableValue => {
            const F: &str = "intern_string_sequence_variable_value";
            let bound = bind(
                F,
                &[param("string_sequence", true, false, true)],
                args,
                heap,
            )?;
            let value = bound[0].expect("required");
            let items = match ListRef::from_value(value) {
                Some(list) => list.iter().collect::<Vec<_>>(),
                None => match TupleRef::from_value(value) {
                    Some(tuple) => tuple.iter().collect(),
                    None if value.get_type() == "range" => value.iterate(heap)?.collect(),
                    None => return Err(type_error(F, "string_sequence", value, "sequence")),
                },
            };
            let strings = cast_strings(&items, "string_sequence")?;
            Ok(frozen_string_list(eval, &strings))
        }
        Op::ExecOs => {
            const F: &str = "exec_os";
            let bound = bind(F, &[param("ctx", true, false, true)], args, heap)?;
            let ctx = bound[0].expect("required");
            if ctx.get_type() != "ctx" {
                return Err(type_error(F, "ctx", ctx, "ctx"));
            }
            Ok(heap.alloc(exec_os()))
        }
    }
}

/// `getOsFromConstraintsOrHost(executionPlatform).name()`
/// (CcStarlarkInternal.java:497-502). The native profile's only execution
/// platform is the host (docs/TOOLCHAINS.md), so this is `OS.getCurrent()`.
fn exec_os() -> &'static str {
    match std::env::consts::OS {
        "linux" => "LINUX",
        "macos" => "DARWIN",
        "windows" => "WINDOWS",
        "freebsd" => "FREEBSD",
        "openbsd" => "OPENBSD",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests;
