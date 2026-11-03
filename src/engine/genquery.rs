//! The native `genquery` rule (rules/genquery/GenQuery.java, docs/AQUERY.md):
//! the query result over the transitive closure of `scope`, computed at
//! analysis (crate::engine::query's `Engine::genquery`) and written by a
//! FileWriteAction-like native write (Bazel's QueryResultAction), gzipped
//! with `compressed_output`.
use super::*;

/// What a genquery target writes: its output's content.
pub(super) struct GenQueryOutput {
    pub content: Vec<u8>,
}

impl Engine {
    /// GenQuery.create: the query output of `rule` (a genquery).
    pub(super) fn genquery_output(&mut self, rule: &Rule) -> Result<GenQueryOutput, String> {
        let error = |message: String| {
            located(
                rule.location.as_deref(),
                format!(
                    "in genquery rule {}: {message}",
                    crate::rule_declarations::canonical_form(&rule.label)
                ),
            )
        };
        let attrs = rule
            .native
            .as_ref()
            .map(|native| &native.attrs)
            .ok_or_else(|| error("missing attributes".into()))?;
        use crate::custom_rule::AttrValue;
        let expression = match attrs.get("expression") {
            Some(AttrValue::String(text)) => text.clone(),
            _ => String::new(),
        };
        let opts = match attrs.get("opts") {
            Some(AttrValue::Strings(values)) => values.clone(),
            _ => Vec::new(),
        };
        let strict = !matches!(attrs.get("strict"), Some(AttrValue::Bool(false)));
        let compressed = matches!(attrs.get("compressed_output"), Some(AttrValue::Bool(true)));
        let mut scope = Vec::new();
        if let Some(AttrValue::Labels(labels)) = attrs.get("scope") {
            for text in labels {
                scope.push(self.resolve_rule_src(rule, text)?);
            }
        }
        let (bytes, warnings) = self
            .genquery(&expression, &opts, &scope, strict)
            .map_err(error)?;
        for warning in warnings {
            self.warnings.push(warning);
        }
        let content = if compressed {
            // GenQueryOutputStream(compressed): GZIP of the output.
            use std::io::Write as _;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&bytes).map_err(|e| e.to_string())?;
            encoder.finish().map_err(|e| e.to_string())?
        } else {
            bytes
        };
        Ok(GenQueryOutput { content })
    }
}
