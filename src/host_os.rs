//! Immutable compile-target metadata. Only Linux/x86_64 has a reference-backed
//! mapping; this is not runtime detection or a general cross-platform profile.
use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::collections::StarlarkHasher;
use starlark::environment::{Methods, MethodsBuilder};
use starlark::starlark_module;
use starlark::values::{Heap, Value, ValueLike};
use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use crate::repository_budget::HostBudget;

#[derive(Debug, Allocative)]
pub(crate) struct HostMetadata {
    name: String,
    arch: String,
    // Extension factors use the canonical OS enum, not the JVM getter name.
    os_factor: String,
}

impl HostMetadata {
    fn bounded(name: &str, arch: &str, os_factor: &str) -> Result<Self, String> {
        let mut budget = HostBudget::default();
        budget.node()?;
        for value in [name, arch, os_factor] {
            budget.string(value)?;
        }
        Ok(Self {
            name: name.to_owned(),
            arch: arch.to_owned(),
            os_factor: os_factor.to_owned(),
        })
    }

    fn current() -> Result<Self, String> {
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Self::bounded("linux", "amd64", "linux")
        } else {
            Err("repository host metadata is unsupported for this compile target (requires Linux/x86_64)".to_owned())
        }
    }

    fn charge(&self, budget: &mut HostBudget) -> Result<(), String> {
        budget.node()?;
        budget.string(&self.name)?;
        budget.string(&self.arch)?;
        budget.string(&self.os_factor)
    }

    pub(crate) fn factors<'a>(
        &'a self,
        os: bool,
        arch: bool,
        budget: &mut HostBudget,
    ) -> Result<HostFactors<'a>, String> {
        budget.node()?;
        let factors = HostFactors {
            os: os.then_some(self.os_factor.as_str()),
            arch: arch.then_some(self.arch.as_str()),
        };
        for value in [factors.os, factors.arch].into_iter().flatten() {
            budget.string(value)?;
        }
        Ok(factors)
    }

    #[cfg(test)]
    pub(crate) fn fixture(name: &str, arch: &str, os_factor: &str) -> Arc<Self> {
        // Synthetic native-only fixtures, not runtime evidence for other hosts.
        Arc::new(Self::bounded(name, arch, os_factor).unwrap())
    }
}

#[derive(Debug, Default)]
pub(crate) struct HostSnapshot {
    metadata: OnceLock<Result<Arc<HostMetadata>, String>>,
}

impl HostSnapshot {
    pub(crate) fn get(&self) -> Result<Arc<HostMetadata>, String> {
        self.metadata
            .get_or_init(|| HostMetadata::current().map(Arc::new))
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn fixture(metadata: Result<Arc<HostMetadata>, String>) -> Self {
        Self {
            metadata: OnceLock::from(metadata),
        }
    }
}

#[derive(Debug, Default, serde::Serialize)]
pub(crate) struct HostFactors<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    os: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arch: Option<&'a str>,
}

impl HostFactors<'_> {
    pub(crate) fn is_empty(&self) -> bool {
        self.os.is_none() && self.arch.is_none()
    }
}

/// Version inside the hashed execution inputs deliberately rekeys every executed
/// repository once. The legacy ExecutionInputs serializer remains unchanged.
pub(crate) fn repository_inputs(
    host: &HostMetadata,
    execution: &crate::repository_ctx::ExecutionInputs,
) -> Result<Vec<u8>, String> {
    #[derive(serde::Serialize)]
    struct Host<'a> {
        name: &'a str,
        arch: &'a str,
    }
    #[derive(serde::Serialize)]
    struct HostRepoFingerprint<'a> {
        host_input_profile: &'static str,
        host: Host<'a>,
        execution: &'a crate::repository_ctx::ExecutionInputs,
    }
    let mut budget = HostBudget::default();
    budget.node()?;
    budget.string("repository-host-v1")?;
    host.charge(&mut budget)?;
    serde_json::to_vec(&HostRepoFingerprint {
        host_input_profile: "repository-host-v1",
        host: Host {
            name: &host.name,
            arch: &host.arch,
        },
        execution,
    })
    .map_err(|error| format!("cannot serialize repository execution inputs: {error}"))
}

const REPR: &str =
    "<unknown object com.google.devtools.build.lib.bazel.repository.starlark.StarlarkOS>";
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_identity(counter: &AtomicU64) -> anyhow::Result<u64> {
    counter
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| anyhow!("repository_os identity space exhausted"))
}

// Only derive-generated marker implementations need unsafe permission. This
// simple value contains owned Rust metadata and an identity, never heap values,
// evaluators, environment capabilities or watches. Freezing retains identity.
#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::values::{
        NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, starlark_value,
    };

    #[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct RepositoryOs {
        pub metadata: Arc<HostMetadata>,
        pub id: u64,
    }
    starlark::starlark_simple_value!(RepositoryOs);

    impl fmt::Display for RepositoryOs {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(REPR)
        }
    }
    #[starlark_value(type = "repository_os")]
    impl<'v> StarlarkValue<'v> for RepositoryOs {
        fn get_methods() -> Option<&'static Methods> {
            Some(OS_METHODS.methods())
        }
        fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
            Ok(other
                .downcast_ref::<Self>()
                .is_some_and(|other| self.id == other.id))
        }
        fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
            self.id.hash(hasher);
            Ok(())
        }
    }
}
use values::RepositoryOs;

starlark::methods_static!(OS_METHODS = properties);

#[starlark_module]
fn properties(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn name<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let host = this
            .downcast_ref::<RepositoryOs>()
            .ok_or_else(|| anyhow!("expected repository_os"))?;
        HostBudget::default()
            .string(&host.metadata.name)
            .map_err(anyhow::Error::msg)?;
        Ok(heap.alloc(host.metadata.name.as_str()))
    }
    #[starlark(attribute)]
    fn arch<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let host = this
            .downcast_ref::<RepositoryOs>()
            .ok_or_else(|| anyhow!("expected repository_os"))?;
        HostBudget::default()
            .string(&host.metadata.arch)
            .map_err(anyhow::Error::msg)?;
        Ok(heap.alloc(host.metadata.arch.as_str()))
    }
    #[starlark(attribute)]
    fn environ<'v>(this: Value<'v>) -> anyhow::Result<Value<'v>> {
        let _ = this
            .downcast_ref::<RepositoryOs>()
            .ok_or_else(|| anyhow!("expected repository_os"))?;
        bail!("repository_os.environ is unsupported in the native host metadata profile")
    }
}

pub(crate) fn alloc<'v>(heap: Heap<'v>, metadata: Arc<HostMetadata>) -> anyhow::Result<Value<'v>> {
    metadata
        .charge(&mut HostBudget::default())
        .map_err(anyhow::Error::msg)?;
    Ok(heap.alloc(RepositoryOs {
        metadata,
        id: next_identity(&NEXT_ID)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::Module;
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};

    fn evaluate(source: &str) -> Result<(), String> {
        Module::with_temp_heap(|module| {
            let host = HostSnapshot::default().get().unwrap();
            module.set("x", alloc(module.heap(), host.clone()).unwrap());
            module.set("y", alloc(module.heap(), host).unwrap());
            Evaluator::new(&module)
                .eval_module(
                    AstModule::parse("host.star", source.to_owned(), &Dialect::Standard).unwrap(),
                    &crate::package::bzl_globals(),
                )
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }

    #[test]
    fn reference_linux_metadata_and_structural_surface() {
        evaluate(&format!(
            r#"
(type(x) == 'repository_os') or fail('type')
(dir(x) == ['arch', 'environ', 'name']) or fail('dir')
(str(x) == {REPR:?} and repr(x) == {REPR:?}) or fail('repr')
(x.name == 'linux' and x.arch == 'amd64') or fail('metadata')
(type(x.name) == 'string' and type(x.arch) == 'string') or fail('strings')
(x.name.lower().startswith('lin') and x.name.endswith('ux')) or fail('operations')
(json.encode(x.name) == '\"linux\"' and json.encode(x.arch) == '\"amd64\"') or fail('json')
hasattr(x, 'environ') or fail('structural presence')
(not hasattr(x, 'unknown') and getattr(x, 'unknown', 'fallback') == 'fallback') or fail('fallback')
({{x:'safe'}}[x] == 'safe' and x == x) or fail('hash')
# Distinct allocation identity is source-derived, not a cross-context reference probe.
(x != y and len({{x:1,y:2}}) == 2 and x != x.name) or fail('identity')
"#
        ))
        .unwrap();
    }

    #[test]
    fn unsupported_property_does_not_fall_back_or_serialize() {
        for source in ["x.environ", "getattr(x, 'environ', {})"] {
            let error = evaluate(source).unwrap_err();
            assert!(
                error.contains("repository_os.environ is unsupported"),
                "{error}"
            );
        }
        for source in [
            "x.name = 'changed'",
            "x.arch = 'changed'",
            "x.environ = {}",
            "x.unknown",
        ] {
            assert!(evaluate(source).is_err(), "{source}");
        }
        let error = evaluate("json.encode(x)").unwrap_err();
        assert!(
            error.contains("cannot encode unsupported value as JSON"),
            "{error}"
        );
    }

    #[test]
    fn freeze_retains_identity_fields_and_ownership_after_owner_drop() {
        let metadata = HostMetadata::fixture("synthetic name", "synthetic arch", "canonical");
        let weak = Arc::downgrade(&metadata);
        let (frozen, id, hash) = Module::with_temp_heap(|module| {
            let x = alloc(module.heap(), metadata.clone()).unwrap();
            let id = x.downcast_ref::<RepositoryOs>().unwrap().id;
            let hash = x.get_hashed().unwrap().hash();
            module.set("x", x);
            let frozen = module.freeze().unwrap();
            (frozen.get("x").unwrap(), id, hash)
        });
        drop(metadata);
        assert!(weak.upgrade().is_some());
        Module::with_temp_heap(|module| {
            let x = module.heap().access_owned_frozen_value(&frozen);
            assert_eq!(x.downcast_ref::<RepositoryOs>().unwrap().id, id);
            assert_eq!(x.get_hashed().unwrap().hash(), hash);
            module.set("x", x);
            drop(frozen);
            Evaluator::new(&module).eval_module(
                AstModule::parse("frozen.star", "(x.name == 'synthetic name' and x.arch == 'synthetic arch' and {x:42}[x] == 42) or fail('frozen')".to_owned(), &Dialect::Standard).unwrap(),
                &crate::package::bzl_globals(),
            ).unwrap();
        });
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn local_identity_counter_exhaustion_never_wraps() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(next_identity(&counter).unwrap(), u64::MAX - 1);
        for _ in 0..2 {
            assert!(
                next_identity(&counter)
                    .unwrap_err()
                    .to_string()
                    .contains("exhausted")
            );
            assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
        }
        let counter = AtomicU64::new(1);
        std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        (0..100)
                            .map(|_| next_identity(&counter).unwrap())
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            let ids: std::collections::BTreeSet<_> = threads
                .into_iter()
                .flat_map(|t| t.join().unwrap())
                .collect();
            assert_eq!(ids.len(), 800);
        });
    }

    #[test]
    fn versioned_repository_frame_keeps_inner_legacy_bytes_and_excludes_factors_and_identity() {
        let inputs = crate::repository_ctx::ExecutionInputs::default();
        let host = HostMetadata::fixture("linux", "amd64", "linux");
        let bytes = repository_inputs(&host, &inputs).unwrap();
        assert_eq!(serde_json::to_vec(&inputs).unwrap(), br#"{"templates":[]}"#);
        assert_eq!(bytes, br#"{"host_input_profile":"repository-host-v1","host":{"name":"linux","arch":"amd64"},"execution":{"templates":[]}}"#);
        let canonical_only = HostMetadata::fixture("linux", "amd64", "other");
        assert_eq!(bytes, repository_inputs(&canonical_only, &inputs).unwrap());
        for host in [
            HostMetadata::fixture("other", "amd64", "linux"),
            HostMetadata::fixture("linux", "other", "linux"),
        ] {
            assert_ne!(bytes, repository_inputs(&host, &inputs).unwrap());
        }
        assert!(HostMetadata::bounded(&"x".repeat(9 * 1024 * 1024), "a", "b").is_err());
    }
}
