//! Filesystem-free analysis of the supported Starlark custom-rule subset.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::{anyhow, bail, ensure};
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::structs::AllocStruct;
use starlark::values::{FrozenHeapName, Heap, OwnedFrozenValue, Value};

use crate::custom_rule::{AttrSpec, AttrValue};
use crate::package::Rule;
use crate::repository::RepositorySet;
use crate::starlark_types::{File, alloc_file, alloc_target, default_files};

#[derive(Clone, Debug)]
pub struct DependencyTarget {
    pub label: String,
    pub files: Vec<File>,
    pub is_source: bool,
    /// Actual ordinary rule target, not a source/output file or package group.
    pub is_rule: bool,
    /// Actual package-group kind; never inferred from provider presence.
    pub is_package_group: bool,
    pub providers: BTreeMap<String, OwnedFrozenValue>,
    pub provider_files: Vec<File>,
}

pub use crate::analysis_runtime::RegisteredAction;
pub(crate) use crate::analysis_runtime::repository_context;
use crate::analysis_runtime::{Context, alloc_actions};

#[derive(Debug)]
pub struct AnalysisResult {
    pub files: Vec<File>,
    pub declared: Vec<File>,
    pub actions: Vec<RegisteredAction>,
    pub providers: BTreeMap<String, OwnedFrozenValue>,
    pub provider_files: Vec<File>,
}

fn dependency<'a>(
    label: &str,
    name: &str,
    spec: &AttrSpec,
    dependencies: &'a BTreeMap<String, DependencyTarget>,
) -> anyhow::Result<&'a DependencyTarget> {
    let target = dependencies
        .get(label)
        .ok_or_else(|| anyhow!("attribute {name}: dependency {label} was not resolved"))?;
    ensure!(
        !target.is_source || spec.allow_files || spec.allow_single_file,
        "attribute {name}: source file {label} is not allowed (allow_files=False)"
    );
    if spec.allow_single_file {
        ensure!(
            target.files.len() == 1,
            "attribute {name}: {label} must provide exactly one file, got {}",
            target.files.len()
        );
    }
    if target.is_package_group {
        // Upstream's placement exception deliberately uses printable-name
        // substrings, not provider identity or AND/OR satisfaction. Groups are
        // non-rules, so an additional missing provider does not reject them.
        ensure!(
            matches!(name, "visibility" | "_allowlist_function_transition")
                || spec.required_providers.iter().flatten().any(|provider| {
                    provider
                        .display_name
                        .contains("PackageSpecificationProvider")
                        || provider.display_name.contains("PackageSpecificationInfo")
                }),
            "attribute {name}: package group '{label}' is misplaced here (they are only allowed in the visibility attribute)"
        );
    } else if target.is_rule {
        ensure!(
            spec.required_providers.is_empty()
                || spec.required_providers.iter().any(|clause| {
                    clause.iter().all(|provider| {
                        provider.id == crate::providers::DEFAULT_INFO_PROVIDER_ID
                            || target.providers.contains_key(&provider.id)
                    })
                }),
            "attribute {name}: '{label}' does not have mandatory providers: {}",
            spec.required_providers
                .iter()
                .map(|clause| clause
                    .iter()
                    .map(|provider| format!("'{}'", provider.display_name))
                    .collect::<Vec<_>>()
                    .join(", "))
                .map(|clause| format!("[{clause}]"))
                .collect::<Vec<_>>()
                .join(" or ")
        );
    }
    Ok(target)
}

fn make_ctx<'v>(
    rule: &Rule,
    context: &Context,
    dependencies: &BTreeMap<String, DependencyTarget>,
    heap: Heap<'v>,
) -> anyhow::Result<Value<'v>> {
    let invocation = rule
        .custom
        .as_ref()
        .ok_or_else(|| anyhow!("{} is not a custom rule", rule.label))?;
    let mut attrs = BTreeMap::new();
    let mut files = BTreeMap::new();
    let mut single_files = BTreeMap::new();
    let mut outputs = BTreeMap::new();
    attrs.insert("name".to_owned(), heap.alloc(rule.label.name.as_str()));
    for (name, value) in &invocation.attrs {
        let spec = invocation
            .schema
            .get(name)
            .ok_or_else(|| anyhow!("attribute {name} has no schema"))?;
        let attr = match value {
            AttrValue::String(value) => heap.alloc(value.as_str()),
            AttrValue::Strings(value) => heap.alloc(value.clone()),
            AttrValue::StringDict(entries) => {
                crate::attribute_values::alloc_string_dict(heap, entries)?
            }
            AttrValue::StringListDict(entries) => {
                crate::attribute_values::alloc_string_list_dict(heap, entries)?
            }
            AttrValue::Int(value) => heap.alloc(*value),
            AttrValue::Ints(values) => heap.alloc(values.clone()),
            AttrValue::LabelKeyedStringDict(entries) => {
                // Nonempty values are rejected by the unanalyzed-feature gate.
                ensure!(
                    entries.is_empty(),
                    "attribute {name}: attr.label_keyed_string_dict values are {}",
                    crate::rule_declarations::NOT_ANALYZED
                );
                heap.alloc(starlark::values::dict::AllocDict::EMPTY)
            }
            AttrValue::Bool(value) => heap.alloc(*value),
            AttrValue::Label(value) => {
                let target = value
                    .as_ref()
                    .map(|label| dependency(label, name, spec, dependencies))
                    .transpose()?;
                let target_files = target
                    .map(|target| target.files.clone())
                    .unwrap_or_default();
                files.insert(
                    name.clone(),
                    heap.alloc(
                        target_files
                            .iter()
                            .cloned()
                            .map(|file| alloc_file(heap, file))
                            .collect::<Vec<_>>(),
                    ),
                );
                if spec.allow_single_file {
                    single_files.insert(
                        name.clone(),
                        target_files
                            .first()
                            .map(|file| alloc_file(heap, file.clone()))
                            .unwrap_or_else(Value::new_none),
                    );
                }
                target
                    .map(|target| {
                        crate::starlark_types::alloc_target_with_providers(
                            heap,
                            &target.label,
                            target.files.clone(),
                            target.providers.clone(),
                        )
                    })
                    .unwrap_or_else(Value::new_none)
            }
            AttrValue::Labels(labels) => {
                let targets = labels
                    .iter()
                    .map(|label| dependency(label, name, spec, dependencies))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                files.insert(
                    name.clone(),
                    heap.alloc(
                        targets
                            .iter()
                            .flat_map(|target| target.files.iter())
                            .cloned()
                            .map(|file| alloc_file(heap, file))
                            .collect::<Vec<_>>(),
                    ),
                );
                heap.alloc(
                    targets
                        .iter()
                        .map(|target| {
                            crate::starlark_types::alloc_target_with_providers(
                                heap,
                                &target.label,
                                target.files.clone(),
                                target.providers.clone(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            }
            AttrValue::Output(value) => {
                let output = value
                    .as_ref()
                    .map(|name| context.declare(name))
                    .transpose()?
                    .map(|file| alloc_file(heap, file))
                    .unwrap_or_else(Value::new_none);
                outputs.insert(name.clone(), output);
                // Output attributes belong to ctx.outputs, not ctx.attr.
                continue;
            }
            AttrValue::Outputs(values) => {
                let output = heap.alloc(
                    values
                        .iter()
                        .map(|name| context.declare(name).map(|file| alloc_file(heap, file)))
                        .collect::<anyhow::Result<Vec<_>>>()?,
                );
                outputs.insert(name.clone(), output);
                continue;
            }
        };
        attrs.insert(name.clone(), attr);
    }
    builtin_ctx_attrs(rule, invocation, &mut attrs, &mut files, heap)?;
    let label = alloc_target(heap, &rule.label.to_string(), Vec::new())
        .get_attr_error("label", heap)
        .map_err(|error| anyhow!(error.to_string()))?;
    let backing = heap.alloc(AllocStruct([
        ("attr", heap.alloc(AllocStruct(attrs))),
        ("files", heap.alloc(AllocStruct(files))),
        ("file", heap.alloc(AllocStruct(single_files))),
        ("outputs", heap.alloc(AllocStruct(outputs))),
        ("label", label),
        ("actions", alloc_actions(heap, context.scope.root())),
    ]));
    if invocation.build_setting.is_some() {
        // The configured value (StarlarkRuleContext.getBuildSettingValue,
        // analysis/starlark/StarlarkRuleContext.java:738-757): the command-line
        // value set by crate::engine, else build_setting_default.
        let value = match &invocation.setting_value {
            Some(value) => value.clone(),
            None => setting_default(invocation)?,
        };
        Ok(crate::build_setting_ctx::wrap(
            heap,
            backing,
            value,
            context.scope.root(),
        ))
    } else {
        Ok(crate::analysis_ctx::alloc_rule(
            heap,
            backing,
            context.scope.root(),
            None,
            invocation.target_platform.clone(),
        ))
    }
}

/// The typed `build_setting_default` of a build-setting rule invocation.
pub(crate) fn setting_default(
    invocation: &crate::custom_rule::CustomInvocation,
) -> anyhow::Result<crate::configuration::SettingValue> {
    use crate::configuration::SettingValue;
    Ok(
        match invocation
            .attrs
            .get("build_setting_default")
            .ok_or_else(|| anyhow!("build-setting invocation has no typed default"))?
        {
            AttrValue::Bool(value) => SettingValue::Bool(*value),
            AttrValue::Int(value) => SettingValue::Int(i64::from(*value)),
            AttrValue::String(value) => SettingValue::String(value.clone()),
            AttrValue::Strings(values) => SettingValue::StringList(values.clone()),
            other => bail!("unsupported build-setting default {other:?}"),
        },
    )
}

/// `ctx.attr`/`ctx.files` entries of Bazel's built-in attributes
/// (StarlarkRuleContext.java:205-266, StarlarkAttributesCollection.java:225-390):
/// every attribute of the rule class is exposed under its public name except
/// `aspect_hints` (filtered, StarlarkRuleContext.java:207-209) and
/// `exec_group_compatible_with` (shouldIgnore, :255-259); public names are
/// unique (clashes fail when the rule is exported, RuleClass.java:1905-1918).
/// Non-dependency values convert directly (`Attribute.valueToStarlark`, so an
/// unset `deprecation` is `None`); dependency label lists become Targets and
/// have `ctx.files` entries. Values are the configured (select-resolved)
/// ones; non-empty dependency lists never reach here (resolved-value gaps,
/// docs/COMMON_RULE_ATTRIBUTES.md).
fn builtin_ctx_attrs<'v>(
    rule: &Rule,
    invocation: &crate::custom_rule::CustomInvocation,
    attrs: &mut BTreeMap<String, Value<'v>>,
    files: &mut BTreeMap<String, Value<'v>>,
    heap: Heap<'v>,
) -> anyhow::Result<()> {
    use crate::native_rules::NativeType;
    let builtin = &invocation.declaration.builtin;
    let target = &builtin.target;
    let empty_targets = |name: &str| -> anyhow::Result<Value<'v>> {
        ensure!(
            target.labels(name).is_empty() && !target.configurable.contains_key(name),
            "{}: built-in attribute '{name}' dependencies are {}",
            rule.label,
            crate::rule_declarations::NOT_ANALYZED
        );
        Ok(heap.alloc(Vec::<Value<'v>>::new()))
    };
    let labels = |values: &[String]| -> anyhow::Result<Value<'v>> {
        Ok(heap.alloc(
            values
                .iter()
                .map(|label| crate::starlark_types::alloc_label(heap, label))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ))
    };
    let class = crate::native_rules::starlark_builtins(builtin.kind);
    let mut values = Vec::new();
    for attribute in class.attrs() {
        let name = attribute.name;
        if matches!(name, "aspect_hints" | "exec_group_compatible_with") {
            continue;
        }
        let explicit = target.attrs.get(name);
        let value = match (name, attribute.ty) {
            ("testonly", _) => heap.alloc(builtin.testonly),
            ("deprecation", _) => builtin
                .deprecation
                .as_deref()
                .map(|text| heap.alloc(text))
                .unwrap_or_else(Value::new_none),
            ("tags", _) => heap.alloc(target.tags.clone()),
            // Explicit values are stored; otherwise Rule.getAttr computes them
            // from the interior call stack, "" outside macros (Rule.java:395-405).
            ("generator_name" | "generator_function" | "generator_location", _)
                if !matches!(explicit, Some(AttrValue::String(_))) =>
            {
                let computed = builtin.generator.as_ref().map(|generator| match name {
                    "generator_name" => Some(generator.name.as_str()),
                    "generator_function" => Some(generator.function.as_str()),
                    _ => generator.location.as_deref(),
                });
                match computed {
                    None => heap.alloc(""),
                    Some(Some(text)) => heap.alloc(text),
                    Some(None) => Value::new_none(),
                }
            }
            ("visibility", _) => labels(builtin.visibility.as_deref().unwrap_or_default())?,
            (_, NativeType::LabelList) => {
                files.insert(name.to_owned(), heap.alloc(Vec::<Value<'v>>::new()));
                empty_targets(name)?
            }
            (_, NativeType::NodepLabelList) => labels(&target.labels(name))?,
            (_, NativeType::String) => match explicit {
                Some(AttrValue::String(text)) => heap.alloc(text.as_str()),
                _ => heap.alloc(match name {
                    "size" => "medium",
                    "timeout" => crate::custom_rule::default_test_timeout(
                        target.string("size").unwrap_or("medium"),
                    ),
                    _ => "",
                }),
            },
            (_, NativeType::StringList) => heap.alloc(target.strings(name)),
            (_, NativeType::StringDict) => match explicit {
                Some(AttrValue::StringDict(entries)) => {
                    crate::attribute_values::alloc_string_dict(heap, entries)?
                }
                _ => crate::attribute_values::alloc_string_dict(heap, &[])?,
            },
            (_, NativeType::Bool) => heap.alloc(matches!(explicit, Some(AttrValue::Bool(true)))),
            (_, NativeType::Int) => heap.alloc(match explicit {
                Some(AttrValue::Int(value)) => *value,
                // shard_count defaults to -1 (StarlarkRuleClassFunctions.java:281).
                _ => -1,
            }),
            (_, ty) => bail!(
                "{}: built-in attribute '{name}' of type {} has no ctx.attr value",
                rule.label,
                ty.name()
            ),
        };
        values.push((name.to_owned(), value));
    }
    // Private built-in dependency attributes have public names `_x`
    // (Attribute.getStarlarkName, packages/Attribute.java:2307-2312). Without
    // --experimental_action_listener `_action_listener` is empty, and
    // `$config_dependencies` is never resolved as a prerequisite
    // (analysis/DependencyResolutionHelpers.java:408-416), so both are empty
    // target lists even when the target has select() conditions.
    if !builtin.kind.dependency_resolution {
        for name in ["_action_listener", "_config_dependencies"] {
            files.insert(name.to_owned(), heap.alloc(Vec::<Value<'v>>::new()));
            values.push((name.to_owned(), heap.alloc(Vec::<Value<'v>>::new())));
        }
    }
    for (name, value) in values {
        attrs.insert(name, value);
    }
    Ok(())
}

pub fn analyze(
    rule: &Rule,
    dependencies: &BTreeMap<String, DependencyTarget>,
) -> Result<AnalysisResult, String> {
    analyze_in(
        rule,
        dependencies,
        Arc::new(RepositorySet::new(std::path::PathBuf::from("."))),
    )
}

pub fn analyze_in(
    rule: &Rule,
    dependencies: &BTreeMap<String, DependencyTarget>,
    repositories: Arc<RepositorySet>,
) -> Result<AnalysisResult, String> {
    let (result, printed) = analyze_buffered(rule, dependencies, repositories);
    for line in printed {
        eprintln!("{line}");
    }
    result
}

/// `print()` output of an implementation, kept until the caller decides
/// whether the attempt counts (an analysis retried for a deferred target
/// platform must print once, crate::engine).
#[derive(Default)]
struct BufferedPrints(RefCell<Vec<String>>);

impl starlark::PrintHandler for BufferedPrints {
    fn println(&self, text: &str) -> starlark::Result<()> {
        self.0.borrow_mut().push(text.to_owned());
        Ok(())
    }
}

/// [`analyze_in`] without writing the implementation's `print()` output:
/// the result and the printed lines, in order.
pub(crate) fn analyze_buffered(
    rule: &Rule,
    dependencies: &BTreeMap<String, DependencyTarget>,
    repositories: Arc<RepositorySet>,
) -> (Result<AnalysisResult, String>, Vec<String>) {
    let prints = BufferedPrints::default();
    let result = analyze_inner(rule, dependencies, repositories, &prints)
        .map_err(|error| format!("analysis of {}: {error:#}", rule.label));
    (result, prints.0.into_inner())
}

fn analyze_inner(
    rule: &Rule,
    dependencies: &BTreeMap<String, DependencyTarget>,
    repositories: Arc<RepositorySet>,
    prints: &BufferedPrints,
) -> anyhow::Result<AnalysisResult> {
    let invocation = rule
        .custom
        .as_ref()
        .ok_or_else(|| anyhow!("not a custom rule"))?;
    // Only configured targets are analyzed: every select() was resolved by
    // crate::configured_attributes::configure (docs/CONFIGURATION.md).
    ensure!(
        invocation.declaration.configurable.is_empty()
            && invocation
                .declaration
                .builtin
                .target
                .configurable
                .is_empty(),
        "internal error: unresolved select() reached analysis"
    );
    // Declared load-time features and resolved attribute values without native
    // analysis semantics fail explicitly instead of being silently ignored
    // (docs/LOAD_DECLARATIONS.md); crate::engine reports them before deps.
    let declaration = &invocation.declaration;
    let mut gaps = declaration.transition_gaps.clone();
    gaps.extend(declaration.toolchain_gaps.iter().cloned());
    gaps.extend(declaration.unanalyzed.iter().cloned());
    gaps.extend(crate::custom_rule::value_gaps(invocation));
    if !gaps.is_empty() {
        bail!(
            "declared rule features {}: {}",
            crate::rule_declarations::NOT_ANALYZED,
            gaps.join("; ")
        );
    }
    for (name, spec) in &invocation.schema {
        if spec.non_empty
            && match invocation.attrs.get(name) {
                Some(AttrValue::StringListDict(entries)) => entries.is_empty(),
                Some(AttrValue::StringDict(entries)) => entries.is_empty(),
                Some(
                    value @ (AttrValue::Strings(_)
                    | AttrValue::Labels(_)
                    | AttrValue::Outputs(_)
                    | AttrValue::Ints(_)),
                ) => crate::custom_rule::AttrKind::is_empty_value(value),
                _ => false,
            }
        {
            // RuleContext.java:1548 via EventHandlingErrorReporter.completeAttributeMessage
            // (analysis/EventHandlingErrorReporter.java:92-101).
            return Err(anyhow!(
                "in {name} attribute of {} rule {}: attribute must be non empty",
                invocation.declaration.rule_class,
                crate::rule_declarations::canonical_form(&rule.label)
            ));
        }
    }
    let context = Context {
        label: rule.label.clone(),
        dependency_files: dependencies
            .values()
            .flat_map(|target| target.files.iter().chain(&target.provider_files).cloned())
            .collect(),
        declared: RefCell::new(Vec::new()),
        actions: RefCell::new(Vec::new()),
        active: Arc::new(AtomicBool::new(true)),
        top_level_functions: invocation.top_level_functions.clone(),
        repositories,
        scope: crate::analysis_runtime::ScopeState::new(
            rule.label.to_string(),
            crate::subrule::direct_ids(invocation.subrules.iter().map(|value| value.value()))?,
        ),
    };
    for output in &rule.outs {
        context.declare(output)?;
    }
    let (files, providers, provider_files) = Module::with_temp_heap(
        |module| -> anyhow::Result<_> {
            let ctx = make_ctx(rule, &context, dependencies, module.heap())?;
            // Bazel's implicit DefaultInfo includes only predeclared outputs.
            let predeclared = context.declared.borrow().clone();
            let mut eval = Evaluator::new(&module);
            eval.set_max_callstack_size(128)
                .map_err(|error| anyhow!(error.to_string()))?;
            eval.extra = Some(&context);
            eval.set_print_handler(prints);
            let implementation = module
                .heap()
                .access_owned_frozen_value(&invocation.implementation);
            let returned = eval.eval_function(implementation, &[ctx], &[]);
            // Invalidate capabilities before inspecting or freezing providers,
            // on both successful callback return and ordinary evaluation error.
            context.scope.finish();
            context.active.store(false, Ordering::Release);
            let returned = returned.map_err(|error| anyhow!(error.to_string()))?;
            let files = default_files(returned)?.unwrap_or(predeclared);
            let custom = crate::providers::returned_custom_providers(returned)?;
            // checkDeclaredProviders (analysis/starlark/StarlarkRuleConfiguredTargetUtil.java:138-148)
            // iterates getStarlarkProviders() only: built-in provider keys
            // (DefaultInfo, OutputGroupInfo, ...) are never checked (:141).
            for provider in &invocation.declaration.provides {
                ensure!(
                    provider.id.starts_with("builtin:") || custom.contains_key(&provider.id),
                    "rule advertised the '{}' provider, but this provider was not among those returned",
                    provider.display_name
                );
            }
            // Executable/test rules are accepted at load time but rejected by the
            // unanalyzed-feature gate above, so every rule analyzed here is
            // non-executable/non-test. Future executable support MUST use real
            // rule classification here. The regular constructor sets
            // error_on_non_executable=true; testing.TestEnvironment sets it to
            // false (StarlarkRuleConfiguredTargetUtil: ruleWarning, provider kept).
            if custom
                .get(crate::run_environment_info::PROVIDER_ID)
                .is_some_and(|info| {
                    crate::run_environment_info::errors_on_non_executable_rule(*info)
                })
            {
                bail!(
                    "Returning RunEnvironmentInfo from a non-executable, non-test target has no effect"
                );
            }
            let mut slots = Vec::new();
            let mut provider_files = BTreeSet::new();
            for (identity, provider) in custom {
                provider_files.extend(crate::providers::provider_files(provider)?);
                let slot = format!("$provider:{}", slots.len());
                module.set(&slot, provider);
                slots.push((identity, slot));
            }
            drop(eval);
            let frozen = module
                .freeze_named(FrozenHeapName::User(Box::new(format!(
                    "analysis:{}",
                    rule.label
                ))))
                .map_err(|error| anyhow!("cannot freeze analysis providers: {error:?}"))?;
            let providers = slots
                .into_iter()
                .map(|(identity, slot)| {
                    frozen
                        .get_any_visibility(&slot)
                        .map(|(value, _)| (identity, value))
                })
                .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
            Ok((
                files,
                providers,
                provider_files.into_iter().collect::<Vec<_>>(),
            ))
        },
    )?;
    for file in files.iter().chain(&provider_files) {
        ensure!(
            context.known_file(file),
            "DefaultInfo contains unknown file {}",
            file.path
        );
    }
    let declared = std::mem::take(&mut *context.declared.borrow_mut());
    let actions = std::mem::take(&mut *context.actions.borrow_mut());
    for file in &declared {
        if !actions.iter().any(|action| action.outputs.contains(file)) {
            bail!("declared output {} has no generating action", file.path);
        }
    }
    let mut unique = BTreeSet::new();
    let files = files
        .into_iter()
        .filter(|file| unique.insert(file.clone()))
        .collect();
    Ok(AnalysisResult {
        files,
        declared,
        actions,
        providers,
        provider_files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::Label;
    use starlark::environment::Globals;
    use starlark::syntax::{AstModule, Dialect};

    #[test]
    fn string_dict_analysis_nonempty_freeze_unicode_and_action_environment() {
        let root = tempfile::Builder::new()
            .prefix("string-dict-analysis-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        std::fs::write(
            root.path().join("defs.bzl"),
            r#"
def impl(ctx):
    d = ctx.attr.d
    if ctx.attr.mutate:
        d["new"] = "changed"
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(outputs=[out], command="unused", env=d)
    return [DefaultInfo(files=depset([out]))]
r = rule(implementation=impl, attrs={"d":attr.string_dict(allow_empty=False), "mutate":attr.bool()})
"#,
        )
        .unwrap();
        std::fs::write(
            root.path().join("BUILD.bazel"),
            r#"
load(":defs.bzl", "r")
r(name="omitted")
r(name="empty", d={})
r(name="blank", d={"z":""})
r(name="good", d={"z":"café☃", "a":"@missing//p:t"})
r(name="mutate", d={"z":"v"}, mutate=True)
"#,
        )
        .unwrap();
        // Loading/query succeeds for both omitted and explicit empty values.
        let package = crate::package::evaluate(root.path(), "").unwrap();
        for name in ["omitted", "empty"] {
            let rule = &package[&Label::parse(&format!("//:{name}"), "").unwrap()];
            assert!(
                analyze(rule, &BTreeMap::new())
                    .unwrap_err()
                    .contains("attribute must be non empty")
            );
        }
        let rule = &package[&Label::parse("//:good", "").unwrap()];
        assert!(rule.srcs.is_empty());
        let result = analyze(rule, &BTreeMap::new()).unwrap();
        assert_eq!(result.actions[0].action.environment["z"], "café☃");
        assert_eq!(result.actions[0].action.environment["a"], "@missing//p:t");
        analyze(
            &package[&Label::parse("//:blank", "").unwrap()],
            &BTreeMap::new(),
        )
        .unwrap();
        let error = analyze(
            &package[&Label::parse("//:mutate", "").unwrap()],
            &BTreeMap::new(),
        )
        .unwrap_err()
        .to_lowercase();
        assert!(
            error.contains("frozen") || error.contains("immutable"),
            "{error}"
        );
    }

    #[test]
    fn run_environment_direct_return_rejected_after_callback_before_action_handoff() {
        let root = tempfile::Builder::new()
            .prefix("run-environment-analysis-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        std::fs::write(
            root.path().join("defs.bzl"),
            r#"
P = provider()
def impl(ctx):
    ctx.actions.write(ctx.outputs.out, "registered, never executed")
    info = RunEnvironmentInfo({"FLAG": "seed"}, ["HOME"])
    if ctx.attr.mode == "callback_failure":
        fail("callback reached after registration")
    if ctx.attr.mode == "direct":
        return info
    if ctx.attr.mode == "list":
        return [info]
    if ctx.attr.mode == "with_default":
        return [DefaultInfo(files=depset([ctx.outputs.out])), info]
    if ctx.attr.mode == "test_environment":
        return [DefaultInfo(files=depset([ctx.outputs.out])), testing.TestEnvironment({"FLAG": "seed"}, ["HOME"])]
    if ctx.attr.mode == "nested":
        return P(info=info, nested=[{"k": struct(info=(info,))}])
    if ctx.attr.mode == "nested_default":
        return [P(info=info), DefaultInfo()]
    return [P(info=info), DefaultInfo(files=depset())]
r = rule(implementation=impl, attrs={"mode": attr.string(), "out": attr.output()})
"#,
        )
        .unwrap();
        for mode in [
            "direct",
            "list",
            "with_default",
            "callback_failure",
            "nested",
            "nested_default",
            "nested_empty",
            "test_environment",
        ] {
            std::fs::write(
                root.path().join("BUILD.bazel"),
                format!(
                    "load(':defs.bzl', 'r')\nr(name='producer', mode='{mode}', out='output')\n"
                ),
            )
            .unwrap();
            let package = crate::package::evaluate(root.path(), "").unwrap();
            let rule = &package[&Label::parse("//:producer", "").unwrap()];
            let result = analyze(rule, &BTreeMap::new());
            match mode {
                "direct" | "list" | "with_default" => {
                    let error = result.unwrap_err();
                    assert!(error.contains("Returning RunEnvironmentInfo from a non-executable, non-test target has no effect"), "{mode}: {error}");
                }
                // StarlarkRuleConfiguredTargetUtil: TestEnvironment instances
                // only warn (shouldErrorOnNonExecutableRule=false) and are kept.
                "test_environment" => {
                    let result = result.unwrap();
                    assert_eq!(result.files.len(), 1);
                    assert_eq!(result.providers.len(), 1);
                    assert!(
                        result
                            .providers
                            .contains_key(crate::run_environment_info::PROVIDER_ID)
                    );
                }
                "callback_failure" => assert!(
                    result
                        .unwrap_err()
                        .contains("callback reached after registration")
                ),
                _ => {
                    let result = result.unwrap();
                    assert_eq!(result.actions.len(), 1);
                    assert!(result.actions[0].action.environment.is_empty());
                    assert_eq!(
                        result.actions[0].action.write.as_ref().unwrap().content,
                        b"registered, never executed"
                    );
                    assert_eq!(result.declared.len(), 1);
                    assert_eq!(result.files.len(), usize::from(mode != "nested_empty"));
                    assert!(result.provider_files.is_empty());
                    assert_eq!(result.providers.len(), 1);
                    assert!(
                        !result
                            .providers
                            .contains_key(crate::run_environment_info::PROVIDER_ID)
                    );
                }
            }
            assert!(
                !root
                    .path()
                    .join("bazel-out/rust-fastbuild/bin/output")
                    .exists()
            );
        }
    }

    #[test]
    fn run_environment_nested_consumer_and_actual_provider_requirement_gate() {
        let root = tempfile::Builder::new()
            .prefix("run-environment-consumer-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        std::fs::write(root.path().join("defs.bzl"), r#"
P = provider()
def producer_impl(ctx):
    return P(info=RunEnvironmentInfo({"FLAG": "seed"}, ["HOME", "HOME"]))
producer = rule(implementation=producer_impl)
def consumer_impl(ctx):
    if ctx.attr.strict:
        fail("strict consumer callback must not execute")
    target = ctx.attr.dep
    (RunEnvironmentInfo not in target) or fail("nested native key leaked")
    info = target[P].info
    copy = info.environment
    copy["FLAG"] = "local"
    (info.environment == {"FLAG": "seed"}) or fail("snapshot")
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(outputs=[out], command="printf '%s' \"$FLAG\"", env=info.environment)
    return [DefaultInfo(files=depset([out]))]
consumer = rule(implementation=consumer_impl, attrs={"dep": attr.label(providers=[P]), "strict": attr.bool()})
strict = rule(implementation=consumer_impl, attrs={"dep": attr.label(providers=[RunEnvironmentInfo]), "strict": attr.bool(default=True)})
"#).unwrap();
        std::fs::write(root.path().join("BUILD.bazel"), "load(':defs.bzl', 'producer', 'consumer', 'strict')\nproducer(name='producer')\nconsumer(name='consumer', dep=':producer')\nstrict(name='strict', dep=':producer')\n").unwrap();
        let package = crate::package::evaluate(root.path(), "").unwrap();
        let producer = analyze(
            &package[&Label::parse("//:producer", "").unwrap()],
            &BTreeMap::new(),
        )
        .unwrap();
        let deps = BTreeMap::from([(
            "//:producer".into(),
            DependencyTarget {
                label: "//:producer".into(),
                files: producer.files,
                is_source: false,
                is_rule: true,
                is_package_group: false,
                providers: producer.providers,
                provider_files: producer.provider_files,
            },
        )]);
        let consumer = analyze(&package[&Label::parse("//:consumer", "").unwrap()], &deps).unwrap();
        assert_eq!(consumer.actions.len(), 1);
        assert_eq!(
            consumer.actions[0].action.environment,
            BTreeMap::from([("FLAG".into(), "seed".into())])
        );
        let error = analyze(&package[&Label::parse("//:strict", "").unwrap()], &deps).unwrap_err();
        assert!(
            error.contains("mandatory providers") && error.contains("RunEnvironmentInfo"),
            "{error}"
        );
        assert!(!error.contains("strict consumer callback"), "{error}");
    }

    fn provider_fixture() -> (
        BTreeMap<String, DependencyTarget>,
        BTreeMap<String, AttrSpec>,
    ) {
        let frozen = Module::with_temp_heap(|module| {
            let context = crate::loading::ExtensionContext {
                byte_strings: false,
                label: Label::parse("//defs:providers.bzl", "").unwrap(),
                repositories: Arc::new(RepositorySet::new(std::path::PathBuf::from("."))),
            };
            let globals = starlark::environment::GlobalsBuilder::standard()
                .with(crate::custom_rule::rule_globals)
                .with(crate::providers::globals)
                .with(crate::template_variable_info::install)
                .build();
            let ast = AstModule::parse(
                "//defs:providers.bzl",
                r#"
P = provider()
Q = provider()
NotPackageSpecificationInfo = provider()
NotPackageSpecificationProvider = provider()
T = platform_common.TemplateVariableInfo
p = P()
t = T({"key": "value"})
unrestricted = attr.label()
default = attr.label(providers=[DefaultInfo])
both = attr.label(providers=[P, T])
alternative = attr.label_list(providers=[[P, Q], [T]])
missing = attr.label(providers=[Q])
group = attr.label(providers=[[NotPackageSpecificationInfo, Q]])
old_group_name = attr.label(providers=[NotPackageSpecificationProvider])
erased = attr.label(providers=[[], [NotPackageSpecificationInfo]])
"#
                .into(),
                &Dialect::Standard,
            )
            .unwrap();
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                eval.eval_module(ast, &globals).unwrap();
            }
            module.freeze().unwrap()
        });
        let providers = ["p", "t"]
            .into_iter()
            .map(|name| {
                let value = frozen.get(name).unwrap();
                (
                    crate::providers::instance_identity(value.value()).unwrap(),
                    value,
                )
            })
            .collect();
        let specs = [
            "unrestricted",
            "default",
            "both",
            "alternative",
            "missing",
            "group",
            "old_group_name",
            "erased",
        ]
        .into_iter()
        .map(|name| {
            let value = frozen.get(name).unwrap();
            (
                name.to_owned(),
                crate::custom_rule::attribute_spec(value.value()).unwrap(),
            )
        })
        .collect();
        let target = DependencyTarget {
            label: "//pkg:dep".into(),
            files: Vec::new(),
            is_source: false,
            is_rule: true,
            is_package_group: false,
            providers,
            provider_files: Vec::new(),
        };
        (BTreeMap::from([(target.label.clone(), target)]), specs)
    }

    #[test]
    fn ordinary_provider_constraints_use_actual_instances_and_synthesized_default() {
        let (mut deps, specs) = provider_fixture();
        for spec in ["unrestricted", "default", "both", "alternative"] {
            dependency("//pkg:dep", "dep", &specs[spec], &deps).unwrap();
        }
        let error = dependency("//pkg:dep", "dep", &specs["missing"], &deps)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("attribute dep")
                && error.contains("mandatory providers")
                && error.contains("Q"),
            "{error}"
        );
        deps.get_mut("//pkg:dep").unwrap().providers.clear();
        dependency("//pkg:dep", "another_attr", &specs["default"], &deps).unwrap();
        for spec in ["both", "alternative", "group"] {
            assert!(dependency("//pkg:dep", "another_attr", &specs[spec], &deps).is_err());
        }
        // An empty files collection does not turn an ordinary rule into a file.
        assert!(deps["//pkg:dep"].files.is_empty());
    }

    #[test]
    fn package_groups_have_separate_name_placement_gate_not_provider_satisfaction() {
        let (mut deps, specs) = provider_fixture();
        let group = deps.get_mut("//pkg:dep").unwrap();
        group.is_rule = false;
        group.is_package_group = true;
        group.providers.clear();
        for spec in ["group", "old_group_name"] {
            dependency("//pkg:dep", "g", &specs[spec], &deps).unwrap();
        }
        for spec in ["unrestricted", "default", "erased", "both"] {
            let error = dependency("//pkg:dep", "g", &specs[spec], &deps)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("package group") && error.contains("misplaced"),
                "{error}"
            );
            for name in ["visibility", "_allowlist_function_transition"] {
                dependency("//pkg:dep", name, &specs[spec], &deps).unwrap();
            }
        }
        let mut single = specs["group"].clone();
        single.allow_single_file = true;
        assert!(
            dependency("//pkg:dep", "g", &single, &deps)
                .unwrap_err()
                .to_string()
                .contains("exactly one file")
        );
    }

    #[test]
    fn file_targets_skip_provider_matching_but_keep_file_checks_first() {
        let (mut deps, specs) = provider_fixture();
        let target = deps.get_mut("//pkg:dep").unwrap();
        target.is_rule = false;
        target.is_source = true;
        target.providers.clear();
        let mut spec = specs["missing"].clone();
        assert!(
            dependency("//pkg:dep", "src", &spec, &deps)
                .unwrap_err()
                .to_string()
                .contains("allow_files=False")
        );
        spec.allow_files = true;
        dependency("//pkg:dep", "src", &spec, &deps).unwrap();
        spec.allow_single_file = true;
        assert!(
            dependency("//pkg:dep", "src", &spec, &deps)
                .unwrap_err()
                .to_string()
                .contains("exactly one file")
        );
        deps.get_mut("//pkg:dep").unwrap().files.push(File {
            path: "pkg/dep".into(),
            short_path: "pkg/dep".into(),
            owner: "//pkg:dep".into(),
            is_source: true,
        });
        dependency("//pkg:dep", "src", &spec, &deps).unwrap();
        // Output-file targets also have no rule class. Preserve the existing
        // is_source-only permission behavior independently of provider checks.
        deps.get_mut("//pkg:dep").unwrap().is_source = false;
        dependency("//pkg:dep", "out", &specs["missing"], &deps).unwrap();
    }

    #[test]
    fn provider_checks_precede_callback_and_check_each_attribute_and_list_item() {
        let root = tempfile::Builder::new()
            .prefix("provider-analysis-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        std::fs::write(
            root.path().join("defs.bzl"),
            r#"
P = provider()
def impl(ctx):
    fail("consumer callback entered")
r = rule(implementation=impl, attrs={
    "a": attr.label(providers=[DefaultInfo]),
    "b": attr.label(providers=[P]),
    "many": attr.label_list(providers=[platform_common.TemplateVariableInfo]),
})
"#,
        )
        .unwrap();
        let (mut deps, _) = provider_fixture();
        let mut second = deps["//pkg:dep"].clone();
        second.label = "//pkg:second".into();
        second.providers.clear();
        deps.insert(second.label.clone(), second);
        for (call, expected) in [
            (
                "r(name='consumer', a='//pkg:dep', b='//pkg:dep')",
                "attribute b",
            ),
            (
                "r(name='consumer', many=['//pkg:dep', '//pkg:second'])",
                "attribute many",
            ),
            ("r(name='consumer', b=None)", "consumer callback entered"),
        ] {
            std::fs::write(
                root.path().join("BUILD.bazel"),
                format!("load(':defs.bzl', 'r')\n{call}\n"),
            )
            .unwrap();
            let package = crate::package::evaluate(root.path(), "").unwrap();
            let rule = &package[&Label::parse("//:consumer", "").unwrap()];
            // Use the producer's real instance under its own exported identity;
            // this definition's distinct P intentionally cannot satisfy it.
            let error = analyze(rule, &deps).unwrap_err();
            assert!(error.contains(expected), "{call}: {error}");
            if expected == "attribute many" {
                assert!(error.contains("//pkg:second"), "{error}");
            }
            if expected != "consumer callback entered" {
                assert!(!error.contains("consumer callback entered"), "{error}");
            }
        }
    }

    fn empty_context() -> Context {
        Context {
            label: Label::parse("//pkg:rule", "").unwrap(),
            dependency_files: BTreeSet::new(),
            declared: RefCell::new(Vec::new()),
            actions: RefCell::new(Vec::new()),
            active: Arc::new(AtomicBool::new(true)),
            top_level_functions: Vec::new(),
            repositories: Arc::new(RepositorySet::new(std::path::PathBuf::from("."))),
            scope: crate::analysis_runtime::ScopeState::new("//pkg:rule".into(), BTreeSet::new()),
        }
    }

    fn evaluate(source: &str) -> anyhow::Result<Context> {
        let context = empty_context();
        Module::with_temp_heap(|module| -> anyhow::Result<()> {
            module.set(
                "ctx",
                module.heap().alloc(AllocStruct([(
                    "actions",
                    alloc_actions(module.heap(), context.scope.root()),
                )])),
            );
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            let ast = AstModule::parse("analysis-test.star", source.to_owned(), &Dialect::Standard)
                .map_err(|error| anyhow!(error.to_string()))?;
            eval.eval_module(ast, &Globals::standard())
                .map_err(|error| anyhow!(error.to_string()))?;
            Ok(())
        })?;
        Ok(context)
    }

    #[test]
    fn declarations_are_owned_canonical_and_idempotent() {
        let context = empty_context();
        let first = context.declare("dir/result").unwrap();
        assert_eq!(first, context.declare("dir/result").unwrap());
        assert_eq!(first.path, "bazel-out/rust-fastbuild/bin/pkg/dir/result");
        assert_eq!(first.short_path, "pkg/dir/result");
        assert_eq!(first.owner, "//pkg:rule");
        assert!(!first.is_source);
        assert_eq!(context.declared.borrow().len(), 1);
        for name in [
            "",
            "../out",
            "/out",
            "//other:out",
            ":out",
            "a//b",
            "a/./b",
            "a\\b",
        ] {
            assert!(context.declare(name).is_err(), "accepted {name:?}");
        }
    }

    #[test]
    fn write_records_exact_bytes_without_shell_or_io() {
        let context = evaluate("out = ctx.actions.declare_file('out')\nctx.actions.write(out, 'hi\\x00hé\\n', is_executable=True)\n").unwrap();
        let actions = context.actions.borrow();
        let action = &actions[0];
        assert_eq!(action.action.id, "//pkg:rule#0");
        assert!(action.action.command.is_empty());
        assert!(action.action.inputs.is_empty());
        assert!(action.inputs.is_empty());
        let write = action.action.write.as_ref().unwrap();
        assert_eq!(write.content, "hi\0hé\n".as_bytes());
        assert!(write.executable);
    }

    #[test]
    fn run_shell_records_arguments_environment_and_local_edges() {
        let context = evaluate(r#"first = ctx.actions.declare_file('first')
second = ctx.actions.declare_file('second')
ctx.actions.run_shell(outputs=[second], inputs=[first], command='cat "$1" > "$2"', arguments=[first.path, second.path], env={'WORD': 'a b'}, mnemonic='Copy', progress_message='copying')
ctx.actions.write(first, 'hello')
"#).unwrap();
        let actions = context.actions.borrow();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].inputs, actions[1].outputs);
        assert_eq!(actions[0].action.environment["WORD"], "a b");
        assert_eq!(actions[0].action.arguments.len(), 2);
        assert!(actions[0].action.inputs.is_empty());
        assert!(actions[0].action.write.is_none());
    }

    #[test]
    fn duplicate_producers_and_unsupported_options_are_rejected() {
        for (body, expected) in [
            (
                "ctx.actions.write(out, 'a')\nctx.actions.write(out, 'b')",
                "more than one",
            ),
            (
                "ctx.actions.run_shell(outputs=[], command='true')",
                "at least one",
            ),
            (
                "ctx.actions.run_shell(outputs=[out], command='true', use_default_shell_env=True)",
                "unsupported",
            ),
            (
                "ctx.actions.run_shell(outputs=[out], command='true', tools=[])",
                "tools",
            ),
            (
                "ctx.actions.run_shell(outputs=[out], command='true', arguments=[1])",
                "strings",
            ),
            (
                "ctx.actions.run_shell(outputs=[out], command='true', env={'X': 1})",
                "strings",
            ),
        ] {
            let source = format!("out = ctx.actions.declare_file('out')\n{body}\n");
            let error = match evaluate(&source) {
                Ok(_) => panic!("accepted {body}"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains(expected), "expected {expected:?} in {error}");
        }
    }

    #[test]
    fn foreign_outputs_and_unknown_inputs_are_rejected() {
        let context = empty_context();
        let output = context.declare("out").unwrap();
        let mut foreign = output.clone();
        foreign.owner = "//other:rule".into();
        assert!(
            context
                .register(
                    vec![foreign.clone()],
                    Vec::new(),
                    "true".into(),
                    None,
                    None,
                    Vec::new(),
                    BTreeMap::new()
                )
                .is_err()
        );
        assert!(
            context
                .register(
                    vec![output],
                    vec![foreign],
                    "true".into(),
                    None,
                    None,
                    Vec::new(),
                    BTreeMap::new()
                )
                .is_err()
        );
        assert!(context.actions.borrow().is_empty());
    }
}
