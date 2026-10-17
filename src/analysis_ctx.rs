//! Native fallible context properties. Introspection is structural; reads consume
//! a current scope capability. Runtime contexts deliberately cannot be frozen.
use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::environment::{Methods, MethodsBuilder};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value,
    ValueLike, starlark_value,
};
use std::fmt;

use crate::analysis_runtime::ScopeHandle;
use crate::configuration::SettingValue;

use crate::ctx_surface::RuleSurface;
use crate::platforms::TargetPlatform;
use crate::toolchain_info::ToolchainContextData;
use std::sync::Arc;

/// The analysis error of an implementation that needs a deferred target
/// platform.
pub use crate::platforms::NEEDS_TARGET_PLATFORM;

#[allow(unsafe_code)]
mod values {
    use super::*;
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, StarlarkPagablePanic)]
    pub(super) struct RuleCtx<'v> {
        pub backing: Value<'v>,
        #[allocative(skip)]
        #[trace(static)]
        pub scope: ScopeHandle,
        /// The rule's target platform (ctx.target_platform_has_constraint).
        #[allocative(skip)]
        #[trace(static)]
        pub platform: TargetPlatform,
        /// The resolved toolchain context (`ctx.toolchains`); `None` for
        /// rules without toolchain resolution.
        #[allocative(skip)]
        #[trace(static)]
        pub toolchains: Option<Arc<ToolchainContextData>>,
        /// The configuration and rule facts of the N23c members
        /// (crate::ctx_surface).
        #[allocative(skip)]
        #[trace(static)]
        pub surface: Arc<RuleSurface>,
    }
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, StarlarkPagablePanic)]
    pub(super) struct SettingCtx<'v> {
        pub ctx: RuleCtx<'v>,
        /// The configured value (command line or `build_setting_default`).
        #[allocative(skip)]
        #[trace(static)]
        pub value: SettingValue,
    }
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, Trace, StarlarkPagablePanic)]
    pub(super) struct SubruleCtx<'v> {
        /// The subrule's exported name (StarlarkSubrule.getName).
        #[allocative(skip)]
        #[trace(static)]
        pub name: String,
        pub label: Value<'v>,
        pub actions: Value<'v>,
        /// SubruleFragmentCollection (crate::subrule_fragments).
        pub fragments: Value<'v>,
        #[allocative(skip)]
        #[trace(static)]
        pub scope: ScopeHandle,
        #[allocative(skip)]
        #[trace(static)]
        pub toolchains: crate::subrule::SubruleToolchains,
    }
}
use values::{RuleCtx, SettingCtx, SubruleCtx};

/// StarlarkRuleContext.repr (analysis/starlark/StarlarkRuleContext.java:590-597)
/// for a rule (not aspect) context; `label` is the canonical form
/// (StarlarkRuleContext.java:197).
pub(crate) fn rule_context_repr(label: &str) -> String {
    format!("<rule context for {label}>")
}

/// A label Value's canonical form; other values (native unit-test stand-ins)
/// print as str().
fn canonical_label(value: Value<'_>) -> String {
    crate::starlark_types::label_canonical_form(value).unwrap_or_else(|| value.to_str())
}

impl RuleCtx<'_> {
    fn label(&self) -> String {
        starlark::values::structs::StructRef::from_value(self.backing)
            .and_then(|fields| {
                fields
                    .iter()
                    .find(|(name, _)| name.as_str() == "label")
                    .map(|(_, value)| canonical_label(value))
            })
            .unwrap_or_default()
    }
}

impl fmt::Display for RuleCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&rule_context_repr(&self.label()))
    }
}
// A build setting's ctx is the same StarlarkRuleContext.
impl fmt::Display for SettingCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.ctx.fmt(f)
    }
}
/// SubruleContext.repr (analysis/starlark/StarlarkSubrule.java:493-501).
pub(crate) fn subrule_context_repr(name: &str, label: &str) -> String {
    format!("<{name} context for {label}>")
}
impl fmt::Display for SubruleCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&subrule_context_repr(
            &self.name,
            &canonical_label(self.label),
        ))
    }
}

starlark::methods_static!(RULE_METHODS = all_rule_properties);
starlark::methods_static!(SETTING_METHODS = all_setting_properties);
starlark::methods_static!(SUBRULE_METHODS = subrule_properties);

/// The StarlarkRuleContextApi members (dir order, 36 in Bazel 9.3).
const CTX_MEMBERS: [&str; 36] = [
    "actions",
    "aspect_ids",
    "attr",
    "bin_dir",
    "build_file_path",
    "build_setting_value",
    "check_placeholders",
    "configuration",
    "coverage_instrumented",
    "created_actions",
    "disabled_features",
    "exec_groups",
    "executable",
    "expand_location",
    "expand_make_variables",
    "features",
    "file",
    "files",
    "fragments",
    "genfiles_dir",
    "info_file",
    "label",
    "outputs",
    "package_relative_label",
    "resolve_command",
    "resolve_tools",
    "rule",
    "runfiles",
    "split_attr",
    "super",
    "target_platform_has_constraint",
    "tokenize",
    "toolchains",
    "var",
    "version_file",
    "workspace_name",
];

fn unknown_ctx_member(name: &str) -> String {
    let fields = CTX_MEMBERS.map(str::to_owned);
    crate::ctx_surface::no_field_message("ctx", name, &fields)
}

#[starlark_value(type = "ctx")]
impl<'v> StarlarkValue<'v> for RuleCtx<'v> {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        crate::ctx_surface::ctx_super(name, heap)
    }
    fn dir_attr(&self) -> Vec<String> {
        vec!["super".to_owned()]
    }
    fn get_methods() -> Option<&'static Methods> {
        Some(RULE_METHODS.methods())
    }
    fn unknown_attribute_message(&self, name: &str) -> Option<String> {
        Some(unknown_ctx_member(name))
    }
}
#[starlark_value(type = "ctx")]
impl<'v> StarlarkValue<'v> for SettingCtx<'v> {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        crate::ctx_surface::ctx_super(name, heap)
    }
    fn dir_attr(&self) -> Vec<String> {
        vec!["super".to_owned()]
    }
    fn get_methods() -> Option<&'static Methods> {
        Some(SETTING_METHODS.methods())
    }
    fn unknown_attribute_message(&self, name: &str) -> Option<String> {
        Some(unknown_ctx_member(name))
    }
}
#[starlark_value(type = "subrule_ctx")]
impl<'v> StarlarkValue<'v> for SubruleCtx<'v> {
    fn get_methods() -> Option<&'static Methods> {
        Some(SUBRULE_METHODS.methods())
    }
}

fn field<'v>(this: Value<'v>, name: &str, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let ctx = rule_ctx(this)?;
    ctx.scope.check_current()?;
    ctx.backing
        .get_attr_error(name, heap)
        .map_err(|error| anyhow!(error.to_string()))
}

#[starlark_module]
fn rule_properties(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn attr<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        field(this, "attr", heap)
    }
    #[starlark(attribute)]
    fn files<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        field(this, "files", heap)
    }
    #[starlark(attribute)]
    fn file<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        field(this, "file", heap)
    }
    #[starlark(attribute)]
    fn outputs<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        field(this, "outputs", heap)
    }
    #[starlark(attribute)]
    fn label<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        field(this, "label", heap)
    }
    #[starlark(attribute)]
    fn actions<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        field(this, "actions", heap)
    }
    /// StarlarkRuleContext.getExecutable (analysis/starlark/StarlarkRuleContext.java:692-697).
    #[starlark(attribute)]
    fn executable<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        field(this, "executable", heap)
    }
    /// StarlarkRuleContext.runfiles (analysis/starlark/StarlarkRuleContext.java:1059-1125).
    #[allow(clippy::too_many_arguments)]
    fn runfiles<'v>(
        this: Value<'v>,
        files: Option<Value<'v>>,
        transitive_files: Option<Value<'v>>,
        collect_data: Option<Value<'v>>,
        collect_default: Option<Value<'v>>,
        symlinks: Option<Value<'v>>,
        root_symlinks: Option<Value<'v>>,
        #[starlark(require = named, default = false)] skip_conflict_checking: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let heap = eval.heap();
        let collect_data_value = field(this, "$collect_data", heap)?;
        let collect_default_value = field(this, "$collect_default", heap)?;
        let collected = |value: Value<'v>| {
            crate::runfiles::Runfiles::from_value(value)
                .ok_or_else(|| anyhow!("internal error: collected runfiles"))
        };
        let runfiles = crate::runfiles::ctx_runfiles(
            crate::runfiles::RunfilesArguments {
                files,
                transitive_files,
                collect_data,
                collect_default,
                symlinks,
                root_symlinks,
                skip_conflict_checking,
            },
            (
                collected(collect_data_value)?,
                collected(collect_default_value)?,
            ),
            eval,
        )?;
        Ok(eval.heap().alloc(runfiles))
    }
    /// StarlarkRuleContext.toolchains (analysis/starlark/StarlarkRuleContext.java:877-898).
    #[starlark(attribute)]
    fn toolchains<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let ctx = rule_ctx(this)?;
        ctx.scope.check_current()?;
        Ok(crate::toolchain_info::alloc_context(
            heap,
            ctx.toolchains.clone(),
        ))
    }
    /// StarlarkRuleContext.targetPlatformHasConstraint
    /// (analysis/starlark/StarlarkRuleContext.java:902-904; RuleContext.java:1144-1150):
    /// whether the target platform's value for the setting is this value.
    fn target_platform_has_constraint<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] constraint_value: Value<'v>,
    ) -> anyhow::Result<bool> {
        let ctx = rule_ctx(this)?;
        ctx.scope.check_current()?;
        let Some(value) = crate::platform_providers::constraint_value(constraint_value) else {
            bail!(
                "in call to target_platform_has_constraint(), parameter 'constraintValue' got value of type '{}', want 'ConstraintValueInfo'",
                constraint_value.get_type()
            );
        };
        match &ctx.platform {
            TargetPlatform::Absent => Ok(false),
            TargetPlatform::Deferred => bail!("{NEEDS_TARGET_PLATFORM}"),
            TargetPlatform::Resolved(platform) => Ok(platform
                .constraints
                .get(&value.setting)
                .is_some_and(|found| found == *value)),
        }
    }
}

fn rule_ctx<'v>(this: Value<'v>) -> anyhow::Result<&'v RuleCtx<'v>> {
    if let Some(ctx) = this.downcast_ref::<RuleCtx>() {
        return Ok(ctx);
    }
    Ok(&this
        .downcast_ref::<SettingCtx>()
        .ok_or_else(|| anyhow!("expected ctx"))?
        .ctx)
}

fn all_setting_properties(builder: &mut MethodsBuilder) {
    rule_properties(builder);
    crate::analysis_ctx_surface::surface_properties(builder);
    setting_properties(builder);
}

fn all_rule_properties(builder: &mut MethodsBuilder) {
    rule_properties(builder);
    crate::analysis_ctx_surface::surface_properties(builder);
    crate::analysis_ctx_surface::non_setting_properties(builder);
}

/// The rule surface of a rule context value (ObjcConfiguration's `ctx` argument).
pub(crate) fn rule_surface<'v>(value: Value<'v>) -> Option<&'v RuleSurface> {
    rule_ctx(value).ok().map(|ctx| &*ctx.surface)
}

/// The current rule context's surface, after the scope check
/// (StarlarkRuleContext.checkMutable).
pub(crate) fn current_surface<'v>(value: Value<'v>) -> anyhow::Result<&'v Arc<RuleSurface>> {
    let ctx = rule_ctx(value)?;
    ctx.scope.check_current()?;
    Ok(&ctx.surface)
}

#[starlark_module]
fn setting_properties(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn build_setting_value<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let ctx = this
            .downcast_ref::<SettingCtx>()
            .ok_or_else(|| anyhow!("expected build-setting ctx"))?;
        ctx.ctx.scope.check_current()?;
        // A fresh value per read (mutable list copies).
        match &ctx.value {
            SettingValue::Bool(value) => Ok(heap.alloc(*value)),
            SettingValue::Int(value) => Ok(heap.alloc(*value)),
            SettingValue::String(value) => Ok(heap.alloc(value.as_str())),
            SettingValue::StringList(value) => Ok(heap.alloc(value.clone())),
            SettingValue::Label(_) => bail!("unsupported build-setting value type label"),
        }
    }
}

fn subctx<'v>(this: Value<'v>) -> anyhow::Result<&'v SubruleCtx<'v>> {
    let ctx = this
        .downcast_ref::<SubruleCtx>()
        .ok_or_else(|| anyhow!("expected subrule_ctx"))?;
    ctx.scope.check_current()?;
    Ok(ctx)
}

#[starlark_module]
fn subrule_properties(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn label<'v>(this: Value<'v>) -> anyhow::Result<Value<'v>> {
        Ok(subctx(this)?.label)
    }
    #[starlark(attribute)]
    fn actions<'v>(this: Value<'v>) -> anyhow::Result<Value<'v>> {
        Ok(subctx(this)?.actions)
    }
    /// SubruleContext.getFragmentCollection (StarlarkSubrule.java:405-412).
    #[starlark(attribute)]
    fn fragments<'v>(this: Value<'v>) -> anyhow::Result<Value<'v>> {
        Ok(subctx(this)?.fragments)
    }
    /// SubruleContext.toolchains (StarlarkSubrule.java:377-397).
    #[starlark(attribute)]
    fn toolchains<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        use crate::subrule::SubruleToolchains;
        match &subctx(this)?.toolchains {
            // StarlarkToolchainContext.TOOLCHAINS_NOT_VALID.
            SubruleToolchains::NotValid => Ok(crate::toolchain_info::alloc_context(heap, None)),
            SubruleToolchains::NoAutoExecGroups => bail!(
                "subrules using toolchains must enable automatic exec-groups. For more info, see https://bazel.build/extending/auto-exec-groups#migration-aegs"
            ),
            SubruleToolchains::AutoExecGroups => bail!(
                "subrule toolchains with automatic exec groups are not implemented by the native profile"
            ),
        }
    }
}

pub(crate) fn alloc_rule<'v>(
    heap: Heap<'v>,
    backing: Value<'v>,
    scope: ScopeHandle,
    value: Option<SettingValue>,
    platform: TargetPlatform,
    toolchains: Option<Arc<ToolchainContextData>>,
    surface: Arc<RuleSurface>,
) -> Value<'v> {
    let ctx = RuleCtx {
        backing,
        scope,
        platform,
        toolchains,
        surface,
    };
    match value {
        Some(value) => heap.alloc_complex_no_freeze(SettingCtx { ctx, value }),
        None => heap.alloc_complex_no_freeze(ctx),
    }
}

pub(crate) fn alloc_subrule<'v>(
    heap: Heap<'v>,
    name: &str,
    label: Value<'v>,
    actions: Value<'v>,
    fragments: Value<'v>,
    scope: ScopeHandle,
    toolchains: crate::subrule::SubruleToolchains,
) -> Value<'v> {
    heap.alloc_complex_no_freeze(SubruleCtx {
        name: name.to_owned(),
        label,
        actions,
        fragments,
        scope,
        toolchains,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis_runtime::ScopeState;
    use starlark::environment::{Globals, Module};
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};
    use starlark::values::structs::AllocStruct;
    use std::collections::BTreeSet;

    /// Methods bind without reading the context.
    fn is_method(name: &str) -> bool {
        matches!(
            name,
            "target_platform_has_constraint"
                | "runfiles"
                | "coverage_instrumented"
                | "expand_make_variables"
                | "expand_location"
                | "tokenize"
                | "check_placeholders"
                | "package_relative_label"
                | "resolve_command"
                | "resolve_tools"
                | "created_actions"
                | "super"
        )
    }

    #[test]
    fn native_rule_and_setting_properties_guard_reads_not_introspection() {
        for setting in [false, true] {
            Module::with_temp_heap(|module| {
                let heap = module.heap();
                let owner = ScopeState::new("rule".into(), BTreeSet::from(["S".into()]));
                let backing = heap.alloc(AllocStruct([
                    (
                        "attr",
                        heap.alloc(AllocStruct([("name", heap.alloc("target"))])),
                    ),
                    (
                        "files",
                        heap.alloc(AllocStruct([("srcs", heap.alloc(Vec::<Value>::new()))])),
                    ),
                    (
                        "file",
                        heap.alloc(AllocStruct([("src", Value::new_none())])),
                    ),
                    (
                        "outputs",
                        heap.alloc(AllocStruct([("out", Value::new_none())])),
                    ),
                    (
                        "executable",
                        heap.alloc(AllocStruct([("tool", Value::new_none())])),
                    ),
                    ("label", heap.alloc("//:target")),
                    (
                        "actions",
                        crate::analysis_runtime::alloc_actions(heap, owner.root()),
                    ),
                ]));
                let ctx = alloc_rule(
                    heap,
                    backing,
                    owner.root(),
                    setting.then(|| SettingValue::StringList(vec!["seed".into()])),
                    TargetPlatform::Absent,
                    None,
                    Default::default(),
                );
                module.set("ctx", ctx);
                // Every StarlarkRuleContext member of Bazel 9.3 (dir(ctx)).
                let expected = CTX_MEMBERS.to_vec();
                assert_eq!(ctx.dir_attr(), expected);
                // Members whose read fails in every rule context (aspect-only,
                // unmodelled, or a build setting's value of a non-setting).
                let failing = |name: &str| {
                    matches!(name, "aspect_ids" | "rule" | "info_file" | "version_file")
                        || (!setting && name == "build_setting_value")
                };
                for name in expected.iter().filter(|name| !failing(name)) {
                    ctx.get_attr_error(name, heap).unwrap();
                }
                let child = owner.enter("S".into(), BTreeSet::new()).unwrap();
                // Methods bind without reading; properties guard the read.
                for name in expected.iter().filter(|name| !is_method(name)) {
                    let error = ctx.get_attr_error(name, heap).unwrap_err();
                    assert!(
                        error.to_string().contains("scope receiver"),
                        "{name}: {error}"
                    );
                }
                let mut eval = Evaluator::new(&module);
                eval.eval_module(AstModule::parse("ctx.star", "(hasattr(ctx, 'label') and 'label' in dir(ctx) and getattr(ctx, 'missing', 'fallback') == 'fallback') or fail('structure')\n".into(), &Dialect::Standard).unwrap(), &Globals::standard()).unwrap();
                let error = eval
                    .eval_module(
                        AstModule::parse(
                            "ctx.star",
                            "getattr(ctx, 'label', None)".into(),
                            &Dialect::Standard,
                        )
                        .unwrap(),
                        &Globals::standard(),
                    )
                    .unwrap_err();
                assert!(error.to_string().contains("scope receiver"));
                drop(child);
                assert_eq!(
                    ctx.get_attr_error("label", heap).unwrap().unpack_str(),
                    Some("//:target")
                );
                owner.finish();
                for name in expected.iter().filter(|name| !is_method(name)) {
                    assert!(ctx.get_attr_error(name, heap).is_err());
                }
            });
        }
    }

    #[test]
    fn ordinary_subrule_and_action_runtime_capabilities_reject_freeze() {
        for kind in ["rule", "subrule", "actions"] {
            Module::with_temp_heap(|module| {
                let heap = module.heap();
                let owner = ScopeState::new("rule".into(), BTreeSet::new());
                let actions = crate::analysis_runtime::alloc_actions(heap, owner.root());
                let value = match kind {
                    "rule" => alloc_rule(
                        heap,
                        heap.alloc(AllocStruct([("label", heap.alloc("//:target"))])),
                        owner.root(),
                        None,
                        TargetPlatform::Absent,
                        None,
                        Default::default(),
                    ),
                    "subrule" => alloc_subrule(
                        heap,
                        "s",
                        heap.alloc("//:target"),
                        actions,
                        Value::new_none(),
                        owner.root(),
                        crate::subrule::SubruleToolchains::NotValid,
                    ),
                    _ => actions,
                };
                module.set("capability", value);
                assert!(module.freeze().is_err(), "accepted {kind}");
            });
        }
    }
}
