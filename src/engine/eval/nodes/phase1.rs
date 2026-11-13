//! The phase-1 bodies (docs/EVALUATOR.md, worker B): the sequential
//! walk's phase-1 analysis (`analyze` and the memo functions it calls),
//! written once over [`Host`] — the walk's `Walk` (the engine and its
//! phase-1 maps, today's behaviour) or the evaluator's `EvalHost` (node
//! values, transcript events, `Cx::need`).
#![allow(clippy::too_many_arguments)]

use super::host::Host;
use crate::engine::*;

pub(crate) trait Phase1: Host {
    fn resolve_rule_src(&self, rule: &Rule, text: &str) -> Result<Label, String> {
        if matches!(rule.kind, RuleKind::Custom)
            || (rule.native.is_some() && (text.starts_with("//") || text.starts_with("@@")))
        {
            // Custom and native attributes were canonicalized during package evaluation.
            // In particular, //p:t here denotes main, even in an external rule.
            Label::parse_canonical(text)
        } else {
            // Native rules keep raw apparent-repository labels; Bazel's
            // attribute conversion never fails on a non-visible repository
            // (Label.parseWithPackageContext), only its package lookup does
            // (crate::label::unresolvable_package_message).
            self.repositories()
                .parse_deferred(text, &rule.label.repository, &rule.label.package)
        }
    }

    /// [`Self::visible`] for a source file's visibility.
    fn visible_source(
        &mut self,
        label: &Label,
        declaring: &PackageId,
        requesting: Option<&PackageId>,
    ) -> Result<bool, String> {
        let visibility = self.source_visibility(label);
        self.visible(&visibility, declaring, requesting)
    }

    /// Resolve every effective group reference, even when a pure scope or the
    /// same-authority fast path would already grant access. Roots pass None:
    /// their outgoing visibility dependencies are validated, not access-checked.
    fn visible(
        &mut self,
        visibility: &Visibility,
        declaring: &PackageId,
        requesting: Option<&PackageId>,
    ) -> Result<bool, String> {
        let mut group_grant = false;
        for label in visibility.group_labels() {
            let node = self.resolve_group(label)?;
            group_grant |= requesting.is_some_and(|package| node.contains(package));
        }
        Ok(
            requesting
                .is_none_or(|package| visibility.allows_in(declaring, package) || group_grant),
        )
    }

    /// The walk's analysis of `label` in `config` (Engine::analyze_key's
    /// body): sources, the load-time gates, the configured-target memo and
    /// the active chain, then [`Self::analyze_rule`]. The evaluator's
    /// `analyze` (a `Src`/`Ct1` need) and `Ct1` body split it at the memo.
    fn analyze_key_body(
        &mut self,
        label: &Label,
        config: ConfigId,
        apply_rule_transition: bool,
    ) -> Result<(), String> {
        if self.source_analyzed(label) {
            self.track_source(label);
            return Ok(());
        }
        let rule = match self.target(label)? {
            Target::Source(_) if self.ahead() => {
                return Err(self.ahead_abort("source analysis"));
            }
            Target::Source(_) => {
                self.track_source(label);
                let visibility = self.source_visibility(label);
                self.visible(&visibility, &label.package_id(), None)?;
                self.record_source_analyzed(label);
                return Ok(());
            }
            Target::Rule(rule) => rule,
            Target::Output(rule, _) => {
                check_macro_namespace(&rule, label)?;
                rule
            }
        };
        self.rule_gates(&rule)?;
        let key = ConfiguredTargetKey {
            label: rule.label.clone(),
            config,
            apply_rule_transition,
        };
        let tracked = self.track_key(&key);
        self.record_keep_going_edge(&key);
        if self.configured_contains(&key) {
            return Ok(());
        }
        if self.ahead() {
            return Err(self.ahead_abort("configured-target analysis"));
        }
        self.track_enter(tracked);
        let plan_len = self.plan_len();
        self.track_entry(tracked, plan_len);
        self.prefetch_rule(&rule);
        if let Some(start) = self.active_position(&key) {
            let active = self.active_keys();
            return Err(self.cycle_error(&rule, &active[..start], &active[start..]));
        }
        if self.active_len() >= 256 {
            return Err("native prototype dependency depth limit (256) exceeded".into());
        }
        self.analyze_rule(rule, key, tracked)
    }

    /// The load-time gates of a rule target, checked before memoization
    /// (on every visit in the walk; once per `Ct1` node in the evaluator).
    fn rule_gates(&mut self, rule: &Arc<Rule>) -> Result<(), String> {
        // Check the requested target and its generator before memoization, but
        // never scan unrelated outputs or impose this analysis gate on query.
        check_macro_namespace(rule, &rule.label)?;
        if matches!(rule.kind, RuleKind::PackageGroup(_)) {
            self.resolve_group(&rule.label)?;
        }
        // A rule transition decides the configuration everything else is
        // evaluated in (TargetAndConfigurationProducer); `config.none()` is
        // not modelled.
        if let Some(custom) = &rule.custom
            && !custom.declaration.transition_gaps.is_empty()
        {
            return Err(custom_gap_error(rule, &custom.declaration.transition_gaps));
        }
        Ok(())
    }

    /// Phase 1 of the configured target `key` of `rule` (a `Ct1` node's
    /// body): everything after the memo and the cycle check.
    fn analyze_rule(
        &mut self,
        rule: Arc<Rule>,
        key: ConfiguredTargetKey,
        tracked: Option<u32>,
    ) -> Result<(), String> {
        let config = key.config;
        // A Starlark rule transition (RuleTransitionApplier): a target whose
        // configuration it changes is the target analyzed under the
        // transitioned key.
        if key.apply_rule_transition
            && rule
                .custom
                .as_ref()
                .is_some_and(|custom| custom.transitions.contains_key(""))
        {
            self.active_push(&key);
            let delegate = self.rule_transition(&rule, config)?;
            if let Some(owner) = delegate {
                self.analyze_key(&rule.label, owner.config, owner.apply_rule_transition)?;
                self.active_pop();
                let node = self.configured_node(&owner).unwrap_or_default();
                self.publish_node(&key, node);
                self.record_delegation(key, owner);
                return Ok(());
            }
            self.active_pop();
        }
        let configuration = self.configuration(config);
        let no_config = configuration.native.no_config;
        self.active_push(&key);
        if self.cquery()
            && let RuleKind::PackageGroup(group) = &rule.kind
        {
            for include in &group.includes {
                self.analyze(include, config)?;
                self.capture_cquery_edge(&key, include.clone(), config, "includes", false, false);
            }
        }
        // constraint_setting and constraint_value are analyzed under
        // NoConfigTransition (rules/platform/ConstraintSettingRule.java:42,
        // ConstraintValueRule.java:41): select() in their configurable
        // attributes would be evaluated in the no-op configuration, which is
        // not modelled.
        if let RuleKind::Native(kind @ ("constraint_setting" | "constraint_value")) = rule.kind
            && !configured_attributes::rule_config_dependencies(&rule).is_empty()
        {
            return Err(format!(
                "{}: select() in {kind} attributes is not implemented in the native profile (Bazel analyzes {kind} targets in the no-op configuration, NoConfigTransition)",
                rule.label
            ));
        }
        // select() in the no-config configuration would match config_settings
        // against CoreOptions defaults only, with every other fragment
        // missing: not modelled.
        if no_config && !configured_attributes::rule_config_dependencies(&rule).is_empty() {
            return Err(format!(
                "{}: select() in the no-config configuration (a package_metadata dependency, NoConfigTransition) is not implemented in the native profile",
                rule.label
            ));
        }
        // DependencyContextProducerWithCompatibilityCheck
        // (analysis/producers/DependencyContextProducerWithCompatibilityCheck.java:80-110, 132-145):
        // a rule that uses toolchain resolution resolves its target platform
        // before its select() conditions. A non-default target platform is
        // resolved so; the default host platform only where a constraint
        // needs it (constraint_value keys in config_conditions,
        // config_setting constraint_values matching below; docs/PLATFORMS.md).
        // Inside a platform's tree (the empty configuration, no
        // PlatformConfiguration) no rule resolves toolchains
        // (ToolchainContextUtil.java:79-82: platformConfig == null).
        // The no-config configuration has no PlatformConfiguration either
        // (CommonOptions.EMPTY_OPTIONS): no toolchain resolution, so no
        // execution platform and exec edges keep it
        // (ExecutionTransitionFactory.java:192-197).
        let in_empty_configuration = self.platform_tree() || no_config;
        if in_empty_configuration {
            self.record_empty_configuration(&key);
        }
        let uses_toolchains = uses_toolchain_resolution(&rule) && !in_empty_configuration;
        if self.cquery() && uses_toolchains {
            let platform = configuration.native.target_platform().clone();
            self.platform_info(&platform, config)
                .map_err(|e| e.target_text())?;
        }
        // The exec groups (ToolchainContextUtil.getUnloadedToolchainContextsInputs,
        // RuleTransitionApplier): before the target platform.
        let (use_auto_exec_groups, processed_groups) =
            self.exec_group_inputs(&rule, config, !in_empty_configuration)?;
        let mut target_platform = None;
        if uses_toolchains && !self.default_target_platform(config) {
            target_platform = Some(self.target_platform(config)?);
        }
        // PrerequisiteProducer order (analysis/producers/PrerequisiteProducer.java):
        // select() conditions, then toolchain resolution, then dependency
        // resolution (attribute validation, dependencies), then the factory.
        let ordered = self.config_conditions(&rule, config)?;
        if self.cquery() {
            for (condition, _) in &ordered {
                let dependency = self.resolve_rule_src(&rule, condition)?;
                self.capture_cquery_edge(
                    &key,
                    dependency,
                    config,
                    "$config_dependencies",
                    false,
                    false,
                );
            }
        }
        // Platform compatibility after the target platform and the select()
        // conditions, before toolchain resolution and every other dependency
        // (DependencyContextProducerWithCompatibilityCheck.java:159-189).
        let compatibility =
            self.check_target_compatibility(&rule, config, &ordered, uses_toolchains)?;
        if let Some(incompatible) = compatibility.incompatible {
            self.publish_incompatible(key, rule, ordered, incompatible);
            self.active_pop();
            return Ok(());
        }
        if self.cquery() && uses_toolchains {
            self.capture_cquery_edge(
                &key,
                configuration.native.target_platform().clone(),
                config,
                "$target_platform",
                true,
                false,
            );
        }
        if let Some(custom) = &rule.custom
            && !custom.declaration.toolchain_gaps.is_empty()
        {
            return Err(custom_gap_error(&rule, &custom.declaration.toolchain_gaps));
        }
        // ToolchainResolutionFunction (skyframe/toolchains/ToolchainResolutionFunction.java:62-131)
        // for the default exec group; under automatic exec groups its key has
        // no toolchain types (ToolchainContextUtil.createDefaultToolchainContextKey,
        // :120-144).
        let toolchain_context = if uses_toolchains {
            let mut key = self.toolchain_context_key(&rule, config)?;
            if use_auto_exec_groups {
                key.types.clear();
            }
            self.resolve_toolchains(&rule, config, key)?
        } else {
            None
        };
        // Every other exec group of the ToolchainCollection, after the
        // default context (UnloadedToolchainContextsProducer.java:88-126;
        // only the first error propagates, :181-186): the declared groups
        // (the default `test` group of a Starlark test rule,
        // RuleClass.java:153-167), then the automatic groups. Divergence D3
        // (docs/TEST_FLAGS.md): without the built-in profile there is no
        // `@bazel_tools`, so the default test group is not resolved.
        let mut exec_groups = crate::exec_groups::TargetExecGroups {
            use_auto_exec_groups,
            groups: Vec::new(),
        };
        if uses_toolchains {
            for group in processed_groups {
                let types = group
                    .key
                    .types
                    .iter()
                    .map(|requirement| requirement.label.clone())
                    .collect();
                let context = if group.builtin_only {
                    None
                } else {
                    self.resolve_toolchains(&rule, config, group.key)?
                };
                exec_groups.groups.push(crate::exec_groups::ResolvedGroup {
                    name: group.name,
                    types,
                    context,
                    builtin_only: group.builtin_only,
                });
            }
        }
        let conditions = ordered.iter().cloned().collect::<BTreeMap<_, _>>();
        let resolver = configured_attributes::Resolver {
            target: &rule.label,
            conditions: &conditions,
            config_hash: config.to_string(),
        };
        // DependencyResolutionHelpers.visitRule reports a validation failure
        // at the rule's location (analysis/DependencyResolutionHelpers.java:309-313).
        let configured_rule = configured_attributes::configure(&rule, &resolver)
            .map_err(|error| located(rule.location.as_deref(), error))?;
        let duplicate_errors =
            configured_attributes::concatenated_duplicate_errors(&rule, &configured_rule);
        let mut rule = configured_rule;
        // An alias-like rule has one dependency, its (resolved) actual: the
        // `actual` attribute, or the late-bound `:alias` of label_flag /
        // label_setting (rules/LabelBuildSettings.java:62-83).
        let build_setting = self.build_setting_info(&rule)?;
        if forwards(&rule) {
            let actual = self.alias_actual(&rule, &configuration, build_setting.as_ref())?;
            if rule.srcs.as_slice() != [actual.as_str()] {
                Arc::make_mut(&mut rule).srcs = vec![actual];
            }
        }
        // Late-bound attributes skip the visibility check
        // (CommonPrerequisiteValidator.java:159-163); rules that can have any
        // provider skip the testonly check (:730-738).
        let late_bound_rule = matches!(rule.kind, RuleKind::Native("label_flag" | "label_setting"));
        let analysis_dependent = rule
            .custom
            .as_ref()
            .map(|custom| {
                crate::late_bound::analysis_dependent_labels(&custom.schema, &custom.attrs)
            })
            .unwrap_or_default();
        let skip_testonly = forwards(&rule);
        let requesting_package = rule_authority(&rule);
        // The configuration of each dependency edge
        // (DependencyResolutionHelpers.getExecutionPlatformLabel,
        // analysis/DependencyResolutionHelpers.java:223-253): the exec
        // transition needs the parent's execution platform, which toolchain
        // resolution chose before any dependency (ExecutionTransitionFactory).
        let edges = self.rule_edges(&rule, config);
        // The native genrule embeds this real built-in source in its action
        // instead of requesting a provider view. Configured querying retains
        // its source-file dependency when the built-in repository is enabled.
        if self.cquery()
            && matches!(rule.kind, RuleKind::Genrule)
            && self.repositories().root("bazel_tools").is_ok()
        {
            let setup = Label::parse_canonical("@@bazel_tools//tools/genrule:genrule-setup.sh")?;
            self.analyze(&setup, config)?;
            self.capture_cquery_edge(&key, setup, config, "$genrule_setup", true, false);
        }
        // ExecutionTransitionFactory.create (analysis/config/ExecutionTransitionFactory.java:76-114):
        // in the no-config (and a platform's empty) configuration
        // CommonOptions.EMPTY_OPTIONS has no Starlark exec transition
        // (`starlarkExecConfig = null`, analysis/config/CommonOptions.java:34-41),
        // so every exec-transition dependency edge fails to create its
        // transition before any dependency is analyzed - unless the rule has
        // an attribute named `license_kinds` (AttributeMap.has, :109-111, the
        // rules_license license() hack), whose exec edges become
        // NoConfigTransition. Genrules always have the exec edge
        // `$genrule_setup` (bazel/rules/genrule/BazelGenRuleRule.java:44-46),
        // Starlark test rules their `$test_*` tool edges
        // (StarlarkRuleClassFunctions.java:290-352).
        if in_empty_configuration {
            let custom_exec = rule.custom.as_ref().is_some_and(|custom| {
                custom.declaration.builtin.kind.test && !custom.schema.contains_key("license_kinds")
            });
            let has_license_kinds = rule
                .custom
                .as_ref()
                .is_some_and(|custom| custom.schema.contains_key("license_kinds"));
            let exec_edge = edges
                .iter()
                .any(|edge| matches!(edge.cfg, EdgeCfg::Exec(_)));
            if matches!(rule.kind, RuleKind::Genrule)
                || custom_exec
                || (exec_edge && !has_license_kinds)
            {
                return Err(located(
                    rule.location.as_deref(),
                    "expected a Starlark exec transition definition, but was null".to_owned(),
                ));
            }
        }
        // The configuration of each Starlark-transition attribute, computed
        // at its first edge (an error names that edge, DependencyProducer
        // getMessageWithEdgeTransitionInfo, analysis/producers/DependencyProducer.java:258-266).
        let mut attribute_configs: BTreeMap<String, TransitionConfigurations> = BTreeMap::new();
        let exec_config = if uses_toolchains
            && edges.iter().any(|edge| edge.cfg == EdgeCfg::Exec(None))
        {
            let exec_config = self.exec_configuration(&rule, config, toolchain_context.as_ref())?;
            self.record_exec_config(&key, exec_config);
            exec_config
        } else {
            // No execution platform: the exec transition is a no-op
            // (ExecutionTransitionFactory.java:194-197).
            config
        };
        // The exec configurations of named exec groups
        // (config.exec(exec_group = NAME)): the group's execution platform.
        let mut group_configs: BTreeMap<String, ConfigId> = BTreeMap::new();
        let mut prerequisites = Vec::new();
        let mut incompatible_deps = compatibility.dependencies;
        let hinted: Vec<(Label, ConfigId)> = edges
            .iter()
            .filter_map(|edge| {
                let config = match &edge.cfg {
                    EdgeCfg::Target => config,
                    EdgeCfg::Exec(None) => exec_config,
                    _ => return None,
                };
                let label = self.resolve_rule_src(&rule, &edge.label).ok()?;
                Some((label, config))
            })
            .collect();
        self.hint_edges(&hinted);
        for edge in edges {
            let dep_configs = match &edge.cfg {
                EdgeCfg::Target => EdgeConfigurations::One(config),
                EdgeCfg::Exec(None) => EdgeConfigurations::One(exec_config),
                EdgeCfg::NoConfig => EdgeConfigurations::One(self.intern_no_config()),
                // DependencyResolutionHelpers.getExecutionPlatformLabel
                // (analysis/DependencyResolutionHelpers.java:223-253): a
                // group of the ToolchainCollection gives its execution
                // platform; an unknown group is a dependency error when the
                // rule has toolchain contexts (DependencyProducer.ExecGroupErrorEmitter);
                // without them there is no execution platform and the
                // transition is a no-op.
                EdgeCfg::Exec(Some(group)) if uses_toolchains => {
                    EdgeConfigurations::One(if let Some(group_config) = group_configs.get(group) {
                        *group_config
                    } else {
                        let Some(resolved) =
                            exec_groups.groups.iter().find(|item| item.name == *group)
                        else {
                            return Err(located(
                                rule.location.as_deref(),
                                format!(
                                    "Attr '{}' declares a transition for non-existent exec group '{group}'",
                                    native_attribute_name(&edge.attribute)
                                ),
                            ));
                        };
                        let context = resolved.context.clone();
                        let group_config =
                            self.exec_configuration(&rule, config, context.as_ref())?;
                        self.record_group_exec_config(&key, group, group_config);
                        group_configs.insert(group.clone(), group_config);
                        group_config
                    })
                }
                EdgeCfg::Exec(Some(_)) => EdgeConfigurations::One(config),
                // Applied per dependency edge (DependencyProducer), so
                // print() output repeats per edge; the result is per
                // attribute (its values do not depend on the edge).
                EdgeCfg::Starlark(attribute) => {
                    let dependency = self.resolve_rule_src(&rule, &edge.label)?;
                    let id = self.attribute_transition(&rule, config, attribute, &dependency)?;
                    attribute_configs.insert(attribute.clone(), Arc::clone(&id));
                    EdgeConfigurations::Transition(id)
                }
            };
            for dep_config in dep_configs.ids() {
                let src = &edge.label;
                let late_bound = late_bound_rule
                    || edge.attribute == ":java_plugins"
                    || analysis_dependent.contains(src)
                    || RUN_UNDER_ATTRIBUTES.contains(&edge.attribute.as_str());
                let dependency = self.resolve_rule_src(&rule, src)?;
                if self.cquery() {
                    self.capture_cquery_edge(
                        &key,
                        dependency.clone(),
                        dep_config,
                        if forwards(&rule) {
                            if late_bound_rule { ":alias" } else { "actual" }
                        } else {
                            &edge.attribute
                        },
                        matches!(edge.cfg, EdgeCfg::Exec(_)),
                        false,
                    );
                }
                // The allowlist Bazel adds to rules with a Starlark transition
                // lives in @bazel_tools, which the explicit-module profile
                // (--experimental-builtin-modules=false) does not provide.
                if edge.attribute == crate::custom_rule::FUNCTION_TRANSITION_ALLOWLIST_ATTRIBUTE
                    && dependency.repository == "bazel_tools"
                    && self.repositories().root("bazel_tools").is_err()
                {
                    return Err(located(
                        rule.location.as_deref(),
                        format!(
                            "the Starlark transition allowlist {} of {} needs the built-in @bazel_tools repository, which the explicit-module profile (--experimental-builtin-modules=false) does not provide; drop --experimental-builtin-modules=false, or declare `_allowlist_function_transition = attr.label(default = \"//tools/allowlists/function_transition_allowlist\")` with a package_group of that name",
                            crate::rule_declarations::canonical_form(&dependency),
                            crate::rule_declarations::canonical_form(&rule.label)
                        ),
                    ));
                }
                let target = self.target(&dependency).map_err(|error| {
                    let error = self.missing_edge_error(&rule, &dependency, error);
                    if analysis_dependent.contains(src) {
                        // Name the gap: the edge is a resolved late-bound default
                        // (crate::late_bound), e.g. apple.xcode_config_label's
                        // @bazel_tools//tools/cpp:host_xcodes.
                        crate::late_bound::edge_error(&rule.label, &dependency, error)
                    } else {
                        error
                    }
                })?;
                self.analyze(&dependency, dep_config)?;
                if let Some(cause) = self.target_incompatibility(&dependency, dep_config)? {
                    incompatible_deps.push((
                        ConfiguredTargetKey::new(dependency.clone(), dep_config),
                        cause,
                    ));
                }
                prerequisites.push((
                    edge.attribute.clone(),
                    dependency,
                    dep_config,
                    late_bound,
                    target,
                ));
            }
        }
        // Selected toolchain implementations are prerequisites; registration
        // candidates do not propagate incompatibility to this consumer.
        for context in std::iter::once(&toolchain_context)
            .chain(exec_groups.groups.iter().map(|group| &group.context))
            .flatten()
        {
            for implementation in context.resolved_labels() {
                self.toolchain_implementation(
                    &implementation,
                    config,
                    &context.exec_platform.label,
                    rule.location.as_deref(),
                )
                .map_err(|error| located(rule.location.as_deref(), error))?;
                self.target(&implementation)
                    .map_err(|error| self.missing_edge_error(&rule, &implementation, error))?;
                self.analyze(&implementation, config)?;
                if let Some(cause) = self.target_incompatibility(&implementation, config)? {
                    incompatible_deps.push((
                        ConfiguredTargetKey::new(implementation.clone(), config),
                        cause,
                    ));
                }
                self.capture_cquery_edge(&key, implementation, config, "$toolchains", true, false);
            }
        }
        let platform_attributes = platform_prerequisite_attributes(&rule);
        let platform_prerequisites =
            self.resolve_native_prerequisites(&rule, config, &platform_attributes)?;
        for dependency in platform_prerequisites.iter().flatten() {
            if let Some(cause) = self.target_incompatibility(dependency, config)? {
                incompatible_deps
                    .push((ConfiguredTargetKey::new(dependency.clone(), config), cause));
            }
        }
        // IncompatibleTargetChecker.java:277-311 runs on the resolved
        // prerequisite map, before RuleContext/factory validation. These are
        // the exact exemptions, independent of toolchain-resolution mode.
        if !incompatible_deps.is_empty()
            && !matches!(
                rule.kind,
                RuleKind::Native("toolchain" | "config_setting" | "label_flag")
            )
        {
            self.publish_incompatible(
                key,
                rule,
                ordered,
                Arc::new(compatibility::Incompatibility::Dependencies(
                    incompatible_deps.into_boxed_slice(),
                )),
            );
            self.active_pop();
            return Ok(());
        }
        if let Some(error) = compatibility.provider_error {
            return Err(error);
        }
        native_analysis_gate(&rule)?;
        if let Some(custom) = &rule.custom
            && !custom.declaration.unanalyzed.is_empty()
        {
            return Err(custom_gap_error(&rule, &custom.declaration.unanalyzed));
        }
        resolved_value_gate(&rule)?;
        self.visible(&rule.visibility, &rule_authority(&rule), None)?;
        for (attribute, dependency, dep_config, late_bound, target) in prerequisites {
            let is_source = matches!(target, Target::Source(_));
            let forwarding = matches!(&target, Target::Rule(dep) if forwards(dep));
            // CommonPrerequisiteValidator.validateDirectPrerequisiteVisibility
            // (analysis/CommonPrerequisiteValidator.java:181-221): an implicit
            // (private, `_x`) attribute of a Starlark rule - subrule-lifted
            // attributes included - is checked against the package of the
            // `.bzl` that defines the rule class first, then against the
            // consumer; a failure names that `.bzl` (:384-386). That `.bzl`
            // is the rule class's definition environment label
            // (getOwnerDefinitionBzl, :133-143): the module being initialized
            // when rule() ran, not a helper `.bzl` that called rule().
            let implicit_owner = rule
                .custom
                .as_ref()
                .filter(|_| attribute.starts_with('_') && !late_bound)
                .and_then(|custom| {
                    Label::parse_canonical(&custom.declaration.definition_environment_label).ok()
                });
            let (dep_visibility, dep_declaring) = match &target {
                Target::Rule(dep) | Target::Output(dep, _) => {
                    (dep.visibility.clone(), rule_authority(dep))
                }
                Target::Source(_) => (self.source_visibility(&dependency), dependency.package_id()),
            };
            let owner_visible = match &implicit_owner {
                Some(owner) => {
                    self.visible(&dep_visibility, &dep_declaring, Some(&owner.package_id()))?
                }
                None => false,
            };
            let visible_from = implicit_owner.as_ref();
            let visible = owner_visible
                || match target {
                    Target::Rule(dep) | Target::Output(dep, _) => {
                        check_macro_namespace(&dep, &dependency)?;
                        check_macro_namespace(&dep, &dep.label)?;
                        if matches!(dep.kind, RuleKind::PackageGroup(_))
                            && !matches!(rule.kind, RuleKind::Custom)
                        {
                            return Err(format!(
                                "{}: package group '{dependency}' is misplaced in {}",
                                rule.label, attribute
                            ));
                        }
                        late_bound
                            || self.visible(
                                &dep.visibility,
                                &rule_authority(&dep),
                                Some(&requesting_package),
                            )?
                    }
                    Target::Source(_) => {
                        late_bound
                            || self.visible_source(
                                &dependency,
                                &dependency.package_id(),
                                Some(&requesting_package),
                            )?
                    }
                };
            if !forwarding {
                if !visible {
                    return Err(visibility_error(
                        &rule,
                        &describe_with_aliases(&[], &dependency, "target"),
                        is_source,
                        visible_from,
                    ));
                }
                self.validate_prerequisite(
                    &rule,
                    &dependency,
                    &|| describe_with_aliases(&[], &dependency, "target"),
                    skip_testonly,
                )?;
                continue;
            }
            // The configured alias (its actual's label and kind) is needed to
            // describe and validate the edge (AliasProvider.describeTargetWithAliases).
            let (chain, actual) = self.alias_chain(dep_config, &dependency);
            // Formatted only for a reported error.
            let description = || describe_with_aliases(&chain, &actual, "target");
            if !visible {
                let is_source = matches!(self.target(&actual)?, Target::Source(_));
                return Err(visibility_error(
                    &rule,
                    &description(),
                    is_source,
                    visible_from,
                ));
            }
            self.validate_prerequisite(&rule, &actual, &description, skip_testonly)?;
        }
        // Starlark rules: prerequisite errors need the analyzed prerequisites'
        // files and providers, so they are reported before the implementation
        // runs (Self::starlark_prerequisite_errors).
        // package_metadata (no-config edges): native rules here; a Starlark
        // rule's are part of its prerequisite pass, as a built-in attribute
        // before its own attributes.
        if rule.custom.is_none() {
            let errors = self.package_metadata_errors_p1(&rule)?;
            if !errors.is_empty() {
                return Err(errors
                    .into_iter()
                    .map(|error| located(rule.location.as_deref(), error))
                    .collect::<Vec<_>>()
                    .join("\n"));
            }
        }
        // TestSuite.checkTestsAndSuites (rules/test/TestSuite.java:44-49,
        // 104-118) after RuleContext's prerequisite validation.
        let errors = self.test_suite_errors(&rule, config)?;
        if !errors.is_empty() {
            return Err(crate::rule_errors::located_events(
                rule.location.as_deref(),
                errors,
            ));
        }
        if !attribute_configs.is_empty() {
            let splits = self.split_attribute_dependencies(&rule, &attribute_configs)?;
            if rule.custom.is_some() {
                let custom = Arc::make_mut(&mut rule).custom.as_mut().unwrap();
                custom.split_dependencies = Arc::new(splits);
            }
            self.record_edge_configs(&key, attribute_configs);
        }
        // config_setting flag_values/constraint_values and the platform rules'
        // label attributes: prerequisites analyzed, then validated
        // (RuleContext.Builder, analysis/RuleContext.java:1700-1745, 1845-1880, 2165-2172).
        let (flag_prerequisites, constraint_prerequisites) =
            self.config_setting_prerequisites(&rule, config)?;
        self.validate_native_prerequisites(
            &rule,
            config,
            &platform_attributes,
            &platform_prerequisites,
        )?;
        // RuleContext.Builder.validateExtraPrerequisites first checks resolved
        // label lists for duplicates (analysis/RuleContext.java:2110-2113,
        // 1553-1568), attribute errors at the rule's location.
        if rule.custom.is_some() {
            self.record_deferred_duplicates(&key, duplicate_errors.clone());
        } else if !duplicate_errors.is_empty() {
            return Err(duplicate_errors
                .iter()
                .map(|error| located(rule.location.as_deref(), error.clone()))
                .collect::<Vec<_>>()
                .join("\n"));
        }
        // RuleContext.Builder.validateExtraPrerequisites validates the config
        // conditions like direct prerequisites, after dependency analysis
        // (analysis/RuleContext.java:2104-2152). Under the DEFAULT_PUBLIC
        // policy the validated prerequisite is the END of an alias chain
        // (`condition.fromConfiguredTargetNoCheck(getActual())`, :2148-2150;
        // AliasConfiguredTarget.getActual, rules/AliasConfiguredTarget.java:229-233):
        // the alias's own visibility is skipped and the message names the
        // config_setting without an alias description.
        for (condition, _) in &ordered {
            let dependency = self.resolve_rule_src(&rule, condition)?;
            let (_, actual) = self.alias_chain(config, &dependency);
            if let Target::Rule(dep) = self.target(&actual)? {
                // Formatted only for a reported error.
                let description = || describe_with_aliases(&[], &actual, "target");
                if !self.visible(
                    &dep.visibility,
                    &rule_authority(&dep),
                    Some(&requesting_package),
                )? {
                    return Err(visibility_error(&rule, &description(), false, None));
                }
                self.validate_prerequisite(&rule, &actual, &description, skip_testonly)?;
            }
        }
        // ConfigSetting.create (rules/config/ConfigSetting.java:111-166).
        let mut matching = match (&rule.kind, &rule.native) {
            (RuleKind::Native("config_setting"), Some(native)) => {
                let repositories = self.repositories().clone();
                let context = rule.label.clone();
                let mut warnings = Vec::new();
                let mut run = |configuration: &Configuration, platform: Option<&PlatformInfo>| {
                    crate::config_setting::analyze(
                        &rule.label,
                        native,
                        configuration,
                        &flag_prerequisites,
                        &constraint_prerequisites,
                        platform,
                        &|text| repositories.parse(text, &context.repository, &context.package),
                        &mut warnings,
                    )
                };
                let mut result = run(&configuration, target_platform.as_deref());
                if result.as_ref().err().map(String::as_str)
                    == Some(crate::config_setting::NEEDS_TARGET_PLATFORM)
                {
                    let platform = self.target_platform(config)?;
                    result = run(&configuration, Some(&platform));
                }
                // Rule warnings precede the rule's errors; an identical
                // warning (another configuration, the retry above) is
                // reported once (EmittedEventState.addWarning).
                for warning in warnings {
                    let warning = located(rule.location.as_deref(), warning);
                    self.warn_once(warning);
                }
                Some(result.map_err(|error| located_lines(rule.location.as_deref(), &error))?)
            }
            _ => None,
        };
        // ConstraintSetting/ConstraintValue/Platform.create (rules/platform/*.java).
        let (mut constraint_setting, mut constraint_value, mut platform) =
            self.platform_rule_providers(&rule, config, &platform_prerequisites)?;
        // ToolchainType.create / Toolchain.create (rules/ToolchainType.java:40-52,
        // rules/platform/Toolchain.java:42-110).
        let (mut toolchain_type, mut declared_toolchain) =
            self.toolchain_rule_providers(&rule, config, &platform_prerequisites)?;
        if let Some(declared) = &declared_toolchain {
            self.prefetch_compatible_toolchain(declared);
        }
        // ConfigFeatureFlag.create (rules/config/ConfigFeatureFlag.java:109-169).
        let mut feature_flag = self.config_feature_flag_provider(&rule)?;
        // AliasConfiguredTarget forwards the actual's providers; label_flag and
        // label_setting override BuildSettingProvider (LateBoundAlias.java:58-79).
        let mut actual = None;
        let mut incompatible = None;
        let mut build_setting = build_setting;
        if forwards(&rule) {
            let target = self.resolve_rule_src(&rule, &rule.srcs[0])?;
            let forwarded = self
                .configured_node(&ConfiguredTargetKey::new(target.clone(), config))
                .unwrap_or_default();
            matching = forwarded.matching;
            incompatible = forwarded.incompatible;
            if build_setting.is_none() {
                build_setting = forwarded.build_setting;
            }
            constraint_setting = forwarded.constraint_setting;
            constraint_value = forwarded.constraint_value;
            platform = forwarded.platform;
            toolchain_type = forwarded.toolchain_type;
            declared_toolchain = forwarded.declared_toolchain;
            feature_flag = forwarded.feature_flag;
            actual = Some(target);
        }
        // ctx.build_setting_value of a Starlark build setting.
        if let Some(info) = &build_setting
            && rule.custom.is_some()
        {
            let custom = Arc::make_mut(&mut rule).custom.as_mut().unwrap();
            custom.setting_value = Some(info.value(&configuration));
        }
        self.active_pop();
        if let Some(context) = toolchain_context {
            self.record_toolchain_context(&key, context);
        }
        if uses_toolchains {
            self.record_exec_groups(&key, exec_groups);
        }
        let incompatible_now = incompatible.is_some();
        self.publish_node(
            &key,
            ConfiguredNode {
                incompatible,
                matching,
                build_setting,
                actual,
                constraint_setting,
                constraint_value,
                platform,
                toolchain_type,
                declared_toolchain,
                feature_flag,
            },
        );
        if rule.custom.is_some() {
            self.record_configured_rule(&key, &rule);
        }
        if let Err(error) = self.analyze_aspect_edges(&rule, &key) {
            // The target failed (--keep_going reaches it again: it is
            // analyzed again, never a memo hit of a node never planned).
            self.unpublish_node(&key);
            return Err(error);
        }
        if self.cquery() {
            self.cquery_rule(&key, &rule, incompatible_now.then_some(ordered), false);
        }
        let plan_len = self.plan_len();
        self.track_position(tracked, plan_len);
        self.plan_push(key, rule);
        Ok(())
    }

    /// The aspects attached to a rule's attribute edges need their own
    /// dependency analysis (N58): every aspect application reachable from
    /// the rule's attributes — the aspects' own attribute dependencies and
    /// the applications propagated along each dependency's `attr_aspects`
    /// edges — is analyzed here, before the rule's planning
    /// (AspectFunction's AspectValue dependencies,
    /// skyframe/AspectFunction.java:682-703).
    fn analyze_aspect_edges(
        &mut self,
        rule: &Rule,
        key: &ConfiguredTargetKey,
    ) -> Result<(), String> {
        if !rule.custom.as_ref().is_some_and(|custom| {
            custom
                .schema
                .values()
                .any(|spec| !spec.declaration.aspects.is_empty())
        }) {
            return Ok(());
        }
        let mut seen: BTreeSet<(ConfiguredTargetKey, Vec<String>)> = BTreeSet::new();
        for edge in self.rule_edges(rule, key.config) {
            let Some(spec) = rule
                .custom
                .as_ref()
                .and_then(|custom| custom.schema.get(&edge.attribute))
            else {
                continue;
            };
            if spec.declaration.aspects.is_empty() {
                continue;
            }
            let aspects = self.attribute_aspects(rule, &spec.declaration.aspects)?;
            let edge_configs = self.analyzed_edge_configurations(key, &edge.cfg)?;
            let dependency = self.resolve_rule_src(rule, &edge.label)?;
            for edge_config in edge_configs.ids() {
                let applications = self.analyze_aspect_applications(
                    &dependency,
                    edge_config,
                    aspects.clone(),
                    &mut seen,
                )?;
                if self.cquery() {
                    self.cquery_request(key, applications);
                }
            }
        }
        Ok(())
    }

    /// Analyze every aspect application of `aspects` on `label`'s configured
    /// target (and, recursively, the applications they propagate along the
    /// target's `attr_aspects` edges): the aspect collection of the edge,
    /// each applied aspect's own attribute dependencies, then the
    /// propagated applications (DependencyResolver over the aspect path).
    #[allow(clippy::too_many_arguments)]
    fn analyze_aspect_applications(
        &mut self,
        label: &Label,
        config: ConfigId,
        aspects: Vec<std::sync::Arc<crate::aspects::AspectDefinition>>,
        seen: &mut BTreeSet<(ConfiguredTargetKey, Vec<String>)>,
    ) -> Result<Vec<u32>, String> {
        let key = ConfiguredTargetKey::new(label.clone(), config);
        if self.target_incompatibility(label, config)?.is_some() {
            return Ok(Vec::new());
        }
        // The application's configuration is the base configured target's,
        // after a rule transition's delegation (AspectKeyCreator over the
        // ConfiguredTargetKey; as Self::plan_aspect_collection).
        let config = self
            .rule_delegation(&key)
            .map_or(config, |owner| owner.config);
        // Aspects never apply in the no-config configuration
        // (AspectFunction.java:296-305: NonApplicableAspect).
        if self.configuration(config).native.no_config {
            return Ok(Vec::new());
        }
        let key = self.rule_delegation(&key).unwrap_or(key);
        // Aspects do not apply to source files or package groups
        // (AspectFunction.java:327-334); an output file applies its
        // generating rule when the aspect asks for it
        // (apply_to_generating_rules, AspectDefinition.java).
        let mut base_label = key.label.clone();
        let mut target = self.target(&base_label)?;
        if let Target::Output(generating, _) = &target {
            let applies = aspects.iter().any(|aspect| {
                aspect.declaration.apply_to_generating_rules
                    || aspect.identity == crate::aspects::VALIDATION_ASPECT_IDENTITY
            });
            if !applies {
                return Ok(Vec::new());
            }
            base_label = generating.label.clone();
            target = self.target(&base_label)?;
        }
        let Target::Rule(base_rule) = target else {
            return Ok(Vec::new());
        };
        if matches!(base_rule.kind, RuleKind::PackageGroup(_)) {
            return Ok(Vec::new());
        }
        // Alias edges resolve to the actual target's aspect application
        // (createAspectsKeys, ToplevelStarlarkAspectFunction.java:140-157).
        while let Some(actual) = self
            .configured_node(&ConfiguredTargetKey::new(base_label.clone(), config))
            .and_then(|node| node.actual.clone())
        {
            base_label = actual;
        }
        let base_key = ConfiguredTargetKey::new(base_label.clone(), config);
        let base_rule = match self.target(&base_label)? {
            // Loading declarations still contain select() placeholders. Using
            // them here omits selected deps, whereas phase 2 later walks the
            // configured rule and demands aspect applications never analyzed.
            // Publish/read the shared resolved declaration in phase 1, not as
            // a side effect of whichever targets plan_pending committed first.
            Target::Rule(rule) => self.configured_rule(&base_key).unwrap_or(rule),
            _ => return Ok(Vec::new()),
        };
        if no_config_rule_class(&base_rule) {
            return Ok(Vec::new());
        }
        let collection = self.aspect_collection_for(aspects, &base_rule)?;
        if collection.is_empty() {
            return Ok(Vec::new());
        }
        // Function `attr_aspects` evaluated over this configured target
        // (computeAttributeAspects, DependencyMapProducer.java:360-380).
        let collection = self.with_computed_attr_aspects(collection, &base_key, &base_rule)?;
        let applications = if self.cquery() {
            self.cquery_applications(&base_key, &collection, &base_rule)
        } else {
            Vec::new()
        };
        let memo = (base_key.clone(), collection.descriptors());
        if !seen.insert(memo) {
            return Ok(applications);
        }
        let incoming: Vec<std::sync::Arc<crate::aspects::AspectDefinition>> = collection
            .used
            .iter()
            .map(|(aspect, _)| aspect.clone())
            .collect();
        for (index, (aspect, _)) in collection.used.iter().enumerate() {
            // A NonApplicableAspect (AspectFunction.java:332-339) resolves
            // no dependencies of its own.
            if aspect.identity == crate::aspects::VALIDATION_ASPECT_IDENTITY
                || !aspect.applies_to_rule(&base_rule)
            {
                continue;
            }
            // The aspect's own attribute dependencies, each in its
            // attribute's configuration (Self::aspect_attribute_configurations),
            // validated from the aspect's `.bzl` package
            // (CommonPrerequisiteValidator, Self::analyze_aspect_attribute_edges).
            // The aspect's own toolchain contexts first: they decide its
            // execution platforms (DependencyContextProducer runs before
            // the dependencies); a ToolchainException is the application's
            // error as is (AspectFunction.java:510-515).
            let toolchains = self.aspect_toolchain_contexts(&base_rule, config, aspect)?;
            if let Some(&application) = applications.get(index) {
                // Record per application, including memoized toolchain contexts.
                // The resolution memo is not an edge owner.
                self.cquery_aspect_targets(application, &toolchains.cquery_dependencies);
                for context in std::iter::once(toolchains.default.as_ref())
                    .chain(
                        toolchains
                            .groups
                            .groups
                            .iter()
                            .map(|group| group.context.as_ref()),
                    )
                    .flatten()
                {
                    for label in context.resolved_labels() {
                        self.cquery_aspect_target(application, label, config);
                    }
                }
            }
            let attribute_configs = self.aspect_attribute_configurations(
                &base_rule,
                &base_key,
                aspect,
                aspect,
                &toolchains,
            )?;
            let path = collection.path_aspects(&aspect.descriptor);
            self.analyze_aspect_attribute_edges(
                &base_rule,
                &base_label,
                aspect,
                aspect,
                None,
                &attribute_configs,
                &path,
                applications.get(index).copied(),
                seen,
            )?;
            // The attributes of the base aspects of the path that
            // `ctx.rule.attr` holds are dependencies of this application
            // too (DependencyResolutionHelpers.getAttributes/
            // addAspectAttributes), each in its attribute's configuration
            // over THIS aspect's execution platforms.
            for (owner, names) in crate::aspects::base_aspect_attributes(&path, &base_rule) {
                let owner_configs = self.aspect_attribute_configurations(
                    &base_rule,
                    &base_key,
                    &owner,
                    aspect,
                    &toolchains,
                )?;
                self.analyze_aspect_attribute_edges(
                    &base_rule,
                    &base_label,
                    &owner,
                    aspect,
                    Some(&names),
                    &owner_configs,
                    &path,
                    None,
                    seen,
                )?;
            }
        }
        // The applications propagated along the base target's edges: each
        // aspect of the collection that propagates along the attribute name
        // applies to the edge's dependency (computePropagatingAspects), in
        // the edge's configuration exactly as the application's planning
        // reaches it (Self::plan_aspect_application). A native base rule's
        // attribute edges its own analysis does not resolve are analyzed
        // first (Self::native_aspect_edges).
        let native_edges = self.native_aspect_edges(&base_rule);
        for edge in &native_edges {
            let edge_configs = self.analyzed_edge_configurations(&base_key, &edge.cfg)?;
            let dependency = self.resolve_rule_src(&base_rule, &edge.label)?;
            for edge_config in edge_configs.ids() {
                self.analyze(&dependency, edge_config)?;
            }
        }
        let mut edges = self.rule_edges(&base_rule, base_key.config);
        edges.extend(native_edges);
        for edge in edges {
            let attribute_aspects = base_rule
                .custom
                .as_ref()
                .and_then(|custom| custom.schema.get(&edge.attribute))
                .map(|spec| spec.declaration.aspects.clone())
                .unwrap_or_default();
            let attribute_aspects = self.loaded_attribute_aspects(&base_rule, &attribute_aspects);
            let propagating =
                crate::aspects::propagating_aspects(&attribute_aspects, &incoming, &edge.attribute);
            if propagating.is_empty() {
                continue;
            }
            if edge.cfg == EdgeCfg::NoConfig {
                continue;
            }
            let edge_configs = self.analyzed_edge_configurations(&base_key, &edge.cfg)?;
            let dependency = self.resolve_rule_src(&base_rule, &edge.label)?;
            for edge_config in edge_configs.ids() {
                let children = self.analyze_aspect_applications(
                    &dependency,
                    edge_config,
                    propagating.clone(),
                    seen,
                )?;
                if self.cquery() {
                    self.cquery_propagated(
                        &applications,
                        &collection,
                        &children,
                        &edge.attribute,
                        &attribute_aspects,
                    );
                }
            }
        }
        // The applications propagated to the base target's resolved
        // toolchains (`toolchains_aspects`, BaseTargetToolchainDependencyKind;
        // AspectResolutionHelpers.computePropagatingAspectsToToolchainDep),
        // in the base configuration like the toolchain dependencies.
        for (implementation, type_labels) in self.base_target_toolchains(&base_key) {
            let propagating = crate::aspects::propagating_to_toolchain(&incoming, &type_labels);
            if propagating.is_empty() {
                continue;
            }
            self.analyze_aspect_applications(&implementation, config, propagating, seen)?;
        }
        Ok(applications)
    }

    /// The base target's resolved toolchains an aspect may propagate to
    /// (AspectFunction.getBaseTargetToolchainContexts over the default
    /// context and the exec groups' contexts): each resolved
    /// implementation once per type, with the type label and its requested
    /// aliases.
    fn base_target_toolchains(&mut self, key: &ConfiguredTargetKey) -> Vec<(Label, Vec<Label>)> {
        let mut result: Vec<(Label, Vec<Label>)> = Vec::new();
        let default = self.toolchain_context_of(key);
        let groups = self.exec_groups_of(key);
        let contexts = default.iter().chain(groups.iter().flat_map(|groups| {
            groups
                .groups
                .iter()
                .filter_map(|group| group.context.as_ref())
        }));
        for context in contexts {
            for (info, implementation) in &context.resolved {
                let mut labels = vec![info.type_label.clone()];
                for (requested, requested_info) in &context.requested {
                    if requested_info.type_label == info.type_label && !labels.contains(requested) {
                        labels.push(requested.clone());
                    }
                }
                if !result
                    .iter()
                    .any(|(seen, seen_labels)| seen == implementation && *seen_labels == labels)
                {
                    result.push((implementation.clone(), labels));
                }
            }
        }
        result
    }

    /// Phase 1 of the label attributes of `owner` (the applied aspect
    /// `main` itself, or a base aspect of its path whose attributes
    /// `ctx.rule.attr` holds, restricted to `names`) in one application of
    /// `main` to `base_rule`: each dependency analyzed in its attribute's
    /// configuration, with the aspects of the path that propagate over the
    /// attribute (collectPropagatingAspects: never the owner itself), and
    /// visibility-checked from the owner's `.bzl` package
    /// (CommonPrerequisiteValidator). `capture` is the cquery application
    /// of `main` recording the owner's edges (its own attributes only).
    #[allow(clippy::too_many_arguments)]
    fn analyze_aspect_attribute_edges(
        &mut self,
        base_rule: &Rule,
        base_label: &Label,
        owner: &crate::aspects::AspectDefinition,
        main: &crate::aspects::AspectDefinition,
        names: Option<&[String]>,
        attribute_configs: &BTreeMap<String, TransitionConfigurations>,
        path: &[std::sync::Arc<crate::aspects::AspectDefinition>],
        capture: Option<u32>,
        seen: &mut BTreeSet<(ConfiguredTargetKey, Vec<String>)>,
    ) -> Result<(), String> {
        let authority = crate::label::PackageId {
            repository: owner.definition_label.repository.clone(),
            package: owner.definition_label.package.clone(),
        };
        let failure = |error: String| {
            format!(
                "in {} aspect on {} rule {}: {error}",
                main.identity,
                base_rule.rule_class_name(),
                crate::rule_declarations::canonical_form(base_label)
            )
        };
        for (name, spec) in &owner.declaration.attrs {
            if names.is_some_and(|names| !names.contains(name)) {
                continue;
            }
            let Some(branches) = attribute_configs.get(name) else {
                continue;
            };
            let propagating = crate::aspects::propagating_over_aspect_attribute(path, owner, name);
            for text in aspect_attribute_labels(spec) {
                // Aspect attribute labels resolve relative to the
                // aspect's `.bzl` package (LabelConverter.
                // forBzlEvaluatingThread at aspect() declaration).
                let dependency = self.repositories().parse(
                    &text,
                    &owner.definition_label.repository,
                    &owner.definition_label.package,
                )?;
                for (_, attribute_config) in branches.iter() {
                    let attribute_config = *attribute_config;
                    self.analyze(&dependency, attribute_config)
                        .map_err(failure)?;
                    if let Some(index) = capture {
                        self.cquery_aspect_target(index, dependency.clone(), attribute_config);
                    }
                    if !propagating.is_empty() {
                        self.analyze_aspect_applications(
                            &dependency,
                            attribute_config,
                            propagating.clone(),
                            seen,
                        )?;
                    }
                    let dep_rule = match self.target(&dependency)? {
                        Target::Rule(dep_rule) => dep_rule,
                        Target::Output(generating, _) => generating,
                        _ => continue,
                    };
                    if let Err(error) = self.visible(&dep_rule.visibility, &authority, None) {
                        return Err(failure(error));
                    }
                }
            }
        }
        Ok(())
    }

    /// [`crate::aspects::aspect_collection`] with the aspects'
    /// `propagation_predicate`s evaluated over `rule`
    /// (evaluatePropagationPredicate), once per (label, descriptor).
    fn aspect_collection_for(
        &mut self,
        aspects: Vec<std::sync::Arc<crate::aspects::AspectDefinition>>,
        rule: &Rule,
    ) -> Result<crate::aspects::AspectCollection, String> {
        crate::aspects::aspect_collection(aspects, rule, &mut |aspect| {
            let memo = (rule.label.clone(), aspect.descriptor.clone());
            if let Some(result) = self.aspect_predicate(&memo) {
                return Ok(result);
            }
            match crate::aspect_propagation::propagation_predicate(aspect, rule) {
                Ok(result) => {
                    self.set_aspect_predicate(memo, result);
                    Ok(result)
                }
                Err(error) => {
                    self.memo_failed();
                    Err(error)
                }
            }
        })
    }

    /// `collection` with each function `attr_aspects` evaluated over the
    /// configured target `key` (`rule`, select-resolved): the aspect
    /// definitions carry the list they propagate along from this target
    /// (AspectResolutionHelpers.computeAttributeAspects); memoized per
    /// (configured target, descriptor). An evaluation error fails at the
    /// target (DependencyMapProducer.step: `e.getMessageWithStack()` at
    /// the target's location).
    fn with_computed_attr_aspects(
        &mut self,
        mut collection: crate::aspects::AspectCollection,
        key: &ConfiguredTargetKey,
        rule: &Rule,
    ) -> Result<crate::aspects::AspectCollection, String> {
        for (aspect, _) in &mut collection.used {
            if aspect.declaration.attr_aspects.is_some()
                && aspect.declaration.toolchains_aspects.is_some()
            {
                continue;
            }
            let memo = (key.clone(), aspect.descriptor.clone());
            let (attr_aspects, toolchains_aspects) = match self.aspect_attr_aspects(&memo) {
                Some(computed) => computed,
                None => {
                    let computed = (|| -> Result<_, String> {
                        let attr_aspects =
                            crate::aspect_propagation::computed_attr_aspects(aspect, rule)
                                .map_err(|error| located(rule.location.as_deref(), error))?
                                .map(Arc::from);
                        let toolchains_aspects =
                            crate::aspect_propagation::computed_toolchains_aspects(aspect, rule)
                                .map_err(|error| located(rule.location.as_deref(), error))?
                                .map(Arc::from);
                        Ok((attr_aspects, toolchains_aspects))
                    })();
                    let computed = match computed {
                        Ok(computed) => computed,
                        Err(error) => {
                            self.memo_failed();
                            return Err(error);
                        }
                    };
                    self.set_aspect_attr_aspects(memo, computed.clone());
                    computed
                }
            };
            let mut definition = aspect.as_ref().clone();
            definition.computed_attr_aspects = attr_aspects;
            definition.computed_toolchains_aspects = toolchains_aspects;
            *aspect = std::sync::Arc::new(definition);
        }
        Ok(collection)
    }

    /// The aspect's OWN ToolchainCollection in configuration `config`
    /// (AspectFunction.getUnloadedToolchainContextsInputs,
    /// AspectFunction.java:811-838, with DependencyContextProducer and
    /// UnloadedToolchainContextsProducer): the default exec group's context
    /// over the aspect's `exec_compatible_with` and — without automatic exec
    /// groups — its `toolchains` (ToolchainContextUtil.
    /// createDefaultToolchainContextKey; no forced execution platform), then
    /// DeclaredExecGroup.process's groups: the aspect's `exec_groups`, and
    /// under automatic exec groups (an `_use_auto_exec_groups` attribute,
    /// else `--incompatible_auto_exec_groups`; shouldUseAutoExecGroups,
    /// :840-849) one group per toolchain type. Never the base rule's
    /// contexts. The resolved toolchains are dependencies of the
    /// application, analyzed in the base configuration
    /// (DependencyKind.forExecGroup; DependencyProducer.java:160-175).
    /// Memoized per (configuration, aspect): resolution depends on nothing
    /// else.
    fn aspect_toolchain_contexts(
        &mut self,
        base_rule: &Rule,
        config: ConfigId,
        aspect: &crate::aspects::AspectDefinition,
    ) -> Result<Arc<AspectToolchains>, String> {
        let _shared = incremental::SharedScope::enter("aspect_toolchain_contexts");
        let memo = (config, aspect.identity.clone());
        if let Some(found) = self.aspect_toolchains(&memo) {
            return Ok(found);
        }
        match self.compute_aspect_toolchains(base_rule, config, aspect) {
            Ok(toolchains) => {
                let toolchains = Arc::new(toolchains);
                self.set_aspect_toolchains(memo, toolchains.clone());
                Ok(toolchains)
            }
            Err(error) => {
                self.memo_failed();
                Err(error)
            }
        }
    }

    /// [`Self::aspect_toolchain_contexts`] computed.
    fn compute_aspect_toolchains(
        &mut self,
        base_rule: &Rule,
        config: ConfigId,
        aspect: &crate::aspects::AspectDefinition,
    ) -> Result<AspectToolchains, String> {
        let declaration = &aspect.declaration;
        let use_auto = match declaration
            .attrs
            .iter()
            .find(|(name, _)| name == "_use_auto_exec_groups")
            .map(|(_, spec)| &spec.default)
        {
            None => {
                self.configuration(config)
                    .native
                    .incompatible_auto_exec_groups
            }
            Some(crate::custom_rule::AttrValue::Bool(value)) => *value,
            Some(_) => {
                return Err(format!(
                    "aspect {}: a non-boolean '_use_auto_exec_groups' attribute: Bazel 9.3 reads it as a boolean (AspectFunction.shouldUseAutoExecGroups) and fails; not supported by the native profile",
                    aspect.identity
                ));
            }
        };
        let types = requirements(&declaration.toolchains)?;
        let mut exec_constraints: Vec<Label> = Vec::new();
        for text in &declaration.exec_compatible_with {
            let label = Label::parse_canonical(text)?;
            if !exec_constraints.contains(&label) {
                exec_constraints.push(label);
            }
        }
        let mut toolchains = AspectToolchains {
            default: None,
            cquery_dependencies: Vec::new(),
            groups: crate::exec_groups::TargetExecGroups {
                use_auto_exec_groups: use_auto,
                groups: Vec::new(),
            },
        };
        // A platform's tree and the no-config configuration resolve no
        // toolchains (as a rule there, Self::analyze_key).
        let in_empty_configuration =
            self.platform_tree() || self.configuration(config).native.no_config;
        if !in_empty_configuration {
            let mut processed = Vec::new();
            for group in &declaration.exec_groups {
                let mut constraints = Vec::new();
                for text in &group.exec_compatible_with {
                    constraints.push(Label::parse_canonical(text)?);
                }
                processed.push((
                    group.name.clone(),
                    ToolchainContextKey {
                        types: requirements(&group.toolchains)?,
                        exec_constraints: constraints,
                        forced_exec: None,
                    },
                ));
            }
            if use_auto {
                for requirement in &types {
                    processed.push((
                        requirement.label.to_string(),
                        ToolchainContextKey {
                            types: vec![requirement.clone()],
                            exec_constraints: exec_constraints.clone(),
                            forced_exec: None,
                        },
                    ));
                }
            }
            let default_key = ToolchainContextKey {
                types: if use_auto { Vec::new() } else { types.clone() },
                exec_constraints: exec_constraints.clone(),
                forced_exec: None,
            };
            self.set_resolving_aspect_toolchains(true);
            let resolved = (|| -> Result<(), String> {
                let cquery_key = self.cquery().then(|| default_key.clone());
                toolchains.default = self.resolve_toolchains(base_rule, config, default_key)?;
                if let Some(key) = cquery_key {
                    toolchains.cquery_dependencies.extend(
                        self.capture_cquery_aspect_toolchain_context(
                            config,
                            &aspect.descriptor,
                            &key,
                        ),
                    );
                }
                for (name, key) in processed {
                    let group_types = key
                        .types
                        .iter()
                        .map(|requirement| requirement.label.clone())
                        .collect();
                    let cquery_key = self.cquery().then(|| key.clone());
                    let context = self.resolve_toolchains(base_rule, config, key)?;
                    if let Some(key) = cquery_key {
                        toolchains.cquery_dependencies.extend(
                            self.capture_cquery_aspect_toolchain_context(
                                config,
                                &aspect.descriptor,
                                &key,
                            ),
                        );
                    }
                    toolchains
                        .groups
                        .groups
                        .push(crate::exec_groups::ResolvedGroup {
                            name,
                            types: group_types,
                            context,
                            builtin_only: false,
                        });
                }
                Ok(())
            })();
            self.set_resolving_aspect_toolchains(false);
            resolved?;
            // The resolved toolchain implementations, in ToolchainCollection
            // order (default first), each once.
            let implementations = std::iter::once(toolchains.default.as_ref())
                .chain(
                    toolchains
                        .groups
                        .groups
                        .iter()
                        .map(|group| group.context.as_ref()),
                )
                .flatten()
                .flat_map(|context| {
                    context
                        .resolved_labels()
                        .into_iter()
                        .map(|label| (label, context.exec_platform.label.clone()))
                })
                .collect::<Vec<_>>();
            for (implementation, exec_platform) in implementations {
                self.toolchain_implementation(
                    &implementation,
                    config,
                    &exec_platform,
                    base_rule.location.as_deref(),
                )
                .map_err(|error| located(base_rule.location.as_deref(), error))?;
                self.target(&implementation)
                    .map_err(|error| self.missing_edge_error(base_rule, &implementation, error))?;
                self.analyze(&implementation, config)?;
            }
        }
        Ok(toolchains)
    }

    /// The configuration of each label attribute of `aspect` applied to the
    /// configured target `base_key` (AspectFunction's dependency
    /// resolution: DependencyProducer over the main aspect's attributes,
    /// analysis/producers/DependencyProducer.java:184-219, with
    /// DependencyResolutionHelpers.getExecutionPlatformLabel,
    /// analysis/DependencyResolutionHelpers.java:195-253): the base
    /// target's configuration for `cfg = "target"`; for `cfg = "exec"` or
    /// `config.exec()` (ExecutionTransitionFactory) the exec transition for
    /// the execution platform of the ASPECT's own default exec group (its
    /// toolchain context, AspectFunction.getUnloadedToolchainContextsInputs,
    /// AspectFunction.java:811-838 — the host platform natively, the only
    /// execution platform; docs/TOOLCHAINS.md), never the base rule's; the
    /// attribute's Starlark transition applied to the base configuration
    /// over the base target's attributes (AttributeTransitionData.attributes,
    /// the base rule's attribute map). A named exec group the aspect does
    /// not declare is DependencyProducer.ExecGroupErrorEmitter's error at
    /// the base target. Memoized per application
    /// (Self::aspect_attribute_configs), read by its planning.
    #[allow(clippy::too_many_arguments)]
    fn aspect_attribute_configurations(
        &mut self,
        base_rule: &Rule,
        base_key: &ConfiguredTargetKey,
        aspect: &crate::aspects::AspectDefinition,
        main: &crate::aspects::AspectDefinition,
        toolchains: &AspectToolchains,
    ) -> Result<Arc<BTreeMap<String, TransitionConfigurations>>, String> {
        let memo = (base_key.clone(), aspect_attribute_memo_name(aspect, main));
        if let Some(configs) = self.aspect_attribute_configs(&memo) {
            return Ok(configs);
        }
        match self.compute_aspect_attribute_configurations(base_rule, base_key, aspect, toolchains)
        {
            Ok(configs) => {
                let configs = Arc::new(configs);
                self.set_aspect_attribute_configs(memo, configs.clone());
                Ok(configs)
            }
            Err(error) => {
                self.memo_failed();
                Err(error)
            }
        }
    }

    /// [`Self::aspect_attribute_configurations`] computed.
    fn compute_aspect_attribute_configurations(
        &mut self,
        base_rule: &Rule,
        base_key: &ConfiguredTargetKey,
        aspect: &crate::aspects::AspectDefinition,
        toolchains: &AspectToolchains,
    ) -> Result<BTreeMap<String, TransitionConfigurations>, String> {
        use crate::rule_declarations::Cfg;
        let config = base_key.config;
        let mut exec_config = None;
        let mut group_configs: BTreeMap<String, ConfigId> = BTreeMap::new();
        let mut configs = BTreeMap::new();
        for (name, spec) in &aspect.declaration.attrs {
            if aspect_attribute_labels(spec).is_empty() {
                continue;
            }
            // Attribute.getName: the native name (`$x` for `_x`).
            let attribute =
                crate::custom_rule::native_attribute_name(name, spec.declaration.late_bound);
            let edge_config = match &spec.declaration.cfg {
                Cfg::Target => config,
                Cfg::Exec(group)
                    if group.as_deref().is_none_or(|group| {
                        group == crate::exec_groups::DEFAULT_EXEC_GROUP_NAME
                    }) =>
                {
                    match exec_config {
                        Some(exec) => exec,
                        None => {
                            let exec = self.exec_configuration(
                                base_rule,
                                config,
                                toolchains.default.as_ref(),
                            )?;
                            exec_config = Some(exec);
                            exec
                        }
                    }
                }
                // An exec group of the aspect's ToolchainCollection gives its
                // execution platform.
                Cfg::Exec(Some(group)) => match group_configs.get(group) {
                    Some(&exec) => exec,
                    None => {
                        let Some(resolved) = toolchains
                            .groups
                            .groups
                            .iter()
                            .find(|resolved| resolved.name == *group)
                        else {
                            return Err(located(
                                base_rule.location.as_deref(),
                                format!(
                                    "Attr '{attribute}' declares a transition for non-existent exec group '{group}'"
                                ),
                            ));
                        };
                        let context = resolved.context.clone();
                        let exec = self.exec_configuration(base_rule, config, context.as_ref())?;
                        group_configs.insert(group.clone(), exec);
                        exec
                    }
                },
                Cfg::Starlark {
                    analysis_test: false,
                    ..
                } => {
                    let transition = aspect.attribute_transition(name).ok_or_else(|| {
                        format!(
                            "aspect {}: attribute '{name}' declares a Starlark transition that the native profile did not retain; {}",
                            aspect.identity,
                            crate::rule_declarations::NOT_ANALYZED
                        )
                    })?;
                    let def = crate::starlark_transitions::TransitionDef::of(&transition)?;
                    let attributes =
                        configured_attributes::transition_attributes(base_rule, None, &[]);
                    let dependency = aspect_attribute_labels(spec)
                        .first()
                        .and_then(|text| {
                            self.repositories()
                                .parse(
                                    text,
                                    &aspect.definition_label.repository,
                                    &aspect.definition_label.package,
                                )
                                .ok()
                        })
                        .map(|label| crate::rule_declarations::canonical_form(&label))
                        .unwrap_or_default();
                    let branches = self
                        .apply_transition(
                            &def,
                            crate::starlark_transitions::Kind::Attribute,
                            &base_rule.label,
                            config,
                            &attributes,
                        )
                        .map_err(|failure| {
                            let mut lines = failure.events;
                            lines.push(located(
                                base_rule.location.as_deref(),
                                format!(
                                    "on dependency edge {} ({}) -|{attribute}|-> {dependency}: {}",
                                    crate::rule_declarations::canonical_form(&base_rule.label),
                                    &config.to_string()[..7],
                                    failure.message
                                ),
                            ));
                            lines.join("\n")
                        })?;
                    configs.insert(name.clone(), branches);
                    continue;
                }
                other => {
                    return Err(format!(
                        "aspect {}: attribute '{name}' cfg={} (configuration transition): {}",
                        aspect.identity,
                        other.describe(),
                        crate::rule_declarations::NOT_ANALYZED
                    ));
                }
            };
            configs.insert(name.clone(), Arc::from([(String::new(), edge_config)]));
        }
        Ok(configs)
    }

    /// The configuration of `rule`'s exec-transition edges (its default exec
    /// group): the exec transition (crate::config_registry::exec_transition)
    /// for the execution platform chosen by its toolchain resolution — the
    /// host platform, the only execution platform (docs/TOOLCHAINS.md),
    /// resolved here when toolchain resolution needed no platform (the lazy
    /// default host platform, docs/PLATFORMS.md).
    fn exec_configuration(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        context: Option<&UnloadedToolchainContext>,
    ) -> Result<ConfigId, String> {
        let _shared = incremental::SharedScope::enter("exec_configuration");
        let platform = match context {
            Some(context) => context.exec_platform.label.clone(),
            None => {
                let host = self.configuration(config).native.host_platform.clone();
                self.platform_info(&host, config)
                    .map_err(|error| self.platform_resolution_error(rule, config, &host, error))?
                    .label
                    .clone()
            }
        };
        // Everything the transition depends on besides `config` and
        // `platform` is fixed until Engine::set_configuration (the memo,
        // Host::exec_transition).
        self.exec_transition(config, &platform).map_err(|error| {
            // An invalid exec output directory name fails the dependency's
            // configuration (InvalidMnemonicException), reported at the rule.
            match error.strip_prefix(EXEC_INTERN_ERROR) {
                Some(error) => located(rule.location.as_deref(), error.to_owned()),
                None => error,
            }
        })
    }

    /// The exec transition of `config` for `platform` (an `ExecCfg`
    /// node's body): an interning error is marked [`EXEC_INTERN_ERROR`]
    /// (the consumer reports it at its rule).
    fn compute_exec_transition(
        &mut self,
        config: ConfigId,
        platform: &Label,
    ) -> Result<ConfigId, String> {
        let target_scoped = self.target_scoped_flags(config)?;
        self.intern_exec(config, platform.clone(), &target_scoped)
            .map_err(|error| format!("{EXEC_INTERN_ERROR}{error}"))
    }

    /// The Starlark flags of the top-level configuration and of `config`
    /// whose rule explicitly sets `scope = "target"` (Scope.ScopeType.TARGET,
    /// analysis/config/Scope.java:31-46): the flags the exec transition of
    /// `config` resets, also covering the top-level configuration's own so
    /// that the memoized exec baseline (crate::config_registry) is the same
    /// whichever configuration computes it first.
    fn target_scoped_flags(&mut self, config: ConfigId) -> Result<BTreeSet<Label>, String> {
        let mut labels = self
            .top_configuration()
            .starlark
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        labels.extend(self.configuration(config).starlark.keys().cloned());
        let mut flags = BTreeSet::new();
        for label in labels {
            if self.flag_scope(&label)? {
                flags.insert(label);
            }
        }
        Ok(flags)
    }

    /// Whether the Starlark flag `label` explicitly sets `scope = "target"`
    /// (a `FlagScope` node's body).
    fn compute_flag_scope(&mut self, label: &Label) -> Result<bool, String> {
        Ok(match self.target(label)? {
            Target::Rule(rule) => self
                .build_setting_info(&rule)?
                .is_some_and(|info| info.scope.is_target()),
            _ => false,
        })
    }

    /// The retained Starlark transition of `rule` under `key` (`""` for
    /// the rule's `cfg`, else the attribute name).
    fn transition_def(
        rule: &Rule,
        key: &str,
    ) -> Result<Option<crate::starlark_transitions::TransitionDef>, String> {
        rule.custom
            .as_ref()
            .and_then(|custom| custom.transitions.get(key))
            .map(crate::starlark_transitions::TransitionDef::of)
            .transpose()
    }

    /// StarlarkBuildSettingsDetailsFunction
    /// (skyframe/StarlarkBuildSettingsDetailsFunction.java:60-330) for the
    /// Starlark settings a transition reads or writes (inputs then outputs,
    /// StarlarkTransitionCache.getAllStarlarkBuildSettings): each must be a
    /// build setting, possibly through unconfigured aliases. Errors are the
    /// TransitionException texts (a loading exception as `CLASS: MESSAGE`).
    fn transition_details(
        &mut self,
        def: &crate::starlark_transitions::TransitionDef,
    ) -> Result<TransitionDetails, String> {
        let _shared = incremental::SharedScope::enter("transition_details");
        self.transition_details_for(def.starlark_settings())
    }

    /// getBuildSettingPackages (:139-195): rounds over the not yet verified
    /// labels — every label (also an alias's actual) may be seen once, so a
    /// transition naming both an alias and its actual, or an alias cycle,
    /// is `Dependency cycle involving 'L' detected in aliased build
    /// settings`; their packages are loaded, then each is verified
    /// (verifyBuildSettingsAndGetAliases, :210-262) and the next round holds
    /// the aliases' actuals. Then each setting's details are those of the
    /// end of its alias chain (getActual, :272-290).
    fn compute_transition_details(
        &mut self,
        settings: &[Label],
    ) -> Result<TransitionDetails, String> {
        let mut seen = BTreeSet::new();
        let mut targets: BTreeMap<Label, SettingTarget> = BTreeMap::new();
        let mut unverified = settings.to_vec();
        while !unverified.is_empty() {
            for label in &unverified {
                if !seen.insert(label.clone()) {
                    return Err(format!(
                        "Dependency cycle involving '{}' detected in aliased build settings",
                        crate::rule_declarations::canonical_form(label)
                    ));
                }
            }
            let mut packages: Vec<PackageId> = Vec::new();
            for label in &unverified {
                let package = label.package_id();
                if !packages.contains(&package) {
                    packages.push(package);
                }
            }
            for package in &packages {
                self.load_package(package)
                    .map_err(|error| PlatformError::loading(error).target_text())?;
            }
            let mut next = Vec::new();
            for label in &unverified {
                let target = self.setting_target(label)?;
                if let SettingTarget::Alias(actual) = &target
                    && !next.contains(actual)
                {
                    next.push(actual.clone());
                }
                targets.insert(label.clone(), target);
            }
            unverified = next;
        }
        let mut details = BTreeMap::new();
        for label in settings {
            let mut current = label;
            let detail = loop {
                match targets.get(current) {
                    Some(SettingTarget::Alias(actual)) => current = actual,
                    Some(SettingTarget::Setting(detail)) => break detail.clone(),
                    None => {
                        return Err(format!(
                            "internal error: build setting {current} was not loaded"
                        ));
                    }
                }
            };
            details.insert(label.clone(), detail);
        }
        Ok(details)
    }

    /// One label of verifyBuildSettingsAndGetAliases (:210-262).
    fn setting_target(&mut self, label: &Label) -> Result<SettingTarget, String> {
        let shown = crate::rule_declarations::canonical_form(label);
        let not_a_setting =
            || format!("attempting to transition on '{shown}' which is not a build setting");
        let rule = match self
            .target(label)
            .map_err(|error| PlatformError::loading(error).target_text())?
        {
            Target::Source(_) => return Err(not_a_setting()),
            // Target.getAssociatedRule of an output file is its generating
            // rule, which is not a build setting.
            Target::Output(..) => return Err(not_a_setting()),
            Target::Rule(rule) => rule,
        };
        if let (RuleKind::Native("alias"), Some(native)) = (&rule.kind, &rule.native) {
            if native.configurable.contains_key("actual") {
                return Err(format!(
                    "attempting to transition on aliased build setting '{shown}', the actual value of which uses select(). Aliased build settings with configured actual values is not supported."
                ));
            }
            let Some(crate::custom_rule::AttrValue::Label(Some(actual))) =
                native.attrs.get("actual")
            else {
                return Err(not_a_setting());
            };
            return Ok(SettingTarget::Alias(self.resolve_rule_src(&rule, actual)?));
        }
        let Some(info) = self.build_setting_info(&rule)? else {
            return Err(not_a_setting());
        };
        Ok(SettingTarget::Setting(
            crate::starlark_transitions::SettingDetail {
                actual: info.label.clone(),
                ty: info.ty,
                default: info.default,
            },
        ))
    }

    /// Apply a Starlark transition attached to `target` to `from`
    /// (TransitionApplier with StarlarkTransitionCache): the resulting
    /// configuration, interned with its output path mnemonic.
    fn apply_transition(
        &mut self,
        def: &crate::starlark_transitions::TransitionDef,
        kind: crate::starlark_transitions::Kind,
        target: &Label,
        from: ConfigId,
        attributes: &crate::starlark_transitions::AttributeValues,
    ) -> Result<TransitionConfigurations, crate::starlark_transitions::Failure> {
        use sha2::Digest;
        let failure = |message: String| crate::starlark_transitions::Failure {
            events: Vec::new(),
            message,
        };
        let digest: [u8; 32] = sha2::Sha256::digest(
            serde_json::to_vec(&(
                target.to_string(),
                kind == crate::starlark_transitions::Kind::Rule,
                attributes,
            ))
            .map_err(|error| failure(error.to_string()))?,
        )
        .into();
        let cache_key = (def.id, from, digest);
        // The evaluator's `Transition` node (its prints are replayed as the
        // cache below replays them).
        if let Some(applied) = self.transition_node(&cache_key, &|| super::host::TransitionInput {
            def: def.clone(),
            kind,
            target: target.clone(),
            from,
            attributes: attributes.clone(),
        }) {
            return applied;
        }
        // StarlarkTransitionCache.computeIfAbsent (:128-185): a hit replays
        // the application's non-error events (print() output), so every
        // application prints, also the IdempotencyChecker's and a
        // delegate's re-application (RuleTransitionApplier).
        if let Some((id, printed)) = self.transition_cache_get(&cache_key) {
            for line in &printed {
                self.print(line);
            }
            return Ok(id);
        }
        let (id, printed) = self.compute_transition(def, kind, target, from, attributes)?;
        self.transition_cache_put(cache_key, (Arc::clone(&id), printed));
        Ok(id)
    }

    /// One Starlark transition application (a cache miss; the evaluator's
    /// `Transition` node): its configurations and print() lines (printed).
    fn compute_transition(
        &mut self,
        def: &crate::starlark_transitions::TransitionDef,
        kind: crate::starlark_transitions::Kind,
        target: &Label,
        from: ConfigId,
        attributes: &crate::starlark_transitions::AttributeValues,
    ) -> Result<(TransitionConfigurations, Vec<String>), crate::starlark_transitions::Failure> {
        let failure = |message: String| crate::starlark_transitions::Failure {
            events: Vec::new(),
            message,
        };
        let configuration = self.configuration(from).clone();
        let mapped = def.with_flag_aliases(&configuration.native.flag_aliases);
        let def = &mapped;
        let details = self.transition_details(def).map_err(failure)?;
        let applied = crate::starlark_transitions::apply(
            def,
            &crate::starlark_transitions::Application {
                kind,
                from: &configuration,
                details: &details,
                target,
                attributes,
                repositories: self.repositories(),
            },
        )?;
        for line in &applied.printed {
            self.print(line);
        }
        let target_scoped = self.target_scoped_flags(from).map_err(failure)?;
        let id: TransitionConfigurations = applied
            .configurations
            .into_iter()
            .map(|(key, config)| {
                self.intern_derived(config, &target_scoped)
                    .map(|id| (key, id))
                    .map_err(failure)
            })
            .collect::<Result<Vec<_>, _>>()?
            .into();
        Ok((id, applied.printed))
    }

    /// RuleTransitionApplier (analysis/producers/RuleTransitionApplier.java:130-378):
    /// the target platform and select() conditions of `rule` in its
    /// requested configuration, the rule's Starlark transition over its
    /// attributes (StarlarkRuleTransitionProvider.createTransition,
    /// analysis/starlark/StarlarkRuleTransitionProvider.java:94-200), then
    /// the idempotency check: `None` for an identity transition (the target
    /// is analyzed under its requested key), else the key it is analyzed
    /// under — with `shouldApplyRuleTransition` only when applying the
    /// transition again gives the same configuration (IDEMPOTENT), without
    /// it otherwise (NON_IDEMPOTENT). Errors are reported at the rule.
    fn rule_transition(
        &mut self,
        rule: &Rule,
        config: ConfigId,
    ) -> Result<Option<ConfiguredTargetKey>, String> {
        let _shared = incremental::SharedScope::enter("rule_transition");
        let Some(def) = Self::transition_def(rule, "")? else {
            return Ok(None);
        };
        let uses_toolchains = uses_toolchain_resolution(rule) && !self.platform_tree();
        if uses_toolchains && !self.default_target_platform(config) {
            self.target_platform(config)?;
        }
        let ordered = self.config_conditions(rule, config)?;
        let conditions = ordered.into_iter().collect::<BTreeMap<_, _>>();
        let resolver = configured_attributes::Resolver {
            target: &rule.label,
            conditions: &conditions,
            config_hash: config.to_string(),
        };
        let attributes = configured_attributes::transition_attributes(
            rule,
            Some(&resolver),
            &def.given_outputs(),
        );
        let report = |failure: crate::starlark_transitions::Failure| {
            let mut lines = failure.events;
            lines.push(located(rule.location.as_deref(), failure.message));
            lines.join("\n")
        };
        let kind = crate::starlark_transitions::Kind::Rule;
        let post = self
            .apply_transition(&def, kind, &rule.label, config, &attributes)
            .map_err(report)?[0]
            .1;
        if post == config {
            return Ok(None);
        }
        let again = self
            .apply_transition(&def, kind, &rule.label, post, &attributes)
            .map_err(report)?[0]
            .1;
        Ok(Some(if again == post {
            ConfiguredTargetKey::new(rule.label.clone(), post)
        } else {
            ConfiguredTargetKey::without_rule_transition(rule.label.clone(), post)
        }))
    }

    /// The configuration of the dependencies of `rule`'s attribute
    /// `attribute`, whose `cfg` is a Starlark transition
    /// (StarlarkAttributeTransitionProvider, analysis/starlark/StarlarkAttributeTransitionProvider.java:105-135):
    /// the transition over the rule's configured attribute values. A
    /// failure is reported at the rule as `on dependency edge L (CONFIG)
    /// -|ATTR|-> DEP: MESSAGE` (DependencyProducer.java:230-266; CONFIG is
    /// the first 7 hex digits of the native ConfigId).
    fn attribute_transition(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        attribute: &str,
        dependency: &Label,
    ) -> Result<TransitionConfigurations, String> {
        let _shared = incremental::SharedScope::enter("attribute_transition");
        let def = Self::transition_def(rule, attribute)?.ok_or_else(|| {
            format!(
                "{}: attribute '{attribute}' declares a Starlark transition that the native profile did not retain (a lifted subrule attribute); {}",
                rule.label,
                crate::rule_declarations::NOT_ANALYZED
            )
        })?;
        let attributes = configured_attributes::transition_attributes(rule, None, &[]);
        self.apply_transition(
            &def,
            crate::starlark_transitions::Kind::Attribute,
            &rule.label,
            config,
            &attributes,
        )
        .map_err(|failure| {
            let mut lines = failure.events;
            lines.push(located(
                rule.location.as_deref(),
                format!(
                    "on dependency edge {} ({}) -|{}|-> {}: {}",
                    crate::rule_declarations::canonical_form(&rule.label),
                    &config.to_string()[..7],
                    native_attribute_name(attribute),
                    crate::rule_declarations::canonical_form(dependency),
                    failure.message
                ),
            ));
            lines.join("\n")
        })
    }

    /// A missing dependency edge (missing_edge), preceded by the target
    /// loading event when the target belongs to another package.
    fn missing_edge_error(&self, rule: &Rule, dependency: &Label, error: String) -> String {
        let event = self.foreign_target_event(&rule.label.package_id(), dependency, &error);
        let edge = missing_edge(rule, dependency, error);
        match event {
            Some(event) => format!("{event}\n{edge}"),
            None => edge,
        }
    }

    /// Bazel reports a missing target of another package first as that
    /// package's loading event, located at its BUILD file
    /// (`/ws/p/BUILD.bazel: no such target '//p:x': ...`, an event of the
    /// target lookup, as observed in the paired captures N16b
    /// A10/missing_target_edge and N16c B2/lx_use_cv_missing), before the
    /// consumer's edge error.
    fn foreign_target_event(
        &self,
        consumer: &PackageId,
        dependency: &Label,
        error: &str,
    ) -> Option<String> {
        if dependency.package_id() == *consumer {
            return None;
        }
        self.target_loading_event(dependency, error)
    }

    /// The loading event of a missing target, at its package's BUILD file.
    fn target_loading_event(&self, dependency: &Label, error: &str) -> Option<String> {
        if !error.starts_with("no such target '") {
            return None;
        }
        let root = self.repositories().root(&dependency.repository).ok()?;
        let build = workspace::package_build_file(root, &dependency.package).ok()?;
        Some(format!("{}: {error}", build.display()))
    }

    /// AbstractLabelCycleReporter.maybeReportCycle (skyframe/AbstractLabelCycleReporter.java:81-170):
    /// `in KIND L: cycle in dependency graph:` at the location of the cycle's
    /// first element, the path to the cycle indented by four spaces, then the
    /// cycle drawn with `.-> `, `|   `, `` `-- `` (`[self-edge]` for one
    /// element). Configured target keys print as `LABEL (CONFIG)`; the native
    /// configuration is the full native ConfigId, not Bazel's BuildOptions
    /// checksum, and the single native configuration has no separate
    /// top-level key (docs/CONFIGURATION.md).
    fn cycle_error(
        &self,
        head: &Rule,
        path: &[ConfiguredTargetKey],
        cycle: &[ConfiguredTargetKey],
    ) -> String {
        let key = ConfiguredTargetKey::to_string;
        let mut message = String::from("cycle in dependency graph:");
        // The path starts at the requested target's top-level key: for a
        // test-trimmed rule, its key in the untrimmed configuration, then its
        // own (AbstractLabelCycleReporter prints every key of the path,
        // skyframe/AbstractLabelCycleReporter.java:105-114).
        if let Some(root) = &self.requested_trimmed_root()
            && path.first().or(cycle.first()) == Some(root)
        {
            message.push_str(&format!(
                "\n    {} ({})",
                crate::rule_declarations::canonical_form(&root.label),
                self.configuration(root.config).untrimmed_id()
            ));
        }
        for element in path {
            message.push_str("\n    ");
            message.push_str(&key(element));
        }
        message.push_str("\n.-> ");
        message.push_str(&key(&cycle[0]));
        if cycle.len() == 1 {
            message.push_str(" [self-edge]\n`--");
        } else {
            for element in &cycle[1..] {
                message.push_str("\n|   ");
                message.push_str(&key(element));
            }
            message.push_str("\n`-- ");
            message.push_str(&key(&cycle[0]));
        }
        located(
            head.location.as_deref(),
            crate::rule_errors::RuleEvents {
                rule_class: head.rule_class_name(),
                label: &crate::rule_declarations::canonical_form(&head.label),
            }
            .rule_message(&message),
        )
    }

    /// The alias chain from `label` (AliasProvider.getAliasChain) and its
    /// end (AliasConfiguredTarget.getLabel, rules/AliasConfiguredTarget.java:178-185).
    fn alias_chain(&mut self, config: ConfigId, label: &Label) -> (Vec<Label>, Label) {
        let mut chain = Vec::new();
        let mut current = label.clone();
        while let Some(actual) = self
            .configured_node(&ConfiguredTargetKey::new(current.clone(), config))
            .and_then(|node| node.actual.clone())
        {
            if chain.contains(&current) {
                break;
            }
            chain.push(current);
            current = actual;
        }
        (chain, current)
    }

    /// The configured actual of an alias-like rule, as label text: `actual`
    /// after select() resolution, or the label_flag/label_setting value
    /// (LabelBuildSettings.ACTUAL, rules/LabelBuildSettings.java:62-83).
    fn alias_actual(
        &self,
        rule: &Rule,
        configuration: &Configuration,
        build_setting: Option<&BuildSettingInfo>,
    ) -> Result<String, String> {
        if let Some(info) = build_setting {
            return match info.value(configuration) {
                SettingValue::Label(label) => Ok(label.to_string()),
                other => Err(format!(
                    "internal error: label build setting {} has value {other}",
                    rule.label
                )),
            };
        }
        match rule
            .native
            .as_ref()
            .and_then(|native| native.attrs.get("actual"))
        {
            Some(crate::custom_rule::AttrValue::Label(Some(actual))) => Ok(actual.clone()),
            _ => Err(format!(
                "internal error: alias {} has no resolved actual",
                rule.label
            )),
        }
    }

    /// The BuildSettingProvider of a build-setting rule: Starlark rules with
    /// `build_setting` (RuleConfiguredTargetBuilder.java:250-259) and
    /// label_flag/label_setting (NODEP_LABEL, LabelBuildSettings.java:97).
    fn build_setting_info(&self, rule: &Rule) -> Result<Option<BuildSettingInfo>, String> {
        if let Some(custom) = &rule.custom
            && let Some(setting) = &custom.build_setting
        {
            return Ok(Some(BuildSettingInfo {
                label: rule.label.clone(),
                ty: SettingType::of(setting.kind),
                default: crate::analysis::setting_default(custom)
                    .map_err(|error| format!("{}: {error}", rule.label))?,
                is_flag: setting.flag,
                repeatable: setting.repeatable,
                scope: custom.declaration.explicit_scope.clone(),
            }));
        }
        let RuleKind::Native(kind @ ("label_flag" | "label_setting")) = rule.kind else {
            return Ok(None);
        };
        let Some(crate::custom_rule::AttrValue::Label(Some(default))) = rule
            .native
            .as_ref()
            .and_then(|native| native.attrs.get("build_setting_default"))
        else {
            return Err(format!(
                "internal error: {kind} {} has no build_setting_default",
                rule.label
            ));
        };
        Ok(Some(BuildSettingInfo {
            label: rule.label.clone(),
            ty: SettingType::Label,
            default: SettingValue::Label(self.resolve_rule_src(rule, default)?),
            is_flag: kind == "label_flag",
            repeatable: false,
            // The native `scope` attribute (rules/LabelBuildSettings.java:93-96)
            // is stored only when explicitly specified.
            scope: match rule
                .native
                .as_ref()
                .and_then(|native| native.attrs.get("scope"))
            {
                Some(crate::custom_rule::AttrValue::String(scope)) => {
                    crate::starlark_options::ScopeAttribute::Value(scope.clone())
                }
                _ => crate::starlark_options::ScopeAttribute::Unspecified,
            },
        }))
    }

    /// A test_suite's `tests` (TestSuiteRule.java:96-104, `allowedFileTypes()`
    /// = no files): a file prerequisite is RuleContext's attribute error;
    /// then TestSuite.checkTestsAndSuites (rules/test/TestSuite.java:104-118):
    /// a prerequisite (its alias chain's end) that is neither a test rule
    /// (TestTagsProvider) nor a test_suite (TransitiveTestsProvider) is `in
    /// tests attribute of test_suite rule L: expecting a test or a test_suite
    /// rule but 'M' is not one`, M being `dep.getLabel()`: for an alias the
    /// end of its chain (AliasConfiguredTarget.getLabel returns its actual's
    /// label, rules/AliasConfiguredTarget.java:171-179).
    fn test_suite_errors(&mut self, rule: &Rule, config: ConfigId) -> Result<Vec<String>, String> {
        let mut errors = Vec::new();
        let (RuleKind::Native("test_suite"), Some(native)) = (&rule.kind, &rule.native) else {
            return Ok(errors);
        };
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let mut members = Vec::new();
        for text in native.labels("tests") {
            let dependency = self.resolve_rule_src(rule, &text)?;
            let (chain, actual) = self.alias_chain(config, &dependency);
            match self.no_files_error_p1(&chain, &actual)? {
                Some(error) => {
                    errors.push(format!(
                        "in tests attribute of test_suite rule {shown}: {error}"
                    ));
                }
                None => members.push(actual),
            }
        }
        if !errors.is_empty() {
            return Ok(errors);
        }
        for actual in members {
            let accepted = match self.target(&actual)? {
                Target::Rule(member) => {
                    test_suite_filter_tags(&member).is_some()
                        || matches!(member.kind, RuleKind::Native("test_suite"))
                }
                _ => false,
            };
            if !accepted {
                errors.push(format!(
                    "in tests attribute of test_suite rule {shown}: expecting a test or a test_suite rule but '{actual}' is not one"
                ));
            }
        }
        Ok(errors)
    }

    /// The `flag_values` and `constraint_values` prerequisites of a
    /// `config_setting`: every key and label is analyzed first, then each
    /// edge is validated like RuleContext.Builder does
    /// (validateDirectPrerequisiteType, analysis/RuleContext.java:1845-1880,
    /// with no allowed file types, ConfigRuleClasses.java:292-294, 314-316;
    /// then visibility, testonly and deprecation), in attribute order. All
    /// errors are reported.
    fn config_setting_prerequisites(
        &mut self,
        rule: &Rule,
        config: ConfigId,
    ) -> Result<
        (
            Vec<crate::config_setting::FlagPrerequisite>,
            Vec<crate::config_setting::ConstraintPrerequisite>,
        ),
        String,
    > {
        let (RuleKind::Native("config_setting"), Some(native)) = (&rule.kind, &rule.native) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let mut entries = match native.attrs.get("flag_values") {
            Some(crate::custom_rule::AttrValue::LabelKeyedStringDict(entries)) => entries.clone(),
            _ => Vec::new(),
        };
        let explicit_flag_labels = entries
            .iter()
            .map(|(key, _)| self.resolve_rule_src(rule, key))
            .collect::<Result<BTreeSet<_>, _>>()?;
        // ConfigSetting.getSettings: values whose keys are flag aliases are
        // Starlark expectations, including aliases of removed native flags.
        let aliases = &self.configuration(config).native.flag_aliases;
        let mut aliased = Vec::new();
        if let Some(crate::custom_rule::AttrValue::StringDict(values)) = native.attrs.get("values")
        {
            aliased.extend(values.iter().filter_map(|(name, value)| {
                aliases
                    .get(name)
                    .map(|label| (name.clone(), label.clone(), value.clone()))
            }));
        }
        if let Some(crate::custom_rule::AttrValue::StringDict(defines)) =
            native.attrs.get("define_values")
            && let Some(label) = aliases.get("define")
        {
            aliased.extend(defines.iter().map(|(name, value)| {
                (
                    "define".to_owned(),
                    label.clone(),
                    format!("{name}={value}"),
                )
            }));
        }
        let alias_labels = aliased
            .iter()
            .map(|(_, label, _)| label.clone())
            .collect::<BTreeSet<_>>();
        for (name, label, value) in aliased {
            let mut found = false;
            for (key, existing) in &entries {
                if self.resolve_rule_src(rule, key)? == label {
                    if existing != &value {
                        return Err(format!(
                            "Conflicting flag value expectations: {}: --{name} is a flag alias for --{label}, but values and flag_values match different values ({value:?} and {existing:?})",
                            rule.label
                        ));
                    }
                    found = true;
                }
            }
            if !found {
                entries.push((
                    format!("@@{}//{}:{}", label.repository, label.package, label.name),
                    value,
                ));
            }
        }
        let mut keys = Vec::new();
        for (key, value) in entries {
            let dependency = self.resolve_rule_src(rule, &key)?;
            self.target(&dependency)
                .map_err(|error| self.missing_edge_error(rule, &dependency, error))?;
            self.analyze(&dependency, config)?;
            keys.push(("flag_values", dependency, Some(value)));
        }
        for text in native.labels("constraint_values") {
            let dependency = self.resolve_rule_src(rule, &text)?;
            self.target(&dependency)
                .map_err(|error| self.missing_edge_error(rule, &dependency, error))?;
            self.analyze(&dependency, config)?;
            keys.push(("constraint_values", dependency, None));
        }
        // UserDefinedFlagMatch reads the FeatureFlagInfo a key returns
        // (ConfigSetting.java:651-687): a Starlark key's implementation runs
        // first (Self::plan_pending).
        let mut starlark_keys = Vec::new();
        for (attribute, dependency, _) in &keys {
            if *attribute != "flag_values" {
                continue;
            }
            let actual = self.alias_chain(config, dependency).1;
            if matches!(
                self.target(&actual),
                Ok(Target::Rule(dep)) if matches!(dep.kind, RuleKind::Custom)
            ) {
                starlark_keys.push(ConfiguredTargetKey::new(dependency.clone(), config));
            }
        }
        if !starlark_keys.is_empty() {
            self.flush_pending(rule, &starlark_keys)?;
        }
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let requesting_package = rule_authority(rule);
        let mut errors = Vec::new();
        let mut flags = Vec::new();
        let mut constraints = Vec::new();
        for (attribute, dependency, value) in keys {
            if self.cquery() {
                if attribute != "flag_values" || explicit_flag_labels.contains(&dependency) {
                    self.capture_cquery_edge(
                        &ConfiguredTargetKey::new(rule.label.clone(), config),
                        dependency.clone(),
                        config,
                        attribute,
                        false,
                        false,
                    );
                }
                if attribute == "flag_values" && alias_labels.contains(&dependency) {
                    self.capture_cquery_edge(
                        &ConfiguredTargetKey::new(rule.label.clone(), config),
                        dependency.clone(),
                        config,
                        ":flag_alias_settings",
                        true,
                        false,
                    );
                }
            }
            let (chain, actual) = self.alias_chain(config, &dependency);
            let is_source = matches!(self.target(&actual)?, Target::Source(_));
            if let Some(error) = self.no_files_error_p1(&chain, &actual)? {
                errors.push(format!(
                    "in {attribute} attribute of config_setting rule {shown}: {error}"
                ));
            }
            let visible = match self.target(&dependency)? {
                Target::Rule(dep) | Target::Output(dep, _) => self.visible(
                    &dep.visibility,
                    &rule_authority(&dep),
                    Some(&requesting_package),
                )?,
                Target::Source(_) => self.visible_source(
                    &dependency,
                    &dependency.package_id(),
                    Some(&requesting_package),
                )?,
            };
            // Formatted only for a reported error.
            let description = || describe_with_aliases(&chain, &actual, "target");
            if !visible {
                errors.push(visibility_message(rule, &description(), is_source, None));
            }
            if let Some(error) = self.prerequisite_error(rule, &actual, &description, false)? {
                errors.push(error);
            }
            let node = self.configured_node(&ConfiguredTargetKey::new(dependency.clone(), config));
            match value {
                Some(value) => flags.push(crate::config_setting::FlagPrerequisite {
                    feature_flag: node
                        .as_ref()
                        .and_then(|node| node.feature_flag.clone())
                        .or_else(|| {
                            self.planned_feature_flag(&ConfiguredTargetKey::new(
                                dependency.clone(),
                                config,
                            ))
                        }),
                    specified: dependency,
                    actual,
                    value,
                    setting: node.as_ref().and_then(|node| node.build_setting.clone()),
                }),
                None => constraints.push(crate::config_setting::ConstraintPrerequisite {
                    actual,
                    value: node.as_ref().and_then(|node| node.constraint_value.clone()),
                }),
            }
        }
        if !errors.is_empty() {
            return Err(errors
                .into_iter()
                .map(|error| located(rule.location.as_deref(), error))
                .collect::<Vec<_>>()
                .join("\n"));
        }
        Ok((flags, constraints))
    }

    /// RuleContext.Builder.createTargetMap for native label attributes
    /// (analysis/RuleContext.java:1708-1745, 2165-2172): every prerequisite is
    /// analyzed, then validated in attribute order — validateRuleDependency
    /// for rules (allowed rule classes, else mandatory providers, :2002-2093),
    /// the no-files check for files (:1845-1880), then visibility, testonly
    /// and deprecation. All errors are reported at the rule's location.
    fn resolve_native_prerequisites(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        attributes: &[PrerequisiteAttribute],
    ) -> Result<Vec<Vec<Label>>, String> {
        let mut resolved = Vec::new();
        for attribute in attributes {
            let mut labels = Vec::new();
            for text in &attribute.labels {
                let dependency = self.resolve_rule_src(rule, text)?;
                self.target(&dependency)
                    .map_err(|error| self.missing_edge_error(rule, &dependency, error))?;
                self.analyze(&dependency, config)?;
                if self.cquery() {
                    self.capture_cquery_edge(
                        &ConfiguredTargetKey::new(rule.label.clone(), config),
                        dependency.clone(),
                        config,
                        attribute.name,
                        false,
                        false,
                    );
                }
                labels.push(dependency);
            }
            resolved.push(labels);
        }
        Ok(resolved)
    }

    fn validate_native_prerequisites(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        attributes: &[PrerequisiteAttribute],
        resolved: &[Vec<Label>],
    ) -> Result<(), String> {
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let class = rule.rule_class_name().to_owned();
        let requesting_package = rule_authority(rule);
        let mut errors = Vec::new();
        for (attribute, labels) in attributes.iter().zip(resolved) {
            let prefix = crate::rule_errors::RuleEvents {
                rule_class: &class,
                label: &shown,
            }
            .attribute_message(attribute.name, "");
            for dependency in labels {
                let (chain, actual) = self.alias_chain(config, dependency);
                let actual_target = self.target(&actual)?;
                let is_source = matches!(actual_target, Target::Source(_));
                let type_error = match &actual_target {
                    Target::Rule(actual_rule) => rule_dependency_error(
                        attribute,
                        &chain,
                        &actual,
                        actual_rule,
                        self.configured_node(&ConfiguredTargetKey::new(actual.clone(), config))
                            .as_ref(),
                    ),
                    _ => self.no_files_error_p1(&chain, &actual)?,
                };
                if let Some(error) = type_error {
                    errors.push(format!("{prefix}{error}"));
                }
                let visible = match self.target(dependency)? {
                    Target::Rule(dep) | Target::Output(dep, _) => self.visible(
                        &dep.visibility,
                        &rule_authority(&dep),
                        Some(&requesting_package),
                    )?,
                    Target::Source(_) => self.visible_source(
                        dependency,
                        &dependency.package_id(),
                        Some(&requesting_package),
                    )?,
                };
                // Formatted only for a reported error.
                let description = || describe_with_aliases(&chain, &actual, "target");
                if !visible {
                    errors.push(visibility_message(rule, &description(), is_source, None));
                }
                if let Some(error) = self.prerequisite_error(rule, &actual, &description, false)? {
                    errors.push(error);
                }
            }
        }
        if !errors.is_empty() {
            return Err(errors
                .into_iter()
                .map(|error| located(rule.location.as_deref(), error))
                .collect::<Vec<_>>()
                .join("\n"));
        }
        Ok(())
    }

    /// The providers of `constraint_setting` (rules/platform/ConstraintSetting.java:227-304),
    /// `constraint_value` (ConstraintValue.java:338-353) and `platform`
    /// (Platform.java:44-120) from their validated prerequisites.
    fn platform_rule_providers(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        prerequisites: &[Vec<Label>],
    ) -> Result<PlatformProviders, String> {
        let Some(native) = rule.native.clone() else {
            return Ok((None, None, None));
        };
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let class = rule.rule_class_name().to_owned();
        let attribute_error = |attribute: &str, message: &str| {
            located(
                rule.location.as_deref(),
                crate::rule_errors::RuleEvents {
                    rule_class: &class,
                    label: &shown,
                }
                .attribute_message(attribute, message),
            )
        };
        let mut nodes: BTreeMap<Label, ConfiguredNode> = BTreeMap::new();
        for label in prerequisites.iter().flatten() {
            if let Some(found) =
                self.configured_node(&ConfiguredTargetKey::new(label.clone(), config))
            {
                nodes.insert(label.clone(), found);
            }
        }
        let node = |label: &Label| nodes.get(label);
        let internal = |what: &str| {
            format!(
                "internal error: validated prerequisite of {} lacks {what}",
                rule.label
            )
        };
        match rule.kind {
            RuleKind::Native("constraint_setting") => {
                // validateDefaultConstraintValue (ConstraintSetting.java:270-304).
                let mut default_value = None;
                if let Some(crate::custom_rule::AttrValue::Label(Some(text))) =
                    native.attrs.get("default_constraint_value")
                {
                    let default = self.resolve_rule_src(rule, text)?;
                    if default.package_id() != rule.label.package_id() {
                        return Err(attribute_error(
                            "default_constraint_value",
                            "The default constraint value must be defined in the same package as the constraint setting itself.",
                        ));
                    }
                    if self.target(&default).is_err() {
                        return Err(attribute_error(
                            "default_constraint_value",
                            &format!(
                                "The default constraint value '{}' does not exist",
                                crate::rule_declarations::canonical_form(&default)
                            ),
                        ));
                    }
                    default_value = Some(default);
                }
                let refined = match prerequisites.first().and_then(|labels| labels.first()) {
                    Some(label) => Some(Box::new(
                        node(label)
                            .and_then(|node| node.constraint_value.clone())
                            .ok_or_else(|| internal("ConstraintValueInfo"))?,
                    )),
                    None => None,
                };
                Ok((
                    Some(ConstraintSettingInfo {
                        label: rule.label.clone(),
                        default_value,
                        refined,
                    }),
                    None,
                    None,
                ))
            }
            RuleKind::Native("constraint_value") => {
                let setting = prerequisites
                    .first()
                    .and_then(|labels| labels.first())
                    .and_then(node)
                    .and_then(|node| node.constraint_setting.clone())
                    .ok_or_else(|| internal("ConstraintSettingInfo"))?;
                Ok((
                    None,
                    Some(ConstraintValueInfo {
                        setting: Arc::new(setting),
                        label: rule.label.clone(),
                    }),
                    None,
                ))
            }
            RuleKind::Native("platform") => {
                let provided = |index: usize| prerequisites.get(index).cloned().unwrap_or_default();
                let mut constraint_values = Vec::new();
                for label in provided(0) {
                    constraint_values.push(
                        node(&label)
                            .and_then(|node| node.constraint_value.clone())
                            .ok_or_else(|| internal("ConstraintValueInfo"))?,
                    );
                }
                let mut parents = Vec::new();
                for label in provided(1) {
                    parents.push(
                        node(&label)
                            .and_then(|node| node.platform.clone())
                            .ok_or_else(|| internal("PlatformInfo"))?,
                    );
                }
                let required_settings = provided(2)
                    .iter()
                    .map(|label| self.alias_chain(config, label).1)
                    .collect();
                let mut allowed_toolchain_types = Vec::new();
                for text in native.labels("allowed_toolchain_types") {
                    allowed_toolchain_types.push(self.resolve_rule_src(rule, &text)?);
                }
                let exec_properties = match native.attrs.get("exec_properties") {
                    Some(crate::custom_rule::AttrValue::StringDict(entries)) => entries.clone(),
                    _ => Vec::new(),
                };
                let attributes = crate::platforms::PlatformAttributes {
                    parents,
                    constraint_values,
                    remote_execution_properties: native
                        .string("remote_execution_properties")
                        .unwrap_or_default()
                        .to_owned(),
                    exec_properties,
                    flags: native.strings("flags"),
                    required_settings,
                    check_toolchain_types: matches!(
                        native.attrs.get("check_toolchain_types"),
                        Some(crate::custom_rule::AttrValue::Bool(true))
                    ),
                    allowed_toolchain_types,
                    missing_toolchain_error: native
                        .string("missing_toolchain_error")
                        .unwrap_or(crate::platforms::DEFAULT_MISSING_TOOLCHAIN_ERROR)
                        .to_owned(),
                };
                let mapping = self.repositories().mapping("").cloned().unwrap_or_default();
                let platform =
                    crate::platforms::create_platform(&rule.label, attributes, &|label| {
                        crate::platforms::shorthand_display_form(label, &mapping)
                    })
                    .map_err(|errors| {
                        errors
                            .iter()
                            .map(|error| attribute_error(error.attribute, &error.message))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })?;
                Ok((None, None, Some(Arc::new(platform))))
            }
            _ => Ok((None, None, None)),
        }
    }

    /// The target platform of configuration `config`
    /// (PlatformConfiguration.getTargetPlatform, analysis/PlatformConfiguration.java:84-86),
    /// resolved like PlatformFunction. Loading errors carry Bazel's
    /// exception-class prefix (PlatformError::target_text).
    fn target_platform(&mut self, config: ConfigId) -> Result<Arc<PlatformInfo>, String> {
        let _shared = incremental::SharedScope::enter("target_platform");
        let label = self.configuration(config).native.target_platform().clone();
        self.platform_info(&label, config)
            .map_err(|error| error.target_text())
    }

    /// Whether the target platform of `config` is the default host
    /// platform, which the native profile resolves only where a constraint
    /// needs it.
    fn default_target_platform(&self, config: ConfigId) -> bool {
        *self.configuration(config).native.target_platform()
            == crate::configuration::default_host_platform()
    }

    /// ToolchainException.asConfiguredValueCreationException
    /// (skyframe/toolchains/ToolchainException.java:67-91) as reported by
    /// ConfiguredTargetFunction (:394-404): `While resolving toolchains for
    /// target L (CONFIG): MESSAGE` at the rule's location, after `events`
    /// (CONFIG is the native identifier, docs/PLATFORMS.md).
    fn toolchain_error(
        &self,
        rule: &Rule,
        config: ConfigId,
        events: &str,
        message: &str,
    ) -> String {
        // An aspect's own toolchain resolution: AspectFunction reports the
        // ToolchainException's message itself (AspectCreationException with
        // a LabelCause, AspectFunction.java:510-515), no target prefix.
        if self.resolving_aspect_toolchains() {
            return if events.is_empty() {
                message.to_owned()
            } else {
                format!("{events}\n{message}")
            };
        }
        let id = config.to_string();
        let error = located(
            rule.location.as_deref(),
            format!(
                "While resolving toolchains for target {} ({}): {message}",
                crate::rule_declarations::canonical_form(&rule.label),
                &id[..7]
            ),
        );
        if events.is_empty() {
            error
        } else {
            format!("{events}\n{error}")
        }
    }

    /// An InvalidPlatformException of PlatformKeys (PlatformKeys.java:150-166):
    /// a loading error names the label as a non-platform (paired capture
    /// B2/host_host_platform_fg_platforms_p_mac); other errors keep their
    /// reported lines and wrap the last one.
    fn platform_resolution_error(
        &self,
        rule: &Rule,
        config: ConfigId,
        label: &Label,
        error: PlatformError,
    ) -> String {
        let text = match error {
            PlatformError::Missing { .. } => crate::platforms::not_a_platform(label),
            PlatformError::Other(text) => text,
        };
        match text.rsplit_once('\n') {
            Some((events, last)) => self.toolchain_error(rule, config, events, last),
            None => self.toolchain_error(rule, config, "", &text),
        }
    }

    fn resolution_error(&self, rule: &Rule, config: ConfigId, error: ResolutionError) -> String {
        match error {
            ResolutionError::Toolchain(message) => self.toolchain_error(rule, config, "", &message),
            ResolutionError::Reported(events, message) => {
                self.toolchain_error(rule, config, &events, &message)
            }
            ResolutionError::Propagated(message) => message,
            ResolutionError::Aborted(message) => format!("{ANALYSIS_ABORTED}{message}"),
        }
    }

    /// Record `implementation` as a toolchain implementation of a parent
    /// whose execution platform is `exec_platform`. Bazel keys the
    /// implementation's configured target by that platform
    /// (ConfiguredTargetKey.executionPlatformLabel, DependencyResolver.java:835-866);
    /// the native key has no execution platform, so one implementation
    /// needed for two execution platforms in one configuration fails
    /// explicitly.
    fn toolchain_implementation(
        &mut self,
        implementation: &Label,
        config: ConfigId,
        exec_platform: &Label,
        location: Option<&str>,
    ) -> Result<(), String> {
        let _shared = incremental::SharedScope::enter("toolchain_implementation");
        let key = ConfiguredTargetKey::new(implementation.clone(), config);
        match self.claim_toolchain_implementation(key, exec_platform, location) {
            Some(existing) if existing != *exec_platform => Err(format!(
                "toolchain implementation {} is needed for the execution platforms {} and {} in one configuration: configured targets keyed by their execution platform (ConfiguredTargetKey.executionPlatformLabel) are not implemented in the native profile",
                crate::rule_declarations::canonical_form(implementation),
                crate::rule_declarations::canonical_form(&existing),
                crate::rule_declarations::canonical_form(exec_platform),
            )),
            Some(_) | None => Ok(()),
        }
    }

    /// ToolchainContextUtil.getUnloadedToolchainContextsInputs
    /// (skyframe/toolchains/ToolchainContextUtil.java:60-165): the rule
    /// class's toolchain types and execution constraints; a toolchain
    /// implementation forces its parent's execution platform.
    fn toolchain_context_key(
        &mut self,
        rule: &Rule,
        config: ConfigId,
    ) -> Result<ToolchainContextKey, String> {
        let mut key = ToolchainContextKey {
            forced_exec: self.forced_exec(&ConfiguredTargetKey::new(rule.label.clone(), config)),
            ..Default::default()
        };
        if let Some(custom) = &rule.custom {
            key.types = requirements(&custom.declaration.toolchain_types)?;
            key.exec_constraints = self.default_exec_constraints(rule)?;
        }
        // updateToolchainTypesFromAttribute (ToolchainContextUtil.java:61-64,
        // 94-117): a genrule's (nonconfigurable) `toolchains` are optional
        // toolchain types, silently ignored when not a toolchain_type
        // (Make-variable targets).
        if let (RuleKind::Genrule, Some(native)) = (&rule.kind, &rule.native) {
            // getExecutionPlatformConstraints (:150-167): the target's
            // `exec_compatible_with` (an ImmutableSet).
            for text in native.labels("exec_compatible_with") {
                let label = self.resolve_rule_src(rule, &text)?;
                if !key.exec_constraints.contains(&label) {
                    key.exec_constraints.push(label);
                }
            }
            for text in native.labels("toolchains") {
                let requirement = ToolchainTypeRequirement {
                    label: self.resolve_rule_src(rule, &text)?,
                    mandatory: false,
                    ignore_if_invalid: true,
                };
                if !key.types.contains(&requirement) {
                    key.types.push(requirement);
                }
            }
        }
        Ok(key)
    }

    /// Label.getDisplayForm with the main repository's mapping
    /// (cmdline/Label.java:511-513, RepositoryName.getDisplayForm).
    fn display_form(&self, label: &Label) -> String {
        if label.repository.is_empty() {
            return label.to_string();
        }
        // Borrowed, not copied: one lookup per call (the main mapping has
        // every module's apparent name).
        let apparent = self.repositories().mapping("").ok().and_then(|mapping| {
            mapping
                .iter()
                .find(|(_, canonical)| **canonical == label.repository)
                .map(|(apparent, _)| format!("@{apparent}"))
        });
        let repository = apparent.unwrap_or_else(|| format!("@@{}", label.repository));
        format!("{repository}//{}:{}", label.package, label.name)
    }

    /// ToolchainContextUtil.getExecutionPlatformConstraints (:150-167): the
    /// rule class's constraints, then the target's `exec_compatible_with`
    /// (an ImmutableSet).
    fn default_exec_constraints(&self, rule: &Rule) -> Result<Vec<Label>, String> {
        let Some(custom) = &rule.custom else {
            return Ok(Vec::new());
        };
        let mut labels = Vec::new();
        for text in &custom.declaration.exec_compatible_with {
            labels.push(Label::parse_canonical(text)?);
        }
        for text in custom
            .declaration
            .builtin
            .target
            .labels("exec_compatible_with")
        {
            labels.push(self.resolve_rule_src(rule, &text)?);
        }
        let mut unique = Vec::new();
        for label in labels {
            if !unique.contains(&label) {
                unique.push(label);
            }
        }
        Ok(unique)
    }

    /// RuleClass.AutoExecGroupsMode.DYNAMIC.isEnabled (packages/RuleClass.java:179-203)
    /// for a Starlark rule: its `_use_auto_exec_groups` attribute, else
    /// `--incompatible_auto_exec_groups`.
    fn use_auto_exec_groups(&self, rule: &Rule, config: ConfigId) -> Result<bool, String> {
        let Some(custom) = &rule.custom else {
            return Ok(false);
        };
        match custom.attrs.get("_use_auto_exec_groups") {
            None => Ok(self.configuration(config)
                .native
                .incompatible_auto_exec_groups),
            Some(crate::custom_rule::AttrValue::Bool(value)) => Ok(*value),
            Some(_) => Err(located(
                rule.location.as_deref(),
                "a non-boolean '_use_auto_exec_groups' attribute: Bazel 9.3 reads it as a boolean (RuleClass.AutoExecGroupsMode.isEnabled) and fails; not supported by the native profile".to_owned(),
            )),
        }
    }

    /// ToolchainContextUtil.getUnloadedToolchainContextsInputs
    /// (skyframe/toolchains/ToolchainContextUtil.java:42-93) for the exec
    /// groups: RuleClass.AutoExecGroupsMode.DYNAMIC (packages/RuleClass.java:179-203:
    /// a `_use_auto_exec_groups` attribute, else
    /// `--incompatible_auto_exec_groups`), the per-group
    /// `exec_group_compatible_with` constraints (:169-245, an
    /// InvalidExecGroupException at the target's location,
    /// skyframe/DependencyResolver.java:627-636) and DeclaredExecGroup.process
    /// (packages/DeclaredExecGroup.java:94-150): the declared groups (the
    /// default `test` group of a test rule that declares none,
    /// StarlarkRuleClassFunctions.java:1066-1082), then one automatic group
    /// per rule toolchain type, named by its Label.toString().
    fn exec_group_inputs(
        &self,
        rule: &Rule,
        config: ConfigId,
        with_platform_configuration: bool,
    ) -> Result<(bool, Vec<crate::exec_groups::ProcessedGroup>), String> {
        use crate::exec_groups::{ProcessedGroup, TEST_EXEC_GROUP_NAME, invalid_exec_group};
        let Some(custom) = &rule.custom else {
            return Ok((false, Vec::new()));
        };
        let use_auto = self.use_auto_exec_groups(rule, config)?;
        let default_types = requirements(&custom.declaration.toolchain_types)?;
        let default_exec = self.default_exec_constraints(rule)?;
        // The rule class's declared groups: rule(exec_groups), then the
        // default test group.
        let mut declared: Vec<(String, Vec<ToolchainTypeRequirement>, Vec<Label>, bool)> =
            Vec::new();
        for group in &custom.declaration.exec_groups {
            let mut constraints = Vec::new();
            for text in &group.exec_compatible_with {
                constraints.push(Label::parse_canonical(text)?);
            }
            declared.push((
                group.name.clone(),
                requirements(&group.toolchains)?,
                constraints,
                false,
            ));
        }
        if custom.declaration.builtin.kind.test
            && !declared
                .iter()
                .any(|(name, ..)| name == TEST_EXEC_GROUP_NAME)
        {
            declared.push((
                TEST_EXEC_GROUP_NAME.to_owned(),
                crate::toolchains::default_test_exec_group_key().types,
                Vec::new(),
                !self.repositories().has_builtin_modules(),
            ));
        }
        // getPerExecGroupExecutionPlatformConstraints (:169-245).
        let mut extra: Vec<(String, Vec<Label>)> = Vec::new();
        let mut extra_auto: Vec<(Label, Vec<Label>)> = Vec::new();
        let entries = match custom
            .declaration
            .builtin
            .target
            .attrs
            .get("exec_group_compatible_with")
        {
            Some(crate::custom_rule::AttrValue::StringListDict(entries))
                if with_platform_configuration =>
            {
                entries.clone()
            }
            _ => Vec::new(),
        };
        let target = crate::rule_declarations::canonical_form(&rule.label);
        let invalid = |key: &str, valid: &[&str]| {
            located(
                rule.location.as_deref(),
                invalid_exec_group("execution constraints", &target, &[key], valid),
            )
        };
        for (key, texts) in entries {
            let mut labels = Vec::new();
            for text in &texts {
                labels.push(self.resolve_rule_src(rule, text)?);
            }
            if crate::exec_groups::is_valid_group_name(&key) {
                if !declared.iter().any(|(name, ..)| *name == key) {
                    let names = declared
                        .iter()
                        .map(|(name, ..)| name.as_str())
                        .collect::<Vec<_>>();
                    return Err(invalid(&key, &names));
                }
                match extra.iter_mut().find(|(name, _)| *name == key) {
                    Some((_, existing)) => existing.extend(labels),
                    None => extra.push((key, labels)),
                }
            } else if use_auto {
                let Ok(label) =
                    self.repositories()
                        .parse(&key, &rule.label.repository, &rule.label.package)
                else {
                    return Err(invalid(&key, &[]));
                };
                if !default_types
                    .iter()
                    .any(|requirement| requirement.label == label)
                {
                    // Display forms of the types, for a main-repository rule
                    // only (:218-226).
                    let suggestions = if rule.label.repository.is_empty() {
                        default_types
                            .iter()
                            .map(|requirement| self.display_form(&requirement.label))
                            .collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    };
                    let names = suggestions.iter().map(String::as_str).collect::<Vec<_>>();
                    return Err(invalid(&key, &names));
                }
                match extra_auto.iter_mut().find(|(seen, _)| *seen == label) {
                    Some((_, existing)) => existing.extend(labels),
                    None => extra_auto.push((label, labels)),
                }
            } else {
                return Err(invalid(&key, &[]));
            }
        }
        let union = |base: &[Label], more: Option<&Vec<Label>>| {
            let mut labels = base.to_vec();
            for label in more.into_iter().flatten() {
                if !labels.contains(label) {
                    labels.push(label.clone());
                }
            }
            labels
        };
        let mut processed = Vec::new();
        for (name, types, constraints, builtin_only) in declared {
            let more = extra
                .iter()
                .find(|(group, _)| *group == name)
                .map(|(_, labels)| labels);
            processed.push(ProcessedGroup {
                key: ToolchainContextKey {
                    types,
                    exec_constraints: union(&constraints, more),
                    forced_exec: None,
                },
                name,
                builtin_only,
            });
        }
        if use_auto {
            for requirement in default_types {
                let more = extra_auto
                    .iter()
                    .find(|(label, _)| *label == requirement.label)
                    .map(|(_, labels)| labels);
                processed.push(ProcessedGroup {
                    name: requirement.label.to_string(),
                    key: ToolchainContextKey {
                        exec_constraints: union(&default_exec, more),
                        types: vec![requirement],
                        forced_exec: None,
                    },
                    builtin_only: false,
                });
            }
        }
        Ok((use_auto, processed))
    }

    /// ToolchainResolutionFunction.compute (skyframe/toolchains/ToolchainResolutionFunction.java:62-131)
    /// for a toolchain-resolving rule: PlatformKeys (the target platform,
    /// then the host platform as the only execution platform, filtered by
    /// the rule's execution constraints; PlatformKeys.java:78-224), the
    /// toolchain types (ToolchainTypeLookupUtil.java:44-170), the registered
    /// toolchains and the per-type resolution. The default host platform is
    /// resolved only when the rule has toolchain types or execution
    /// constraints (docs/TOOLCHAINS.md); a non-default `--host_platform` is
    /// always validated.
    fn resolve_toolchains(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        key: ToolchainContextKey,
    ) -> Result<Option<UnloadedToolchainContext>, String> {
        let _shared = incremental::SharedScope::enter("resolve_toolchains");
        let configuration = self.configuration(config).clone();
        let host = configuration.native.host_platform.clone();
        let target_label = configuration.native.target_platform().clone();
        let default_host = host == crate::configuration::default_host_platform();
        // `--extra_execution_platforms` and the modules'
        // `register_execution_platforms`: the registered execution platforms
        // come before the host platform (PlatformKeys.findExecutionPlatformKeys,
        // :106-133), so a rule without toolchain types may run on one of
        // them; resolved first, like Bazel.
        let extra = !configuration.native.extra_execution_platforms.is_empty()
            || !self
                .module_execution_platforms(key.needs_platforms())?
                .is_empty();
        if !key.needs_platforms() && !extra {
            if default_host {
                return Ok(None);
            }
            let host_info = self
                .platform_info(&host, config)
                .map_err(|error| self.platform_resolution_error(rule, config, &host, error))?;
            // PlatformKeys.isPlatformSuitable (:265-275): without toolchain
            // types a platform that checks toolchain types is unsuitable,
            // unless forced.
            if host_info.check_toolchain_types && key.forced_exec.is_none() {
                let target = self.platform_info(&target_label, config).map_err(|error| {
                    self.platform_resolution_error(rule, config, &target_label, error)
                })?;
                return Err(self.toolchain_error(
                    rule,
                    config,
                    "",
                    &crate::toolchains::no_matching_platform(&[], &target.label, &[host_info]),
                ));
            }
            return Ok(None);
        }
        let registered_platforms = if extra {
            self.execution_platforms(rule, config, &key)?
        } else {
            Arc::new(Vec::new())
        };
        let target = self
            .platform_info(&target_label, config)
            .map_err(|error| self.platform_resolution_error(rule, config, &target_label, error))?;
        let host_info = self
            .platform_info(&host, config)
            .map_err(|error| self.platform_resolution_error(rule, config, &host, error))?;
        // PlatformKeys.build (:66-104): the registered execution platforms,
        // then the host platform unless it is one of them (a LinkedHashSet
        // of the alias-resolved keys).
        let mut candidates = registered_platforms.as_ref().clone();
        if !candidates
            .iter()
            .any(|platform| platform.label == host_info.label)
        {
            candidates.push(host_info.clone());
        }
        // Phase-1 prefetch: from here on, the implementation of every
        // toolchain compatible with these platforms is queued as soon as
        // the toolchain is analyzed (`prefetch_compatible_toolchain`).
        self.prefetch_platforms(&target, &host_info);
        // PlatformKeys.filterExecutionPlatforms (:168-205).
        let mut constraints = Vec::new();
        for label in &key.exec_constraints {
            let value = self
                .exec_constraint(rule, label, config)
                .map_err(|error| self.resolution_error(rule, config, error))?;
            constraints.push(value);
        }
        let required = constraints.iter().collect::<Vec<_>>();
        // ToolchainResolutionDebugPrinter.reportRemovedExecutionPlatform
        // (PlatformKeys.filterPlatform :207-228) under
        // --toolchain_resolution_debug.
        let debug = crate::toolchains::debug_resolution(&[&rule.label])
            || crate::toolchains::debug_resolution(
                &key.types
                    .iter()
                    .map(|requirement| &requirement.label)
                    .collect::<Vec<_>>(),
            );
        let exec = candidates
            .into_iter()
            .filter(|platform| {
                let missing = required
                    .iter()
                    .filter(|value| {
                        platform.constraints.get(&value.setting).as_ref() != Some(**value)
                    })
                    .collect::<Vec<_>>();
                if debug {
                    for value in &missing {
                        crate::toolchains::removed_execution_platform(
                            &platform.label,
                            &value.label,
                        );
                    }
                }
                missing.is_empty()
            })
            .collect::<Vec<_>>();
        let mut lookups = Vec::new();
        for requirement in &key.types {
            let info = self
                .toolchain_type_info(rule, requirement, config)
                .map_err(|error| self.resolution_error(rule, config, error))?;
            lookups.push((requirement.clone(), info));
        }
        let types = crate::toolchains::resolved_types(&lookups);
        let requested = crate::toolchains::requested_types(&lookups);
        // SingleToolchainResolutionFunction reads RegisteredToolchainsValue
        // only for a type to resolve (:72-84).
        let registered = if types.is_empty() {
            Arc::new(RegisteredToolchains::default())
        } else {
            if self.registered_toolchains_in_progress(config) {
                return Err(self.toolchain_cycle(config, &key, &types[0].requirement.label));
            }
            match self.registered_toolchains(config) {
                // The evaluator: a back edge to the registration.
                Err(ResolutionError::Propagated(text)) if text == REGISTRATION_IN_PROGRESS => {
                    return Err(self.toolchain_cycle(config, &key, &types[0].requirement.label));
                }
                result => result.map_err(|error| self.resolution_error(rule, config, error))?,
            }
        };
        // PlatformKeys.find (:230-245): the forced platform is the target
        // platform or one of the execution platforms.
        let forced = key.forced_exec.as_ref().and_then(|forced| {
            if *forced == target.label {
                Some(target.clone())
            } else {
                exec.iter().find(|exec| exec.label == *forced).cloned()
            }
        });
        let resolved = crate::toolchains::resolve(
            &types,
            requested,
            crate::toolchains::Platforms {
                target: &target,
                exec: &exec,
                forced,
                debug_target: crate::toolchains::debug_resolution(&[&rule.label]),
            },
            &registered,
        )
        .map_err(|message| self.toolchain_error(rule, config, "", &message))?;
        // A context that needed no platform and runs on the host platform
        // stays the lazy host context (`None`, as without
        // `--extra_execution_platforms`).
        if !key.needs_platforms() && resolved.exec_platform.label == host_info.label {
            return Ok(None);
        }
        // Phase-1 prefetch: the selected implementations are analyzed after
        // the rule's attribute dependencies.
        self.prefetch_resolved(&resolved);
        Ok(Some(resolved))
    }

    /// RegisteredToolchainsCycleReporter.maybeReportCycle
    /// (skyframe/toolchains/RegisteredToolchainsCycleReporter.java:61-139)
    /// with AbstractLabelCycleReporter.printCycle (:134-170): a registered
    /// toolchain target whose analysis (`active`, from the registered target
    /// to the rule) needs the registered toolchains. The cycle is
    /// RegisteredToolchains, the configured targets, the rule's toolchain
    /// context key (`toolchain types ...`) and its first type's single
    /// resolution (`toolchain type T`); reported without location.
    fn toolchain_cycle(
        &mut self,
        config: ConfigId,
        key: &ToolchainContextKey,
        first_type: &Label,
    ) -> String {
        let shown = crate::rule_declarations::canonical_form;
        let tail = format!(
            "\n|   toolchain types {}\n|   toolchain type {}\n`-- RegisteredToolchains",
            key.types
                .iter()
                .map(|requirement| shown(&requirement.label))
                .collect::<Vec<_>>()
                .join(", "),
            shown(first_type)
        );
        // The evaluator builds the chain bottom-up (linearize::chains).
        if let Some(open) = self.open_cycle_report(true, &tail) {
            return open;
        }
        let active = self.active_keys();
        let head = active
            .first()
            .map(|key| shown(&key.label))
            .unwrap_or_default();
        let mut report = format!(
            "Misconfigured toolchains: {head} is declared as a toolchain but has inappropriate dependencies. Declared toolchains should be created with the 'toolchain' rule and should not have dependencies that themselves require toolchains.\n.-> RegisteredToolchains"
        );
        for element in &active {
            report.push_str("\n|   ");
            report.push_str(&shown(&element.label));
        }
        report.push_str(&tail);
        self.record_toolchain_cycle_report(config, report.clone());
        report
    }

    /// A target loaded for toolchain resolution: a loading error is the
    /// NoSuchThingException itself (`com.google...NoSuchTargetException:
    /// ...`, ToolchainTypeLookupUtil.java:139-144,
    /// ConstraintValueLookupUtil.java:101-105), preceded by the loading event
    /// of another package's BUILD file.
    fn resolution_target(&mut self, rule: &Rule, label: &Label) -> Result<(), ResolutionError> {
        // An unresolvable apparent repository (crate::toolchains::deferred_label_text):
        // the BuildFileNotFoundException of its package, also reported as an
        // event without location (paired capture B4/resolution_errors/unknown_repo).
        if let Some(message) = crate::label::unresolvable_package_message(&label.package_id()) {
            return Err(ResolutionError::Reported(
                message.clone(),
                format!(
                    "com.google.devtools.build.lib.packages.BuildFileNotFoundException: {message}"
                ),
            ));
        }
        self.target(label).map(|_| ()).map_err(|error| {
            // The loading event is reported for the rule's own package too
            // (paired capture A5/resolution_errors/missing_type).
            let _ = rule;
            let event = self.target_loading_event(label, &error);
            let message = PlatformError::loading(error).target_text();
            match event {
                Some(event) => ResolutionError::Reported(event, message),
                None => ResolutionError::Toolchain(message),
            }
        })
    }

    /// ConstraintValueLookupUtil.findConstraintValueInfo
    /// (skyframe/toolchains/ConstraintValueLookupUtil.java:62-92).
    fn exec_constraint(
        &mut self,
        rule: &Rule,
        label: &Label,
        config: ConfigId,
    ) -> Result<ConstraintValueInfo, ResolutionError> {
        self.resolution_target(rule, label)?;
        self.analyze(label, config)
            .map_err(ResolutionError::Propagated)?;
        let node = self.configured_node(&ConfiguredTargetKey::new(label.clone(), config));
        node.as_ref()
            .and_then(|node| node.constraint_value.clone())
            .ok_or_else(|| {
                ResolutionError::Toolchain(crate::toolchains::not_a_constraint_value(
                    &self.alias_chain(config, label).1,
                ))
            })
    }

    /// ToolchainTypeLookupUtil.findToolchainTypeInfo
    /// (skyframe/toolchains/ToolchainTypeLookupUtil.java:97-148): the type
    /// target is analyzed in the rule's configuration; `None` for an invalid
    /// type that is ignored.
    fn toolchain_type_info(
        &mut self,
        rule: &Rule,
        requirement: &ToolchainTypeRequirement,
        config: ConfigId,
    ) -> Result<Option<ToolchainTypeInfo>, ResolutionError> {
        let label = &requirement.label;
        self.resolution_target(rule, label)?;
        self.analyze(label, config)
            .map_err(ResolutionError::Propagated)?;
        let node = self.configured_node(&ConfiguredTargetKey::new(label.clone(), config));
        if let Some(info) = node.as_ref().and_then(|node| node.toolchain_type.clone()) {
            return Ok(Some(info));
        }
        if requirement.ignore_if_invalid {
            return Ok(None);
        }
        let is_toolchain = node.is_some_and(|node| node.declared_toolchain.is_some());
        Err(ResolutionError::Toolchain(
            crate::toolchains::invalid_toolchain_type(
                &self.alias_chain(config, label).1,
                is_toolchain,
            ),
        ))
    }

    /// RegisteredExecutionPlatformsFunction.compute
    /// (skyframe/toolchains/RegisteredExecutionPlatformsFunction.java:72-157)
    /// for a configuration with `--extra_execution_platforms`, once per
    /// configuration: the platforms in registration order (alias-resolved
    /// PlatformInfo). Module registrations (`register_execution_platforms`)
    /// come after the configuration's (Self::module_execution_platforms).
    /// A registered target that needs toolchain resolution itself is
    /// RegisteredExecutionPlatformsCycleReporter's cycle.
    fn execution_platforms(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        key: &ToolchainContextKey,
    ) -> Result<ExecutionPlatforms, String> {
        let _shared = incremental::SharedScope::enter("execution_platforms");
        if self.execution_platforms_in_progress(config) {
            return Err(self.execution_platform_cycle(config, key));
        }
        match self.registered_execution_platforms(config, &rule.label) {
            // The evaluator: a back edge to the registration.
            Err(ResolutionError::Propagated(text)) if text == REGISTRATION_IN_PROGRESS => {
                Err(self.execution_platform_cycle(config, key))
            }
            result => result.map_err(|error| self.resolution_error(rule, config, error)),
        }
    }

    fn compute_execution_platforms(
        &mut self,
        config: ConfigId,
    ) -> Result<ExecutionPlatforms, ResolutionError> {
        // The configuration's platforms, parsed with the main repository
        // mapping (:86-99), then the modules' registrations, each in its
        // module's repository (:107-111, 165-189).
        let mut patterns = self
            .configuration(config)
            .native
            .extra_execution_platforms
            .iter()
            .map(|pattern| (String::new(), pattern.clone()))
            .collect::<Vec<_>>();
        patterns.extend(
            self.module_execution_platforms(true)
                .map_err(ResolutionError::Propagated)?,
        );
        let labels =
            self.expand_registration_patterns(&patterns, Registration::ExecutionPlatform)?;
        // configureRegisteredExecutionPlatforms (:193-243): each target in
        // the configuration; its key is the alias-resolved label.
        let mut platforms: Vec<(Label, Arc<PlatformInfo>)> = Vec::new();
        let mut cycle = None;
        for label in labels {
            let info = match self.target(&label) {
                Ok(Target::Rule(rule)) if rule.label == label && has_platform_info(&rule) => self
                    .platform_info(&label, config)
                    .map_err(|error| platform_resolution_failure(&label, error)),
                // Any other target is configured like a dependency: one
                // that resolves toolchains needs these platforms (a cycle);
                // otherwise it lacks PlatformInfo (InvalidPlatformException,
                // after its own errors).
                _ => match self.analyze_detached(&label, config) {
                    Ok(()) => self
                        .configured_node(&ConfiguredTargetKey::new(label.clone(), config))
                        .and_then(|node| node.platform.clone())
                        .ok_or_else(|| {
                            ResolutionError::Toolchain(crate::platforms::not_a_platform(&label))
                        }),
                    Err(error) => match self.take_execution_platform_cycle_report(config) {
                        Some(report) if error.contains(&report) => {
                            cycle = Some(report);
                            continue;
                        }
                        _ => Err(ResolutionError::Reported(
                            error,
                            crate::platforms::not_a_platform(&label),
                        )),
                    },
                },
            };
            let info = info?;
            if let Some((first, _)) = platforms.iter().find(|(_, seen)| seen.label == info.label) {
                return Err(ResolutionError::Propagated(format!(
                    "the registered execution platforms {} and {} are the same platform {}: Bazel 9.3 crashes on it (IllegalArgumentException: Multiple entries with same key, ImmutableMap.Builder.buildOrThrow in RegisteredExecutionPlatformsFunction.configureRegisteredExecutionPlatforms, skyframe/toolchains/RegisteredExecutionPlatformsFunction.java:243), which is not implemented in the native profile",
                    crate::rule_declarations::canonical_form(first),
                    crate::rule_declarations::canonical_form(&label),
                    crate::rule_declarations::canonical_form(&info.label),
                )));
            }
            platforms.push((label, info));
        }
        if let Some(report) = cycle {
            return Err(ResolutionError::Propagated(report));
        }
        Ok(Arc::new(
            platforms.into_iter().map(|(_, info)| info).collect(),
        ))
    }

    /// RegisteredExecutionPlatformsCycleReporter.maybeReportCycle
    /// (skyframe/toolchains/RegisteredExecutionPlatformsCycleReporter.java:55-104)
    /// with AbstractLabelCycleReporter.printCycle: a registered execution
    /// platform target whose analysis (`active`, from the registered target
    /// to the rule) needs the configuration's execution platforms. The
    /// cycle is RegisteredExecutionPlatforms, the configured targets and the
    /// rule's toolchain context key (`toolchain types ...`); reported
    /// without location.
    fn execution_platform_cycle(&mut self, config: ConfigId, key: &ToolchainContextKey) -> String {
        let shown = crate::rule_declarations::canonical_form;
        let tail = format!(
            "\n|   toolchain types {}\n`-- RegisteredExecutionPlatforms",
            key.types
                .iter()
                .map(|requirement| shown(&requirement.label))
                .collect::<Vec<_>>()
                .join(", ")
        );
        // The evaluator builds the chain bottom-up (linearize::chains).
        if let Some(open) = self.open_cycle_report(false, &tail) {
            return open;
        }
        let active = self.active_keys();
        let head = active
            .first()
            .map(|key| shown(&key.label))
            .unwrap_or_default();
        let mut report = format!(
            "Misconfigured execution platforms: {head} is declared as a platform but has inappropriate dependencies. Execution platforms should not have dependencies that themselves require toolchain resolution.\n.-> RegisteredExecutionPlatforms"
        );
        for element in &active {
            report.push_str("\n|   ");
            report.push_str(&shown(&element.label));
        }
        report.push_str(&tail);
        self.record_execution_platform_cycle_report(config, report.clone());
        report
    }

    fn compute_registered_toolchains(
        &mut self,
        config: ConfigId,
    ) -> Result<Arc<RegisteredToolchains>, ResolutionError> {
        // --extra_toolchains, reversed so the last one wins, parsed with the
        // main repository mapping (:82-93); then the modules of the
        // dependency graph (:96-104, 149-172).
        let mut patterns = self
            .configuration(config)
            .native
            .extra_toolchains
            .iter()
            .rev()
            .map(|pattern| (String::new(), pattern.clone()))
            .collect::<Vec<_>>();
        for (repository, labels) in self
            .repositories()
            .toolchain_registrations()
            .map_err(ResolutionError::Propagated)?
        {
            patterns.extend(labels.into_iter().map(|label| (repository.clone(), label)));
        }
        self.warm_toolchain_patterns(&patterns);
        let labels = self.expand_toolchain_patterns(&patterns)?;
        // Analyzed one after another below: on the pool all at once.
        let hinted: Vec<(Label, ConfigId)> =
            labels.iter().map(|label| (label.clone(), config)).collect();
        self.hint_edges(&hinted);
        // configureRegisteredToolchains (:174-215).
        let mut declared = Vec::new();
        // Of several registered targets in a toolchain cycle, Bazel reports
        // the last one (paired capture B4/resolution_errors/extra_cyc_all_o:
        // `//cyc:all` names //cyc:type_is_user, not //cyc:exec_user).
        let mut cycle = None;
        let mut analyzed = Vec::new();
        for label in labels {
            self.set_computing_toolchain(&label);
            match self.analyze_detached(&label, config) {
                Ok(()) => analyzed.push(label),
                Err(error) => match self.take_toolchain_cycle_report(config) {
                    Some(report) if error.contains(&report) => cycle = Some(report),
                    _ => return Err(ResolutionError::Propagated(error)),
                },
            }
        }
        if let Some(report) = cycle {
            return Err(ResolutionError::Propagated(report));
        }
        for label in analyzed {
            let info = self
                .configured_node(&ConfiguredTargetKey::new(label.clone(), config))
                .and_then(|node| node.declared_toolchain.clone())
                .ok_or_else(|| {
                    ResolutionError::Toolchain(crate::toolchains::invalid_registered_toolchain(
                        &crate::rule_declarations::canonical_form(&label),
                        "target does not provide the DeclaredToolchainInfo provider",
                    ))
                })?;
            declared.push(info);
        }
        // ConfigMatchingUtil.validate per toolchain (:120-141).
        let mut toolchains = Vec::new();
        for info in declared {
            match crate::toolchains::target_settings_match(&info.target_label, &info.settings) {
                Ok(true) => toolchains.push(info),
                Ok(false) => {}
                Err(message) => {
                    return Err(ResolutionError::Toolchain(
                        crate::toolchains::invalid_registered_toolchain(
                            &crate::rule_declarations::canonical_form(&info.target_label),
                            &message,
                        ),
                    ));
                }
            }
        }
        Ok(Arc::new(RegisteredToolchains { toolchains }))
    }

    /// TargetPatternUtil.expandTargetPatterns (skyframe/TargetPatternUtil.java:44-87)
    /// with FilteringPolicies.ruleTypeExplicit("toolchain")
    /// (pkgcache/FilteringPolicies.java:45-47): patterns in order, the first
    /// copy of a label kept; wildcards keep only `toolchain` rules (manual
    /// tags are not filtered), explicit labels are kept whatever they are.
    /// `:all` lists a package's targets by name; `/...` visits packages in
    /// the RecursivePkgFunction order (sorted directory entries, each
    /// directory's subpackages before itself: a STABLE_ORDER nested set
    /// iterates postorder, collect/nestedset/NestedSet.java:190-230,
    /// skyframe/RecursivePkgFunction.java:88-116).
    fn expand_toolchain_patterns(
        &mut self,
        patterns: &[(String, String)],
    ) -> Result<Vec<Label>, ResolutionError> {
        self.expand_registration_patterns(patterns, Registration::Toolchain)
    }

    /// ConfigFeatureFlag.create (rules/config/ConfigFeatureFlag.java:109-169)
    /// after RuleContext's checks: `allowed_values` must be non empty
    /// (RuleContext.checkAttributesNonEmpty, analysis/RuleContext.java:1531-1549),
    /// then the `$allowlist_config_feature_flag` package group
    /// (`@bazel_tools//tools/allowlists/config_feature_flag:config_feature_flag`,
    /// ConfigFeatureFlag.java:56-64, ConfigRuleClasses.java:459-466) must contain
    /// the rule's package (Allowlist.isAvailableFor, analysis/Allowlist.java:88-99).
    fn config_feature_flag_provider(
        &mut self,
        rule: &Rule,
    ) -> Result<Option<crate::feature_flags::FeatureFlagValue>, String> {
        let (RuleKind::Native("config_feature_flag"), Some(native)) = (&rule.kind, &rule.native)
        else {
            return Ok(None);
        };
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        let allowed = native.strings("allowed_values");
        if allowed.is_empty() {
            return Err(located(
                rule.location.as_deref(),
                format!(
                    "in allowed_values attribute of config_feature_flag rule {shown}: attribute must be non empty"
                ),
            ));
        }
        let allowlist = Label::parse_canonical(
            "@@bazel_tools//tools/allowlists/config_feature_flag:config_feature_flag",
        )?;
        if !self.repositories().has_builtin_modules() {
            return Err(located(
                rule.location.as_deref(),
                format!(
                    "config_feature_flag rule {shown}: its allowlist {} needs the built-in @bazel_tools repository, which the explicit-module profile (--experimental-builtin-modules=false) does not provide; drop --experimental-builtin-modules=false",
                    crate::rule_declarations::canonical_form(&allowlist)
                ),
            ));
        }
        let group = self.resolve_group(&allowlist)?;
        if !group.contains(&rule.label.package_id()) {
            // AllowlistChecker (ConfigRuleClasses.java:459-466); the genuine
            // embedded group is `packages = ["public"]`.
            return Err(located(
                rule.location.as_deref(),
                format!(
                    "in config_feature_flag rule {shown}: the config_feature_flag rule is not available in this package"
                ),
            ));
        }
        crate::feature_flags::analyze(
            &shown,
            &allowed,
            native
                .explicit("default_value")
                .then(|| native.string("default_value").unwrap_or_default()),
        )
        .map(Some)
        .map_err(|errors| {
            errors
                .into_iter()
                .map(|error| located(rule.location.as_deref(), error))
                .collect::<Vec<_>>()
                .join("\n")
        })
    }

    /// ToolchainType.create (rules/ToolchainType.java:40-52) and
    /// Toolchain.create (rules/platform/Toolchain.java:42-110) from the
    /// validated prerequisites (crate::toolchains).
    fn toolchain_rule_providers(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        prerequisites: &[Vec<Label>],
    ) -> Result<
        (
            Option<ToolchainTypeInfo>,
            Option<Arc<DeclaredToolchainInfo>>,
        ),
        String,
    > {
        let Some(native) = &rule.native else {
            return Ok((None, None));
        };
        match rule.kind {
            RuleKind::Native("toolchain_type") => Ok((
                Some(ToolchainTypeInfo::new(
                    rule.label.clone(),
                    native.string("no_match_error").unwrap_or_default(),
                )),
                None,
            )),
            RuleKind::Native("toolchain") => {
                let mut nodes: BTreeMap<Label, ConfiguredNode> = BTreeMap::new();
                for label in prerequisites.iter().flatten() {
                    if let Some(found) =
                        self.configured_node(&ConfiguredTargetKey::new(label.clone(), config))
                    {
                        nodes.insert(label.clone(), found);
                    }
                }
                let node = |label: &Label| nodes.get(label);
                let internal = |what: &str| {
                    format!(
                        "internal error: validated prerequisite of {} lacks {what}",
                        rule.label
                    )
                };
                let provided = |index: usize| prerequisites.get(index).cloned().unwrap_or_default();
                let toolchain_type = provided(0)
                    .first()
                    .and_then(node)
                    .and_then(|node| node.toolchain_type.clone())
                    .ok_or_else(|| internal("ToolchainTypeInfo"))?;
                let constraints = |labels: Vec<Label>| {
                    labels
                        .iter()
                        .map(|label| {
                            node(label)
                                .and_then(|node| node.constraint_value.clone())
                                .ok_or_else(|| internal("ConstraintValueInfo"))
                        })
                        .collect::<Result<Vec<_>, _>>()
                };
                let exec_compatible_with = constraints(provided(1))?;
                let target_compatible_with = constraints(provided(2))?;
                let target_settings = provided(3)
                    .iter()
                    .map(|label| {
                        node(label)
                            .and_then(|node| node.matching.clone())
                            .ok_or_else(|| internal("ConfigMatchingProvider"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let Some(crate::custom_rule::AttrValue::Label(Some(toolchain))) =
                    native.attrs.get("toolchain")
                else {
                    return Err(internal("a toolchain attribute"));
                };
                let toolchain = self.resolve_rule_src(rule, toolchain)?;
                let shown = crate::rule_declarations::canonical_form(&rule.label);
                let declared = crate::toolchains::declare_toolchain(
                    &rule.label,
                    crate::toolchains::ToolchainAttributes {
                        toolchain_type,
                        exec_compatible_with,
                        target_compatible_with,
                        use_target_platform_constraints: matches!(
                            native.attrs.get("use_target_platform_constraints"),
                            Some(crate::custom_rule::AttrValue::Bool(true))
                        ),
                        target_settings,
                        toolchain,
                    },
                )
                .map_err(|errors| {
                    errors
                        .iter()
                        .map(|error| {
                            located(
                                rule.location.as_deref(),
                                format!(
                                    "in {} attribute of toolchain rule {shown}: {}",
                                    error.attribute, error.message
                                ),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })?;
                Ok((None, Some(Arc::new(declared))))
            }
            _ => Ok((None, None)),
        }
    }

    fn compute_platform_info(
        &mut self,
        label: &Label,
        config: ConfigId,
    ) -> Result<Arc<PlatformInfo>, PlatformError> {
        let shown = crate::rule_declarations::canonical_form(label);
        // An apparent repository the main repository does not see stays an
        // unresolvable label (`@@[unknown repo 'R' requested from @@]`,
        // crate::configuration) until the package is looked up.
        if let Some(message) = crate::label::unresolvable_package_message(&label.package_id()) {
            return Err(PlatformError::Missing {
                class: "BuildFileNotFoundException",
                message,
            });
        }
        // The default --host_platform is @bazel_tools//tools:host_platform
        // (PlatformOptions.java:46-56), an alias of @platforms//host whose
        // constraints the platforms module's host_platform extension computes
        // from the host (docs/PLATFORMS.md). Without the built-in bazel_tools
        // repository there is nothing to resolve.
        if label.repository == "bazel_tools"
            && let Err(error) = self.repositories().root("bazel_tools")
        {
            return Err(PlatformError::Other(format!(
                "the target platform {shown} (the default host platform) needs the built-in @bazel_tools repository, which the explicit-module profile (--experimental-builtin-modules=false) does not provide ({error}); drop --experimental-builtin-modules=false, or select a platform with --platforms"
            )));
        }
        // PlatformLookupUtil.hasPlatformInfo before analysis (:160-172);
        // package and target errors propagate (InvalidPlatformException(NoSuchThingException)).
        let rule = match self.target(label) {
            Err(error) => return Err(PlatformError::loading(error)),
            Ok(Target::Rule(rule) | Target::Output(rule, _)) => rule,
            Ok(Target::Source(_)) => {
                return Err(PlatformError::Other(crate::platforms::not_a_platform(
                    label,
                )));
            }
        };
        if rule.label != *label || !has_platform_info(&rule) {
            return Err(PlatformError::Other(crate::platforms::not_a_platform(
                label,
            )));
        }
        self.check_platform_tree(label)?;
        self.analyze_detached(label, config).map_err(|error| {
            PlatformError::Other(format!(
                "{error}\n{}",
                crate::platforms::not_a_platform(label)
            ))
        })?;
        let platform = self
            .configured_node(&ConfiguredTargetKey::new(label.clone(), config))
            .and_then(|node| node.platform.clone())
            .ok_or_else(|| {
                PlatformError::Other(crate::platforms::not_a_platform(
                    &self.alias_chain(config, label).1,
                ))
            })?;
        // PlatformFunction parses PlatformInfo.flags into the configuration
        // (:102-125, ParsedFlagsValue): not modelled.
        if !platform.flags.is_empty() {
            return Err(PlatformError::Other(format!(
                "platform {shown} sets flags [{}]: platform-based flags (platform(flags = ...), analysis/platform/PlatformFunction.java:102-125) are not implemented in the native profile",
                platform.flags.join(", ")
            )));
        }
        Ok(platform)
    }

    /// The alias chain from `label` through plain (select()-free) aliases,
    /// stopping at a label setting (no actual in the empty configuration):
    /// the aliases passed and the end.
    fn static_alias_chain(&mut self, label: &Label) -> (Vec<Label>, Label) {
        let mut chain = Vec::new();
        let mut current = label.clone();
        loop {
            let Ok(Target::Rule(rule)) = self.target(&current) else {
                break;
            };
            if !matches!(rule.kind, RuleKind::Native("alias")) || chain.contains(&current) {
                break;
            }
            let Some(crate::custom_rule::AttrValue::Label(Some(actual))) = rule
                .native
                .as_ref()
                .and_then(|native| native.attrs.get("actual"))
            else {
                break;
            };
            let Ok(actual) = self.resolve_rule_src(&rule, actual) else {
                break;
            };
            chain.push(current);
            current = actual;
        }
        (chain, current)
    }

    /// Bazel analyzes a platform and its whole dependency tree in
    /// `CommonOptions.EMPTY_OPTIONS` (PlatformFunction.configuredTargetDep,
    /// analysis/platform/PlatformFunction.java:46-52;
    /// analysis/config/CommonOptions.java:32-42: CoreOptions defaults only,
    /// no platform options and no Starlark build settings). The native
    /// profile analyzes in the build configuration, which gives the same
    /// result only when nothing in the tree depends on the configuration:
    ///
    /// * a `label_flag`/`label_setting` on the platform label's alias chain
    ///   resolves to no actual there, so it provides no PlatformInfo (Bazel
    ///   names it: `Target //:plat_flag was referenced as a platform, but
    ///   does not provide PlatformInfo`, paired capture B2 platforms*_plat_flag,
    ///   platforms*_fg_flag); one reached through a platform rule's attribute
    ///   fails that rule's prerequisite validation (Bazel's text);
    /// * select() keys anywhere in the tree, label settings reached otherwise
    ///   and `required_settings` naming config_settings (config_setting
    ///   matching, only used by execution-platform resolution, N17) fail
    ///   explicitly.
    fn check_platform_tree(&mut self, platform: &Label) -> Result<(), PlatformError> {
        let shown = crate::rule_declarations::canonical_form(platform);
        let mut current = platform.clone();
        let mut chain = BTreeSet::new();
        while chain.insert(current.clone()) {
            let Ok(Target::Rule(rule)) = self.target(&current) else {
                break;
            };
            match rule.kind {
                RuleKind::Native("label_flag" | "label_setting") => {
                    return Err(PlatformError::Other(crate::platforms::not_a_platform(
                        &current,
                    )));
                }
                RuleKind::Native("alias") => {
                    let Some(crate::custom_rule::AttrValue::Label(Some(actual))) = rule
                        .native
                        .as_ref()
                        .and_then(|native| native.attrs.get("actual"))
                    else {
                        break;
                    };
                    match self.resolve_rule_src(&rule, actual) {
                        Ok(actual) => current = actual,
                        Err(_) => break,
                    }
                }
                _ => break,
            }
        }
        let gap = |what: String| {
            PlatformError::Other(format!(
                "{what} in the dependency tree of platform {shown} is not implemented in the native profile: Bazel analyzes platforms and their dependencies in the empty configuration (analysis/platform/PlatformFunction.java:46-52, analysis/config/CommonOptions.java:32-42), which the native profile does not model"
            ))
        };
        let mut stack = vec![platform.clone()];
        let mut visited = BTreeSet::new();
        while let Some(label) = stack.pop() {
            if visited.len() >= 100_000 || !visited.insert(label.clone()) {
                continue;
            }
            let Ok(Target::Rule(rule)) = self.target(&label) else {
                continue;
            };
            let rule_shown = crate::rule_declarations::canonical_form(&rule.label);
            if !configured_attributes::rule_config_dependencies(&rule).is_empty() {
                return Err(gap(format!("{rule_shown}: select()")));
            }
            if let RuleKind::Native(kind @ ("label_flag" | "label_setting")) = rule.kind {
                return Err(gap(format!("{rule_shown}: {kind}")));
            }
            // A label setting reached through a platform rule's attribute
            // has no providers in the empty configuration: Bazel's
            // prerequisite error (paired capture A8/gap_flag_parent,
            // B8/target_cv_flag_platform), then InvalidPlatformException.
            let attributes = platform_prerequisite_attributes(&rule);
            let mut errors = Vec::new();
            let mut required_are_settings = true;
            for attribute in &attributes {
                for text in &attribute.labels {
                    let Ok(dependency) = self.resolve_rule_src(&rule, text) else {
                        continue;
                    };
                    let (chain, end) = self.static_alias_chain(&dependency);
                    let Ok(Target::Rule(end_rule)) = self.target(&end) else {
                        required_are_settings &= attribute.name != "required_settings";
                        continue;
                    };
                    if attribute.name == "required_settings" {
                        required_are_settings &=
                            matches!(end_rule.kind, RuleKind::Native("config_setting"));
                    }
                    if matches!(
                        end_rule.kind,
                        RuleKind::Native("label_flag" | "label_setting")
                    ) && let Some(error) =
                        rule_dependency_error(attribute, &chain, &end, &end_rule, None)
                    {
                        errors.push(located(
                            rule.location.as_deref(),
                            crate::rule_errors::RuleEvents {
                                rule_class: rule.rule_class_name(),
                                label: &rule_shown,
                            }
                            .attribute_message(attribute.name, &error),
                        ));
                    }
                }
            }
            if !errors.is_empty() {
                errors.push(crate::platforms::not_a_platform(platform));
                return Err(PlatformError::Other(errors.join("\n")));
            }
            // Non-config_setting required settings fail prerequisite
            // validation, which the analysis below reports like Bazel.
            if let (RuleKind::Native("platform"), Some(native)) = (&rule.kind, &rule.native)
                && !native.labels("required_settings").is_empty()
                && required_are_settings
            {
                return Err(gap(format!(
                    "{rule_shown}: required_settings (config_setting matching for execution platform resolution, N17)"
                )));
            }
            if let Some(native) = &rule.native {
                stack.extend(native.input_labels.iter().cloned());
            }
            if let Some(custom) = &rule.custom {
                stack.extend(
                    custom
                        .declaration
                        .builtin
                        .target
                        .input_labels
                        .iter()
                        .cloned(),
                );
                for text in crate::package::custom_dependency_labels(&custom.attrs) {
                    if let Ok(dependency) = self.resolve_rule_src(&rule, &text) {
                        stack.push(dependency);
                    }
                }
            }
        }
        Ok(())
    }

    /// IncompatibleTargetChecker.IncompatibleTargetProducer
    /// (analysis/constraints/IncompatibleTargetChecker.java:126-227). The
    /// configured `target_compatible_with` labels (the built-in attribute
    /// of Starlark rules and of native rules that are not exempt from
    /// constraint checking) are analyzed in the rule's own configuration;
    /// each must provide a ConstraintValueInfo or a ConfigMatchingProvider
    /// (the attribute's mandatory providers, BaseRuleClasses.java:331-337),
    /// and they are the rule's `ctx.attr.target_compatible_with`. For a rule
    /// that uses toolchain resolution (`checked`), with a known target
    /// platform: each constraint value must be the platform's value of its
    /// setting (ConstraintCollection.hasConstraintValue) and each
    /// config_setting must match; duplicate constraint settings
    /// (ConstraintCollection.validateConstraints) and config_setting errors
    /// are validation errors at the rule's location. An incompatible result
    /// carries sorted/deduplicated causes and replaces normal rule analysis.
    /// Mandatory-provider errors are deferred until after indirect replacement,
    /// just like RuleContext validation. Build/test root skip policy is a separate
    /// fail-closed boundary; cquery includes these results (TARGET_COMPATIBILITY.md).
    #[allow(clippy::too_many_arguments)]
    fn check_target_compatibility(
        &mut self,
        rule: &Rule,
        config: ConfigId,
        conditions: &[(String, ConfigMatching)],
        checked: bool,
    ) -> Result<compatibility::DirectCheck, String> {
        // The toolchain rule's own target_compatible_with is a toolchain
        // resolution input (platform_prerequisite_attributes).
        if matches!(rule.kind, RuleKind::Native("toolchain")) {
            return Ok(Default::default());
        }
        // Most targets do not set the attribute: no condition map for them.
        if !configured_attributes::has_native_value(rule, "target_compatible_with") {
            return Ok(Default::default());
        }
        let conditions = conditions.iter().cloned().collect::<BTreeMap<_, _>>();
        let resolver = configured_attributes::Resolver {
            target: &rule.label,
            conditions: &conditions,
            config_hash: config.to_string(),
        };
        let labels = match configured_attributes::configured_native_value(
            rule,
            &resolver,
            "target_compatible_with",
        )
        .map_err(|error| located(rule.location.as_deref(), error))?
        {
            Some(crate::custom_rule::AttrValue::Labels(labels)) if !labels.is_empty() => labels,
            _ => return Ok(Default::default()),
        };
        let mut constraints = Vec::new();
        let mut settings = Vec::new();
        let mut unprovided = None;
        let mut dependencies = Vec::new();
        for text in &labels {
            let label = self.resolve_rule_src(rule, text)?;
            self.analyze(&label, config)?;
            if self.cquery() {
                self.capture_cquery_edge(
                    &ConfiguredTargetKey::new(rule.label.clone(), config),
                    label.clone(),
                    config,
                    "target_compatible_with",
                    false,
                    false,
                );
            }
            let node = self
                .configured_node(&ConfiguredTargetKey::new(label.clone(), config))
                .unwrap_or_default();
            if node.constraint_value.is_none() && node.matching.is_none() {
                unprovided.get_or_insert(label.clone());
            }
            if let Some(cause) = node.incompatible {
                dependencies.push((ConfiguredTargetKey::new(label.clone(), config), cause));
            }
            if let Some(value) = node.constraint_value {
                constraints.push(value);
            }
            if let Some(matching) = node.matching {
                // ConfiguredTarget.getOriginalLabel: the label as written.
                settings.push((label, matching));
            }
        }
        let provider_error = |label: &Label| {
            located(
                rule.location.as_deref(),
                format!(
                    "in target_compatible_with attribute of {} rule {}: '{}' does not have mandatory providers: ConfigMatchingProvider or 'ConstraintValueInfo'",
                    rule.rule_class_name(),
                    crate::rule_declarations::canonical_form(&rule.label),
                    crate::rule_declarations::canonical_form(label)
                ),
            )
        };
        if !checked {
            return Ok(compatibility::DirectCheck {
                incompatible: None,
                provider_error: unprovided.map(|label| provider_error(&label)),
                dependencies,
            });
        }
        crate::platforms::validate_constraints(&constraints)
            .map_err(|error| located(rule.location.as_deref(), error))?;
        // ConfigMatchingProvider.accumulateMatchResults (:155-168).
        let mut errors: Vec<(&Label, &[String])> = Vec::new();
        let mut not_matching = Vec::new();
        for (label, matching) in &settings {
            match &matching.result {
                configured_attributes::MatchResult::InError(messages) => {
                    errors.push((label, messages))
                }
                configured_attributes::MatchResult::NoMatch(_) => not_matching.push(label),
                configured_attributes::MatchResult::Match => {}
            }
        }
        if !errors.is_empty() {
            return Err(located(
                rule.location.as_deref(),
                errors
                    .iter()
                    .map(|(label, messages)| {
                        format!("For config_setting {label}: {}", messages.join(", "))
                    })
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        // The target platform is needed only to compare constraint values
        // (a config_setting's result is already known).
        let platform = if constraints.is_empty() {
            None
        } else {
            Some(self.target_platform(config)?)
        };
        let mut unmet = platform.as_ref().map_or_else(Vec::new, |platform| {
            constraints
                .iter()
                .filter(|value| platform.constraints.get(&value.setting).as_ref() != Some(*value))
                .collect::<Vec<_>>()
        });
        if unmet.is_empty() && not_matching.is_empty() {
            return Ok(compatibility::DirectCheck {
                incompatible: None,
                provider_error: unprovided.map(|label| provider_error(&label)),
                dependencies,
            });
        }
        // IncompatiblePlatformProvider.incompatibleDueToConstraintsAndConfigSettings
        // (analysis/IncompatiblePlatformProvider.java:113-125): the unmet
        // constraints sorted by label and the non-matching config_settings
        // sorted (Label.compareTo: repository, package, name — the native
        // Label order), each without duplicates (a constraint value listed
        // directly and through an alias is one ConstraintValueInfo; a label
        // has one setting, so equal values are adjacent once sorted).
        unmet.sort_by(|left, right| left.label.cmp(&right.label));
        unmet.dedup();
        not_matching.sort();
        not_matching.dedup();
        Ok(compatibility::DirectCheck {
            incompatible: Some(Arc::new(compatibility::Incompatibility::Direct {
                platform: platform.as_ref().map(|platform| platform.label.clone()),
                constraints: unmet.iter().map(|value| value.label.clone()).collect(),
                settings: not_matching.into_iter().cloned().collect(),
            })),
            ..Default::default()
        })
    }

    /// ConfigConditionsProducer (analysis/producers/ConfigConditionsProducer.java:91-172):
    /// every `$config_dependencies` label is analyzed in this configuration;
    /// each must then provide a ConfigMatchingProvider
    /// (ConfigConditions.fromConfiguredTarget, analysis/config/ConfigConditions.java:64-83).
    fn config_conditions(
        &mut self,
        rule: &Rule,
        config: ConfigId,
    ) -> Result<Vec<(String, ConfigMatching)>, String> {
        let timer = crate::plan_timings::StageTimer::start(
            crate::plan_timings::PlanStage::ConfigConditions,
            u64::MAX,
            0,
        );
        let result = self.config_conditions_timed(rule, config);
        timer.finish();
        result
    }

    fn config_conditions_timed(
        &mut self,
        rule: &Rule,
        config: ConfigId,
    ) -> Result<Vec<(String, ConfigMatching)>, String> {
        let mut keys = Vec::new();
        for text in configured_attributes::rule_config_dependencies(rule) {
            let label = self.resolve_rule_src(rule, &text)?;
            // A key that fails to load or analyze is reported by itself, then
            // the consumer adds `errors encountered resolving select() keys
            // for L` at its location (ConfigConditionsProducer.emitErrorIfMostImportant,
            // analysis/producers/ConfigConditionsProducer.java:121-129, 200-211).
            // A missing key of another package is first reported at that
            // package's BUILD file (the target loading event, paired capture
            // B2/lx_missing_key).
            let loaded = self.target(&label).map(|_| ()).map_err(|error| {
                self.foreign_target_event(&rule.label.package_id(), &label, &error)
                    .unwrap_or(error)
            });
            loaded
                .and_then(|()| self.analyze(&label, config))
                .map_err(|error| {
                    format!(
                        "{error}\n{}",
                        located(
                            rule.location.as_deref(),
                            format!(
                                "errors encountered resolving select() keys for {}",
                                crate::rule_declarations::canonical_form(&rule.label)
                            )
                        )
                    )
                })?;
            keys.push((text, label));
        }
        let mut conditions = Vec::new();
        for (text, label) in keys {
            let key = ConfiguredTargetKey::new(label.clone(), config);
            // The invalid-condition message names the prerequisite's target,
            // the end of an alias chain (ConfigConditionsProducer.java:156-165,
            // ConfiguredTargetAndData.getTargetLabel).
            let label = self.alias_chain(config, &label).1;
            // ConfigConditions.fromConfiguredTarget (analysis/config/ConfigConditions.java:64-83):
            // a ConfigMatchingProvider, or a constraint_value matched against
            // the target platform when the consuming rule uses toolchain
            // resolution (otherwise the key is not a valid condition).
            let node = self.configured_node(&key);
            let mut matching = node.as_ref().and_then(|node| node.matching.clone());
            if matching.is_none()
                && let Some(value) = node.as_ref().and_then(|node| node.constraint_value.clone())
                && uses_toolchain_resolution(rule)
            {
                let platform = self.target_platform(config)?;
                matching = Some(value.config_matching(&platform));
            }
            let Some(matching) = matching else {
                let target = crate::rule_declarations::canonical_form(&rule.label);
                return Err(located(
                    rule.location.as_deref(),
                    format!(
                        "{} is not a valid select() condition for {target}.\nTo inspect the select(), run: bazel query --output=build {target}.\nFor more help, see https://bazel.build/reference/be/functions#select.\n\n",
                        crate::rule_declarations::canonical_form(&label)
                    ),
                ));
            };
            // Phase-1 prefetch: which select() branches the build certainly
            // takes (crate::prefetch::certain_labels).
            self.prefetch_note_condition(&text, &matching);
            conditions.push((text, matching));
        }
        Ok(conditions)
    }

    /// CommonPrerequisiteValidator.validateDirectPrerequisiteForTestOnly and
    /// validateDirectPrerequisiteForDeprecation (CommonPrerequisiteValidator.java:683-776)
    /// with BazelPrerequisiteValidator (bazel/rules/BazelPrerequisiteValidator.java:24-33:
    /// no experimental packages, same logical package = same package). Messages
    /// use RuleContext's `in CLASS rule LABEL: ` prefix
    /// (analysis/EventHandlingErrorReporter.java:80-82).
    fn validate_prerequisite(
        &mut self,
        rule: &Rule,
        dependency: &Label,
        description: &dyn Fn() -> String,
        skip_testonly: bool,
    ) -> Result<(), String> {
        // A RuleContext.ruleError at the consuming rule's location.
        self.prerequisite_error(rule, dependency, description, skip_testonly)?
            .map_or(Ok(()), |error| {
                Err(located(rule.location.as_deref(), error))
            })
    }

    /// The testonly error of [`Self::validate_prerequisite`] (`None` when
    /// valid); deprecation warnings are recorded. `dependency` is the end of
    /// an alias chain and `description` its describeTargetWithAliases text.
    fn prerequisite_error(
        &mut self,
        rule: &Rule,
        dependency: &Label,
        description: &dyn Fn() -> String,
        skip_testonly: bool,
    ) -> Result<Option<String>, String> {
        let (generating, output) = match self.target(dependency)? {
            Target::Rule(dep) => (dep, false),
            Target::Output(dep, _) => (dep, true),
            Target::Source(_) => return Ok(None),
        };
        // The texts are formatted only for a reported error or warning.
        let shown = || crate::rule_declarations::canonical_form(&rule.label);
        let prefix = || format!("in {} rule {}: ", rule.rule_class_name(), shown());
        let args = &self.package_args(&rule.label.package_id());
        let dependency_args = &self.package_args(&generating.label.package_id());
        // Output files of testonly rules are only checked with
        // --incompatible_check_testonly_for_output_files (default false,
        // analysis/config/CoreOptions.java:690-701).
        let testonly_error = (!skip_testonly
            && !output
            && generating.is_test_only(dependency_args)
            && !rule.is_test_only(args))
        .then(|| {
            format!(
                "{}non-test target '{}' depends on testonly {} and doesn't have testonly attribute set",
                prefix(),
                shown(),
                description()
            )
        });
        // validate() runs the testonly check, then the deprecation check, for
        // each edge (CommonPrerequisiteValidator.java:48-60): an edge's
        // deprecation warning is still reported with its testonly error.
        if rule.deprecation_warning(args).is_none()
            && rule.label.package_id() != generating.label.package_id()
            && let Some(warning) = generating.deprecation_warning(dependency_args)
        {
            let shown_dependency = crate::rule_declarations::canonical_form(dependency);
            let message = if output {
                format!(
                    "{}target '{}' depends on the output file {shown_dependency} of a deprecated rule {}': {warning}",
                    prefix(),
                    shown(),
                    crate::rule_declarations::canonical_form(&generating.label)
                )
            } else {
                format!(
                    "{}target '{}' depends on deprecated target '{shown_dependency}': {warning}",
                    prefix(),
                    shown()
                )
            };
            // Skyframe reports an identical warning event once
            // (EmittedEventState.addWarning, skyframe/EmittedEventState.java:32-52):
            // an edge to the same deprecated target in two configurations,
            // or through two attributes, warns once.
            let warning = located(rule.location.as_deref(), message);
            self.warn_once(warning);
        }
        Ok(testonly_error)
    }

    /// The dependency edges of a configured rule: its attribute edges and,
    /// for a Starlark test rule in the built-in profile (where
    /// `@bazel_tools` and the default `test` exec group exist, D3 of
    /// docs/TEST_FLAGS.md), the implicit test-runner attributes
    /// (TEST_RUNNER_ATTRIBUTES), so they are analyzed and their packages'
    /// repositories reach the target's repository mapping manifest
    /// (RuleContext.getTransitivePackagesForRunfileRepoMappingManifest; N23g).
    fn rule_edges(&mut self, rule: &Rule, config: ConfigId) -> Vec<Edge> {
        let mut edges = dependency_edges(rule);
        // JavaPluginsFlagAliasRule's late-bound :java_plugins: only the
        // selected configuration's plugins, never the universe of flag values.
        // Keeping these in the ordinary edge path also records their packages,
        // source/bzl inputs and configured cquery/snapshot dependencies.
        if matches!(rule.kind, RuleKind::Native("java_plugins_flag_alias")) {
            let mut seen = BTreeSet::new();
            edges.extend(
                self.configuration(config)
                    .native
                    .plugins
                    .iter()
                    .filter(|label| seen.insert(*label))
                    .map(|label| Edge {
                        label: label.to_string(),
                        attribute: ":java_plugins".to_owned(),
                        cfg: EdgeCfg::Exec(None),
                    }),
            );
        }
        // FilegroupRule (rules/filegroup/FilegroupRule.java:36-80): `data`
        // (after `srcs`), target-configuration prerequisites that only
        // contribute runfiles (Filegroup.java:80-104).
        if let (RuleKind::Filegroup, Some(native)) = (&rule.kind, &rule.native) {
            edges.extend(native.labels("data").into_iter().map(|label| Edge {
                label,
                attribute: "data".to_owned(),
                cfg: EdgeCfg::Target,
            }));
        }
        // StarlarkDocExtractRule (rules/starlarkdocextract/
        // StarlarkDocExtractRule.java:33-139): `src` and `deps` are
        // target-configuration prerequisites (their files are read).
        if let (RuleKind::Native("starlark_doc_extract"), Some(native)) = (&rule.kind, &rule.native)
        {
            if let Some(crate::custom_rule::AttrValue::Label(Some(src))) = native.attrs.get("src") {
                edges.push(Edge {
                    label: src.clone(),
                    attribute: "src".to_owned(),
                    cfg: EdgeCfg::Target,
                });
            }
            edges.extend(native.labels("deps").into_iter().map(|label| Edge {
                label,
                attribute: "deps".to_owned(),
                cfg: EdgeCfg::Target,
            }));
        }
        // TestSuiteRule (rules/test/TestSuiteRule.java:96-110): `tests`,
        // then `$implicit_tests`, both in the target configuration.
        if let (RuleKind::Native("test_suite"), Some(native)) = (&rule.kind, &rule.native) {
            let edge = |label: String, attribute: &str| Edge {
                label,
                attribute: attribute.to_owned(),
                cfg: EdgeCfg::Target,
            };
            edges.extend(
                native
                    .labels("tests")
                    .into_iter()
                    .map(|label| edge(label, "tests")),
            );
            edges.extend(
                self.implicit_tests(rule)
                    .iter()
                    .map(|label| edge(label.to_string(), "$implicit_tests")),
            );
        }
        let test = rule
            .custom
            .as_ref()
            .is_some_and(|custom| custom.declaration.builtin.kind.test);
        if test && self.repositories().root("bazel_tools").is_ok() {
            edges.extend(
                TEST_RUNNER_ATTRIBUTES
                    .iter()
                    .map(|(attribute, label)| Edge {
                        label: (*label).to_owned(),
                        attribute: (*attribute).to_owned(),
                        cfg: EdgeCfg::Exec(Some(
                            crate::exec_groups::TEST_EXEC_GROUP_NAME.to_owned(),
                        )),
                    }),
            );
        }
        // The late-bound run_under attribute (after `:coverage_support`,
        // getTestBaseRule order), null in an exec configuration.
        if test
            && let Some(run_under) = self.run_under().cloned()
            && !self.configuration(config).native.is_exec
        {
            let label = &run_under.label;
            let exec = run_under.exec && self.repositories().root("bazel_tools").is_ok();
            edges.push(Edge {
                label: format!("@@{}//{}:{}", label.repository, label.package, label.name),
                attribute: RUN_UNDER_ATTRIBUTES[usize::from(!run_under.exec)].to_owned(),
                cfg: if exec {
                    EdgeCfg::Exec(Some(crate::exec_groups::TEST_EXEC_GROUP_NAME.to_owned()))
                } else {
                    EdgeCfg::Target
                },
            });
        }
        edges
    }

    /// The label attribute edges of a native rule that only an aspect
    /// application on it resolves, beyond [`Self::rule_edges`] (which its
    /// own analysis needs): every other dependency label attribute of the
    /// rule class (`toolchain_type`, `target_settings`, `parents`,
    /// `required_settings`, `constraint_values`, `flag_values`, the common
    /// `aspect_hints`/`compatible_with`/`restricted_to`/
    /// `*_compatible_with`) in the target configuration, and the private
    /// prerequisites with a fixed default (`$genrule_setup`, the
    /// config_feature_flag allowlist; exec-configured). They are
    /// `ctx.rule.attr` Targets and edges aspects propagate along
    /// (StarlarkRuleContext.java:300-303; AspectResolutionHelpers over the
    /// rule's attributes). Empty for Starlark rules.
    fn native_aspect_edges(&self, rule: &Rule) -> Vec<Edge> {
        use crate::native_rules::{NativeType, PrivateAttrKind};
        let (None, Some(native)) = (&rule.custom, &rule.native) else {
            return Vec::new();
        };
        let Some(class) = crate::native_rules::rule_class(&native.rule_class) else {
            return Vec::new();
        };
        let covered: BTreeSet<String> = dependency_edges(rule)
            .into_iter()
            .map(|edge| edge.attribute)
            .chain(["srcs", "tools", "toolchains", "data", "tests"].map(str::to_owned))
            .collect();
        let mut edges = Vec::new();
        let target = |label: String, attribute: &str| Edge {
            label,
            attribute: attribute.to_owned(),
            cfg: EdgeCfg::Target,
        };
        for attribute in &class.attrs {
            let name = attribute.name;
            if covered.contains(name) || name == "exec_group_compatible_with" {
                continue;
            }
            match (attribute.ty, native.attrs.get(name)) {
                (NativeType::Label, Some(crate::custom_rule::AttrValue::Label(Some(label)))) => {
                    edges.push(target(label.clone(), name));
                }
                (NativeType::LabelList, _) => {
                    edges.extend(native.labels(name).into_iter().map(|l| target(l, name)));
                }
                (
                    NativeType::LabelKeyedStringDict,
                    Some(crate::custom_rule::AttrValue::LabelKeyedStringDict(entries)),
                ) => {
                    edges.extend(entries.iter().map(|(l, _)| target(l.clone(), name)));
                }
                _ => {}
            }
        }
        for private in crate::native_rules::aspect_private_attributes(&native.rule_class) {
            if let PrivateAttrKind::Label { label, exec } = private.kind
                && self.repositories().root("bazel_tools").is_ok()
            {
                edges.push(Edge {
                    label: label.to_owned(),
                    attribute: private.native.to_owned(),
                    cfg: if exec {
                        EdgeCfg::Exec(None)
                    } else {
                        EdgeCfg::Target
                    },
                });
            }
        }
        edges
    }
    /// [`Engine::target_incompatibility`] over the host.
    fn target_incompatibility(
        &mut self,
        label: &Label,
        config: ConfigId,
    ) -> Result<Option<Arc<compatibility::Incompatibility>>, String> {
        let owner = match self.target(label)? {
            Target::Output(rule, _) => rule.label.clone(),
            _ => label.clone(),
        };
        Ok(self
            .configured_node(&ConfiguredTargetKey::new(owner, config))
            .and_then(|node| node.incompatible))
    }

    /// [`Engine::publish_incompatible`] over the host.
    fn publish_incompatible(
        &mut self,
        key: ConfiguredTargetKey,
        rule: Arc<Rule>,
        conditions: Vec<(String, ConfigMatching)>,
        incompatible: Arc<compatibility::Incompatibility>,
    ) {
        let direct = matches!(*incompatible, compatibility::Incompatibility::Direct { .. });
        self.publish_node(
            &key,
            ConfiguredNode {
                incompatible: Some(incompatible),
                ..Default::default()
            },
        );
        self.record_configured_rule(&key, &rule);
        if self.cquery() {
            self.cquery_rule(&key, &rule, Some(conditions), direct);
        }
        self.plan_push(key, rule);
    }

    /// [`Engine::analyzed_edge_configurations`] over the host.
    fn analyzed_edge_configurations(
        &mut self,
        key: &ConfiguredTargetKey,
        cfg: &EdgeCfg,
    ) -> Result<EdgeConfigurations, String> {
        Ok(match cfg {
            EdgeCfg::Target => EdgeConfigurations::One(key.config),
            EdgeCfg::Exec(None) => {
                EdgeConfigurations::One(self.exec_config_of(key).unwrap_or(key.config))
            }
            EdgeCfg::Exec(Some(group)) => EdgeConfigurations::One(
                self.group_exec_config_of(key, group).unwrap_or(key.config),
            ),
            EdgeCfg::NoConfig => EdgeConfigurations::One(self.intern_no_config()),
            EdgeCfg::Starlark(attribute) => {
                EdgeConfigurations::Transition(self.edge_configs_of(key, attribute).ok_or_else(
                    || {
                        format!(
                            "internal error: no transitioned configuration for {} attribute {attribute}",
                            key.label
                        )
                    },
                )?)
            }
        })
    }

    /// [`Engine::split_attribute_dependencies`] over the host.
    fn split_attribute_dependencies(
        &mut self,
        rule: &Rule,
        configurations: &BTreeMap<String, TransitionConfigurations>,
    ) -> Result<BTreeMap<String, crate::custom_rule::SplitAttribute>, String> {
        let mut attributes = BTreeMap::new();
        let Some(custom) = &rule.custom else {
            return Ok(attributes);
        };
        for (name, branches) in configurations {
            // The many rules_go patch-transition edges keep their original
            // representation: no per-file split metadata allocation.
            if branches.len() == 1 && branches[0].0.is_empty() {
                continue;
            }
            let value = custom
                .attrs
                .get(name)
                .ok_or_else(|| format!("missing transitioned attribute {name}"))?;
            let labels = analysis::dependency_labels(value)
                .into_iter()
                .map(|text| self.resolve_rule_src(rule, text))
                .collect::<Result<Vec<_>, _>>()?;
            attributes.insert(name.clone(), self.split_attribute(name, labels, branches)?);
        }
        Ok(attributes)
    }

    /// [`Engine::split_attribute`] over the host.
    fn split_attribute(
        &mut self,
        name: &str,
        labels: Vec<Label>,
        branches: &[(String, ConfigId)],
    ) -> Result<crate::custom_rule::SplitAttribute, String> {
        use crate::custom_rule::{SplitAttribute, SplitPrerequisite};
        let mut attribute = SplitAttribute::default();
        let mut seen_labels = BTreeSet::new();
        for label in labels {
            if !seen_labels.insert(label.clone()) {
                continue;
            }
            let target = self.target(&label)?;
            let mut prerequisites: Vec<(Option<ConfigId>, SplitPrerequisite)> = Vec::new();
            for (branch, config) in branches.iter() {
                let identity = match &target {
                    Target::Source(_) => None,
                    Target::Rule(rule) if matches!(rule.kind, RuleKind::PackageGroup(_)) => None,
                    Target::Rule(rule) | Target::Output(rule, _) => {
                        let key = ConfiguredTargetKey::new(rule.label.clone(), *config);
                        Some(
                            self.rule_delegation(&key)
                                .map_or(*config, |owner| owner.config),
                        )
                    }
                };
                if let Some((_, existing)) =
                    prerequisites.iter_mut().find(|(id, _)| *id == identity)
                {
                    // PrerequisitesProducer.cleanupValues: null-configured
                    // targets aggregate all transition keys. For a configured
                    // rule/output, keep only the first branch reaching the
                    // final identity (including incoming rule transitions).
                    if identity.is_none() {
                        existing.transition_keys.push(branch.clone());
                    }
                } else {
                    prerequisites.push((
                        identity,
                        SplitPrerequisite {
                            label: label.clone(),
                            config: *config,
                            map_key: analysis::split_dependency_key(
                                &label.to_string(),
                                name,
                                *config,
                            ),
                            transition_keys: vec![branch.clone()],
                        },
                    ));
                }
            }
            let mnemonic = |id: Option<ConfigId>| {
                id.map_or_else(String::new, |id| self.configuration_mnemonic(id))
            };
            let mut keyed = prerequisites
                .into_iter()
                .map(|(id, prerequisite)| ((mnemonic(id), id), prerequisite))
                .collect::<Vec<_>>();
            keyed.sort_by(|(a, _), (b, _)| a.cmp(b));
            attribute
                .prerequisites
                .extend(keyed.into_iter().map(|(_, p)| p));
        }
        Ok(attribute)
    }

    /// [`Engine::package_metadata_errors`] over the host.
    fn package_metadata_errors_p1(&mut self, rule: &Rule) -> Result<Vec<String>, String> {
        let mut errors = Vec::new();
        if rule.package_metadata.is_empty() {
            return Ok(errors);
        }
        let no_config = self.intern_no_config();
        let shown = crate::rule_declarations::canonical_form(&rule.label);
        for text in &rule.package_metadata {
            let dependency = self.resolve_rule_src(rule, text)?;
            let (chain, actual) = self.alias_chain(no_config, &dependency);
            let target = self.target(&actual);
            if let Some(error) = no_files_error_for(&chain, &actual, target)? {
                errors.push(format!(
                    "in package_metadata attribute of {} rule {shown}: {error}",
                    rule.rule_class_name()
                ));
            }
        }
        Ok(errors)
    }

    /// [`Engine::no_files_error`] over the host.
    fn no_files_error_p1(
        &mut self,
        chain: &[Label],
        actual: &Label,
    ) -> Result<Option<String>, String> {
        let target = self.target(actual);
        no_files_error_for(chain, actual, target)
    }
}

/// The error mark of an `ExecCfg` value whose interning failed (the
/// consumer locates it at its rule).
pub(crate) const EXEC_INTERN_ERROR: &str = "\u{1}exec-intern:";

impl<T: Host + ?Sized> Phase1 for T {}

/// The evaluator's answer to a need of the registered toolchains or
/// execution platforms of a configuration while they are computed (a back
/// edge: the walk's `computing_*` guard; the caller words the report).
pub(crate) const REGISTRATION_IN_PROGRESS: &str = "\u{1}registration in progress";
