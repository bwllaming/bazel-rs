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

#[derive(Debug, Default)]
pub(crate) struct HostBudget {
    bytes: usize,
    nodes: usize,
}

impl HostBudget {
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
        self.attr_value(&spec.default)
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
