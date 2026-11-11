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

    /// [`Self::analyzed_edge_configurations`] over a lookup context (an
    /// aspect job's frozen view: the no-config configuration interned
    /// already).
    pub(super) fn analyzed_edge_configurations_in<C: super::prepare::Lookup + ?Sized>(
        cx: &mut C,
        key: &ConfiguredTargetKey,
        cfg: &EdgeCfg,
    ) -> Result<EdgeConfigurations, String> {
        if *cfg == EdgeCfg::NoConfig {
            return Ok(EdgeConfigurations::One(cx.no_config()?));
        }
        let engine = cx.engine();
        Ok(match cfg {
            EdgeCfg::Target | EdgeCfg::NoConfig => EdgeConfigurations::One(key.config),
            EdgeCfg::Exec(None) => EdgeConfigurations::One(
                engine
                    .exec_configurations
                    .get(key)
                    .copied()
                    .unwrap_or(key.config),
            ),
            EdgeCfg::Exec(Some(group)) => EdgeConfigurations::One(
                engine
                    .group_exec_configurations
                    .get(&(key.clone(), group.clone()))
                    .copied()
                    .unwrap_or(key.config),
            ),
            EdgeCfg::Starlark(attribute) => EdgeConfigurations::Transition(
                engine
                    .edge_configs
                    .get(key)
                    .and_then(|attrs| attrs.get(attribute))
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "internal error: no transitioned configuration for {} attribute {attribute}",
                            key.label
                        )
                    })?,
            ),
        })
    }

    /// A split attribute's prerequisites (over a lookup context: the
    /// coordinator, or an aspect job's frozen view).
    pub(super) fn split_attribute<C: super::prepare::Lookup + ?Sized>(
        cx: &mut C,
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
            let target = cx.target(&label)?;
            let mut prerequisites: Vec<(Option<ConfigId>, SplitPrerequisite)> = Vec::new();
            for (branch, config) in branches.iter() {
                let identity = match &target {
                    Target::Source(_) => None,
                    Target::Rule(rule) if matches!(rule.kind, RuleKind::PackageGroup(_)) => None,
                    Target::Rule(rule) | Target::Output(rule, _) => {
                        let key = ConfiguredTargetKey::new(rule.label.clone(), *config);
                        Some(
                            cx.engine()
                                .rule_delegations
                                .get(&key)
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
            prerequisites.sort_by(|(a, _), (b, _)| {
                let mnemonic = |id: Option<ConfigId>| {
                    id.map_or("", |id| cx.engine().configurations.mnemonic(id))
                };
                mnemonic(*a).cmp(mnemonic(*b)).then(a.cmp(b))
            });
            attribute
                .prerequisites
                .extend(prerequisites.into_iter().map(|(_, p)| p));
        }
        Ok(attribute)
    }
}
