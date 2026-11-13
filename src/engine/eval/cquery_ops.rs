//! cquery on the evaluator (docs/EVALUATOR.md "eval-coverage"): what the
//! walk writes into cquery's configured-graph capture (`cquery::Capture`)
//! while it analyzes, recorded by a body as an [`Event::Cquery`] op and
//! applied to the engine's capture in the tape's order (the walk's).
//!
//! The capture interns targets and aspect applications to dense ids in
//! the order the walk reaches them, and a body reads ids back (the
//! applications of an aspect collection, a toolchain context's targets) to
//! name them in later writes — also through a memo value another body
//! reuses (an aspect's toolchain contexts). A body cannot know the walk's
//! ids, so it gets **symbols**: ids of an evaluation-wide intern keyed by
//! content (the same application or context is the same symbol in every
//! body and attempt, so memo values compare equal), which the replay maps
//! to the capture's ids where the op that made them is applied.
//!
//! [`Event::Cquery`]: super::transcript::Event::Cquery

use std::sync::{Arc, Mutex};

use crate::configuration::ConfigId;
use crate::configured_attributes::ConfigMatching;
use crate::fast_hash::FxHashMap;
use crate::label::Label;

use super::super::{ConfiguredTargetKey, Rule};

/// One capture write of a body (symbols for the capture's ids).
#[derive(Clone, Debug)]
pub(crate) enum CqueryOp {
    /// A configured rule (`capture.rules`), incompatible with these
    /// conditions (`capture.incompatible`), directly so
    /// (`capture.direct_incompatible`).
    Rule {
        key: ConfiguredTargetKey,
        rule: Arc<Rule>,
        incompatible: Option<Vec<(String, ConfigMatching)>>,
        direct: bool,
    },
    /// `capture.applications(base, collection, rule)`, whose ids are
    /// `symbols`.
    Applications {
        base: ConfiguredTargetKey,
        collection: crate::aspects::AspectCollection,
        rule: Arc<Rule>,
        symbols: Vec<u32>,
    },
    /// `capture.request(requester, ids)`.
    Request {
        requester: ConfiguredTargetKey,
        ids: Vec<u32>,
    },
    /// `capture.aspect_targets(id, dependencies)`.
    AspectTargets { id: u32, dependencies: Vec<u32> },
    /// `capture.aspect_target(id, label, config)`.
    AspectTarget {
        id: u32,
        label: Label,
        config: ConfigId,
    },
    /// `capture.propagated(ids, collection, children, attribute, attached)`.
    Propagated {
        ids: Vec<u32>,
        collection: crate::aspects::AspectCollection,
        children: Vec<u32>,
        attribute: String,
        attached: Vec<Arc<crate::aspects::AspectDefinition>>,
    },
    /// `Engine::capture_cquery_aspect_toolchain_context`: the captured
    /// rules among `labels` (its candidates, in order) in `config`, whose
    /// ids are the list `symbol`.
    Context {
        config: ConfigId,
        labels: Vec<Label>,
        symbol: u32,
    },
}

impl CqueryOp {
    /// What identifies the op (equality and hashing: a rerun's events must
    /// equal its abandoned attempt's; rules and aspects by name).
    fn identity(&self) -> String {
        let descriptors = |collection: &crate::aspects::AspectCollection| {
            collection
                .used
                .iter()
                .map(|(aspect, bases)| format!("{}<{}>", aspect.descriptor, bases.join(",")))
                .collect::<Vec<_>>()
                .join(";")
        };
        match self {
            Self::Rule {
                key,
                rule,
                incompatible,
                direct,
            } => format!("rule {key} {} {incompatible:?} {direct}", rule.label),
            Self::Applications {
                base,
                collection,
                rule,
                symbols,
            } => format!(
                "applications {base} {} {} {symbols:?}",
                descriptors(collection),
                rule.label
            ),
            Self::Request { requester, ids } => format!("request {requester} {ids:?}"),
            Self::AspectTargets { id, dependencies } => {
                format!("aspect_targets {id} {dependencies:?}")
            }
            Self::AspectTarget { id, label, config } => {
                format!("aspect_target {id} {label} {config:?}")
            }
            Self::Propagated {
                ids,
                collection,
                children,
                attribute,
                attached,
            } => format!(
                "propagated {ids:?} {} {children:?} {attribute} {:?}",
                descriptors(collection),
                attached
                    .iter()
                    .map(|aspect| aspect.descriptor.as_str())
                    .collect::<Vec<_>>()
            ),
            Self::Context {
                config,
                labels,
                symbol,
            } => format!("context {config:?} {labels:?} {symbol}"),
        }
    }
}

impl PartialEq for CqueryOp {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}

impl Eq for CqueryOp {}

impl std::hash::Hash for CqueryOp {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.identity().hash(state);
    }
}

/// The evaluation's symbols (one per content: an application, a toolchain
/// context's candidates).
#[derive(Default)]
pub(crate) struct Symbols {
    map: Mutex<FxHashMap<String, u32>>,
}

impl Symbols {
    fn intern(&self, key: String) -> u32 {
        let mut map = self
            .map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next = map.len() as u32;
        *map.entry(key).or_insert(next)
    }

    /// The symbols of `capture.applications(base, collection, _)`: each
    /// used aspect by its descriptor and the symbols of the preceding
    /// applications it requires (as the capture keys them).
    pub(crate) fn applications(
        &self,
        base: &ConfiguredTargetKey,
        collection: &crate::aspects::AspectCollection,
    ) -> Vec<u32> {
        let mut ids: Vec<u32> = Vec::with_capacity(collection.used.len());
        for (aspect, bases) in &collection.used {
            let required: Vec<u32> = collection
                .used
                .iter()
                .take(ids.len())
                .enumerate()
                .filter(|(_, (aspect, _))| bases.contains(&aspect.descriptor))
                .map(|(index, _)| ids[index])
                .collect();
            let id = self.intern(format!("a\0{base:?}\0{}\0{required:?}", aspect.descriptor));
            ids.push(id);
        }
        ids
    }

    /// The symbol of a toolchain context's candidate list in `config`, of
    /// the aspect `owner`'s toolchain contexts (a memo the walk computes
    /// once: its lists are the capture's state then).
    pub(crate) fn context(&self, config: ConfigId, owner: &str, labels: &[Label]) -> u32 {
        self.intern(format!("c\0{config:?}\0{owner}\0{labels:?}"))
    }
}

/// The replay's map from symbols to the capture's ids.
#[derive(Default)]
pub(crate) struct Replay {
    ids: FxHashMap<u32, Vec<u32>>,
    /// The first replay error (an internal error the build reports).
    error: Option<String>,
}

impl Replay {
    /// Keep the first replay error.
    pub(crate) fn fail(&mut self, error: String) {
        self.error.get_or_insert(error);
    }

    /// The replay error, if any.
    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The capture ids of `symbols` (a list symbol stands for its ids);
    /// `Err`: a symbol whose op was not applied (an internal error).
    pub(crate) fn map(&self, symbols: &[u32]) -> Result<Vec<u32>, String> {
        let mut ids = Vec::with_capacity(symbols.len());
        for symbol in symbols {
            let Some(mapped) = self.ids.get(symbol) else {
                return Err(format!(
                    "internal error: BAZEL_RS_ANALYSIS=eval: the cquery capture symbol {symbol} is used before the op that makes it; rerun with BAZEL_RS_ANALYSIS=walk"
                ));
            };
            ids.extend_from_slice(mapped);
        }
        Ok(ids)
    }

    /// The application `symbol` (one id).
    pub(crate) fn one(&self, symbol: u32) -> Result<u32, String> {
        match self.map(&[symbol])?.as_slice() {
            [id] => Ok(*id),
            _ => Err(format!(
                "internal error: BAZEL_RS_ANALYSIS=eval: the cquery capture symbol {symbol} is not one application; rerun with BAZEL_RS_ANALYSIS=walk"
            )),
        }
    }

    /// `symbols` name `ids` from now on (the first mapping stays: a memo's
    /// first use made it).
    pub(crate) fn define(&mut self, symbols: &[u32], ids: &[u32]) {
        for (symbol, id) in symbols.iter().zip(ids) {
            self.ids.entry(*symbol).or_insert_with(|| vec![*id]);
        }
    }

    /// The list symbol `symbol` names `ids` from now on.
    pub(crate) fn define_list(&mut self, symbol: u32, ids: Vec<u32>) {
        self.ids.entry(symbol).or_insert(ids);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols_are_by_content_and_lists_expand() {
        let symbols = Symbols::default();
        let config = crate::configuration::Configuration::default().id;
        let a = symbols.context(config, "x", &[Label::parse_canonical("//p:a").unwrap()]);
        let b = symbols.context(config, "x", &[Label::parse_canonical("//p:b").unwrap()]);
        assert_ne!(a, b);
        assert_eq!(
            symbols.context(config, "x", &[Label::parse_canonical("//p:a").unwrap()]),
            a
        );
        assert_ne!(
            symbols.context(config, "y", &[Label::parse_canonical("//p:a").unwrap()]),
            a
        );
        let mut replay = Replay::default();
        replay.define_list(a, vec![3, 4]);
        replay.define(&[b], &[7]);
        assert_eq!(replay.map(&[a, b]).unwrap(), [3, 4, 7]);
        assert_eq!(replay.one(b).unwrap(), 7);
        assert!(replay.one(a).is_err());
        assert!(replay.map(&[99]).is_err());
    }
}
