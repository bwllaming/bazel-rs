//! Actionless configured results (Bazel 9.3 IncompatibleTargetChecker).
use super::*;

#[derive(Default)]
pub(super) struct DirectCheck {
    pub incompatible: Option<Arc<Incompatibility>>,
    pub provider_error: Option<String>,
    pub dependencies: Vec<(ConfiguredTargetKey, Arc<Incompatibility>)>,
}

#[derive(Debug)]
pub(super) enum Incompatibility {
    Direct {
        platform: Option<Label>,
        constraints: Box<[Label]>,
        settings: Box<[Label]>,
    },
    Dependencies(Box<[(ConfiguredTargetKey, Arc<Incompatibility>)]>),
}

impl Incompatibility {
    fn reason(&self) -> String {
        match self {
            Self::Dependencies(deps) => deps[0].1.reason(),
            Self::Direct {
                platform,
                constraints,
                settings,
            } => {
                let plural = |labels: &[Label]| match labels {
                    [only] => format!(" {only}"),
                    many => format!(
                        "s [{}]",
                        many.iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                };
                let mut reasons = Vec::new();
                if !constraints.is_empty() {
                    reasons.push(format!(
                        " target platform ({}) didn't satisfy constraint{}",
                        platform.as_ref().expect("constraints have a platform"),
                        plural(constraints)
                    ));
                }
                if !settings.is_empty() {
                    reasons.push(format!(
                        " target configuration didn't match config_setting{}",
                        plural(settings)
                    ));
                }
                reasons.join(" and")
            }
        }
    }
}

impl Engine {
    /// Original positive SINGLE_TARGET patterns, not expansion cardinality.
    /// AnalysisPhaseRunner.getExplicitTargetPatterns (9.3:546-581).
    pub(super) fn record_explicit_targets(&mut self, patterns: &[String], relative: &str) {
        let mut explicit = BTreeSet::new();
        for text in patterns {
            let Ok(pattern) = crate::target_pattern::parse(text, relative) else {
                continue;
            };
            if !pattern.negative
                && let crate::target_pattern::Kind::SingleTarget { target } = &pattern.kind
                && let Ok(label) = self.repositories.parse(&pattern.label_text(target), "", "")
            {
                explicit.insert(label);
            }
        }
        self.explicit_targets = Some(explicit);
    }

    pub(super) fn check_requested_compatibility(
        &mut self,
        configured: &ConfiguredNodes,
        label: &Label,
        config: ConfigId,
    ) -> Result<(), String> {
        if self.cquery_capture.is_some() {
            return Ok(());
        }
        let Some(cause) = self.target_incompatibility(configured, label, config)? else {
            return Ok(());
        };
        let explicit = self
            .explicit_targets
            .as_ref()
            .is_none_or(|targets| targets.contains(label));
        if !explicit || self.skip_incompatible_explicit_targets {
            self.skipped_incompatible.insert(label.clone());
            return Ok(());
        }
        let mut message = format!(
            "Target {} is incompatible and cannot be built, but was explicitly requested.\nDependency chain:",
            crate::rule_declarations::canonical_form(label)
        );
        let mut key = ConfiguredTargetKey::new(label.clone(), config);
        let mut cause = cause;
        loop {
            let actual = self
                .forwarded_actual(configured, &key)
                .unwrap_or_else(|| key.label.clone());
            let target = self.target(&actual)?;
            let owner = match &target {
                Target::Output(rule, _) => rule.label.clone(),
                _ => actual.clone(),
            };
            let config =
                self.delegated_config(&ConfiguredTargetKey::new(owner, key.config), key.config);
            message.push_str(&format!("\n    {} ({})", actual, &config.to_string()[..6]));
            if let Incompatibility::Dependencies(deps) = cause.as_ref() {
                // An indirectly incompatible output first depends on its
                // generating rule; direct output replacement has no such edge.
                if let Target::Output(rule, _) = target {
                    message.push_str(&format!(
                        "\n    {} ({})",
                        rule.label,
                        &config.to_string()[..6]
                    ));
                }
                key = deps[0].0.clone();
                cause = deps[0].1.clone();
            } else {
                message.push_str(&format!("   <--{}", cause.reason()));
                break;
            }
        }
        if self.keep_going {
            self.incompatible_errors.insert(label.clone(), message);
            self.keep_going_state
                .failed_targets
                .insert(label.clone(), false);
            Ok(())
        } else {
            self.incompatible_errors
                .insert(label.clone(), message.clone());
            Err(format!(
                "Analysis of target '{}' failed; build aborted: {message}",
                crate::rule_declarations::canonical_form(label)
            ))
        }
    }

    /// Known actionless roots survive a later fail-fast analysis error. This
    /// partial reporting data is never a successful request or a saved snapshot.
    pub fn failed_compatibility_results(&mut self) -> (Vec<Label>, crate::bep::plan::BepPlan) {
        self.bep_groups.clear();
        let skipped = self.skipped_incompatible.iter().cloned().collect();
        (skipped, self.bep_plan(&[]))
    }

    pub(super) fn incompatible_dependency_view(
        &mut self,
        configured: &ConfiguredNodes,
        identities: &mut BTreeMap<TargetIdentityKey, Arc<crate::starlark_types::Target>>,
        label: &Label,
        config: ConfigId,
    ) -> Result<DependencyTarget, String> {
        let key = ConfiguredTargetKey::new(label.clone(), config);
        // A label_flag remains a LateBoundAlias and retains BuildSettingProvider
        // in ConfiguredNode. An ordinary alias was replaced by the indirect
        // checker and therefore has its own label/class and no forwarded map.
        let actual = self
            .forwarded_actual(configured, &key)
            .unwrap_or_else(|| label.clone());
        let rule = match self.target(&actual)? {
            Target::Rule(rule) | Target::Output(rule, _) => rule,
            Target::Source(_) => return Err("incompatible source configured target".into()),
        };
        let runfiles = Arc::new(crate::runfiles::TargetRunfiles::default());
        let identity_config = self.delegated_config(
            &ConfiguredTargetKey::new(rule.label.clone(), config),
            config,
        );
        let identity = identities
            .entry((label.clone(), Some(identity_config)))
            .or_insert_with(|| {
                let mut target = analysis::dependency_target_data(
                    &label.to_string(),
                    &actual.to_string(),
                    &[],
                    false,
                    true,
                    false,
                    false,
                    &BTreeMap::new(),
                    &[],
                    &runfiles,
                )
                .with_rule_class(Some(rule.rule_class_name()));
                target.incompatible = true;
                Arc::new(target)
            })
            .clone();
        Ok(DependencyTarget::new(analysis::DependencyView {
            label: actual.to_string(),
            files: Vec::new(),
            is_source: false,
            is_rule: true,
            is_package_group: false,
            is_filegroup: false,
            providers: BTreeMap::new(),
            provider_keys: Vec::new(),
            runfiles,
            rule_class: Some(rule.rule_class_name().to_owned()),
            identity,
        }))
    }

    pub(super) fn target_incompatibility(
        &mut self,
        configured: &ConfiguredNodes,
        label: &Label,
        config: ConfigId,
    ) -> Result<Option<Arc<Incompatibility>>, String> {
        let owner = match self.target(label)? {
            Target::Output(rule, _) => rule.label.clone(),
            _ => label.clone(),
        };
        Ok(configured
            .get(&ConfiguredTargetKey::new(owner, config))
            .and_then(|node| node.incompatible.clone()))
    }

    pub(super) fn publish_incompatible(
        &mut self,
        key: ConfiguredTargetKey,
        rule: Arc<Rule>,
        conditions: Vec<(String, ConfigMatching)>,
        incompatible: Arc<Incompatibility>,
        configured: &mut ConfiguredNodes,
        plan: &mut Plan,
    ) {
        let direct = matches!(*incompatible, Incompatibility::Direct { .. });
        configured.insert(
            key.clone(),
            ConfiguredNode {
                incompatible: Some(incompatible),
                ..Default::default()
            },
        );
        self.configured_rules.insert(key.clone(), rule.clone());
        if let Some(capture) = &mut self.cquery_capture {
            if direct {
                capture.direct_incompatible.insert(key.clone());
            }
            capture.incompatible.insert(key.clone(), conditions);
            capture.rules.insert(key.clone(), rule.clone());
        }
        plan.push((key, rule));
    }
}
