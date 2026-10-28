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
    // Phase two (original-pattern explicitness, skipped tests/BEP) is not yet
    // modeled. Keep build and --nobuild fail-closed without misclassifying this
    // successful configured result as a keep-going success.
    pub(super) fn unsupported_build_error<'a>(&'a self, label: &'a Label) -> (&'a Label, String) {
        match self {
            Self::Dependencies(deps) => deps[0].1.unsupported_build_error(&deps[0].0.label),
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
                (
                    label,
                    format!(
                        "{} is incompatible with its target platform:{}; skipping incompatible targets (IncompatiblePlatformProvider) is not implemented in the native profile (docs/PLATFORMS.md)",
                        crate::rule_declarations::canonical_form(label),
                        reasons.join(" and")
                    ),
                )
            }
        }
    }
}

impl Engine {
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
