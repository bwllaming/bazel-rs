//! Split edge configuration and prerequisite identities. This stores only
//! configurations, labels and keys; provider/runfiles values remain shared.
use super::*;
use crate::custom_rule::{SplitAttribute, SplitPrerequisite};

impl Engine {
    pub(super) fn analyzed_edge_configurations(
        &mut self,
        key: &ConfiguredTargetKey,
        cfg: &EdgeCfg,
    ) -> Result<EdgeConfigurations, String> {
        Ok(match cfg {
            EdgeCfg::Target => EdgeConfigurations::One(key.config),
            EdgeCfg::Exec(None) => EdgeConfigurations::One(self.exec_configurations.get(key).copied().unwrap_or(key.config)),
            EdgeCfg::Exec(Some(group)) => EdgeConfigurations::One(self.group_exec_configurations.get(&(key.clone(), group.clone())).copied().unwrap_or(key.config)),
            EdgeCfg::NoConfig => EdgeConfigurations::One(self.configurations.intern_no_config()),
            EdgeCfg::Starlark(attribute) => EdgeConfigurations::Transition(self.edge_configs.get(key).and_then(|attrs| attrs.get(attribute)).cloned().ok_or_else(|| format!("internal error: no transitioned configuration for {} attribute {attribute}", key.label))?),
        })
    }

    pub(super) fn split_attribute_dependencies(
        &mut self,
        rule: &Rule,
        configurations: &BTreeMap<String, TransitionConfigurations>,
    ) -> Result<BTreeMap<String, SplitAttribute>, String> {
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

    pub(super) fn split_attribute(
        &mut self,
        name: &str,
        labels: Vec<Label>,
        branches: &[(String, ConfigId)],
    ) -> Result<SplitAttribute, String> {
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
                            self.rule_delegations
                                .get(&key)
                                .map_or(*config, |owner| owner.config),
                        )
                    }
                };
                if let Some((_, existing)) =
                    prerequisites.iter_mut().find(|(id, _)| *id == identity)
                {
                    existing.transition_keys.push(branch.clone());
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
            prerequisites.sort_by(|(a, _), (b, _)| {
                let mnemonic =
                    |id: Option<ConfigId>| id.map_or("", |id| self.configurations.mnemonic(id));
                mnemonic(*a).cmp(mnemonic(*b)).then(a.cmp(b))
            });
            attribute
                .prerequisites
                .extend(prerequisites.into_iter().map(|(_, p)| p));
        }
        Ok(attribute)
    }
}
