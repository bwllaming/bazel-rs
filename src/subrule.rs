//! Exported, traced zero-attribute subrules and immediate-caller invocation.
use allocative::Allocative;
use anyhow::{anyhow, bail, ensure};
use starlark::collections::StarlarkHasher;
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator, ParametersSpec, ParametersSpecParam};
use starlark::typing::Ty;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::AllocList;
use starlark::values::{FrozenValue, Value, ValueLike};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::OnceLock;

use crate::loading::ExtensionContext;
use crate::repository_budget::HostBudget;

#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::starlark_complex_value;
    use starlark::values::{
        Coerce, Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
        Trace, starlark_value,
    };

    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct Constructor;

    impl fmt::Display for Constructor {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<built-in function subrule>")
        }
    }

    starlark::starlark_simple_value!(Constructor);

    #[starlark_value(type = "builtin_function_or_method")]
    impl<'v> StarlarkValue<'v> for Constructor {
        fn typechecker_ty(&self) -> Option<Ty> {
            Some(constructor_signature().1.clone())
        }

        // Reexports from independently built globals denote the same builtin.
        // Default unhashability and empty attributes are intentional.
        fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
            Ok(other.downcast_ref::<Self>().is_some())
        }

        fn invoke(
            &self,
            _me: Value<'v>,
            args: &Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            forwarding_budget(args.len()?)?;
            let [implementation, attrs, toolchains, fragments, subrules] = constructor_signature()
                .0
                .collect_into::<5>(args, eval.heap())?;
            construct_subrule(
                implementation.expect("required constructor parameter was bound"),
                attrs.unwrap_or_else(|| eval.heap().alloc(AllocDict::EMPTY)),
                toolchains.unwrap_or_else(|| eval.heap().alloc(AllocList::EMPTY)),
                fragments.unwrap_or_else(|| eval.heap().alloc(AllocList::EMPTY)),
                subrules.unwrap_or_else(|| eval.heap().alloc(AllocList::EMPTY)),
                eval,
            )
            .map_err(starlark::Error::new_other)
        }
    }

    #[derive(
        Debug,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        Trace,
        Freeze,
        Coerce,
        StarlarkPagablePanic,
    )]
    #[repr(C)]
    pub(super) struct SubruleDefinitionGen<V> {
        pub implementation: V,
        pub subrules: Vec<V>,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub definition_label: String,
        #[trace(unsafe_ignore)]
        #[freeze(identity)]
        pub exported: OnceLock<String>,
    }

    impl<V> SubruleDefinitionGen<V> {
        pub(super) fn identity(&self) -> anyhow::Result<String> {
            let name = self
                .exported
                .get()
                .ok_or_else(|| anyhow!("invalid subrule hasn't been exported by a bzl file"))?;
            Ok(format!("{}%{name}", self.definition_label))
        }
    }
    impl<V> fmt::Display for SubruleDefinitionGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "<subrule {}>",
                self.exported
                    .get()
                    .map_or("unexported subrule", String::as_str)
            )
        }
    }
    starlark_complex_value!(pub(super) SubruleDefinition);

    #[starlark_value(type = "Subrule")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for SubruleDefinitionGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn export_as(&self, name: &str, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<()> {
            if self.exported.get().is_none() {
                let context = eval
                    .extra
                    .and_then(|extra| extra.downcast_ref::<ExtensionContext>());
                if context.is_none_or(|context| context.label.to_string() != self.definition_label)
                {
                    return Err(starlark::Error::new_other(anyhow!(
                        "subrule must be exported in its defining extension {}",
                        self.definition_label
                    )));
                }
                let mut budget = HostBudget::default();
                budget
                    .string(&self.definition_label)
                    .and_then(|()| budget.string(name))
                    .map_err(|e| starlark::Error::new_other(anyhow!(e)))?;
                let _ = self.exported.set(name.to_owned());
            }
            Ok(())
        }

        fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
            Ok(self.identity().ok().is_some_and(|id| {
                SubruleDefinition::from_value(other)
                    .is_some_and(|other| other.identity().ok().as_ref() == Some(&id))
            }))
        }

        fn write_hash(&self, _hasher: &mut StarlarkHasher) -> starlark::Result<()> {
            Err(starlark::Error::new_other(anyhow!(
                "unhashable type: 'Subrule'"
            )))
        }

        fn invoke(
            &self,
            _me: Value<'v>,
            args: &Arguments<'v, '_>,
            eval: &mut Evaluator<'v, '_, '_>,
        ) -> starlark::Result<Value<'v>> {
            let identity = self.identity().map_err(starlark::Error::new_other)?;
            let context =
                crate::analysis_runtime::context(eval).map_err(starlark::Error::new_other)?;
            let scope = context.scope.clone();
            let label = context.label.to_string();
            let allowed = direct_ids(self.subrules.iter().map(|value| value.to_value()))
                .map_err(starlark::Error::new_other)?;
            let guard = scope
                .enter(identity, allowed)
                .map_err(starlark::Error::new_other)?;
            // Check permissions before any argument unpacking. names_map clones
            // **kwargs, so charge the public shallow count before materializing
            // either forwarding container. Never traverse borrowed payloads.
            forwarding_budget(args.len()?)?;
            let positions = args.positions(eval.heap())?;
            let named = args.names_map()?;
            // Independently count actual slots: an iterable's reported length
            // must not permit an unchecked extend or size_hint reservation.
            let mut budget = forwarding_budget(named.len())?;
            let named = named
                .iter()
                .map(|(name, value)| (name.as_str(), *value))
                .collect::<Vec<_>>();
            let handle = guard.handle();
            let label = crate::starlark_types::alloc_target(eval.heap(), &label, Vec::new())
                .get_attr_error("label", eval.heap())?;
            let actions = crate::analysis_runtime::alloc_actions(eval.heap(), handle.clone());
            let ctx = crate::analysis_ctx::alloc_subrule(eval.heap(), label, actions, handle);
            let mut positional = vec![ctx];
            for value in positions {
                budget
                    .node()
                    .map_err(|error| starlark::Error::new_other(anyhow!(error)))?;
                positional.push(value);
            }
            // The owned guard holds no RefCell borrow across the call. It also
            // restores the caller on preflight, unpacking, binding or callback
            // errors, including early returns above.
            let result = eval.eval_function(self.implementation.to_value(), &positional, &named);
            drop(guard);
            result
        }
    }
}
use values::{SubruleDefinition, SubruleDefinitionGen};

fn constructor_signature() -> &'static (ParametersSpec<FrozenValue>, Ty) {
    static SIGNATURE: OnceLock<(ParametersSpec<FrozenValue>, Ty)> = OnceLock::new();
    SIGNATURE.get_or_init(|| {
        let (binding, params) = starlark::eval::param_specs(
            "subrule",
            [],
            [],
            None,
            [
                ("implementation", ParametersSpecParam::Required, Ty::any()),
                ("attrs", ParametersSpecParam::Optional, Ty::any()),
                ("toolchains", ParametersSpecParam::Optional, Ty::any()),
                ("fragments", ParametersSpecParam::Optional, Ty::any()),
                ("subrules", ParametersSpecParam::Optional, Ty::any()),
            ],
            None,
        )
        .expect("fixed subrule constructor signature");
        (
            binding,
            Ty::function(params, Ty::starlark_value::<SubruleDefinition>()),
        )
    })
}

/// One shallow slot budget covers the keyword map, both forwarding vectors,
/// their entries and the injected ctx slot. These are independent preflight and
/// actual-count passes, not two charges against one copy allowance. String keys
/// and user payloads stay borrowed; only the argument slots are copied.
fn forwarding_budget(slots: usize) -> starlark::Result<HostBudget> {
    let mut budget = HostBudget::default();
    // Three containers plus the injected context argument.
    for _ in 0..4 {
        budget
            .node()
            .map_err(|error| starlark::Error::new_other(anyhow!(error)))?;
    }
    for _ in 0..slots {
        budget
            .node()
            .map_err(|error| starlark::Error::new_other(anyhow!(error)))?;
    }
    Ok(budget)
}

fn charge(value: Value<'_>, budget: &mut HostBudget) -> anyhow::Result<()> {
    let definition = SubruleDefinition::from_value(value)
        .ok_or_else(|| anyhow!("subrules must contain only real Subrule values"))?;
    budget
        .node()
        .and_then(|()| budget.string(&definition.definition_label))
        .map_err(anyhow::Error::msg)?;
    if let Some(name) = definition.exported.get() {
        budget.string(name).map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

/// Snapshot only after all borrowed elements and their aggregate copy cost pass.
pub(crate) fn snapshot<'v>(
    value: Value<'v>,
    budget: &mut HostBudget,
) -> anyhow::Result<Vec<Value<'v>>> {
    let values = crate::custom_rule::sequence_ref(value)
        .ok_or_else(|| anyhow!("subrules must be a list or tuple of Subrule values"))?;
    budget.node().map_err(anyhow::Error::msg)?;
    preflight(values.iter().copied(), budget)?;
    Ok(values.to_vec())
}

pub(crate) fn preflight<'v>(
    values: impl IntoIterator<Item = Value<'v>>,
    budget: &mut HostBudget,
) -> anyhow::Result<()> {
    for value in values {
        charge(value, budget)?;
    }
    Ok(())
}

/// Rules require exports throughout the graph, even though I0 lifts no attrs.
/// The graph is immutable after construction; visiting each exported identity
/// once bounds shared DAGs. Charge each frontier before copying its edges.
pub(crate) fn validate_exports(
    values: &[Value<'_>],
    budget: &mut HostBudget,
) -> anyhow::Result<()> {
    for &value in values {
        charge(value, budget)?;
    }
    let mut pending = values.to_vec();
    let mut seen = BTreeSet::new();
    while let Some(value) = pending.pop() {
        let definition = SubruleDefinition::from_value(value).expect("validated Subrule");
        let identity = definition.identity()?;
        if seen.insert(identity) {
            for &child in &definition.subrules {
                charge(child, budget)?;
            }
            pending.extend(definition.subrules.iter().copied());
        }
    }
    Ok(())
}

pub(crate) fn direct_ids<'v>(
    values: impl IntoIterator<Item = Value<'v>>,
) -> anyhow::Result<BTreeSet<String>> {
    let mut budget = HostBudget::default();
    let mut ids = BTreeSet::new();
    for value in values {
        charge(value, &mut budget)?;
        ids.insert(
            SubruleDefinition::from_value(value)
                .expect("validated Subrule")
                .identity()?,
        );
    }
    Ok(ids)
}

pub(crate) fn globals(builder: &mut GlobalsBuilder) {
    builder.set("subrule", values::Constructor);
}

fn construct_subrule<'v>(
    implementation: Value<'v>,
    attrs: Value<'v>,
    toolchains: Value<'v>,
    fragments: Value<'v>,
    subrules: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<Value<'v>> {
    let context = eval
        .extra
        .and_then(|extra| extra.downcast_ref::<ExtensionContext>())
        .ok_or_else(|| {
            anyhow!("subrule() definitions are only allowed during .bzl initialization")
        })?;
    ensure!(
        implementation.parameters_spec().is_some(),
        "subrule implementation must be a Starlark function"
    );
    let attrs =
        DictRef::from_value(attrs).ok_or_else(|| anyhow!("subrule attrs must be a dict"))?;
    if !attrs.is_empty() {
        bail!("nonempty subrule attrs are unsupported");
    }
    for (name, value) in [("toolchains", toolchains), ("fragments", fragments)] {
        let values = crate::custom_rule::sequence_ref(value)
            .ok_or_else(|| anyhow!("subrule {name} must be a list or tuple"))?;
        ensure!(values.is_empty(), "nonempty subrule {name} are unsupported");
    }
    let mut budget = HostBudget::default();
    let subrules = snapshot(subrules, &mut budget)?;
    let label = context.label.to_string();
    budget.string(&label).map_err(anyhow::Error::msg)?;
    Ok(eval.heap().alloc(SubruleDefinitionGen {
        implementation,
        subrules,
        definition_label: label,
        exported: OnceLock::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis_runtime::{Context, ScopeState};
    use starlark::environment::{FrozenModule, Globals, Module};
    use starlark::syntax::{AstModule, Dialect};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::sync::{Arc, atomic::AtomicBool};

    fn extension(source: &str, globals: &Globals) -> Result<FrozenModule, String> {
        Module::with_temp_heap(|module| {
            let context = ExtensionContext {
                byte_strings: false,
                label: crate::label::Label::parse_canonical("//:defs.bzl").unwrap(),
                repositories: Arc::new(crate::repository::RepositorySet::new(".".into())),
            };
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&context);
                eval.eval_module(
                    AstModule::parse("//:defs.bzl", source.into(), &Dialect::Standard).unwrap(),
                    globals,
                )
                .map_err(|error| error.to_string())?;
            }
            module.freeze().map_err(|error| format!("{error:?}"))
        })
    }

    fn fixture(defs: &str, build: &str) -> Result<crate::package::Package, String> {
        let root = tempfile::Builder::new()
            .prefix("subrule-owned-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        std::fs::write(root.path().join("defs.bzl"), defs).unwrap();
        std::fs::write(root.path().join("BUILD.bazel"), build).unwrap();
        crate::package::evaluate(root.path(), "")
    }

    fn analyze(defs: &str) -> Result<crate::analysis::AnalysisResult, String> {
        let package = fixture(defs, "load(':defs.bzl', 'r')\nr(name='target')\n")?;
        crate::analysis::analyze(
            &package[&crate::label::Label::parse_canonical("//:target").unwrap()],
            &BTreeMap::new(),
        )
    }

    #[test]
    fn native_constructor_metadata_binding_and_frozen_alias_callable_signature() {
        for globals in [
            crate::package::bzl_globals(),
            crate::package::module_bzl_globals(),
        ] {
            extension(r#"
(type(subrule) == 'builtin_function_or_method') or fail('constructor type')
(repr(subrule) == '<built-in function subrule>' and str(subrule) == repr(subrule)) or fail('constructor repr')
(dir(subrule) == [] and not hasattr(subrule, 'type')) or fail('constructor attributes')
(subrule == subrule and subrule != len) or fail('constructor identity')
def impl(ctx):
    pass
S = subrule(*[], **{'implementation': impl, 'attrs': {}, 'toolchains': (), 'fragments': [], 'subrules': ()})
(type(S) == 'Subrule' and repr(S) == '<subrule S>') or fail('constructor binding')
"#, &globals).unwrap();
            for (source, expected) in [
                ("subrule.type", "type"),
                ("subrule[int]", "not supported"),
                ("{subrule: 1}", "not hashable"),
                (
                    "subrule(implementation=impl, **{'implementation': impl})",
                    "more than once",
                ),
                ("subrule(*[impl], **{})", "named-only"),
                ("subrule(**{'implementation': impl, 'doc': ''})", "doc"),
                ("subrule(implementation=subrule)", "Starlark function"),
            ] {
                let error = extension(&format!("def impl(ctx):\n    pass\n{source}\n"), &globals)
                    .unwrap_err();
                assert!(error.contains(expected), "{source}: {error}");
            }
        }
        let first = extension("R = subrule", &crate::package::bzl_globals())
            .unwrap()
            .get("R")
            .unwrap();
        let second = extension("R = subrule", &crate::package::module_bzl_globals())
            .unwrap()
            .get("R")
            .unwrap();
        Module::with_temp_heap(|module| {
            let first_value = module.heap().access_owned_frozen_value(&first);
            let second_value = module.heap().access_owned_frozen_value(&second);
            drop(first);
            drop(second);
            assert!(first_value.equals(second_value).unwrap());
            assert!(first_value.downcast_ref::<values::Constructor>().is_some());
            assert!(first_value.parameters_spec().is_none());
            assert_ne!(Ty::of_value(first_value), Ty::any());
            first_value
                .check_callable_with([], [("implementation", &Ty::any())], None, None, &Ty::any())
                .unwrap();
            assert!(
                first_value
                    .check_callable_with([], [], None, None, &Ty::any())
                    .is_err()
            );
            assert!(
                first_value
                    .check_callable_with([&Ty::any()], [], None, None, &Ty::any())
                    .is_err()
            );
            assert!(
                first_value
                    .check_callable_with(
                        [],
                        [("implementation", &Ty::any()), ("unknown", &Ty::any())],
                        None,
                        None,
                        &Ty::any()
                    )
                    .is_err()
            );
        });
    }

    #[test]
    fn constructor_strict_signature_profile_and_both_bzl_phases() {
        for globals in [
            crate::package::bzl_globals(),
            crate::package::module_bzl_globals(),
        ] {
            for (call, expected) in [
                ("subrule()", "implementation"),
                ("subrule(impl)", "named-only"),
                ("subrule(implementation=len)", "Starlark function"),
                ("subrule(implementation=provider())", "Starlark function"),
                (
                    "subrule(implementation=rule(implementation=impl))",
                    "Starlark function",
                ),
                ("subrule(implementation=None)", "Starlark function"),
                ("subrule(implementation=impl, doc='x')", "doc"),
                ("subrule(implementation=impl, unknown=[])", "unknown"),
                ("subrule(implementation=impl, attrs=None)", "dict"),
                ("subrule(implementation=impl, attrs=[])", "dict"),
                (
                    "subrule(implementation=impl, attrs={'_x':attr.label()})",
                    "unsupported",
                ),
                (
                    "subrule(implementation=impl, toolchains=None)",
                    "list or tuple",
                ),
                (
                    "subrule(implementation=impl, toolchains=['//:tc'])",
                    "unsupported",
                ),
                (
                    "subrule(implementation=impl, fragments={})",
                    "list or tuple",
                ),
                (
                    "subrule(implementation=impl, fragments=['java'])",
                    "unsupported",
                ),
                (
                    "subrule(implementation=impl, subrules=None)",
                    "list or tuple",
                ),
                ("subrule(implementation=impl, subrules=[impl])", "Subrule"),
                (
                    "subrule(implementation=impl, subrules=[struct()])",
                    "Subrule",
                ),
                ("rule(implementation=impl, subrules=[impl])", "Subrule"),
                (
                    "rule(implementation=impl, subrules=subrule(implementation=impl))",
                    "in call to rule(), parameter 'subrules' got value of type 'Subrule', want 'sequence'",
                ),
                (
                    "rule(implementation=impl, subrules=[subrule(implementation=impl)])",
                    "exported",
                ),
                ("rule(implementation=impl, subrules=[S])", "exported"),
            ] {
                // S itself is exported but its nested dependency is not.
                let source = format!(
                    "def impl(ctx):\n    pass\nS = subrule(implementation=impl, subrules=[subrule(implementation=impl)])\nx = {call}\n"
                );
                let error = extension(&source, &globals).unwrap_err();
                assert!(
                    error.contains(expected),
                    "{call}: expected {expected:?}: {error}"
                );
            }
            extension("def impl():\n    pass\nS = subrule(implementation=impl, attrs={}, toolchains=(), fragments=[], subrules=())\n(type(S) == 'Subrule' and repr(S) == '<subrule S>' and not hasattr(native, 'subrule')) or fail('binding')\n", &globals).unwrap();
        }
    }

    #[test]
    fn constructor_analysis_phase_export_hash_and_budget_boundaries() {
        for (defs, build, expected) in [
            ("", "x = subrule(implementation=len)", "subrule"),
            (
                "def make():\n    def impl(ctx):\n        pass\n    return subrule(implementation=impl)\n",
                "load(':defs.bzl','make')\nx = make()",
                ".bzl initialization",
            ),
            (
                "def impl(ctx):\n    pass\nS = subrule(implementation=impl)\nS()",
                "",
                "rule analysis",
            ),
            (
                "def impl(ctx):\n    pass\nx = [subrule(implementation=impl)]\nx[0]()",
                "",
                "exported",
            ),
            (
                "def impl(ctx):\n    pass\nS = subrule(implementation=impl)\nx = {S:1}",
                "",
                "unhashable",
            ),
            (
                "def impl(ctx):\n    pass\nS = subrule(implementation=impl)\nT = subrule(implementation=impl, subrules=[S]*100001)",
                "",
                "limit exceeded",
            ),
        ] {
            let error = if build.is_empty() {
                extension(defs, &crate::package::bzl_globals()).unwrap_err()
            } else {
                fixture(defs, build).unwrap_err()
            };
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }
    }

    #[test]
    fn frozen_closure_graph_aliases_and_snapshots_survive_all_loader_owners() {
        let package = fixture(
            r#"
P = provider()
def factory(seed):
    captured = [seed]
    def leaf(ctx, suffix, *, ending):
        (dir(ctx) == ['actions', 'fragments', 'label', 'toolchains']) or fail('subrule fields')
        out = ctx.actions.declare_file('result')
        ctx.actions.write(out, captured[0] + suffix + ending)
        return P(file=out, owner=ctx.label)
    return subrule(implementation=leaf)
Leaf = factory('seed')
Alias = Leaf
Other = factory('other')
(Leaf == Alias and Leaf != Other and repr(Alias) == '<subrule Leaf>') or fail('identity')
deps = [Alias]
def outer(ctx):
    return Alias('-A', ending='!\n')
Outer = subrule(implementation=outer, subrules=deps)
deps.clear()
declared = [Outer]
def impl(ctx):
    (dir(ctx) == ['actions', 'attr', 'file', 'files', 'label', 'outputs', 'target_platform_has_constraint', 'toolchains']) or fail('rule fields')
    p = Outer()
    (p.owner == ctx.label) or fail('wrong owner')
    return [p, DefaultInfo(files=depset([p.file]))]
r = rule(implementation=impl, subrules=declared)
declared.clear()
"#,
            "load(':defs.bzl','r')\nr(name='target')",
        )
        .unwrap();
        let rule = package[&crate::label::Label::parse_canonical("//:target").unwrap()].clone();
        // Package loading's scratch workspace and loader modules are already gone.
        drop(package);
        assert_eq!(rule.custom.as_ref().unwrap().subrules.len(), 1);
        let result = crate::analysis::analyze(&rule, &BTreeMap::new()).unwrap();
        assert_eq!(result.actions.len(), 1);
        assert_eq!(
            result.actions[0].action.write.as_ref().unwrap().content,
            b"seed-A!\n"
        );
        assert_eq!(result.files[0].owner, "//:target");
        assert_eq!(result.providers.len(), 1);
        let provider = result.providers.into_values().next().unwrap();
        drop(rule);
        Module::with_temp_heap(|module| {
            let value = module.heap().access_owned_frozen_value(&provider);
            drop(provider);
            let file = value.get_attr_error("file", module.heap()).unwrap();
            assert_eq!(
                crate::starlark_types::unpack_file(file).unwrap().owner,
                "//:target"
            );
        });
    }

    #[test]
    fn immediate_permissions_do_not_expand_with_caller_container_or_transitive_graph() {
        for (declared, call) in [
            ("[Outer]", "Leaf()"),
            ("declared", "Leaf()"),
            ("[BadOuter]", "BadOuter()"),
        ] {
            let defs = format!(
                r#"
def leaf(ctx):
    pass
Leaf = subrule(implementation=leaf)
def outer(ctx):
    Leaf()
Outer = subrule(implementation=outer, subrules=[Leaf])
BadOuter = subrule(implementation=outer)
declared = []
def impl(ctx):
    {call}
r = rule(implementation=impl, subrules={declared})
declared.append(Leaf)
"#
            );
            let error = analyze(&defs).unwrap_err();
            assert!(error.contains("immediate caller"), "{error}");
        }
    }

    #[test]
    fn locked_properties_structural_introspection_and_all_consuming_receivers() {
        for expression in [
            "parent.label",
            "getattr(parent, 'label', None)",
            "parent.attr",
            "parent.files",
            "parent.file",
            "parent.outputs",
            "parent.actions",
            "actions.declare_file('forbidden')",
            "actions.write(out, 'forbidden')",
            "actions.run_shell(outputs=[out], command='true')",
            "actions.run(outputs=[out], executable='/bin/true')",
            "declare('forbidden')",
            "write(out, 'forbidden')",
            "shell(outputs=[out], command='true')",
            "run(outputs=[out], executable='/bin/true')",
        ] {
            let defs = format!(
                r#"
def child(ctx, parent, actions, out, declare, write, shell, run):
    {expression}
S = subrule(implementation=child)
def impl(ctx):
    a = ctx.actions
    out = a.declare_file('out')
    S(ctx, a, out, a.declare_file, a.write, a.run_shell, a.run)
r = rule(implementation=impl, subrules=[S])
"#
            );
            let error = analyze(&defs).unwrap_err();
            assert!(error.contains("scope receiver"), "{expression}: {error}");
        }
        let result = analyze(r#"
def child(ctx, parent):
    text = str(hasattr(parent, 'label')) + '|' + str('label' in dir(parent)) + '|' + getattr(parent, 'missing', 'fallback')
    out = ctx.actions.declare_file('out')
    ctx.actions.write(out, text + '\n')
S = subrule(implementation=child)
def impl(ctx):
    S(ctx)
r = rule(implementation=impl, subrules=[S])
"#).unwrap();
        assert_eq!(
            result.actions[0].action.write.as_ref().unwrap().content,
            b"True|True|fallback\n"
        );
        for field in ["fragments", "toolchains"] {
            let error = analyze(&format!("def child(ctx):\n    ctx.{field}\nS=subrule(implementation=child)\ndef impl(ctx):\n    S()\nr=rule(implementation=impl, subrules=[S])\n")).unwrap_err();
            assert!(error.contains("unsupported"), "{error}");
        }
    }

    #[test]
    fn live_ancestor_args_factories_and_child_builders_keep_root_lifetime() {
        let result = analyze(
            r#"
def child(ctx, actions, factory, parent_args):
    from_object = actions.args()
    from_bound = factory()
    from_child = ctx.actions.args()
    from_object.add('object')
    from_bound.add('bound')
    from_child.add('child')
    parent_args.add('parent')
    return [from_object, from_bound, from_child]
S = subrule(implementation=child)
def impl(ctx):
    parent = ctx.actions.args()
    builders = S(ctx.actions, ctx.actions.args, parent)
    builders[2].add('after')
    parent.add('after-parent')
    out = ctx.actions.declare_file('out')
    ctx.actions.run_shell(outputs=[out], command='unused', arguments=builders+[parent])
r = rule(implementation=impl, subrules=[S])
"#,
        )
        .unwrap();
        assert_eq!(
            result.actions[0].action.arguments,
            [
                "object",
                "bound",
                "child",
                "after",
                "parent",
                "after-parent"
            ]
        );
    }

    #[test]
    fn escaped_context_and_action_factories_are_invalid_even_in_same_subrule_later() {
        for expression in [
            "old.label",
            "getattr(old, 'label', None)",
            "old.actions",
            "old.declare_file('bad')",
            "old.write(out, 'bad')",
            "old.run_shell(outputs=[out], command='true')",
            "old.run(outputs=[out], executable='/bin/true')",
            "old.args()",
            "old()",
        ] {
            let returned = if expression == "old()" {
                "ctx.actions.args"
            } else if expression.contains("old.label")
                || expression.contains("getattr")
                || expression == "old.actions"
            {
                "ctx"
            } else {
                "ctx.actions"
            };
            for later in [false, true] {
                let call = if later {
                    "S(old, out)".to_owned()
                } else {
                    expression.to_owned()
                };
                let defs = format!(
                    r#"
def child(ctx, old=None, out=None):
    if old != None:
        {expression}
    return {returned}
S = subrule(implementation=child)
def impl(ctx):
    old = S()
    out = ctx.actions.declare_file('out')
    {call}
r = rule(implementation=impl, subrules=[S])
"#
                );
                let error = analyze(&defs).unwrap_err();
                assert!(
                    error.contains("scope receiver"),
                    "{expression} later={later}: {error}"
                );
            }
        }
    }

    #[test]
    fn forwarding_budgets_precede_materialization_and_restore_parent_on_errors() {
        let frozen = extension(
            "def callback(ctx, required):\n    return required\nS = subrule(implementation=callback)\nT = subrule(implementation=callback)\n",
            &crate::package::bzl_globals(),
        ).unwrap();
        let allowed = frozen.get("S").unwrap();
        let denied = frozen.get("T").unwrap();
        drop(frozen);
        let context = Context {
            label: crate::label::Label::parse_canonical("//:target").unwrap(),
            output_root: crate::label::GENERATED_EXEC_ROOT.into(),
            action_owner: "//:target".into(),
            dependency_files: BTreeSet::new(),
            declared: RefCell::new(Vec::new()),
            actions: RefCell::new(Vec::new()),
            active: Arc::new(AtomicBool::new(true)),
            top_level_functions: Vec::new(),
            repositories: Arc::new(crate::repository::RepositorySet::new(".".into())),
            scope: ScopeState::new("//:target".into(), direct_ids([allowed.value()]).unwrap()),
        };
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let allowed = heap.access_owned_frozen_value(&allowed);
            let denied = heap.access_owned_frozen_value(&denied);
            let actions = crate::analysis_runtime::alloc_actions(heap, context.scope.root());
            let declare = actions.get_attr_error("declare_file", heap).unwrap();
            let write = actions.get_attr_error("write", heap).unwrap();
            // The large guest containers can trigger GC while evaluating the
            // fixture. Root the host-captured native bound methods explicitly.
            module.set("parent_declare", declare);
            module.set("parent_write", write);
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            eval.eval_module(
                AstModule::parse(
                    "forwarding.star",
                    r#"
large_pos = [None] * 100001
large_kw = {str(i): None for i in range(100001)}
invalid_large_kw = {i: None for i in range(100001)}
def oversized_pos(s):
    return s(*large_pos)
def oversized_kw(s):
    return s(**large_kw)
def oversized_invalid_kw(s):
    return s(**invalid_large_kw)
def invalid_key(s):
    return s(**{1: None})
def duplicate_key(s):
    return s(required=1, **{'required': 2})
def invalid_star(s):
    return s(*None)
def invalid_kw(s):
    return s(**None)
def late_binding(s):
    return s(unexpected=1)
def list_payload(s):
    return s(large_pos)
def dict_payload(s):
    return s(required=large_kw)
"#
                    .into(),
                    &Dialect::Standard,
                )
                .unwrap(),
                &Globals::standard(),
            )
            .unwrap();
            for name in ["oversized_pos", "oversized_kw", "oversized_invalid_kw"] {
                let function = module.get(name).unwrap();
                let error = eval
                    .eval_function(function, &[allowed], &[])
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains("host preparation node limit exceeded"),
                    "{name}: {error}"
                );
                context.scope.root().current(&context.scope).unwrap();
                let error = eval
                    .eval_function(function, &[denied], &[])
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("immediate caller"), "{name}: {error}");
                assert!(!error.contains("limit exceeded"), "{name}: {error}");
                context.scope.root().current(&context.scope).unwrap();
            }
            for (name, expected) in [
                ("invalid_key", "identifier"),
                ("duplicate_key", "more than once"),
                ("invalid_star", "len"),
                ("invalid_kw", "dictionary"),
                ("late_binding", "required"),
            ] {
                let function = module.get(name).unwrap();
                let error = eval
                    .eval_function(function, &[allowed], &[])
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(expected), "{name}: {error}");
                context.scope.root().current(&context.scope).unwrap();
                let error = eval
                    .eval_function(function, &[denied], &[])
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("immediate caller"), "{name}: {error}");
                context.scope.root().current(&context.scope).unwrap();
            }
            // Large borrowed payloads are one slot, not recursively copied or
            // charged. Their identity survives forwarding and callback return.
            for (function, payload) in [("list_payload", "large_pos"), ("dict_payload", "large_kw")]
            {
                let returned = eval
                    .eval_function(module.get(function).unwrap(), &[allowed], &[])
                    .unwrap();
                assert!(returned.ptr_eq(module.get(payload).unwrap()));
                context.scope.root().current(&context.scope).unwrap();
            }
            let out = eval
                .eval_function(
                    module.get("parent_declare").unwrap(),
                    &[heap.alloc("restored")],
                    &[],
                )
                .unwrap();
            eval.eval_function(
                module.get("parent_write").unwrap(),
                &[out, heap.alloc("restored after forwarding failures")],
                &[],
            )
            .unwrap();
            assert_eq!(context.actions.borrow().len(), 1);
            assert_eq!(context.declared.borrow().len(), 1);
        });
    }

    #[test]
    fn callback_and_binding_errors_restore_actual_parent_before_host_continues() {
        let frozen = extension(
            r#"
def callback(ctx, sink, mode):
    sink.append(ctx)
    if mode == 'fail':
        fail('callback failure')
    return ctx
S = subrule(implementation=callback)
"#,
            &crate::package::bzl_globals(),
        )
        .unwrap();
        let owned = frozen.get("S").unwrap();
        drop(frozen);
        let context = Context {
            label: crate::label::Label::parse_canonical("//:target").unwrap(),
            output_root: crate::label::GENERATED_EXEC_ROOT.into(),
            action_owner: "//:target".into(),
            dependency_files: BTreeSet::new(),
            declared: RefCell::new(Vec::new()),
            actions: RefCell::new(Vec::new()),
            active: Arc::new(AtomicBool::new(true)),
            top_level_functions: Vec::new(),
            repositories: Arc::new(crate::repository::RepositorySet::new(".".into())),
            scope: ScopeState::new("//:target".into(), direct_ids([owned.value()]).unwrap()),
        };
        Module::with_temp_heap(|module| {
            let s = module.heap().access_owned_frozen_value(&owned);
            drop(owned);
            let actions =
                crate::analysis_runtime::alloc_actions(module.heap(), context.scope.root());
            let declare = actions
                .get_attr_error("declare_file", module.heap())
                .unwrap();
            let write = actions.get_attr_error("write", module.heap()).unwrap();
            let sink = module.heap().alloc(Vec::<Value>::new());
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            // Injected ctx still fails ordinary argument binding for missing sink/mode.
            assert!(eval.eval_function(s, &[], &[]).is_err());
            context.scope.root().current(&context.scope).unwrap();
            let error = eval
                .eval_function(s, &[sink, module.heap().alloc("fail")], &[])
                .unwrap_err();
            assert!(error.to_string().contains("callback failure"));
            context.scope.root().current(&context.scope).unwrap();
            let escaped = starlark::values::list::ListRef::from_value(sink)
                .unwrap()
                .content()[0];
            assert!(
                escaped
                    .get_attr_error("label", module.heap())
                    .unwrap_err()
                    .to_string()
                    .contains("scope receiver")
            );
            let returned = eval
                .eval_function(s, &[sink, module.heap().alloc("ok")], &[])
                .unwrap();
            assert!(returned.get_attr_error("actions", module.heap()).is_err());
            let out = eval
                .eval_function(declare, &[module.heap().alloc("restored")], &[])
                .unwrap();
            eval.eval_function(write, &[out, module.heap().alloc("parent is live")], &[])
                .unwrap();
            assert_eq!(context.actions.borrow().len(), 1);
            assert_eq!(
                context.actions.borrow()[0]
                    .action
                    .write
                    .as_ref()
                    .unwrap()
                    .content,
                b"parent is live"
            );
            context.scope.finish();
            assert!(
                eval.eval_function(declare, &[module.heap().alloc("expired")], &[])
                    .is_err()
            );
        });
    }
}
