//! `--allow_analysis_failures` (CoreOptions.java:790-802): in a
//! configuration that allows analysis failures, a rule target whose
//! analysis fails, or one of whose prerequisites carries an
//! `AnalysisFailureInfo`, becomes a configured target that provides
//! `AnalysisFailureInfo` with the failures as `causes`
//! (ConfiguredTargetFactory.erroredConfiguredTarget and
//! erroredConfiguredTargetWithFailures,
//! analysis/ConfiguredTargetFactory.java:410-417, 567-622). skylib's
//! `analysistest.make(expect_failure = True)` and rules_testing's
//! `expect_failure` read it (docs/ANALYSIS_TESTS.md).

use allocative::Allocative;
use starlark::environment::Module;
use starlark::starlark_simple_value;
use starlark::values::{
    Heap, NoSerialize, OwnedFrozenValue, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
    Value, starlark_value,
};
use std::fmt;
use std::sync::Arc;

use crate::label::Label;
use crate::repository::RepositorySet;
use crate::starlark_types::{Depset, DepsetNode, Item, Order};

/// The provider key of the native `AnalysisFailureInfo`
/// (crate::rule_declarations::testing_provider_identity).
pub(crate) const PROVIDER_ID: &str = "builtin:AnalysisFailureInfo";

/// The element identity of an `AnalysisFailure` (analysis/test/AnalysisFailure.java).
const FAILURE_ID: &str = "builtin:AnalysisFailure";

// Only derive-generated markers require this exception; the value owns a
// depset, never a heap reference.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    /// AnalysisFailureInfo (analysis/test/AnalysisFailureInfo.java): its
    /// `causes`, a depset of AnalysisFailure.
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct AnalysisFailureInfo {
        pub causes: Depset,
    }
}
use owned::AnalysisFailureInfo;

starlark_simple_value!(AnalysisFailureInfo);

impl fmt::Display for AnalysisFailureInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<instance of AnalysisFailureInfo>")
    }
}

#[starlark_value(type = "AnalysisFailureInfo")]
impl<'v> StarlarkValue<'v> for AnalysisFailureInfo {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        (name == "causes").then(|| heap.alloc(self.causes.clone()))
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["causes".to_owned()]
    }
}

fn freeze(info: AnalysisFailureInfo) -> OwnedFrozenValue {
    Module::with_temp_heap(|module| {
        let value = module.heap().alloc(info);
        module.set("value", value);
        module
            .freeze()
            .expect("freezing an owned AnalysisFailureInfo cannot fail")
            .get("value")
            .expect("the frozen module has the value")
    })
}

/// AnalysisFailureInfo.forAnalysisFailures: one AnalysisFailure(label,
/// message) per suppressed error of the target `label`, in order.
pub(crate) fn failure_info(label: &Label, messages: &[String]) -> anyhow::Result<OwnedFrozenValue> {
    let label_item = Module::with_temp_heap(|module| -> anyhow::Result<Item> {
        let value = crate::starlark_types::alloc_label(module.heap(), &label.to_string())?;
        crate::starlark_types::label_item(value)
            .ok_or_else(|| anyhow::anyhow!("internal error: not a label"))
    })?;
    let direct = messages
        .iter()
        .map(|message| {
            Item::Struct(Arc::new(crate::depset_items::StructItem::new(
                Some((FAILURE_ID.to_owned(), "AnalysisFailure".to_owned())),
                vec![
                    ("label".to_owned(), label_item.clone()),
                    ("message".to_owned(), Item::String(message.clone())),
                ],
                None,
            )))
        })
        .collect();
    let causes = Depset::compose_framework(direct, Vec::new(), Order::Default, None)?;
    Ok(freeze(AnalysisFailureInfo { causes }))
}

/// AnalysisFailureInfo.forAnalysisFailureSets: the prerequisites' causes,
/// one transitive set each, in stable order.
pub(crate) fn propagated(children: Vec<Arc<DepsetNode>>) -> anyhow::Result<OwnedFrozenValue> {
    let causes = Depset::compose_framework(Vec::new(), children, Order::Default, None)?;
    Ok(freeze(AnalysisFailureInfo { causes }))
}

/// The causes of an AnalysisFailureInfo provider value.
pub(crate) fn causes(provider: &OwnedFrozenValue) -> Option<Arc<DepsetNode>> {
    AnalysisFailureInfo::from_value(provider.value()).map(|info| info.causes.node.clone())
}

/// Whether an analysis error is a rule error Bazel would suppress, rather
/// than a native-profile gap or an internal error (which stay failures:
/// suppressing them would turn an unmodelled feature into an expected
/// failure).
pub(crate) fn suppressible(message: &str) -> bool {
    !(message.contains(crate::rule_declarations::NOT_ANALYZED)
        || message.contains("not implemented in the native profile")
        || message.contains("internal error")
        // The engine's retry signals (a deferred target platform, a
        // repository the compute may not fetch): not analysis results.
        || message.contains(crate::platforms::NEEDS_TARGET_PLATFORM)
        || crate::repository_demand::is_needs_repository(message)
        || crate::repository_demand::is_parallel_bzl_cycle(message))
}

/// EvalException.getMessageWithStack (net/starlark/java/eval/EvalException.java)
/// of an implementation error, as a suppressed rule error records it
/// (`"\n" + getMessageWithStack()`, StarlarkRuleConfiguredTargetUtil):
/// `Traceback (most recent call last):`, then per Starlark frame
/// `\tFile "PATH", line L, column C, in NAME` and its trimmed source line,
/// then `Error in BUILTIN: MESSAGE` (or `Error: MESSAGE`).
pub(crate) fn message_with_stack(error: &starlark::Error, repositories: &RepositorySet) -> String {
    let frames = &error.call_stack().frames;
    let mut text = String::from("\nTraceback (most recent call last):\n");
    let mut builtin = None;
    for (index, frame) in frames.iter().enumerate() {
        let position = match frames.get(index + 1) {
            Some(next) => next.location.as_ref(),
            None if index > 0
                && (error.span().is_none() || error.span() == frame.location.as_ref()) =>
            {
                builtin = Some(frame.name.as_str());
                continue;
            }
            None => error.span(),
        };
        let Some(location) = position else {
            continue;
        };
        let begin = location.resolve_span().begin;
        // Java locates a call at its `(` (CallExpression.getLocation).
        let source = location.source_span();
        let column = match source.find('(') {
            Some(paren)
                if source.ends_with(')')
                    && source[..paren]
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '.') =>
            {
                begin.column + paren
            }
            _ => begin.column,
        };
        let filename = location.filename();
        let path = Label::parse_canonical(filename)
            .ok()
            .and_then(|label| {
                repositories
                    .root(&label.repository)
                    .ok()
                    .map(|root| root.join(&label.package).join(&label.name))
            })
            .map_or_else(|| filename.to_owned(), |path| path.display().to_string());
        text.push_str(&format!(
            "\tFile \"{path}\", line {}, column {}, in {}\n",
            begin.line + 1,
            column + 1,
            frame.name
        ));
        let line = location.file.source_line(begin.line).trim();
        if !line.is_empty() {
            text.push_str(&format!("\t\t{line}\n"));
        }
    }
    let message = error.kind().to_string();
    let message = message.trim().to_owned();
    match builtin {
        Some(name) => {
            text.push_str(&format!("Error in {name}: "));
            let own = format!("{name}: ");
            text.push_str(message.strip_prefix(&own).unwrap_or(&message));
        }
        None => {
            text.push_str("Error: ");
            text.push_str(&message);
        }
    }
    text
}
