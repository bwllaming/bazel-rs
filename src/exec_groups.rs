//! Exec groups (increment N35, docs/EXEC_GROUPS.md): the processed exec
//! groups of a rule (packages/DeclaredExecGroup.java), the per-group exec
//! properties (analysis/ExecGroupCollection.java), `ctx.exec_groups`
//! (analysis/starlark/StarlarkExecGroupCollection.java) and the exec group an
//! action runs in (analysis/starlark/StarlarkActionFactory.java).
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::environment::{Methods, MethodsBuilder};
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, ValueLike,
    starlark_value,
};
use starlark::{starlark_module, starlark_simple_value};
use std::fmt;
use std::sync::Arc;

use crate::label::Label;
use crate::toolchain_info::ToolchainContextData;

/// DeclaredExecGroup.DEFAULT_EXEC_GROUP_NAME (packages/DeclaredExecGroup.java:55-57).
pub const DEFAULT_EXEC_GROUP_NAME: &str = "default-exec-group";

/// RuleClass.DEFAULT_TEST_RUNNER_EXEC_GROUP_NAME (packages/RuleClass.java:153).
pub const TEST_EXEC_GROUP_NAME: &str = "test";

/// StarlarkExecGroupCollection.isValidGroupName
/// (analysis/starlark/StarlarkExecGroupCollection.java:76-78).
pub fn is_valid_group_name(name: &str) -> bool {
    name != DEFAULT_EXEC_GROUP_NAME && crate::rule_declarations::is_identifier(name)
}

/// DeclaredExecGroup.isAutomatic (packages/DeclaredExecGroup.java:70-73):
/// automatic exec groups are named by their toolchain type's
/// `Label.toString()`, which is never an identifier.
pub fn is_automatic(name: &str) -> bool {
    !crate::rule_declarations::is_identifier(name) && name != DEFAULT_EXEC_GROUP_NAME
}

/// ExecGroupCollection.InvalidExecGroupException (analysis/ExecGroupCollection.java:219-237):
/// the invalid names joined by `,`, then the first SpellChecker suggestion.
pub fn invalid_exec_group(what: &str, target: &str, invalid: &[&str], valid: &[&str]) -> String {
    let hint = invalid
        .iter()
        .map(|name| crate::native_rules::did_you_mean(name, valid))
        .find(|hint| !hint.is_empty())
        .unwrap_or_default();
    format!(
        "Tried to set {what} for non-existent exec groups on {target}: {}{hint}",
        invalid.join(",")
    )
}

/// ExecGroupCollection.parseExecProperties (analysis/ExecGroupCollection.java:201-216):
/// `property` belongs to the default exec group, `group.property` to
/// `group` (split at the first `.`). An ImmutableTable rejects a repeated
/// cell (`k` with `default-exec-group.k`); Bazel 9.3 fails with an
/// IllegalArgumentException there, natively an explicit error.
pub fn parse_exec_properties(
    raw: &[(String, String)],
) -> anyhow::Result<Vec<(String, String, String)>> {
    let mut table: Vec<(String, String, String)> = Vec::new();
    for (key, value) in raw {
        let (group, property) = match key.split_once('.') {
            Some((group, property)) => (group.to_owned(), property.to_owned()),
            None => (DEFAULT_EXEC_GROUP_NAME.to_owned(), key.clone()),
        };
        if table
            .iter()
            .any(|(seen_group, seen, _)| *seen_group == group && *seen == property)
        {
            bail!(
                "exec_properties sets property {property:?} of exec group {group:?} twice (Bazel 9.3 fails with an IllegalArgumentException in ImmutableTable.Builder.buildOrThrow); not supported by the native profile"
            );
        }
        table.push((group, property, value.clone()));
    }
    Ok(table)
}

fn row<'a>(table: &'a [(String, String, String)], group: &str) -> Vec<(&'a str, &'a str)> {
    table
        .iter()
        .filter(|(row, _, _)| row == group)
        .map(|(_, property, value)| (property.as_str(), value.as_str()))
        .collect()
}

fn put(combined: &mut Vec<(String, String)>, property: &str, value: &str) {
    match combined.iter_mut().find(|(seen, _)| seen == property) {
        Some(entry) => entry.1 = value.to_owned(),
        None => combined.push((property.to_owned(), value.to_owned())),
    }
}

/// One exec group of a configured target's ToolchainCollection for
/// ExecGroupCollection.computeCombinedExecProperties: its name and its
/// execution platform's `exec_properties`.
pub struct GroupPlatform<'a> {
    pub name: &'a str,
    pub platform_properties: &'a [(String, String)],
}

/// ExecGroupCollection.computeCombinedExecProperties
/// (analysis/ExecGroupCollection.java:70-181): `groups` is the
/// ToolchainCollection (default group first), `None` for a rule without
/// toolchain resolution. Unknown exec group names in the target's
/// `exec_properties` are an InvalidExecGroupException when the rule has a
/// toolchain collection. Per group: the platform's default-group properties,
/// the target's default-group properties, the platform's group properties,
/// the target's group properties (later wins).
pub fn combined_exec_properties(
    groups: Option<&[GroupPlatform<'_>]>,
    raw: &[(String, String)],
    target: &str,
) -> anyhow::Result<Vec<(String, ExecProperties)>> {
    let parsed_target = parse_exec_properties(raw)?;
    if let Some(groups) = groups {
        let names = groups.iter().map(|group| group.name).collect::<Vec<_>>();
        let mut unknown: Vec<&str> = Vec::new();
        for (group, _, _) in &parsed_target {
            if group != DEFAULT_EXEC_GROUP_NAME
                && !names.contains(&group.as_str())
                && !unknown.contains(&group.as_str())
            {
                unknown.push(group);
            }
        }
        if !unknown.is_empty() {
            let mut valid = names.clone();
            valid.push(DEFAULT_EXEC_GROUP_NAME);
            bail!(
                "{}",
                invalid_exec_group("exec_properties", target, &unknown, &valid)
            );
        }
    }
    let default_target = row(&parsed_target, DEFAULT_EXEC_GROUP_NAME);
    let mut result = vec![(
        DEFAULT_EXEC_GROUP_NAME.to_owned(),
        default_target
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<Vec<_>>(),
    )];
    for group in groups.unwrap_or_default() {
        let platform = parse_exec_properties(group.platform_properties)?;
        let mut combined = Vec::new();
        for (key, value) in row(&platform, DEFAULT_EXEC_GROUP_NAME) {
            put(&mut combined, key, value);
        }
        for (key, value) in &default_target {
            put(&mut combined, key, value);
        }
        for (key, value) in row(&platform, group.name) {
            put(&mut combined, key, value);
        }
        // The target's group properties, then the default properties it does
        // not set (computeIfAbsent keeps the group's own value): these
        // override the platform's group properties too.
        let target_group = row(&parsed_target, group.name);
        for (key, value) in &target_group {
            put(&mut combined, key, value);
        }
        for (key, value) in &default_target {
            if !target_group.iter().any(|(seen, _)| seen == key) {
                put(&mut combined, key, value);
            }
        }
        match result.iter_mut().find(|(name, _)| name == group.name) {
            Some(entry) => entry.1 = combined,
            None => result.push((group.name.to_owned(), combined)),
        }
    }
    Ok(result)
}

/// One exec group of DeclaredExecGroup.process
/// (packages/DeclaredExecGroup.java:94-150) before resolution: its
/// ToolchainContextKey (UnloadedToolchainContextsProducer.java:110-124; no
/// forced execution platform).
#[derive(Clone, Debug)]
pub struct ProcessedGroup {
    pub name: String,
    pub key: crate::toolchains::ToolchainContextKey,
    /// The default `test` exec group without the built-in profile
    /// (divergence D3, docs/TEST_FLAGS.md): not resolved.
    pub builtin_only: bool,
}

/// One non-default exec group of a configured target's
/// ToolchainCollection after resolution.
#[derive(Clone, Debug)]
pub struct ResolvedGroup {
    pub name: String,
    /// The processed declaration's toolchain type labels, as requested
    /// (ExecGroupCollection.getExecGroup(NAME).toolchainTypes()).
    pub types: Vec<Label>,
    /// `None` when resolution needed no platform (the lazy default host
    /// platform, docs/TOOLCHAINS.md) or for a [`ProcessedGroup::builtin_only`]
    /// group: the execution platform is the host platform.
    pub context: Option<crate::toolchains::UnloadedToolchainContext>,
    pub builtin_only: bool,
}

/// The exec groups of a toolchain-resolving configured target.
#[derive(Clone, Debug, Default)]
pub struct TargetExecGroups {
    /// RuleContext.useAutoExecGroups (RuleContext.java:1069-1074).
    pub use_auto_exec_groups: bool,
    /// ToolchainCollection order without the default group: declared
    /// groups (with the default `test` group), then automatic groups.
    pub groups: Vec<ResolvedGroup>,
}

/// Where a Starlark action runs: its exec group, that group's execution
/// platform and combined exec properties (RuleContext.getActionOwner(execGroup),
/// analysis/RuleContext.java:455-479; ExecGroupCollection.getExecProperties).
/// Natively every execution platform is the host platform and actions run
/// locally, where exec properties have no effect (docs/EXEC_GROUPS.md).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionExecution {
    pub exec_group: String,
    pub platform: Label,
    pub properties: Vec<(String, String)>,
    /// Shared resolved constraints for native action construction (e.g. the
    /// Java UTF-8 locale). The resulting environment is already action-keyed.
    pub constraints: Option<Arc<crate::platforms::ConstraintCollection>>,
}

/// Exec properties in insertion order.
pub type ExecProperties = Vec<(String, String)>;

/// The `toolchain` argument of `ctx.actions.run`/`run_shell`
/// (StarlarkActionFactoryApi: `Label`, string or None, default unbound).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolchainArgument {
    Unbound,
    None,
    Label(Label),
}

/// What the action factory knows of the rule's exec groups
/// (RuleContext.getToolchainContexts / getExecGroups / useAutoExecGroups).
#[derive(Clone, Debug, Default)]
pub struct ActionExecGroups {
    /// The ToolchainCollection's exec group names, the default group first,
    /// with each processed group's toolchain type labels; `None` for a rule
    /// without toolchain resolution (no collection).
    pub groups: Option<Vec<(String, Vec<Label>)>>,
    /// RuleContext.useAutoExecGroups (RuleContext.java:1069-1074).
    pub use_auto_exec_groups: bool,
    /// Whether this is a subrule's action factory
    /// (StarlarkSubrule.SubruleContext.maybeOverride*, :464-481), with the
    /// subrule's requested toolchain types.
    pub subrule: Option<Vec<Label>>,
    /// The target's per-exec-group execution platform and exec properties
    /// (crate::engine's action execution table, computed before the
    /// implementation like ExecGroupCollection): the execution part of each
    /// action's key (crate::shared_actions). Empty where no table was
    /// computed (aspect applications).
    pub executions: Arc<[ActionExecution]>,
}

impl ActionExecGroups {
    /// The keyed execution of an action of exec group `group`
    /// (ActionKeyComputer.getKey: the execution platform, null without a
    /// ToolchainCollection — RuleContext.getExecutionPlatform — and the
    /// group's exec properties).
    pub fn keyed_execution(&self, group: &str) -> crate::shared_actions::KeyedExecution<'_> {
        match self
            .executions
            .iter()
            .find(|execution| execution.exec_group == group)
        {
            Some(execution) => crate::shared_actions::KeyedExecution {
                platform: self.groups.as_ref().map(|_| &execution.platform),
                properties: &execution.properties,
            },
            None => crate::shared_actions::KeyedExecution::default(),
        }
    }

    fn has(&self, name: &str) -> bool {
        self.groups
            .as_ref()
            .is_some_and(|groups| groups.iter().any(|(group, _)| group == name))
    }

    /// StarlarkSubrule.SubruleContext.maybeOverrideExecGroup and
    /// maybeOverrideToolchain (analysis/starlark/StarlarkSubrule.java:464-481):
    /// a subrule may set neither; its toolchain is its only requested type,
    /// or None.
    pub fn override_arguments(
        &self,
        exec_group: Option<&str>,
        toolchain: ToolchainArgument,
    ) -> anyhow::Result<ToolchainArgument> {
        let Some(requested) = &self.subrule else {
            return Ok(toolchain);
        };
        if exec_group.is_some() {
            bail!("'exec_group' may not be specified in subrules");
        }
        if toolchain != ToolchainArgument::Unbound {
            bail!("'toolchain' may not be specified in subrules");
        }
        Ok(match requested.as_slice() {
            [] => ToolchainArgument::None,
            [label] => ToolchainArgument::Label(label.clone()),
            _ => bail!(
                "a subrule requesting {} toolchain types: Bazel 9.3 fails in Iterables.getOnlyElement (StarlarkSubrule.java:478-480); not supported by the native profile",
                requested.len()
            ),
        })
    }

    /// StarlarkActionFactory.checkToolchainParameterIsSet
    /// (analysis/starlark/StarlarkActionFactory.java:130-139), applied under
    /// automatic exec groups to an action without `exec_group` whose
    /// executable or tool is not an executable attribute's
    /// (:433-454, :764-800).
    pub fn check_toolchain_parameter(
        &self,
        exec_group: Option<&str>,
        toolchain: &ToolchainArgument,
    ) -> anyhow::Result<()> {
        if !self.use_auto_exec_groups || exec_group.is_some() {
            return Ok(());
        }
        let many = self.groups.as_ref().is_none_or(|groups| groups.len() > 1);
        if many && *toolchain == ToolchainArgument::Unbound {
            bail!(
                "Couldn't identify if tools are from implicit dependencies or a toolchain. Please set the toolchain parameter. If you're not using a toolchain, set it to 'None'."
            );
        }
        Ok(())
    }

    /// StarlarkActionFactory.verifyExecGroupExists and checkValidGroupName
    /// (:569-587): the `exec_group` of an action.
    pub fn verify_exec_group(&self, group: &str) -> anyhow::Result<()> {
        if !self.has(group) {
            bail!("Action declared for non-existent exec group '{group}'.");
        }
        if !is_valid_group_name(group) {
            bail!("Invalid name for exec group '{group}'.");
        }
        Ok(())
    }

    /// StarlarkActionFactory.determineExecGroup (:858-898).
    pub fn determine(
        &self,
        exec_group: Option<&str>,
        toolchain: &ToolchainArgument,
    ) -> anyhow::Result<String> {
        let toolchain = match toolchain {
            ToolchainArgument::Label(label) => Some(label),
            _ => None,
        };
        if let Some(group) = exec_group {
            self.verify_exec_group(group)?;
            if self.use_auto_exec_groups
                && let Some(toolchain) = toolchain
            {
                let types = self
                    .groups
                    .iter()
                    .flatten()
                    .find(|(name, _)| name == group)
                    .map(|(_, types)| types.as_slice())
                    .unwrap_or_default();
                if !types.contains(toolchain) {
                    bail!(
                        "`toolchain` and `exec_group` parameters inside actions.{{run, run_shell}} are not compatible; use one of them or define `toolchain` which is compatible with the exec_group (already exists inside the `exec_group`)"
                    );
                }
            }
            return Ok(group.to_owned());
        }
        if self.use_auto_exec_groups
            && let Some(toolchain) = toolchain
        {
            let name = toolchain.to_string();
            if !self.has(&name) {
                bail!("Action declared for non-existent toolchain '{name}'.");
            }
            return Ok(name);
        }
        Ok(DEFAULT_EXEC_GROUP_NAME.to_owned())
    }
}

// Only derive-generated marker/trace implementations need this lint
// exception.
#[allow(unsafe_code)]
mod values {
    use super::*;

    /// `ctx.exec_groups` (StarlarkExecGroupCollection): the exec groups of
    /// the rule's ToolchainCollection.
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct ExecGroupCollection {
        #[allocative(skip)]
        pub data: Arc<ExecGroupsData>,
    }

    /// `ctx.exec_groups[NAME]` (StarlarkExecGroupContext, :155-166).
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct ExecGroupContext {
        #[allocative(skip)]
        pub toolchains: GroupToolchains,
    }
}
pub(crate) use values::{ExecGroupCollection, ExecGroupContext};

starlark_simple_value!(ExecGroupCollection);
starlark_simple_value!(ExecGroupContext);

/// The toolchains of one exec group as `ctx.exec_groups[NAME].toolchains`
/// shows them.
#[derive(Clone, Debug)]
pub enum GroupToolchains {
    Resolved(Arc<ToolchainContextData>),
    /// The default `test` exec group without the built-in profile
    /// (divergence D3, docs/TEST_FLAGS.md): its toolchain type lives in
    /// `@bazel_tools`, so it is not resolved.
    BuiltinOnly,
}

/// The ToolchainCollection behind `ctx.exec_groups`.
#[derive(Debug)]
pub struct ExecGroupsData {
    /// The default context's targetDescription (`KIND rule LABEL`).
    pub target_description: String,
    /// The non-default exec groups in ToolchainCollection order.
    pub groups: Vec<(String, GroupToolchains)>,
}

impl ExecGroupsData {
    /// getScrubbedExecGroups (:138-143): the names without the default
    /// group, sorted.
    fn scrubbed(&self) -> Vec<&str> {
        let mut names = self
            .groups
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        names.sort_by(|a, b| crate::native_rules::java_string_cmp(a, b));
        names
    }
}

/// castGroupName (:121-128).
fn group_name<'v>(key: Value<'v>) -> anyhow::Result<&'v str> {
    key.unpack_str().ok_or_else(|| {
        anyhow!(
            "exec groups only support indexing by exec group name, got {} of type {} instead",
            key.to_repr(),
            key.get_type()
        )
    })
}

impl fmt::Display for ExecGroupCollection {
    /// StarlarkExecGroupCollection.repr (:130-136).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<ctx.exec_groups: {}>", self.data.scrubbed().join(", "))
    }
}

impl fmt::Display for ExecGroupContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<exec_group_context>")
    }
}

#[starlark_value(type = "ExecGroupCollection")]
impl<'v> StarlarkValue<'v> for ExecGroupCollection {
    /// StarlarkExecGroupCollection.getIndex (:86-119).
    fn at(&self, index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let name = group_name(index).map_err(starlark::Error::new_other)?;
        let Some((_, toolchains)) = self.data.groups.iter().find(|(group, _)| group == name) else {
            return Err(starlark::Error::new_other(anyhow!(
                "In {}, unrecognized exec group '{name}' requested. Available exec groups: [{}]",
                self.data.target_description,
                self.data.scrubbed().join(", ")
            )));
        };
        Ok(heap.alloc(ExecGroupContext {
            toolchains: toolchains.clone(),
        }))
    }

    /// StarlarkExecGroupCollection.containsKey (:80-84).
    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        let name = group_name(other).map_err(starlark::Error::new_other)?;
        Ok(self.data.groups.iter().any(|(group, _)| group == name))
    }
}

starlark::methods_static!(CONTEXT_METHODS = context_methods);

#[starlark_module]
fn context_methods(builder: &mut MethodsBuilder) {
    /// ExecGroupContextApi.toolchains (starlarkbuildapi/platform/ExecGroupCollectionApi.java).
    #[starlark(attribute)]
    fn toolchains<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let context = this
            .downcast_ref::<ExecGroupContext>()
            .ok_or_else(|| anyhow!("expected ExecGroupContext"))?;
        match &context.toolchains {
            GroupToolchains::Resolved(data) => Ok(crate::toolchain_info::alloc_context(
                heap,
                Some(data.clone()),
            )),
            GroupToolchains::BuiltinOnly => bail!(
                "the toolchains of the default `test` exec group need the built-in @bazel_tools repository, which --experimental-builtin-modules=false disables; without it the group is not resolved by the native profile (divergence D3, docs/TEST_FLAGS.md)"
            ),
        }
    }
}

#[starlark_value(type = "ExecGroupContext")]
impl<'v> StarlarkValue<'v> for ExecGroupContext {
    fn get_methods() -> Option<&'static Methods> {
        Some(CONTEXT_METHODS.methods())
    }
}

pub(crate) fn alloc_collection(heap: Heap<'_>, data: Arc<ExecGroupsData>) -> Value<'_> {
    heap.alloc(ExecGroupCollection { data })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(text: &str) -> Label {
        Label::parse_canonical(text).unwrap()
    }

    #[test]
    fn group_names() {
        assert!(is_valid_group_name("cpp_link"));
        assert!(!is_valid_group_name(DEFAULT_EXEC_GROUP_NAME));
        assert!(!is_valid_group_name("//x:t"));
        assert!(is_automatic("//x:t"));
        assert!(is_automatic("@@bazel_tools//tools/cpp:toolchain_type"));
        assert!(!is_automatic("test"));
        assert!(!is_automatic(DEFAULT_EXEC_GROUP_NAME));
    }

    #[test]
    fn invalid_exec_group_messages() {
        assert_eq!(
            invalid_exec_group(
                "exec_properties",
                "//x:y",
                &["linkk"],
                &["link", "default-exec-group"]
            ),
            "Tried to set exec_properties for non-existent exec groups on //x:y: linkk (did you mean 'link'?)"
        );
        assert_eq!(
            invalid_exec_group("execution constraints", "//x:y", &["zzz"], &[]),
            "Tried to set execution constraints for non-existent exec groups on //x:y: zzz"
        );
    }

    #[test]
    fn combined_properties_follow_precedence() {
        let raw = vec![
            ("k".to_owned(), "target".to_owned()),
            ("g.k".to_owned(), "target-g".to_owned()),
            ("only".to_owned(), "d".to_owned()),
        ];
        let platform = vec![
            ("k".to_owned(), "platform".to_owned()),
            ("p".to_owned(), "platform".to_owned()),
            ("g.p".to_owned(), "platform-g".to_owned()),
        ];
        let groups = [
            GroupPlatform {
                name: DEFAULT_EXEC_GROUP_NAME,
                platform_properties: &platform,
            },
            GroupPlatform {
                name: "g",
                platform_properties: &platform,
            },
            GroupPlatform {
                name: "link",
                platform_properties: &[],
            },
        ];
        let combined = combined_exec_properties(Some(&groups), &raw, "//x:y").unwrap();
        let get = |group: &str| {
            combined
                .iter()
                .find(|(name, _)| name == group)
                .unwrap()
                .1
                .clone()
        };
        let pairs = |items: &[(&str, &str)]| {
            items
                .iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            get(DEFAULT_EXEC_GROUP_NAME),
            pairs(&[("k", "target"), ("p", "platform"), ("only", "d")])
        );
        assert_eq!(
            get("g"),
            pairs(&[("k", "target-g"), ("p", "platform-g"), ("only", "d")])
        );
        let error = combined_exec_properties(
            Some(&groups),
            &[
                ("gx.k".to_owned(), "v".to_owned()),
                ("linkk.k".to_owned(), "v".to_owned()),
            ],
            "//x:y",
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "Tried to set exec_properties for non-existent exec groups on //x:y: gx,linkk (did you mean 'link'?)"
        );
        // Without a toolchain collection the names are not validated.
        assert!(
            combined_exec_properties(None, &[("h.k".to_owned(), "v".to_owned())], "//x:y").is_ok()
        );
        assert!(
            parse_exec_properties(&[
                ("k".to_owned(), "1".to_owned()),
                ("default-exec-group.k".to_owned(), "2".to_owned())
            ])
            .is_err()
        );
    }

    #[test]
    fn action_exec_groups() {
        let t = label("//x:t");
        let groups = ActionExecGroups {
            groups: Some(vec![
                (DEFAULT_EXEC_GROUP_NAME.to_owned(), Vec::new()),
                ("link".to_owned(), vec![label("//x:u")]),
                ("//x:t".to_owned(), vec![t.clone()]),
            ]),
            use_auto_exec_groups: true,
            subrule: None,
            executions: Arc::from([]),
        };
        let unbound = ToolchainArgument::Unbound;
        assert_eq!(
            groups.determine(None, &unbound).unwrap(),
            DEFAULT_EXEC_GROUP_NAME
        );
        assert_eq!(
            groups
                .determine(None, &ToolchainArgument::Label(t.clone()))
                .unwrap(),
            "//x:t"
        );
        assert_eq!(groups.determine(Some("link"), &unbound).unwrap(), "link");
        assert_eq!(
            groups
                .determine(Some("link"), &ToolchainArgument::Label(t.clone()))
                .unwrap_err()
                .to_string(),
            "`toolchain` and `exec_group` parameters inside actions.{run, run_shell} are not compatible; use one of them or define `toolchain` which is compatible with the exec_group (already exists inside the `exec_group`)"
        );
        assert_eq!(
            groups
                .determine(Some("nope"), &unbound)
                .unwrap_err()
                .to_string(),
            "Action declared for non-existent exec group 'nope'."
        );
        assert_eq!(
            groups
                .determine(Some(DEFAULT_EXEC_GROUP_NAME), &unbound)
                .unwrap_err()
                .to_string(),
            "Invalid name for exec group 'default-exec-group'."
        );
        assert_eq!(
            groups
                .determine(None, &ToolchainArgument::Label(label("//x:v")))
                .unwrap_err()
                .to_string(),
            "Action declared for non-existent toolchain '//x:v'."
        );
        assert!(groups.check_toolchain_parameter(None, &unbound).is_err());
        assert!(
            groups
                .check_toolchain_parameter(None, &ToolchainArgument::None)
                .is_ok()
        );
        assert!(
            groups
                .check_toolchain_parameter(Some("link"), &unbound)
                .is_ok()
        );
        // No ToolchainCollection (a build setting): every group is unknown
        // and an unset toolchain parameter is an error under automatic exec
        // groups (StarlarkActionFactory.java:130-139).
        let none = ActionExecGroups {
            groups: None,
            use_auto_exec_groups: true,
            subrule: None,
            executions: Arc::from([]),
        };
        assert!(none.check_toolchain_parameter(None, &unbound).is_err());
        assert_eq!(
            none.determine(Some("link"), &unbound)
                .unwrap_err()
                .to_string(),
            "Action declared for non-existent exec group 'link'."
        );
        assert_eq!(
            none.determine(None, &ToolchainArgument::Label(t.clone()))
                .unwrap_err()
                .to_string(),
            "Action declared for non-existent toolchain '//x:t'."
        );
        let subrule = ActionExecGroups {
            subrule: Some(vec![t.clone()]),
            ..groups.clone()
        };
        assert_eq!(
            subrule.override_arguments(None, unbound.clone()).unwrap(),
            ToolchainArgument::Label(t)
        );
        assert_eq!(
            subrule
                .override_arguments(Some("link"), unbound.clone())
                .unwrap_err()
                .to_string(),
            "'exec_group' may not be specified in subrules"
        );
        assert_eq!(
            subrule
                .override_arguments(None, ToolchainArgument::None)
                .unwrap_err()
                .to_string(),
            "'toolchain' may not be specified in subrules"
        );
    }
}
