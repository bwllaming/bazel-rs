//! The N23c members of the native rule context (crate::analysis_ctx): the
//! StarlarkRuleContext getters of analysis/starlark/StarlarkRuleContext.java
//! (paths relative to `src/main/java/com/google/devtools/build/lib/`) over
//! crate::ctx_surface. Every read checks the current scope
//! (StarlarkRuleContext.checkMutable, :441-447, 491-498).
use anyhow::{anyhow, bail};
use starlark::environment::MethodsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::none::NoneType;
use starlark::values::{Heap, Value};

use crate::analysis_ctx::current_surface;
use crate::ctx_surface as surface;

fn sequence<'v>(value: Value<'v>) -> Option<Vec<Value<'v>>> {
    crate::custom_rule::sequence_ref(value).map(<[Value<'v>]>::to_vec)
}

#[starlark_module]
pub(crate) fn surface_properties(builder: &mut MethodsBuilder) {
    /// StarlarkRuleContext.getFragments (:725-729).
    #[starlark(attribute)]
    fn fragments<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let rule = current_surface(this)?;
        Ok(surface::alloc_rule_fragments(heap, rule.clone()))
    }
    /// StarlarkRuleContext.getConfiguration (:731-735).
    #[starlark(attribute)]
    fn configuration<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let rule = current_surface(this)?;
        Ok(heap.alloc(surface::Configuration {
            surface: rule.config.clone(),
        }))
    }
    /// StarlarkRuleContext.getBinDirectory (:788-792).
    #[starlark(attribute)]
    fn bin_dir<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let rule = current_surface(this)?;
        Ok(surface::alloc_root(heap, &rule.config.bin_dir))
    }
    /// StarlarkRuleContext.getGenfilesDirectory (:794-798), merged into bin.
    #[starlark(attribute)]
    fn genfiles_dir<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let rule = current_surface(this)?;
        Ok(surface::alloc_root(heap, &rule.config.bin_dir))
    }
    /// StarlarkRuleContext.var (:832-850): an immutable dict.
    #[starlark(attribute)]
    fn var<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let rule = current_surface(this)?;
        Ok(surface::frozen_string_dict(
            heap,
            &surface::make_variables(&rule.config)?,
        ))
    }
    /// RuleContext.getWorkspaceName (analysis/RuleContext.java:383-385): the
    /// main repository's workspace name, `_main` under Bzlmod.
    #[starlark(attribute)]
    fn workspace_name<'v>(this: Value<'v>) -> anyhow::Result<&'static str> {
        current_surface(this)?;
        Ok("_main")
    }
    /// StarlarkRuleContext.getBuildFileRelativePath (:1006-1016);
    /// `--incompatible_stop_exporting_build_file_path` is false by default.
    #[starlark(attribute)]
    fn build_file_path<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let rule = current_surface(this)?;
        Ok(heap.alloc(rule.config.build_file_path.as_str()))
    }
    /// StarlarkRuleContext.instrumentCoverage (:760-774): false unless
    /// coverage is enabled (InstrumentedFilesCollector.shouldIncludeLocalSources
    /// returns false first, analysis/test/InstrumentedFilesCollector.java).
    /// With `--collect_code_coverage` (set natively only by a Starlark
    /// transition, N47) the answer needs `--instrumentation_filter` and
    /// `--instrument_test_targets`, which are not modelled: explicit.
    fn coverage_instrumented<'v>(
        this: Value<'v>,
        #[starlark(default = NoneType)] target: Value<'v>,
    ) -> anyhow::Result<bool> {
        let rule = current_surface(this)?;
        if !target.is_none() && target.get_type() != "Target" {
            bail!(
                "in call to coverage_instrumented(), parameter 'target' got value of type '{}', want 'Target or NoneType'",
                target.get_type()
            );
        }
        if rule.config.coverage_enabled {
            bail!(
                "{}",
                crate::ctx_surface::coverage_gap("ctx.coverage_instrumented()")
            );
        }
        Ok(false)
    }
    // ctx.exec_groups is N35's StarlarkExecGroupCollection over the
    // ToolchainCollection (crate::analysis_ctx rule_properties,
    // crate::exec_groups).
    /// StarlarkRuleContext.expandMakeVariables (:950-978).
    fn expand_make_variables<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] attribute_name: &str,
        #[starlark(require = pos)] command: &str,
        #[starlark(require = pos)] additional_substitutions: Value<'v>,
    ) -> anyhow::Result<String> {
        let rule = current_surface(this)?;
        let dict = DictRef::from_value(additional_substitutions).ok_or_else(|| {
            anyhow!(
                "in call to expand_make_variables(), parameter 'additional_substitutions' got value of type '{}', want 'dict'",
                additional_substitutions.get_type()
            )
        })?;
        let mut additional = Vec::new();
        for (key, value) in dict.iter() {
            match (key.unpack_str(), value.unpack_str()) {
                (Some(key), Some(value)) => additional.push((key.to_owned(), value.to_owned())),
                _ => bail!(
                    "got dict<{}, {}> for 'additional_substitutions', want dict<string, string>",
                    key.get_type(),
                    value.get_type()
                ),
            }
        }
        surface::expand_make_variables(rule, attribute_name, command, &additional)
    }
    /// StarlarkRuleContext.expandLocation (:1029-1053).
    fn expand_location<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] input: &str,
        #[starlark(default = NoneType)] targets: Value<'v>,
        #[starlark(require = named, default = false)] short_paths: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<String> {
        let rule = current_surface(this).map_err(starlark::Error::new_other)?;
        let mut entries: Vec<(String, Vec<crate::starlark_types::File>)> = Vec::new();
        if !targets.is_none() {
            let items = sequence(targets).ok_or_else(|| {
                starlark::Error::new_other(anyhow!(
                    "in call to expand_location(), parameter 'targets' got value of type '{}', want 'sequence'",
                    targets.get_type()
                ))
            })?;
            for (index, item) in items.iter().enumerate() {
                let Some((label, files)) = crate::starlark_types::target_location_entry(*item)
                else {
                    return Err(starlark::Error::new_other(anyhow!(
                        "at index {index} of targets, got element of type {}, want Target",
                        item.get_type()
                    )));
                };
                // makeLabelMap (StarlarkRuleContext.java:1288-1293).
                if entries.iter().any(|(seen, _)| *seen == label) {
                    return Err(starlark::Error::new_other(anyhow!(
                        "Label {} is found more than once in 'targets' list.",
                        eval.heap().alloc(label.as_str()).to_repr()
                    )));
                }
                entries.push((label, files));
            }
        }
        if short_paths {
            crate::subrule_fragments::private_api(eval)?;
        }
        surface::expand_location(rule, input, &entries, short_paths)
            .map_err(starlark::Error::new_other)
    }
    /// StarlarkRuleContext.getStableWorkspaceStatus (:994-998).
    #[starlark(attribute)]
    fn info_file<'v>(this: Value<'v>) -> anyhow::Result<NoneType> {
        current_surface(this)?;
        bail!("ctx.info_file is not implemented by the native profile (workspace status artifacts)")
    }
    /// StarlarkRuleContext.getVolatileWorkspaceStatus (:1000-1004).
    #[starlark(attribute)]
    fn version_file<'v>(this: Value<'v>) -> anyhow::Result<NoneType> {
        current_surface(this)?;
        bail!(
            "ctx.version_file is not implemented by the native profile (workspace status artifacts)"
        )
    }
    /// StarlarkRuleContext.getFeatures (:776-780): an immutable list.
    #[starlark(attribute)]
    fn features<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let rule = current_surface(this)?;
        Ok(surface::frozen_string_list(heap, &rule.config.features.on))
    }
    /// StarlarkRuleContext.getDisabledFeatures (:782-786).
    #[starlark(attribute)]
    fn disabled_features<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let rule = current_surface(this)?;
        Ok(surface::frozen_string_list(heap, &rule.config.features.off))
    }
    /// StarlarkRuleContext.tokenize (:917-927).
    fn tokenize<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] option_string: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        current_surface(this)?;
        let tokens = surface::tokenize(option_string)
            .map_err(|error| anyhow!("{error} while tokenizing '{option_string}'"))?;
        Ok(surface::frozen_string_list(heap, &tokens))
    }
    /// StarlarkRuleContext.checkPlaceholders (:933-948).
    fn check_placeholders<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] template: &str,
        #[starlark(require = pos)] allowed_placeholders: Value<'v>,
    ) -> anyhow::Result<bool> {
        current_surface(this)?;
        let allowed = sequence(allowed_placeholders)
            .ok_or_else(|| {
                anyhow!(
                    "in call to check_placeholders(), parameter 'allowed_placeholders' got value of type '{}', want 'sequence'",
                    allowed_placeholders.get_type()
                )
            })?
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                value.unpack_str().map(str::to_owned).ok_or_else(|| {
                    anyhow!(
                        "at index {index} of allowed_placeholders, got element of type {}, want string",
                        value.get_type()
                    )
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(surface::placeholders(template)
            .iter()
            .all(|placeholder| allowed.contains(placeholder)))
    }
    /// StarlarkRuleContext.packageRelativeLabel (:1219-1234).
    fn package_relative_label<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] input: Value<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let rule = current_surface(this)?;
        if input.get_type() == "Label" {
            return Ok(input);
        }
        let Some(text) = input.unpack_str() else {
            bail!(
                "in call to package_relative_label(), parameter 'input' got value of type '{}', want 'string or Label'",
                input.get_type()
            );
        };
        // LabelParser target-name validation (cmdline/LabelParser.java):
        // the target name after the first ':' may not contain another ':'.
        if let Some((_, name)) = text.split_once(':')
            && name.contains(':')
        {
            bail!(
                "invalid label in ctx.package_relative_label: invalid target name '{name}': target names may not contain ':'"
            );
        }
        let label = crate::label::Label::parse_in(
            text,
            &rule.label.repository,
            &rule.label.package,
            &rule.mapping,
        )
        .map_err(|error| anyhow!("invalid label in ctx.package_relative_label: {error}"))?;
        crate::starlark_types::alloc_label(heap, &crate::rule_declarations::canonical_form(&label))
    }
    /// StarlarkRuleContext.resolveCommand (:1137-1195): not modelled.
    #[allow(clippy::too_many_arguments)]
    fn resolve_command<'v>(
        this: Value<'v>,
        #[starlark(require = named)] command: Option<Value<'v>>,
        #[starlark(require = named)] attribute: Option<Value<'v>>,
        #[starlark(require = named)] expand_locations: Option<Value<'v>>,
        #[starlark(require = named)] make_variables: Option<Value<'v>>,
        #[starlark(require = named)] tools: Option<Value<'v>>,
        #[starlark(require = named)] label_dict: Option<Value<'v>>,
        #[starlark(require = named)] execution_requirements: Option<Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = (
            command,
            attribute,
            expand_locations,
            make_variables,
            tools,
            label_dict,
            execution_requirements,
        );
        current_surface(this)?;
        bail!("ctx.resolve_command is not implemented by the native profile")
    }
    /// StarlarkRuleContext.resolveTools (:1197-1217) with
    /// `--incompatible_disallow_ctx_resolve_tools` (default true).
    fn resolve_tools<'v>(
        this: Value<'v>,
        #[starlark(require = named)] tools: Option<Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = tools;
        current_surface(this)?;
        bail!(
            "Pass an executable or tools argument to ctx.actions.run or ctx.actions.run_shell instead of calling ctx.resolve_tools.\nUse --noincompatible_disallow_ctx_resolve_tools to temporarily disable this check."
        )
    }
    /// StarlarkRuleContext.rule (:809-816).
    #[starlark(attribute)]
    fn rule<'v>(this: Value<'v>) -> anyhow::Result<NoneType> {
        current_surface(this)?;
        bail!("'rule' is only available in aspect implementations")
    }
    /// StarlarkRuleContext.aspectIds (:818-829).
    #[starlark(attribute)]
    fn aspect_ids<'v>(this: Value<'v>) -> anyhow::Result<NoneType> {
        current_surface(this)?;
        bail!("'aspect_ids' is only available in aspect implementations")
    }
    /// StarlarkRuleContext.getSplitAttr (:683-689): the attributes with split
    /// transitions; Starlark transitions on attributes are not modelled here.
    #[starlark(attribute)]
    fn split_attr<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let rule = current_surface(this)?;
        if rule.split_transition_attribute {
            bail!(
                "ctx.split_attr of an attribute with a Starlark transition is not implemented by the native profile"
            );
        }
        Ok(surface::empty_struct(heap))
    }
    /// StarlarkRuleContext.createdActions (:667-675): None unless
    /// `_skylark_testable` (not analyzed natively).
    fn created_actions<'v>(this: Value<'v>) -> anyhow::Result<NoneType> {
        current_surface(this)?;
        Ok(NoneType)
    }
}

#[starlark_module]
pub(crate) fn non_setting_properties(builder: &mut MethodsBuilder) {
    /// StarlarkRuleContext.getBuildSettingValue (:738-745) of a non-setting.
    #[starlark(attribute)]
    fn build_setting_value<'v>(this: Value<'v>) -> anyhow::Result<NoneType> {
        let rule = current_surface(this)?;
        bail!(
            "attempting to access 'build_setting_value' of non-build setting {}",
            crate::rule_declarations::canonical_form(&rule.label)
        )
    }
}
