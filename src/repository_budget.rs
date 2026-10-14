//! Conservative host-allocation profile for ordinary extension preparation.
//!
//! This is not a process memory limit or a sandbox. Each preparation phase has
//! a cumulative 64 MiB weighted-byte / 100,000-node budget. Borrowed inputs are
//! charged before copying: 128 bytes per node and eight bytes per payload byte
//! reserve space for retained copies, projection, and escaped JSON. Whole
//! schemas (including overridden and mandatory defaults) are charged deliberately:
//! this is a resource profile, not eager attribute validation or Bazel semantics.

use crate::custom_rule::{AttrSpec, AttrValue};
use crate::extension_calls::RecordedValue;
use std::collections::BTreeMap;

const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_NODES: usize = 100_000;
const MAX_DEPTH: usize = 64;
const COPY_WEIGHT: usize = 8;
const NODE_BYTES: usize = 128;

#[derive(Clone, Debug, Default)]
pub(crate) struct HostBudget {
    bytes: usize,
    nodes: usize,
}

impl HostBudget {
    /// Commit compound reservations only after every component succeeds.
    pub(crate) fn transaction(
        &mut self,
        reserve: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut candidate = self.clone();
        reserve(&mut candidate)?;
        *self = candidate;
        Ok(())
    }

    fn add_bytes(&mut self, bytes: usize) -> Result<(), String> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|n| *n <= MAX_BYTES)
            .ok_or_else(|| "host preparation weighted byte limit exceeded (64 MiB)".to_owned())?;
        Ok(())
    }

    /// Charge a borrowed payload, reserving space for copies and JSON escaping.
    pub(crate) fn bytes(&mut self, bytes: usize) -> Result<(), String> {
        let bytes = bytes
            .checked_mul(COPY_WEIGHT)
            .ok_or_else(|| "host preparation weighted byte limit exceeded (overflow)".to_owned())?;
        self.add_bytes(bytes)
    }

    pub(crate) fn node(&mut self) -> Result<(), String> {
        self.nodes = self
            .nodes
            .checked_add(1)
            .filter(|n| *n <= MAX_NODES)
            .ok_or_else(|| "host preparation node limit exceeded (100000 nodes)".to_owned())?;
        self.add_bytes(NODE_BYTES)
    }

    pub(crate) fn string(&mut self, value: &str) -> Result<(), String> {
        self.node()?;
        self.bytes(value.len())
    }

    pub(crate) fn schema(&mut self, schema: &BTreeMap<String, AttrSpec>) -> Result<(), String> {
        self.node()?;
        for (name, spec) in schema {
            self.string(name)?;
            self.spec(spec)?;
        }
        Ok(())
    }

    pub(crate) fn spec(&mut self, spec: &AttrSpec) -> Result<(), String> {
        self.node()?;
        self.attr_value(&spec.default)?;
        // Preserve the existing cost and serialization profile of empty metadata.
        if !spec.required_providers.is_empty() {
            self.node()?;
            for clause in &spec.required_providers {
                self.node()?;
                for provider in clause {
                    self.node()?;
                    self.string(&provider.id)?;
                    self.string(&provider.display_name)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn attr_value(&mut self, value: &AttrValue) -> Result<(), String> {
        self.node()?;
        match value {
            AttrValue::String(value) => self.bytes(value.len())?,
            AttrValue::Strings(values) | AttrValue::Labels(values) | AttrValue::Outputs(values) => {
                for value in values {
                    self.string(value)?;
                }
            }
            AttrValue::StringDict(entries) => {
                for (key, value) in entries {
                    self.string(key)?;
                    self.string(value)?;
                }
            }
            AttrValue::StringListDict(entries) => {
                for (key, values) in entries {
                    self.string(key)?;
                    self.node()?;
                    for value in values {
                        self.string(value)?;
                    }
                }
            }
            AttrValue::Label(value) | AttrValue::Output(value) => {
                if let Some(value) = value {
                    self.string(value)?;
                }
            }
            AttrValue::Int(_) | AttrValue::Bool(_) => {}
        }
        Ok(())
    }

    fn depth(&mut self, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err("host preparation maximum depth limit exceeded (64)".to_owned());
        }
        self.node()
    }

    pub(crate) fn json(&mut self, value: &serde_json::Value, depth: usize) -> Result<(), String> {
        self.depth(depth)?;
        match value {
            serde_json::Value::String(value) => self.bytes(value.len())?,
            serde_json::Value::Array(values) => {
                for value in values {
                    self.json(value, depth + 1)?;
                }
            }
            serde_json::Value::Object(values) => {
                for (name, value) in values {
                    self.string(name)?;
                    self.json(value, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn recorded(&mut self, value: &RecordedValue, depth: usize) -> Result<(), String> {
        self.depth(depth)?;
        match value {
            RecordedValue::String(value) | RecordedValue::Label(value) => {
                self.bytes(value.len())?
            }
            RecordedValue::List(values) => {
                for value in values {
                    self.recorded(value, depth + 1)?;
                }
            }
            RecordedValue::Dict(values) => {
                for (name, value) in values {
                    self.string(name)?;
                    self.recorded(value, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom_rule::AttrKind;

    #[test]
    fn string_dict_charges_root_keys_and_scalar_values_without_sequence_nodes() {
        let value =
            AttrValue::StringDict(vec![("z".into(), "first".into()), ("a".into(), "".into())]);
        let mut budget = HostBudget::default();
        budget.attr_value(&value).unwrap();
        assert_eq!(budget.nodes, 5);
        assert_eq!(budget.bytes, 5 * NODE_BYTES + COPY_WEIGHT * 7);
        let mut budget = HostBudget::default();
        budget.attr_value(&AttrValue::StringDict(vec![])).unwrap();
        assert_eq!((budget.nodes, budget.bytes), (1, NODE_BYTES));
        for value in [
            AttrValue::StringDict(vec![("x".repeat(1024 * 1024), "".into())]),
            AttrValue::StringDict(vec![("k".into(), "x".repeat(1024 * 1024))]),
        ] {
            let mut budget = HostBudget::default();
            for _ in 0..7 {
                budget.attr_value(&value).unwrap();
            }
            assert!(
                budget
                    .attr_value(&value)
                    .unwrap_err()
                    .contains("weighted byte limit")
            );
        }
        assert!(
            HostBudget::default()
                .attr_value(&AttrValue::StringDict(vec![
                    (String::new(), String::new());
                    50000
                ]))
                .unwrap_err()
                .contains("node limit")
        );
    }

    #[test]
    fn provider_metadata_is_charged_with_defaults_and_repeated_schemas() {
        let mut spec = AttrSpec {
            kind: AttrKind::Label,
            default: AttrValue::Label(None),
            mandatory: false,
            allow_files: false,
            allow_single_file: false,
            non_empty: false,
            required_providers: Vec::new(),
        };
        let mut empty = HostBudget::default();
        empty.spec(&spec).unwrap();
        assert_eq!((empty.nodes, empty.bytes), (2, 2 * NODE_BYTES));
        spec.required_providers = vec![vec![crate::providers::ProviderRequirement {
            id: "//defs:rules.bzl%P".into(),
            display_name: "P".into(),
        }]];
        let mut budget = HostBudget::default();
        budget.spec(&spec).unwrap();
        assert_eq!(budget.nodes, 7);
        assert_eq!(
            budget.bytes,
            7 * NODE_BYTES + COPY_WEIGHT * ("//defs:rules.bzl%P".len() + 1)
        );
        spec.required_providers[0][0].id = "x".repeat(1024 * 1024);
        let schema = BTreeMap::from([("dep".to_owned(), spec.clone())]);
        let mut budget = HostBudget::default();
        for _ in 0..7 {
            budget.schema(&schema).unwrap();
        }
        assert!(
            budget
                .schema(&schema)
                .unwrap_err()
                .contains("weighted byte limit")
        );
        spec.default = AttrValue::Label(Some("x".repeat(7 * 1024 * 1024)));
        assert!(
            HostBudget::default()
                .spec(&spec)
                .unwrap_err()
                .contains("weighted byte limit")
        );
        spec.default = AttrValue::Label(None);
        spec.required_providers[0][0].id.clear();
        spec.required_providers[0][0].display_name = "x".repeat(9 * 1024 * 1024);
        assert!(
            HostBudget::default()
                .spec(&spec)
                .unwrap_err()
                .contains("weighted byte limit")
        );
    }

    #[test]
    fn string_list_dict_charges_root_keys_sequences_and_elements() {
        let value = AttrValue::StringListDict(vec![
            ("z".into(), vec!["first".into(), "second".into()]),
            ("empty".into(), vec![]),
        ]);
        let mut budget = HostBudget::default();
        budget.attr_value(&value).unwrap();
        assert_eq!(budget.nodes, 7);
        assert_eq!(budget.bytes, 7 * NODE_BYTES + COPY_WEIGHT * (1 + 5 + 6 + 5));

        let mut budget = HostBudget::default();
        budget
            .attr_value(&AttrValue::StringListDict(vec![]))
            .unwrap();
        assert_eq!(budget.nodes, 1);
        for value in [
            AttrValue::StringListDict(vec![("x".repeat(1024 * 1024), vec![])]),
            AttrValue::StringListDict(vec![("x".into(), vec!["x".repeat(1024 * 1024)])]),
        ] {
            let mut budget = HostBudget::default();
            for _ in 0..7 {
                budget.attr_value(&value).unwrap();
            }
            assert!(
                budget
                    .attr_value(&value)
                    .unwrap_err()
                    .contains("weighted byte limit")
            );
        }
        let value =
            AttrValue::StringListDict(vec![(String::new(), vec![String::new(); MAX_NODES - 2])]);
        assert!(
            HostBudget::default()
                .attr_value(&value)
                .unwrap_err()
                .contains("node limit")
        );
    }

    #[test]
    fn defaults_and_repeated_schema_aliases_are_charged_without_copying() {
        let schema = BTreeMap::from([(
            "labels".to_owned(),
            AttrSpec {
                kind: AttrKind::LabelList,
                default: AttrValue::Labels(vec!["x".repeat(1024 * 1024)]),
                mandatory: false,
                allow_files: false,
                allow_single_file: false,
                non_empty: false,
                required_providers: Vec::new(),
            },
        )]);
        let mut budget = HostBudget::default();
        for _ in 0..7 {
            budget.schema(&schema).unwrap();
        }
        assert!(
            budget
                .schema(&schema)
                .unwrap_err()
                .contains("host preparation weighted byte limit")
        );
        let mut budget = HostBudget::default();
        budget
            .attr_value(&AttrValue::Label(Some("//:default".to_owned())))
            .unwrap();
        assert_eq!(
            budget.bytes,
            2 * NODE_BYTES + COPY_WEIGHT * "//:default".len()
        );
    }

    #[test]
    fn empty_nodes_unknown_nulls_depth_and_overflow_are_bounded() {
        let mut budget = HostBudget::default();
        for _ in 0..MAX_NODES {
            budget.json(&serde_json::Value::Null, 0).unwrap();
        }
        assert!(budget.node().unwrap_err().contains("node limit"));
        assert!(
            HostBudget::default()
                .bytes(usize::MAX)
                .unwrap_err()
                .contains("overflow")
        );
        let mut value = serde_json::Value::Null;
        for _ in 0..=MAX_DEPTH {
            value = serde_json::Value::Array(vec![value]);
        }
        assert!(
            HostBudget::default()
                .json(&value, 0)
                .unwrap_err()
                .contains("depth limit")
        );
        let mut budget = HostBudget::default();
        budget
            .json(&serde_json::json!({"unknown": null}), 0)
            .unwrap();
        assert_eq!(budget.nodes, 3);
        assert_eq!(budget.bytes, 3 * NODE_BYTES + COPY_WEIGHT * "unknown".len());
    }
}
