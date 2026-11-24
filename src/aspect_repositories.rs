//! B62: the transitive repositories of an aspect application (genuine's
//! AspectValue transitive packages: TransitiveDependencyState over the
//! aspect's own attribute dependencies, its toolchains and the
//! applications it propagates to; analysis/producers/ConfiguredAspectProducer
//! adds them to the consumer's). An application carries them under the
//! reserved provider key [`PROVIDER_ID`] (no exported provider can name
//! it), so every path that carries an application's providers (the aspect
//! memo, the aspect jobs' table, a merged dependency view) carries them; a
//! Starlark rule's prepare adds those of its aspect-applied edges to its
//! own transitive repositories (its runfiles' repo mapping manifest).

use allocative::Allocative;
use starlark::starlark_simple_value;
use starlark::values::{
    Heap, NoSerialize, OwnedFrozenValue, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
    Value, ValueLike as _, starlark_value,
};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

/// The reserved provider key of an application's transitive repositories.
pub(crate) const PROVIDER_ID: &str = "builtin:AspectTransitiveRepositories";

// Only derive-generated markers require this exception: owned data only.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    /// The transitive repositories of an aspect application (or of the
    /// applications merged into one dependency view).
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct AspectRepositories {
        #[allocative(skip)]
        pub repositories: Arc<BTreeSet<String>>,
    }
}
pub(crate) use owned::AspectRepositories;

starlark_simple_value!(AspectRepositories);

impl fmt::Display for AspectRepositories {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} repositories>", self.repositories.len())
    }
}

#[starlark_value(type = "AspectRepositories")]
impl<'v> StarlarkValue<'v> for AspectRepositories {}

/// The value of `repositories` on an application's analysis heap.
pub(crate) fn alloc<'v>(heap: Heap<'v>, repositories: Arc<BTreeSet<String>>) -> Value<'v> {
    heap.alloc(AspectRepositories { repositories })
}

/// The repositories a provider map carries, if any.
pub(crate) fn of(
    providers: &std::collections::BTreeMap<String, OwnedFrozenValue>,
) -> Option<&Arc<BTreeSet<String>>> {
    providers
        .get(PROVIDER_ID)?
        .value()
        .downcast_ref::<AspectRepositories>()
        .map(|value| &value.repositories)
}

/// The union of several applications' repositories (a merged view over
/// more than one), frozen.
pub(crate) fn union<'a>(sets: impl Iterator<Item = &'a OwnedFrozenValue>) -> OwnedFrozenValue {
    let mut union = BTreeSet::new();
    for value in sets {
        if let Some(value) = value.value().downcast_ref::<AspectRepositories>() {
            union.extend(value.repositories.iter().cloned());
        }
    }
    starlark::environment::Module::with_temp_heap(|module| {
        let value = alloc(module.heap(), Arc::new(union));
        module.set("value", value);
        module
            .freeze()
            .expect("freezing owned repositories cannot fail")
            .get("value")
            .expect("the frozen module has the value")
    })
}
