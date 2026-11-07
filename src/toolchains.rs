//! Native toolchain resolution (increment N17a): `toolchain_type` and
//! `toolchain` providers, registered toolchains, and the resolution of a
//! rule's toolchain types against the target platform and the (host-only)
//! execution platforms.
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
//! crate::engine loads and analyzes the targets (type lookup, registered
//! toolchain collection, platform resolution); this module holds the pure
//! data and algorithms (docs/TOOLCHAINS.md).
use std::sync::Arc;

use crate::configured_attributes::{ConfigMatching, MatchResult};
use crate::label::Label;
use crate::platforms::{AttributeError, ConstraintCollection, ConstraintValueInfo, PlatformInfo};

fn shown(label: &Label) -> String {
    crate::rule_declarations::canonical_form(label)
}

/// The repository RepositoryName.createUnvalidated gives an apparent name the
/// requesting repository does not see (cmdline/RepositoryName.java):
/// `[unknown repo 'R' requested from @@C]` (crate::label::unknown_repository).
pub fn unknown_repository(name: &str, requester: &str) -> String {
    crate::label::unknown_repository(name, requester, "")
}

/// The apparent name and the requesting repository of an
/// [`unknown_repository`] name (any did-you-mean suffix ignored).
pub fn unknown_repository_parts(repository: &str) -> Option<(&str, &str)> {
    crate::label::unresolvable_repository(repository).map(|parts| (parts.name, parts.requester))
}

/// Label.parseWithRepoContext of label text whose apparent repository the
/// requesting repository does not see: the label is kept with an
/// unresolvable repository, so `rule(toolchains = ["@nope//:t"])` fails only
/// when it is resolved (paired capture B4/resolution_errors/unknown_repo).
/// Callers with a RepositorySet use RepositorySet::parse_deferred, which
/// also computes the did-you-mean suffix.
pub fn deferred_label_text(text: &str, requester: &str) -> Option<String> {
    crate::label::deferred_label(text, requester, "").map(|label| label.to_string())
}

/// A canonical label string, including [`deferred_label_text`]s.
pub fn parse_label(text: &str) -> Result<Label, String> {
    Label::parse_canonical(text)
}

/// ToolchainTypeRequirement (analysis/config/ToolchainTypeRequirement.java):
/// a requested toolchain type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolchainTypeRequirement {
    pub label: Label,
    pub mandatory: bool,
    /// Set by Bazel only for genrule's `toolchains` attribute
    /// (skyframe/toolchains/ToolchainContextUtil.java:93-117): entries that
    /// are not toolchain types are ignored (crate::engine).
    pub ignore_if_invalid: bool,
}

/// ToolchainTypeInfo (analysis/platform/ToolchainTypeInfo.java): the
/// provider of a `toolchain_type` target (rules/ToolchainType.java:40-52).
/// Equal when both the label and the error are (`:95-104`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolchainTypeInfo {
    pub type_label: Label,
    /// `no_match_error`, `None` when empty.
    pub no_match_error: Option<String>,
}

impl ToolchainTypeInfo {
    /// ToolchainType.create (rules/ToolchainType.java:40-52).
    pub fn new(type_label: Label, no_match_error: &str) -> Self {
        Self {
            type_label,
            no_match_error: (!no_match_error.is_empty()).then(|| no_match_error.to_owned()),
        }
    }
}

/// DeclaredToolchainInfo (analysis/platform/DeclaredToolchainInfo.java:42-57):
/// the provider of a `toolchain` target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredToolchainInfo {
    pub toolchain_type: ToolchainTypeInfo,
    pub exec: ConstraintCollection,
    pub target: ConstraintCollection,
    /// `use_target_platform_constraints` (hasTargetToExecConstraints, :59-64).
    pub use_target_platform: bool,
    /// The `target_settings` ConfigMatchingProviders.
    pub settings: Vec<ConfigMatching>,
    /// The `toolchain` target itself.
    pub target_label: Label,
    /// The NODEP `toolchain` attribute (the implementation).
    pub resolved: Label,
}

/// The validated prerequisites Toolchain.create reads
/// (rules/platform/Toolchain.java:44-62).
#[derive(Clone, Debug)]
pub struct ToolchainAttributes {
    pub toolchain_type: ToolchainTypeInfo,
    pub exec_compatible_with: Vec<ConstraintValueInfo>,
    pub target_compatible_with: Vec<ConstraintValueInfo>,
    pub use_target_platform_constraints: bool,
    pub target_settings: Vec<ConfigMatching>,
    pub toolchain: Label,
}

/// Toolchain.create (rules/platform/Toolchain.java:42-110) with
/// DeclaredToolchainInfo.Builder.build (DeclaredToolchainInfo.java:150-180):
/// `use_target_platform_constraints` excludes the constraint attributes;
/// duplicate constraints are reported per attribute (both when both have
/// them).
pub fn declare_toolchain(
    label: &Label,
    attributes: ToolchainAttributes,
) -> Result<DeclaredToolchainInfo, Vec<AttributeError>> {
    if attributes.use_target_platform_constraints
        && !(attributes.exec_compatible_with.is_empty()
            && attributes.target_compatible_with.is_empty())
    {
        return Err(vec![AttributeError {
            attribute: "use_target_platform_constraints",
            message: "Cannot set use_target_platform_constraints to True and also set exec_compatible_with or target_compatible_with".to_owned(),
        }]);
    }
    let (exec, target) = if attributes.use_target_platform_constraints {
        // buildWithTargetToExecConstraints (:182-190): empty collections
        // marked by identity.
        (
            ConstraintCollection::default(),
            ConstraintCollection::default(),
        )
    } else {
        let exec = ConstraintCollection::build(None, &attributes.exec_compatible_with);
        let target = ConstraintCollection::build(None, &attributes.target_compatible_with);
        let mut errors = Vec::new();
        if let Err(message) = &exec {
            errors.push(AttributeError {
                attribute: "exec_compatible_with",
                message: message.clone(),
            });
        }
        if let Err(message) = &target {
            errors.push(AttributeError {
                attribute: "target_compatible_with",
                message: message.clone(),
            });
        }
        match (exec, target) {
            (Ok(exec), Ok(target)) => (exec, target),
            _ => return Err(errors),
        }
    };
    Ok(DeclaredToolchainInfo {
        toolchain_type: attributes.toolchain_type,
        exec,
        target,
        use_target_platform: attributes.use_target_platform_constraints,
        settings: attributes.target_settings,
        target_label: label.clone(),
        resolved: attributes.toolchain,
    })
}

/// ConfigMatchingUtil.validate (skyframe/toolchains/ConfigMatchingUtil.java:42-82)
/// for `target_settings`: whether every setting matches; settings in error
/// are `Unrecoverable errors resolving config_setting associated with L:
/// For config_setting NAME: E1, E2; ...`.
pub fn target_settings_match(
    toolchain: &Label,
    settings: &[ConfigMatching],
) -> Result<bool, String> {
    let mut errors: Vec<(String, Vec<String>)> = Vec::new();
    let mut success = true;
    for setting in settings {
        match &setting.result {
            MatchResult::Match => {}
            MatchResult::NoMatch(_) => success = false,
            MatchResult::InError(messages) => {
                match errors
                    .iter_mut()
                    .find(|(name, _)| *name == setting.label.name)
                {
                    Some((_, seen)) => seen.extend(messages.iter().cloned()),
                    None => errors.push((setting.label.name.clone(), messages.clone())),
                }
            }
        }
    }
    if !errors.is_empty() {
        return Err(format!(
            "Unrecoverable errors resolving config_setting associated with {}: {}",
            shown(toolchain),
            errors
                .iter()
                .map(|(name, messages)| format!(
                    "For config_setting {name}: {}",
                    messages.join(", ")
                ))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    Ok(success)
}

/// RegisteredToolchainsValue (skyframe/toolchains/RegisteredToolchainsValue.java):
/// the valid registered toolchains of a configuration, in registration order.
#[derive(Clone, Debug, Default)]
pub struct RegisteredToolchains {
    pub toolchains: Vec<Arc<DeclaredToolchainInfo>>,
}

impl RegisteredToolchains {
    /// The toolchains of one type, in registration order
    /// (SingleToolchainResolutionFunction.java:161-168).
    pub fn by_type<'a>(
        &'a self,
        type_label: &'a Label,
    ) -> impl Iterator<Item = &'a Arc<DeclaredToolchainInfo>> + 'a {
        self.toolchains
            .iter()
            .filter(move |toolchain| toolchain.toolchain_type.type_label == *type_label)
    }
}

/// ToolchainContextKey (skyframe/toolchains/ToolchainContextKey.java): what
/// a rule's default toolchain context resolves. The configuration and the
/// target platform are the engine's; the only execution platform is the
/// host platform (docs/TOOLCHAINS.md).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolchainContextKey {
    pub types: Vec<ToolchainTypeRequirement>,
    /// `rule(exec_compatible_with = ...)` labels
    /// (ToolchainContextUtil.getExecutionPlatformConstraints, :147-165).
    pub exec_constraints: Vec<Label>,
    /// The target is a toolchain implementation: its parent's execution
    /// platform (the PlatformInfo label) is forced (ToolchainContextUtil.java:136-141).
    pub forced_exec: Option<Label>,
}

impl ToolchainContextKey {
    /// Whether resolution needs any platform: toolchain types or execution
    /// constraints.
    pub fn needs_platforms(&self) -> bool {
        !self.types.is_empty() || !self.exec_constraints.is_empty()
    }
}

/// PlatformConstants.DEFAULT_TEST_TOOLCHAIN_TYPE
/// (analysis/platform/PlatformConstants.java:27-28).
pub const DEFAULT_TEST_TOOLCHAIN_TYPE: &str =
    "@@bazel_tools//tools/test:default_test_toolchain_type";

/// The ToolchainContextKey of RuleClass.DEFAULT_TEST_RUNNER_EXEC_GROUP
/// (packages/RuleClass.java:153-167): the optional default test toolchain
/// type, no execution constraints, no forced execution platform
/// (UnloadedToolchainContextsProducer.java:110-124).
pub fn default_test_exec_group_key() -> ToolchainContextKey {
    ToolchainContextKey {
        types: vec![ToolchainTypeRequirement {
            label: Label::parse_canonical(DEFAULT_TEST_TOOLCHAIN_TYPE)
                .expect("canonical default test toolchain type"),
            mandatory: false,
            ignore_if_invalid: false,
        }],
        exec_constraints: Vec::new(),
        forced_exec: None,
    }
}

/// UnloadedToolchainContextImpl (skyframe/toolchains/UnloadedToolchainContextImpl.java).
#[derive(Clone, Debug)]
pub struct UnloadedToolchainContext {
    pub target_platform: Arc<PlatformInfo>,
    pub exec_platform: Arc<PlatformInfo>,
    /// The requirements with alias-resolved type labels.
    pub toolchain_types: Vec<ToolchainTypeRequirement>,
    /// requestedLabelToToolchainType: each requested label and, when it
    /// differs, the resolved type label, in request order
    /// (ToolchainTypeLookupUtil.java:76-85).
    pub requested: Vec<(Label, ToolchainTypeInfo)>,
    /// toolchainTypeToResolved for the selected execution platform.
    pub resolved: Vec<(ToolchainTypeInfo, Label)>,
}

impl UnloadedToolchainContext {
    /// The distinct implementation labels, in type order.
    pub fn resolved_labels(&self) -> Vec<Label> {
        let mut labels: Vec<Label> = Vec::new();
        for (_, label) in &self.resolved {
            if !labels.contains(label) {
                labels.push(label.clone());
            }
        }
        labels
    }
}

/// PlatformConfiguration.toolchainResolutionDebugFilter
/// (analysis/PlatformConfiguration.java:125-142): the canonical form,
/// `Pattern.quote`d when it has a regex special character.
pub fn debug_filter(label: &Label) -> String {
    let canonical = shown(label);
    if canonical.contains(['+', '.', '|', '(', '[', '{', '^', '$', '?', '\\', '*']) {
        // java.util.regex.Pattern.quote.
        if !canonical.contains("\\E") {
            return format!("\\Q{canonical}\\E");
        }
        let mut quoted = String::from("\\Q");
        let mut rest = canonical.as_str();
        while let Some(index) = rest.find("\\E") {
            quoted.push_str(&rest[..index]);
            quoted.push_str("\\E\\\\E\\Q");
            rest = &rest[index + 2..];
        }
        quoted.push_str(rest);
        quoted.push_str("\\E");
        return quoted;
    }
    canonical
}

/// SingleToolchainResolutionFunction.checkConstraints (:262-293): a mismatch
/// counts only for settings the toolchain sets itself (hasWithoutDefault);
/// the platform side may match through a setting's default.
fn constraints_match(toolchain: &ConstraintCollection, platform: &PlatformInfo) -> bool {
    toolchain
        .diff(&platform.constraints)
        .iter()
        .all(|setting| !toolchain.has_without_default(setting))
}

/// Whether `toolchain` matches `target_platform` and could execute on
/// `exec_platform` (single_resolution's constraint checks for one
/// toolchain; allowed toolchain types and target settings aside).
pub fn compatible(
    toolchain: &DeclaredToolchainInfo,
    target_platform: &PlatformInfo,
    exec_platform: &PlatformInfo,
) -> bool {
    if !toolchain.use_target_platform && !constraints_match(&toolchain.target, target_platform) {
        return false;
    }
    let exec_constraints = if toolchain.use_target_platform {
        target_platform.constraints.as_ref()
    } else {
        &toolchain.exec
    };
    constraints_match(exec_constraints, exec_platform)
}

/// SingleToolchainResolutionFunction.resolveConstraints (:149-260): the
/// toolchain implementation per execution platform (index into
/// `exec_platforms`) for one type, toolchains in registration order.
pub fn single_resolution(
    type_label: &Label,
    registered: &RegisteredToolchains,
    target_platform: &PlatformInfo,
    exec_platforms: &[Arc<PlatformInfo>],
) -> Vec<(usize, Label)> {
    single_resolution_traced(
        type_label,
        registered,
        target_platform,
        exec_platforms,
        None,
    )
}

/// `--toolchain_resolution_debug` (PlatformOptions.toolchainResolutionDebug,
/// a RegexFilter, default `-.*`): which toolchain types and targets print
/// resolution debug messages (PlatformConfiguration.debugToolchainResolution,
/// analysis/PlatformConfiguration.java:105-121). `None`: the default.
static DEBUG_FILTER: std::sync::RwLock<Option<crate::coverage::RegexFilter>> =
    std::sync::RwLock::new(None);

/// Set this command's `--toolchain_resolution_debug` filter.
pub fn set_debug_filter(filter: Option<crate::coverage::RegexFilter>) {
    *DEBUG_FILTER
        .write()
        .unwrap_or_else(|error| error.into_inner()) = filter;
}

/// PlatformConfiguration.debugToolchainResolution(labels): any canonical
/// form included; no label tests the empty string.
pub fn debug_resolution(labels: &[&Label]) -> bool {
    let guard = DEBUG_FILTER
        .read()
        .unwrap_or_else(|error| error.into_inner());
    let Some(filter) = guard.as_ref() else {
        return false;
    };
    if labels.is_empty() {
        return filter.is_included("");
    }
    labels.iter().any(|label| filter.is_included(&shown(label)))
}

/// The debug events printed so far (process-wide: one build per process).
static PRINTED: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
    std::sync::Mutex::new(None);

/// The printed debug events (BAZEL_RS_EVAL_VERIFY restores them after its
/// shadow analysis, so the verified engine prints what it would alone).
pub(crate) fn printed_debug_events() -> Option<std::collections::HashSet<String>> {
    PRINTED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

pub(crate) fn restore_printed_debug_events(printed: Option<std::collections::HashSet<String>>) {
    *PRINTED.lock().unwrap_or_else(|error| error.into_inner()) = printed;
}

/// One INFO event (crate::ui notes), printed once per identical text:
/// Bazel evaluates each resolution SkyKey once per build.
fn debug_event(text: String) {
    let mut printed = PRINTED.lock().unwrap_or_else(|error| error.into_inner());
    if printed
        .get_or_insert_with(Default::default)
        .insert(text.clone())
    {
        crate::ui::note(&format!("INFO: {text}"));
    }
}

/// ToolchainResolutionDebugPrinter.reportRemovedExecutionPlatform.
pub fn removed_execution_platform(platform: &Label, constraint: &Label) {
    debug_event(format!(
        "ToolchainResolution: Removed execution platform {} from available execution platforms, it is missing constraint {}",
        shown(platform),
        shown(constraint)
    ));
}

/// SingleToolchainResolutionDebugPrinter.EventHandlerImpl's trace lines.
struct Trace(Vec<String>);

impl Trace {
    /// debugMessage: the first line starts `ToolchainResolution: `, the
    /// others are padded by `"INFO: ".length()`; three indent levels.
    fn line(&mut self, indent: usize, text: String) {
        let padding = if self.0.is_empty() { "" } else { "      " };
        self.0.push(format!(
            "{padding}ToolchainResolution: {}{text}",
            "  ".repeat(indent)
        ));
    }

    /// reportMismatchedSettings (:233-275) for a failed checkConstraints.
    fn mismatch(
        &mut self,
        toolchain: &ConstraintCollection,
        platform: &PlatformInfo,
        target_platform: bool,
        declared: &DeclaredToolchainInfo,
    ) {
        let settings: Vec<_> = toolchain
            .diff(&platform.constraints)
            .into_iter()
            .filter(|setting| toolchain.has_without_default(setting))
            .collect();
        if settings.is_empty() {
            return;
        }
        let values = settings
            .iter()
            .filter(|setting| toolchain.has(setting))
            .filter_map(|setting| toolchain.get(setting).map(|value| value.label.name))
            .collect::<Vec<_>>()
            .join(", ");
        let missing = settings
            .iter()
            .filter(|setting| !toolchain.has(setting))
            .map(|setting| setting.label.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let mut detail = String::new();
        if !values.is_empty() {
            detail.push_str(&format!("; mismatching values: {values}"));
        }
        if !missing.is_empty() {
            detail.push_str(&format!("; missing: {missing}"));
        }
        if target_platform {
            self.line(
                1,
                format!(
                    "Rejected toolchain {} (resolves to {}) {detail}",
                    shown(&declared.target_label),
                    shown(&declared.resolved)
                ),
            );
        } else {
            self.line(
                2,
                format!(
                    "Incompatible execution platform {}{detail}",
                    shown(&platform.label)
                ),
            );
        }
    }
}

/// [`single_resolution`] with SingleToolchainResolutionFunction's debug
/// trace (SingleToolchainResolutionDebugPrinter) when `trace` is given.
fn single_resolution_traced(
    type_label: &Label,
    registered: &RegisteredToolchains,
    target_platform: &PlatformInfo,
    exec_platforms: &[Arc<PlatformInfo>],
    mut trace: Option<&mut Trace>,
) -> Vec<(usize, Label)> {
    if let Some(trace) = trace.as_deref_mut() {
        trace.line(
            0,
            format!(
                "Performing resolution of {} for target platform {}",
                shown(type_label),
                shown(&target_platform.label)
            ),
        );
    }
    let mut found: Vec<(usize, Label)> = Vec::new();
    for toolchain in registered.by_type(type_label) {
        // A toolchain with use_target_platform_constraints matches any
        // target platform (:171-181).
        if !toolchain.use_target_platform && !constraints_match(&toolchain.target, target_platform)
        {
            if let Some(trace) = trace.as_deref_mut() {
                trace.mismatch(&toolchain.target, target_platform, true, toolchain);
            }
            continue;
        }
        if let Some(trace) = trace.as_deref_mut() {
            trace.line(
                1,
                format!(
                    "Toolchain {} (resolves to {}) is compatible with target platform, searching for execution platforms:",
                    shown(&toolchain.target_label),
                    shown(&toolchain.resolved)
                ),
            );
        }
        let mut done = true;
        for (index, exec) in exec_platforms.iter().enumerate() {
            if found.iter().any(|(seen, _)| *seen == index) {
                if let Some(trace) = trace.as_deref_mut() {
                    trace.line(
                        2,
                        format!(
                            "Skipping execution platform {}; it has already selected a toolchain",
                            shown(&exec.label)
                        ),
                    );
                }
                continue;
            }
            // allowed_toolchain_types (:195-205).
            if exec.check_toolchain_types && !exec.allowed_toolchain_types.contains(type_label) {
                if let Some(trace) = trace.as_deref_mut() {
                    trace.line(
                        2,
                        format!(
                            "Skipping execution platform {}; its allowed toolchain types does not contain the current toolchain type {}",
                            shown(&exec.label),
                            shown(type_label)
                        ),
                    );
                }
                done = false;
                continue;
            }
            let exec_constraints = if toolchain.use_target_platform {
                target_platform.constraints.as_ref()
            } else {
                &toolchain.exec
            };
            if !constraints_match(exec_constraints, exec) {
                if let Some(trace) = trace.as_deref_mut() {
                    trace.mismatch(exec_constraints, exec, false, toolchain);
                }
                done = false;
                continue;
            }
            if let Some(trace) = trace.as_deref_mut() {
                trace.line(
                    2,
                    format!("Compatible execution platform {}", shown(&exec.label)),
                );
            }
            found.push((index, toolchain.resolved.clone()));
        }
        if done {
            if let Some(trace) = trace.as_deref_mut() {
                trace.line(
                    1,
                    format!(
                        "All execution platforms have been assigned a {} toolchain, stopping",
                        shown(type_label)
                    ),
                );
            }
            break;
        }
    }
    if let Some(trace) = trace {
        if found.is_empty() {
            trace.line(
                0,
                format!(
                    "No {} toolchain found for target platform {}.",
                    shown(type_label),
                    shown(&target_platform.label)
                ),
            );
        } else {
            trace.line(
                0,
                format!(
                    "Recap of selected {} toolchains for target platform {}:",
                    shown(type_label),
                    shown(&target_platform.label)
                ),
            );
            // ImmutableMap insertion order: the execution platform order.
            let mut recap = found.clone();
            recap.sort_by_key(|(index, _)| *index);
            for (index, label) in recap {
                trace.line(
                    1,
                    format!(
                        "Selected {} to run on execution platform {}",
                        shown(&label),
                        shown(&exec_platforms[index].label)
                    ),
                );
            }
        }
    }
    found
}

/// A resolved toolchain type: the requirement (with its resolved label) and
/// the type's provider (ToolchainResolutionFunction.ToolchainType, :133-145).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedType {
    pub requirement: ToolchainTypeRequirement,
    pub info: ToolchainTypeInfo,
}

/// ToolchainResolutionFunction.loadToolchainTypes (:163-187): the requested
/// requirements with their ToolchainTypeInfo (`None`: invalid but ignored)
/// become resolved types, deduplicated like an ImmutableSet.
pub fn resolved_types(
    lookups: &[(ToolchainTypeRequirement, Option<ToolchainTypeInfo>)],
) -> Vec<ResolvedType> {
    let mut types: Vec<ResolvedType> = Vec::new();
    for (requirement, info) in lookups {
        let Some(info) = info else {
            continue;
        };
        let resolved = ResolvedType {
            requirement: ToolchainTypeRequirement {
                label: info.type_label.clone(),
                ..requirement.clone()
            },
            info: info.clone(),
        };
        if !types.contains(&resolved) {
            types.push(resolved);
        }
    }
    types
}

/// requestedLabelToToolchainType (ToolchainTypeLookupUtil.java:76-85): each
/// requested label then the resolved type label, first position kept.
pub fn requested_types(
    lookups: &[(ToolchainTypeRequirement, Option<ToolchainTypeInfo>)],
) -> Vec<(Label, ToolchainTypeInfo)> {
    let mut requested: Vec<(Label, ToolchainTypeInfo)> = Vec::new();
    for (requirement, info) in lookups {
        let Some(info) = info else {
            continue;
        };
        for label in [&requirement.label, &info.type_label] {
            match requested.iter_mut().find(|(seen, _)| seen == label) {
                Some(entry) => entry.1 = info.clone(),
                None => requested.push((label.clone(), info.clone())),
            }
        }
    }
    requested
}

/// UnresolvedToolchainsException.getMessage (ToolchainResolutionFunction.java:381-414).
pub fn no_matching_toolchains(
    target_platform: &PlatformInfo,
    missing: &[ToolchainTypeInfo],
) -> String {
    let rows = missing
        .iter()
        .map(|info| {
            format!(
                "  {}{}",
                shown(&info.type_label),
                info.no_match_error
                    .as_ref()
                    .map(|error| format!(": {error}"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>();
    let filters = missing
        .iter()
        .map(|info| debug_filter(&info.type_label))
        .collect::<Vec<_>>();
    let mut message = format!(
        "No matching toolchains found for types:\n{}\nTo debug, rerun with --toolchain_resolution_debug='{}'",
        rows.join("\n"),
        filters.join("|"),
    );
    // The text block ends with a line break followed by the platform's
    // message; Bazel's event output drops the trailing line break of an
    // empty message (paired capture A5/resolution_errors/quiet_two).
    if let Some(error) = &target_platform.missing_toolchain_error {
        message.push('\n');
        message.push_str(error);
    }
    message
}

/// NoMatchingPlatformData.formatError (skyframe/toolchains/NoMatchingPlatformData.java:50-71).
pub fn no_matching_platform(
    types: &[ToolchainTypeRequirement],
    target_platform: &Label,
    exec_platforms: &[Arc<PlatformInfo>],
) -> String {
    let available = exec_platforms
        .iter()
        .map(|platform| shown(&platform.label))
        .collect::<Vec<_>>()
        .join(", ");
    if types.is_empty() {
        return format!(
            "Unable to find an execution platform for target platform {} from available execution platforms [{available}]",
            shown(target_platform)
        );
    }
    format!(
        "Unable to find an execution platform for toolchains [{}] and target platform {} from available execution platforms [{available}]",
        types
            .iter()
            .map(|requirement| shown(&requirement.label))
            .collect::<Vec<_>>()
            .join(", "),
        shown(target_platform)
    )
}

/// The platforms of a resolution: the target platform and the execution
/// platforms after the `exec_compatible_with` filter (PlatformKeys.java:78-224).
pub struct Platforms<'a> {
    pub target: &'a Arc<PlatformInfo>,
    pub exec: &'a [Arc<PlatformInfo>],
    /// The forced execution platform (an index into `exec`, or the target
    /// platform when it has the same label; PlatformKeys.find, :230-245).
    pub forced: Option<Arc<PlatformInfo>>,
    /// ToolchainContextKey.debugTarget: the target's label matches
    /// `--toolchain_resolution_debug` (ToolchainContextUtil.java:131).
    pub debug_target: bool,
}

/// ToolchainResolutionFunction.determineToolchainImplementations (:189-283)
/// and findExecutionPlatformForToolchains (:302-345) over resolved types.
pub fn resolve(
    types: &[ResolvedType],
    requested: Vec<(Label, ToolchainTypeInfo)>,
    platforms: Platforms<'_>,
    registered: &RegisteredToolchains,
) -> Result<UnloadedToolchainContext, String> {
    // Per type: implementation per execution platform index.
    let mut per_type: Vec<(ResolvedType, Vec<(usize, Label)>)> = Vec::new();
    let mut missing: Vec<ToolchainTypeInfo> = Vec::new();
    // The forced platform when it is not one of the execution platforms
    // (the target platform) is an extra candidate without toolchains: only
    // the execution platforms are resolved (SingleToolchainResolutionFunction).
    let forced_index = platforms.forced.as_ref().map(|forced| {
        platforms
            .exec
            .iter()
            .position(|exec| exec.label == forced.label)
            .unwrap_or(platforms.exec.len())
    });
    let mut candidates = platforms.exec.to_vec();
    if let (Some(index), Some(forced)) = (forced_index, &platforms.forced)
        && index == candidates.len()
    {
        candidates.push(forced.clone());
    }
    for resolved in types {
        let type_label = &resolved.requirement.label;
        let mut trace =
            (platforms.debug_target || debug_resolution(&[type_label])).then(|| Trace(Vec::new()));
        let found = single_resolution_traced(
            type_label,
            registered,
            platforms.target,
            platforms.exec,
            trace.as_mut(),
        );
        if let Some(trace) = trace {
            debug_event(trace.0.join("\n"));
        }
        if found.is_empty() && resolved.requirement.mandatory && !missing.contains(&resolved.info) {
            missing.push(resolved.info.clone());
        }
        per_type.push((resolved.clone(), found));
    }
    if !missing.is_empty() {
        return Err(no_matching_toolchains(platforms.target, &missing));
    }
    let resolved_on = |index: usize| {
        per_type
            .iter()
            .filter(|(_, found)| found.iter().any(|(seen, _)| *seen == index))
            .map(|(resolved, _)| resolved.info.clone())
            .collect::<Vec<_>>()
    };
    // PlatformKeys.isPlatformSuitable (PlatformKeys.java:265-294).
    let suitable = |index: usize, check_allowed: bool| {
        let platform = &candidates[index];
        if types.is_empty() {
            return !(check_allowed && platform.check_toolchain_types);
        }
        let available = resolved_on(index);
        types
            .iter()
            .filter(|resolved| resolved.requirement.mandatory)
            .all(|resolved| available.contains(&resolved.info))
    };
    let mut selected = forced_index.filter(|index| suitable(*index, false));
    if selected.is_none() {
        // Stream.max keeps the first of equal elements (BinaryOperator.maxBy).
        let mut best: Option<(usize, usize)> = None;
        for index in 0..platforms.exec.len() {
            if !suitable(index, true) {
                continue;
            }
            let count = resolved_on(index).len();
            if best.is_none_or(|(_, best_count)| count > best_count) {
                best = Some((index, count));
            }
        }
        selected = best.map(|(index, _)| index);
    }
    let toolchain_types = types
        .iter()
        .map(|resolved| resolved.requirement.clone())
        .collect::<Vec<_>>();
    let Some(selected) = selected else {
        return Err(no_matching_platform(
            &toolchain_types,
            &platforms.target.label,
            platforms.exec,
        ));
    };
    let mut resolved = Vec::new();
    for (resolved_type, found) in &per_type {
        if let Some((_, label)) = found.iter().find(|(index, _)| *index == selected)
            && !resolved
                .iter()
                .any(|(info, _): &(ToolchainTypeInfo, Label)| *info == resolved_type.info)
        {
            resolved.push((resolved_type.info.clone(), label.clone()));
        }
    }
    // ToolchainResolutionDebugPrinter.reportSelectedToolchains.
    let type_labels = types
        .iter()
        .map(|resolved| &resolved.requirement.label)
        .collect::<Vec<_>>();
    if platforms.debug_target || debug_resolution(&type_labels) {
        debug_event(format!(
            "ToolchainResolution: Target platform {}: Selected execution platform {}, {}",
            shown(&platforms.target.label),
            shown(&candidates[selected].label),
            resolved
                .iter()
                .map(|(info, label)| format!(
                    "type {} -> toolchain {}",
                    shown(&info.type_label),
                    shown(label)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(UnloadedToolchainContext {
        target_platform: platforms.target.clone(),
        exec_platform: candidates[selected].clone(),
        toolchain_types,
        requested,
        resolved,
    })
}

/// TargetNotToolchainException (analysis/ResolvedToolchainContext.java:139-153).
pub fn not_a_toolchain(type_label: &Label, implementation: &Label) -> String {
    format!(
        "toolchain type {} resolved to target {}, but that target does not provide ToolchainInfo",
        shown(type_label),
        shown(implementation)
    )
}

/// InvalidToolchainTypeException (skyframe/toolchains/ToolchainTypeLookupUtil.java:120-176).
pub fn invalid_toolchain_type(label: &Label, is_toolchain: bool) -> String {
    format!(
        "Target {} was referenced as a toolchain type, but {}",
        shown(label),
        if is_toolchain {
            "is a toolchain instance. Is the rule definition for the target you're building setting \"toolchains =\" to a toolchain() instead of the expected toolchain_type()?"
        } else {
            "does not provide ToolchainTypeInfo"
        }
    )
}

/// InvalidToolchainLabelException.formatMessage
/// (skyframe/toolchains/RegisteredToolchainsFunction.java:202-246).
pub fn invalid_registered_toolchain(pattern: &str, reason: &str) -> String {
    format!("invalid registered toolchain '{pattern}': {reason}")
}

/// InvalidConstraintValueException.formatError
/// (skyframe/toolchains/ConstraintValueLookupUtil.java:94-124).
pub fn not_a_constraint_value(label: &Label) -> String {
    format!(
        "Target {} was referenced as a constraint_value, but does not provide ConstraintValueInfo",
        shown(label)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configured_attributes::Diff;
    use crate::platforms::{ConstraintSettingInfo, PlatformAttributes};
    use std::collections::{BTreeMap, BTreeSet};

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

    fn platform(name: &str, values: Vec<ConstraintValueInfo>) -> Arc<PlatformInfo> {
        platform_with(name, values, |_| {})
    }

    fn platform_with(
        name: &str,
        values: Vec<ConstraintValueInfo>,
        change: impl FnOnce(&mut PlatformAttributes),
    ) -> Arc<PlatformInfo> {
        let mut attributes = PlatformAttributes {
            constraint_values: values,
            missing_toolchain_error: crate::platforms::DEFAULT_MISSING_TOOLCHAIN_ERROR.to_owned(),
            ..Default::default()
        };
        change(&mut attributes);
        Arc::new(
            crate::platforms::create_platform(&label(name), attributes, &|label| shown(label))
                .unwrap(),
        )
    }

    fn type_info(text: &str) -> ToolchainTypeInfo {
        ToolchainTypeInfo::new(label(text), "")
    }

    fn toolchain(
        name: &str,
        type_label: &str,
        exec: Vec<ConstraintValueInfo>,
        target: Vec<ConstraintValueInfo>,
        resolved: &str,
    ) -> Arc<DeclaredToolchainInfo> {
        Arc::new(
            declare_toolchain(
                &label(name),
                ToolchainAttributes {
                    toolchain_type: type_info(type_label),
                    exec_compatible_with: exec,
                    target_compatible_with: target,
                    use_target_platform_constraints: false,
                    target_settings: Vec::new(),
                    toolchain: label(resolved),
                },
            )
            .unwrap(),
        )
    }

    fn requirement(text: &str, mandatory: bool) -> ToolchainTypeRequirement {
        ToolchainTypeRequirement {
            label: label(text),
            mandatory,
            ignore_if_invalid: false,
        }
    }

    #[test]
    fn compatible_applies_single_resolution_checks_to_one_toolchain() {
        let os = setting("//os:os", None);
        let linux = value(&os, "//os:linux");
        let osx = value(&os, "//os:osx");
        let host = platform("//p:host", vec![linux.clone()]);
        let mac = platform("//p:mac", vec![osx.clone()]);
        let any = toolchain("//t:any", "//t:type", vec![], vec![], "//t:any_impl");
        let linux_only = toolchain(
            "//t:l",
            "//t:type",
            vec![],
            vec![linux.clone()],
            "//t:l_impl",
        );
        let exec_osx = toolchain("//t:e", "//t:type", vec![osx.clone()], vec![], "//t:e_impl");
        assert!(compatible(&any, &host, &host));
        assert!(compatible(&linux_only, &host, &host));
        assert!(!compatible(&linux_only, &mac, &host));
        assert!(!compatible(&exec_osx, &host, &host));
        assert!(compatible(&exec_osx, &host, &mac));
        // use_target_platform_constraints: the target platform's constraints
        // must hold on the execution platform.
        let mut follows = (*any).clone();
        follows.use_target_platform = true;
        assert!(compatible(&follows, &host, &host));
        assert!(!compatible(&follows, &mac, &host));
        // Every implementation single_resolution can pick is compatible.
        let registered = RegisteredToolchains {
            toolchains: vec![linux_only.clone(), exec_osx.clone(), any.clone()],
        };
        for target in [&host, &mac] {
            for (_, resolved) in single_resolution(
                &label("//t:type"),
                &registered,
                target,
                std::slice::from_ref(&host),
            ) {
                let declared = registered
                    .toolchains
                    .iter()
                    .find(|toolchain| toolchain.resolved == resolved)
                    .unwrap();
                assert!(compatible(declared, target, &host), "{resolved}");
            }
        }
    }

    #[test]
    fn diff_and_defaults_follow_constraint_collection() {
        let os = setting("//os:os", None);
        let libc = setting("//libc:libc", Some("//libc:unconstrained"));
        let toolchain =
            ConstraintCollection::build(None, &[value(&libc, "//libc:gnu.2.28")]).unwrap();
        let host = platform("//p:host", vec![value(&os, "//os:linux")]);
        // The platform's libc is the default (`unconstrained`), the
        // toolchain's is gnu.2.28: a mismatch the toolchain itself sets.
        let diff = toolchain.diff(&host.constraints);
        assert_eq!(
            diff.iter().map(|s| s.label.clone()).collect::<Vec<_>>(),
            [label("//libc:libc"), label("//os:os")]
        );
        assert!(toolchain.has_without_default(&libc));
        assert!(!toolchain.has_without_default(&os));
        assert!(!constraints_match(&toolchain, &host));
        // A platform setting with a non-default value does not reject a
        // toolchain that does not mention the setting (#8778).
        let any = ConstraintCollection::build(None, &[value(&os, "//os:linux")]).unwrap();
        let musl = platform(
            "//p:musl",
            vec![value(&os, "//os:linux"), value(&libc, "//libc:musl")],
        );
        assert!(constraints_match(&any, &musl));
        // The toolchain may match through the platform's default.
        let unconstrained =
            ConstraintCollection::build(None, &[value(&libc, "//libc:unconstrained")]).unwrap();
        assert!(constraints_match(&unconstrained, &host));
        // A parent counts for hasWithoutDefault.
        let child = ConstraintCollection {
            parent: Some(Arc::new(toolchain.clone())),
            values: Vec::new(),
        };
        assert!(child.has_without_default(&libc));
    }

    #[test]
    fn declared_toolchain_errors_follow_toolchain_create() {
        let os = setting("//os:os", None);
        let attributes =
            |exec: Vec<ConstraintValueInfo>, target: Vec<ConstraintValueInfo>, use_target: bool| {
                ToolchainAttributes {
                    toolchain_type: type_info("//t:type"),
                    exec_compatible_with: exec,
                    target_compatible_with: target,
                    use_target_platform_constraints: use_target,
                    target_settings: Vec::new(),
                    toolchain: label("//t:impl"),
                }
            };
        let errors = declare_toolchain(
            &label("//t:tc"),
            attributes(vec![value(&os, "//os:linux")], vec![], true),
        )
        .unwrap_err();
        assert_eq!(errors[0].attribute, "use_target_platform_constraints");
        assert_eq!(
            errors[0].message,
            "Cannot set use_target_platform_constraints to True and also set exec_compatible_with or target_compatible_with"
        );
        let both = vec![value(&os, "//os:linux"), value(&os, "//os:osx")];
        let errors =
            declare_toolchain(&label("//t:tc"), attributes(both.clone(), both, false)).unwrap_err();
        assert_eq!(
            errors,
            [
                AttributeError {
                    attribute: "exec_compatible_with",
                    message: "Duplicate constraint values detected: constraint_setting //os:os has [//os:linux, //os:osx]".into()
                },
                AttributeError {
                    attribute: "target_compatible_with",
                    message: "Duplicate constraint values detected: constraint_setting //os:os has [//os:linux, //os:osx]".into()
                }
            ]
        );
        let declared =
            declare_toolchain(&label("//t:tc"), attributes(vec![], vec![], true)).unwrap();
        assert!(declared.use_target_platform && declared.exec.values.is_empty());
        assert_eq!(declared.resolved, label("//t:impl"));
    }

    #[test]
    fn target_settings_follow_config_matching_util() {
        let matching = |name: &str, result: MatchResult| ConfigMatching {
            label: label(name),
            settings: Vec::new(),
            flags: BTreeMap::new(),
            constraints: BTreeSet::new(),
            result,
        };
        let tc = label("//t:tc");
        assert_eq!(target_settings_match(&tc, &[]), Ok(true));
        assert_eq!(
            target_settings_match(&tc, &[matching("//c:a", MatchResult::Match)]),
            Ok(true)
        );
        let no = MatchResult::NoMatch(vec![Diff {
            what: label("//c:f"),
            got: "a".into(),
            want: "b".into(),
        }]);
        assert_eq!(
            target_settings_match(
                &tc,
                &[
                    matching("//c:a", MatchResult::Match),
                    matching("//c:b", no.clone())
                ]
            ),
            Ok(false)
        );
        assert_eq!(
            target_settings_match(
                &tc,
                &[
                    matching("//c:a", no),
                    matching("//c:e", MatchResult::InError(vec!["x".into(), "y".into()])),
                    matching("//d:f", MatchResult::InError(vec!["z".into()])),
                ]
            )
            .unwrap_err(),
            "Unrecoverable errors resolving config_setting associated with //t:tc: For config_setting e: x, y; For config_setting f: z"
        );
    }

    #[test]
    fn single_resolution_follows_registration_order_and_platform_checks() {
        let os = setting("//os:os", None);
        let cpu = setting("//cpu:cpu", None);
        let linux = value(&os, "//os:linux");
        let osx = value(&os, "//os:osx");
        let x86 = value(&cpu, "//cpu:x86_64");
        let host = platform("//p:host", vec![linux.clone(), x86.clone()]);
        let mac = platform("//p:mac", vec![osx.clone()]);
        let registered = RegisteredToolchains {
            toolchains: vec![
                toolchain(
                    "//t:other",
                    "//t:other_type",
                    vec![],
                    vec![],
                    "//t:other_impl",
                ),
                toolchain(
                    "//t:mac",
                    "//t:type",
                    vec![],
                    vec![osx.clone()],
                    "//t:mac_impl",
                ),
                toolchain(
                    "//t:linux_exec_osx",
                    "//t:type",
                    vec![osx.clone()],
                    vec![],
                    "//t:bad",
                ),
                toolchain(
                    "//t:linux",
                    "//t:type",
                    vec![linux.clone()],
                    vec![],
                    "//t:linux_impl",
                ),
                toolchain("//t:any", "//t:type", vec![], vec![], "//t:any_impl"),
            ],
        };
        let found = single_resolution(
            &label("//t:type"),
            &registered,
            &host,
            std::slice::from_ref(&host),
        );
        assert_eq!(found, [(0, label("//t:linux_impl"))]);
        let found = single_resolution(
            &label("//t:type"),
            &registered,
            &mac,
            std::slice::from_ref(&host),
        );
        assert_eq!(found, [(0, label("//t:mac_impl"))]);
        // allowed_toolchain_types.
        let strict = platform_with("//p:strict", vec![linux.clone()], |a| {
            a.check_toolchain_types = true;
            a.allowed_toolchain_types = vec![label("//t:other_type")];
        });
        assert!(
            single_resolution(
                &label("//t:type"),
                &registered,
                &host,
                std::slice::from_ref(&strict)
            )
            .is_empty()
        );
        assert_eq!(
            single_resolution(&label("//t:other_type"), &registered, &host, &[strict]),
            [(0, label("//t:other_impl"))]
        );
        // use_target_platform_constraints: exec constraints are the target
        // platform's.
        let utp = Arc::new(
            declare_toolchain(
                &label("//t:utp"),
                ToolchainAttributes {
                    toolchain_type: type_info("//t:u"),
                    exec_compatible_with: vec![],
                    target_compatible_with: vec![],
                    use_target_platform_constraints: true,
                    target_settings: vec![],
                    toolchain: label("//t:utp_impl"),
                },
            )
            .unwrap(),
        );
        let registered = RegisteredToolchains {
            toolchains: vec![utp],
        };
        assert_eq!(
            single_resolution(
                &label("//t:u"),
                &registered,
                &host,
                std::slice::from_ref(&host)
            )
            .len(),
            1
        );
        assert!(
            single_resolution(
                &label("//t:u"),
                &registered,
                &mac,
                std::slice::from_ref(&host)
            )
            .is_empty()
        );
    }

    #[test]
    fn resolution_errors_and_execution_platform_selection() {
        let os = setting("//os:os", None);
        let linux = value(&os, "//os:linux");
        let host = platform("//p:host", vec![linux.clone()]);
        let registered = RegisteredToolchains {
            toolchains: vec![toolchain(
                "//t:a",
                "//t:a_type",
                vec![],
                vec![],
                "//t:a_impl",
            )],
        };
        let lookups = vec![
            (
                requirement("//t:alias", true),
                Some(type_info("//t:a_type")),
            ),
            (requirement("//t:opt", false), Some(type_info("//t:opt"))),
        ];
        let types = resolved_types(&lookups);
        let requested = requested_types(&lookups);
        assert_eq!(
            requested.iter().map(|(l, _)| shown(l)).collect::<Vec<_>>(),
            ["//t:alias", "//t:a_type", "//t:opt"]
        );
        let context = resolve(
            &types,
            requested,
            Platforms {
                target: &host,
                exec: std::slice::from_ref(&host),
                forced: None,
                debug_target: false,
            },
            &registered,
        )
        .unwrap();
        assert_eq!(
            context.resolved,
            [(type_info("//t:a_type"), label("//t:a_impl"))]
        );
        assert_eq!(context.resolved_labels(), [label("//t:a_impl")]);
        assert_eq!(context.toolchain_types[0].label, label("//t:a_type"));
        // Missing mandatory types.
        let missing = vec![
            (
                requirement("//t:m1", true),
                Some(ToolchainTypeInfo::new(label("//t:m1"), "install m1")),
            ),
            (
                requirement("@@m+//t:m2", true),
                Some(type_info("@@m+//t:m2")),
            ),
        ];
        let error = resolve(
            &resolved_types(&missing),
            requested_types(&missing),
            Platforms {
                target: &host,
                exec: std::slice::from_ref(&host),
                forced: None,
                debug_target: false,
            },
            &registered,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "No matching toolchains found for types:\n  //t:m1: install m1\n  @@m+//t:m2\nTo debug, rerun with --toolchain_resolution_debug='//t:m1|\\Q@@m+//t:m2\\E'\nFor more information on platforms or toolchains see https://bazel.build/concepts/platforms-intro."
        );
        // A platform without a missing_toolchain_error.
        let quiet = platform_with("//p:quiet", vec![], |a| {
            a.missing_toolchain_error = String::new()
        });
        assert!(
            resolve(
                &resolved_types(&missing[..1]),
                vec![],
                Platforms {
                    target: &quiet,
                    exec: std::slice::from_ref(&host),
                    forced: None,
                    debug_target: false,
                },
                &registered,
            )
            .unwrap_err()
            .ends_with("--toolchain_resolution_debug='//t:m1'")
        );
        // No execution platform left (exec_compatible_with filtered it).
        assert_eq!(
            resolve(
                &[],
                vec![],
                Platforms {
                    target: &host,
                    exec: &[],
                    forced: None,
                    debug_target: false,
                },
                &registered,
            )
            .unwrap_err(),
            "Unable to find an execution platform for target platform //p:host from available execution platforms []"
        );
        // check_toolchain_types without types: unsuitable unless forced.
        let strict = platform_with("//p:strict", vec![], |a| a.check_toolchain_types = true);
        let platforms = |forced: Option<Arc<PlatformInfo>>| Platforms {
            debug_target: false,
            target: &host,
            exec: std::slice::from_ref(&strict),
            forced,
        };
        assert_eq!(
            resolve(&[], vec![], platforms(None), &registered).unwrap_err(),
            "Unable to find an execution platform for target platform //p:host from available execution platforms [//p:strict]"
        );
        let forced = resolve(&[], vec![], platforms(Some(strict.clone())), &registered).unwrap();
        assert_eq!(forced.exec_platform.label, label("//p:strict"));
        // Types whose toolchains need another execution platform.
        let osx = value(&os, "//os:osx");
        let registered = RegisteredToolchains {
            toolchains: vec![toolchain(
                "//t:a",
                "//t:a_type",
                vec![osx],
                vec![],
                "//t:a_impl",
            )],
        };
        let optional = vec![(
            requirement("//t:a_type", false),
            Some(type_info("//t:a_type")),
        )];
        let context = resolve(
            &resolved_types(&optional),
            requested_types(&optional),
            Platforms {
                target: &host,
                exec: std::slice::from_ref(&host),
                forced: None,
                debug_target: false,
            },
            &registered,
        )
        .unwrap();
        assert!(context.resolved.is_empty());
        assert_eq!(debug_filter(&label("//a:b")), "//a:b");
        assert_eq!(debug_filter(&label("//a:b.c")), "\\Q//a:b.c\\E");
    }

    #[test]
    fn deferred_unknown_repository_labels() {
        let text = deferred_label_text("@nope//pkg:t", "").unwrap();
        assert_eq!(text, "@@[unknown repo 'nope' requested from @@]//pkg:t");
        let deferred = parse_label(&text).unwrap();
        assert_eq!(
            unknown_repository_parts(&deferred.repository),
            Some(("nope", ""))
        );
        assert_eq!(shown(&deferred), text);
        assert_eq!(
            deferred_label_text("@x//:t", "m+").unwrap(),
            "@@[unknown repo 'x' requested from @@m+]//:t"
        );
        assert!(deferred_label_text("@@x//:t", "").is_none());
        assert_eq!(parse_label("//a:b").unwrap(), label("//a:b"));
    }

    #[test]
    fn messages_follow_bazel() {
        assert_eq!(
            not_a_toolchain(&label("//t:type"), &label("//t:alias")),
            "toolchain type //t:type resolved to target //t:alias, but that target does not provide ToolchainInfo"
        );
        assert_eq!(
            invalid_toolchain_type(&label("//t:fg"), false),
            "Target //t:fg was referenced as a toolchain type, but does not provide ToolchainTypeInfo"
        );
        assert!(invalid_toolchain_type(&label("//t:tc"), true).contains("is a toolchain instance"));
        assert_eq!(
            invalid_registered_toolchain(
                "//t:fg",
                "target does not provide the DeclaredToolchainInfo provider"
            ),
            "invalid registered toolchain '//t:fg': target does not provide the DeclaredToolchainInfo provider"
        );
        assert_eq!(
            not_a_constraint_value(&label("//t:fg")),
            "Target //t:fg was referenced as a constraint_value, but does not provide ConstraintValueInfo"
        );
        assert_eq!(
            no_matching_platform(
                &[requirement("//t:a", true), requirement("//t:b", false)],
                &label("//p:t"),
                &[platform("//p:host", vec![])]
            ),
            "Unable to find an execution platform for toolchains [//t:a, //t:b] and target platform //p:t from available execution platforms [//p:host]"
        );
    }
}
