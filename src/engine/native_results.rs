//! T2 (docs/EVALUATOR.md §8, "Native rules"): the part of a native rule's
//! commit that computes its own results — the transitive `_validation`
//! group, the files to build (`inputs`), the providers and the runfiles —
//! as functions of a lookup context ([`prepare::Lookup`]) and the planned
//! targets' results ([`PlanReads`]). The coordinator's commit
//! (`Engine::commit_native_target`) calls them over the planning state and
//! registers what they found; an analysis worker of a frozen walk calls
//! them over the published results ([`parallel::Frozen`]), so that the
//! rule's dependents need not wait for its commit (a native rule's results
//! carry no producer choice of its own: its Artifacts are its
//! dependencies', registered and checked by their merges).

use super::*;

/// A plain native rule's own results ([`Engine::native_plain_in`]).
pub(super) struct NativePlain {
    pub(super) providers: Option<BTreeMap<String, starlark::values::OwnedFrozenValue>>,
    pub(super) runfiles_info: Option<Arc<crate::runfiles::TargetRunfiles>>,
    pub(super) dry: Vec<Artifact>,
}

/// Whether [`Engine::native_plain_in`] computes `rule`'s results: every
/// native rule but those writing outputs (genrule, genquery,
/// starlark_doc_extract), a package group, a java_plugins_flag_alias and a
/// filegroup with an `output_group` (they read dependency views).
pub(super) fn plain(rule: &Rule) -> bool {
    match &rule.kind {
        RuleKind::Custom | RuleKind::Genrule | RuleKind::PackageGroup(_) => false,
        RuleKind::Native(name) => !matches!(
            *name,
            "genquery" | "starlark_doc_extract" | "java_plugins_flag_alias"
        ),
        RuleKind::Filegroup => !rule
            .native
            .as_ref()
            .is_some_and(|native| !native.string("output_group").unwrap_or_default().is_empty()),
    }
}

/// The `output_group` of a filegroup that selects one (not [`plain`]: its
/// commit reads its `srcs`' dependency views), unless it names an internal
/// group (an attribute error).
pub(super) fn selected_output_group(rule: &Rule) -> Option<String> {
    let (RuleKind::Filegroup, Some(native)) = (&rule.kind, &rule.native) else {
        return None;
    };
    let group = native.string("output_group").unwrap_or_default();
    (!group.is_empty() && !group.ends_with(crate::output_group_info::INTERNAL_SUFFIX))
        .then(|| group.to_owned())
}

/// The Files a filegroup's `output_group` selects from its `srcs`' views
/// (`Engine::commit_native_target`: Filegroup.getArtifactsForOutputGroup,
/// rules/filegroup/Filegroup.java:133-146).
pub(super) fn output_group_files(
    views: &[DependencyTarget],
    output_group: &str,
) -> Vec<crate::starlark_types::File> {
    let mut members = Vec::new();
    for view in views {
        if let Some(set) = view
            .providers
            .get(crate::output_group_info::PROVIDER_ID)
            .and_then(|info| crate::output_group_info::groups(info.value()))
            .and_then(|groups| groups.iter().find(|(name, _)| name == output_group))
            .map(|(_, set)| set)
        {
            members.push(Arc::clone(&set.node));
        }
    }
    crate::output_group_info::compose_members(members)
        .map(|set| crate::output_group_info::depset_files(&set))
        .unwrap_or_default()
}

impl Engine {
    /// The transitive repositories of a native rule's configured target
    /// (`Engine::prepare_target`): those of every configured dependency
    /// edge, added to `transitive`.
    pub(super) fn native_transitive_in(
        &self,
        reads: &dyn PlanReads,
        rule: &Rule,
        key: &ConfiguredTargetKey,
        edges: &[Edge],
        transitive: &mut BTreeSet<String>,
    ) {
        let edge_exec = self.exec_configurations.get(key).copied();
        for edge in edges {
            let Ok(label) = self.resolve_rule_src(rule, &edge.label) else {
                continue;
            };
            let edge_config = match &edge.cfg {
                EdgeCfg::Target => Some(key.config),
                EdgeCfg::Exec(None) => Some(edge_exec.unwrap_or(key.config)),
                EdgeCfg::Exec(Some(group)) => Some(
                    self.group_exec_configurations
                        .get(&(key.clone(), group.clone()))
                        .copied()
                        .unwrap_or(key.config),
                ),
                EdgeCfg::NoConfig => None,
                EdgeCfg::Starlark(_) => {
                    unreachable!("native rules have no Starlark attribute transitions")
                }
            };
            transitive.extend(self.dependency_repositories_in(reads, label, edge_config));
        }
    }

    /// RuleConfiguredTargetBuilder.propagateTransitiveValidationOutputGroups
    /// (:175-176, 294-296, 360-430): the native-rule collection of this
    /// target's attribute prerequisites' `_validation` groups (the
    /// Starlark-rule path is analysis::transitive_validation), composed
    /// into one node of their sets (W1c). Implicit `$` attributes and tool
    /// dependencies (exec/no-config edges: genrule `tools`,
    /// `package_metadata`) never contribute; every dependency was planned
    /// before its dependent (analysis postorder).
    pub(super) fn native_validation_in<C: prepare::Lookup + ?Sized>(
        cx: &mut C,
        reads: &dyn PlanReads,
        rule: &Rule,
        config: ConfigId,
    ) -> Option<crate::starlark_types::Depset> {
        let mut members = Vec::new();
        let edges = cx.engine().rule_edges(rule, config);
        // Rule-class attribute order (AbstractAttributeMapper.
        // getAttributeNames over RuleClass.getAttributes): the built-in
        // `toolchains` attribute before the rule's own `srcs`.
        let mut ordered = edges
            .iter()
            .filter(|edge| edge.attribute == "toolchains")
            .collect::<Vec<_>>();
        ordered.extend(edges.iter().filter(|edge| edge.attribute != "toolchains"));
        for edge in ordered {
            if edge.attribute.starts_with('$')
                || edge.attribute.starts_with('_')
                || !matches!(edge.cfg, EdgeCfg::Target)
            {
                continue;
            }
            let Ok(label) = cx.engine().resolve_rule_src(rule, &edge.label) else {
                continue;
            };
            // OutputGroupInfo.get(prerequisite): an output file answers
            // with its generating rule's `_validation` group
            // (OutputFileConfiguredTarget.java:109-118, as its
            // dependency view does).
            let dep_key = match cx.target(&label) {
                Ok(Target::Output(generating, _)) => {
                    ConfiguredTargetKey::new(generating.label.clone(), config)
                }
                _ => ConfiguredTargetKey::new(label, config),
            };
            if let Some(set) = cx.engine().planned_validation_in(reads, &dep_key) {
                members.push(set.node);
            }
        }
        crate::output_group_info::compose_validation(members)
    }

    /// [`Engine::inputs`] over a lookup context and [`PlanReads`].
    pub(super) fn inputs_in<C: prepare::Lookup + ?Sized>(
        cx: &mut C,
        reads: &dyn PlanReads,
        rule: &Rule,
        config: ConfigId,
    ) -> Result<ResolvedInputs, String> {
        let mut inputs = Vec::new();
        // Lookups only: `inputs` keeps the declaration order.
        let mut seen: HashMap<SharedPath, Artifact> = HashMap::new();
        let mut locations = BTreeMap::new();
        for src in &rule.srcs {
            let label = cx.engine().resolve_rule_src(rule, src)?;
            let artifacts =
                Arc::unwrap_or_clone(Self::shared_artifacts_in(cx, &label, config, reads)?);
            let paths = artifacts
                .iter()
                .map(|a| a.exec.to_path_buf())
                .collect::<Vec<_>>();
            locations.insert(label.to_string(), paths.clone());
            locations.insert(src.clone(), paths);
            for artifact in artifacts {
                if let Some(previous) = seen.get(artifact.exec.as_path()) {
                    // One artifact without its owner: a generated one is
                    // identified by its producer (two targets' Artifacts of
                    // a shared action's output are one input).
                    if previous.real != artifact.real
                        || previous.producer != artifact.producer
                        || (artifact.producer.is_none() && previous.owner != artifact.owner)
                    {
                        return Err(format!(
                            "different artifacts map to the same execution path: {}",
                            artifact.exec.display()
                        ));
                    }
                } else {
                    seen.insert(artifact.exec.clone(), artifact.clone());
                    inputs.push(artifact);
                }
            }
        }
        Ok((inputs, locations))
    }

    /// The results of a [`plain`] native rule (`Engine::commit_native_target`
    /// after its validation and inputs): its platform / feature-flag
    /// providers, an alias's forwarded providers and runfiles, a
    /// filegroup's and a test_suite's runfiles, its files to build
    /// (`inputs`). `register`: each dependency artifact a filegroup's
    /// prerequisites hand over (the commit registers them in the index).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn native_plain_in<C: prepare::Lookup + ?Sized>(
        cx: &mut C,
        reads: &dyn PlanReads,
        configured: &ConfiguredNodes,
        rule: &Rule,
        key: &ConfiguredTargetKey,
        config: ConfigId,
        inputs: Vec<Artifact>,
        register: &mut dyn FnMut(&Artifact),
    ) -> Result<NativePlain, String> {
        let dependency_key = |label: Label| ConfiguredTargetKey::new(label, config);
        restricted_to_error(rule)?;
        let mut providers = None;
        let mut runfiles_info = None;
        // The native platform providers, visible to Starlark as
        // dep[platform_common.X] (crate::platform_providers).
        if let Some(node) = configured.get(key)
            && !forwards(rule)
        {
            use crate::platform_providers::Provider;
            let provider = match (
                &node.constraint_setting,
                &node.constraint_value,
                &node.platform,
            ) {
                (Some(info), _, _) => Some(Provider::Setting(info.clone())),
                (_, Some(info), _) => Some(Provider::Value(info.clone())),
                (_, _, Some(info)) => Some(Provider::Platform(info.clone())),
                _ => None,
            };
            if let Some(provider) = provider {
                let (id, value) = crate::platform_providers::freeze(provider)?;
                providers = Some(BTreeMap::from([(id, value)]));
            }
            // ConfigFeatureFlagProvider, visible as
            // dep[config_common.FeatureFlagInfo].
            if let Some(flag) = &node.feature_flag {
                let value = crate::feature_flags::freeze(flag.clone())?;
                providers = Some(BTreeMap::from([(
                    crate::feature_flags::PROVIDER_ID.to_owned(),
                    value,
                )]));
            }
        }
        // An alias-like target forwards its actual's files (its inputs) and
        // providers.
        if forwards(rule) {
            let actual = dependency_key(cx.engine().resolve_rule_src(rule, &rule.srcs[0])?);
            if let Some(forwarded) = reads.providers(&actual) {
                providers = Some(forwarded);
            }
            if let Some(runfiles) = reads.runfiles_info(&actual) {
                runfiles_info = Some(runfiles);
            }
        }
        if let (RuleKind::Filegroup, Some(native)) = (&rule.kind, &rule.native) {
            // Filegroup.create (rules/filegroup/Filegroup.java:52-62): an
            // output group named with OutputGroupInfo.INTERNAL_SUFFIX is
            // an attribute error (a non-empty one is not [`plain`]).
            let output_group = native.string("output_group").unwrap_or_default();
            if output_group.ends_with(crate::output_group_info::INTERNAL_SUFFIX) {
                let label = crate::rule_declarations::canonical_form(&rule.label);
                return Err(crate::rule_errors::located_events(
                    rule.location.as_deref(),
                    [crate::rule_errors::RuleEvents {
                        rule_class: "filegroup",
                        label: &label,
                    }
                    .attribute_message(
                        "output_group",
                        &format!(
                            "Output group {output_group} is not permitted for reference in filegroups."
                        ),
                    )],
                ));
            }
            // Each `srcs` and `data` prerequisite's files to build and
            // runfiles providers (Filegroup.java:80-104). A `data`
            // prerequisite's source files join the artifact index like
            // the `srcs` inputs (they are runfiles).
            let mut prerequisites = |labels: &[String]| -> Result<Vec<_>, String> {
                let mut found = Vec::new();
                for text in labels {
                    let label = cx.engine().resolve_rule_src(rule, text)?;
                    let artifacts = Self::shared_artifacts_in(cx, &label, config, reads)?;
                    for artifact in artifacts.iter() {
                        register(artifact);
                    }
                    let files = artifacts.iter().map(Artifact::file).collect::<Vec<_>>();
                    let actual = cx
                        .engine()
                        .forwarded_actual(configured, &dependency_key(label.clone()))
                        .unwrap_or_else(|| label.clone());
                    let target = cx.target(&actual)?;
                    let runfiles = dependency_runfiles(
                        &target,
                        &files,
                        reads.runfiles_info(&dependency_key(actual)).as_ref(),
                    );
                    found.push((files, runfiles));
                }
                Ok(found)
            };
            let srcs = prerequisites(&rule.srcs)?;
            let data = prerequisites(&native.labels("data"))?;
            let files = inputs.iter().map(Artifact::file).collect::<Vec<_>>();
            fn pairs(
                prerequisites: &[(Vec<File>, Arc<crate::runfiles::TargetRunfiles>)],
            ) -> Vec<(&[File], &crate::runfiles::TargetRunfiles)> {
                prerequisites
                    .iter()
                    .map(|(files, runfiles)| (files.as_slice(), &**runfiles))
                    .collect()
            }
            runfiles_info = Some(Arc::new(
                crate::runfiles::TargetRunfiles::filegroup(&files, &pairs(&srcs), &pairs(&data))
                    .map_err(|error| format!("{}: {error:#}", rule.label))?,
            ));
        }
        if let (RuleKind::Native("test_suite"), Some(native)) = (&rule.kind, &rule.native) {
            // TestSuite.create (rules/test/TestSuite.java:51-90): no files
            // to build; the data runfiles of `tests` and `$implicit_tests`
            // whose test rules (TestTagsProvider: Rule.getRuleTags,
            // through an alias too) match the suite's tags without
            // `manual`; suites are never filtered.
            use crate::test_command::{matches_filters, sort_tags_by_sense};
            let (required, excluded) = sort_tags_by_sense(&native.tags);
            let mut labels = native.labels("tests");
            labels.extend(
                cx.engine()
                    .implicit_tests(rule)
                    .iter()
                    .map(ToString::to_string),
            );
            let mut members = Vec::new();
            for text in labels {
                let label = cx.engine().resolve_rule_src(rule, &text)?;
                let actual = cx
                    .engine()
                    .forwarded_actual(configured, &dependency_key(label.clone()))
                    .unwrap_or_else(|| label.clone());
                let target = cx.target(&actual)?;
                if let Target::Rule(member) = &target
                    && let Some(tags) = rule_tags_of(member)
                    && !matches_filters(&tags, &required, &excluded, true)
                {
                    continue;
                }
                let files = Self::shared_artifacts_in(cx, &label, config, reads)?
                    .iter()
                    .map(Artifact::file)
                    .collect::<Vec<_>>();
                let runfiles = dependency_runfiles(
                    &target,
                    &files,
                    reads.runfiles_info(&dependency_key(actual)).as_ref(),
                );
                members.push((files, runfiles));
            }
            let members = members
                .iter()
                .map(|(files, runfiles)| (files.as_slice(), &**runfiles))
                .collect::<Vec<_>>();
            runfiles_info = Some(Arc::new(
                crate::runfiles::TargetRunfiles::test_suite(&members)
                    .map_err(|error| format!("{}: {error:#}", rule.label))?,
            ));
        }
        Ok(NativePlain {
            providers,
            runfiles_info,
            dry: inputs,
        })
    }
}
