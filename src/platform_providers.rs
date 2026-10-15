//! Starlark views of the native platform providers: `dep[platform_common.X]`
//! and `platform_common.X in dep` for ConstraintSettingInfo,
//! ConstraintValueInfo and PlatformInfo (crate::platforms), and the argument
//! of `ctx.target_platform_has_constraint` (crate::analysis_ctx).
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths are relative to `src/main/java/com/google/devtools/build/lib/`.
//! Without `--experimental_platforms_api` (not a native option) the only
//! field Starlark sees is `ConstraintSettingInfo.has_default_constraint_value`
//! (starlarkbuildapi/platform/ConstraintSettingInfoApi.java:36-56); `label`,
//! `constraint`, `default_constraint_value` and `constraints` are flag-gated
//! (ConstraintValueInfoApi.java:37-51, PlatformInfoApi.java:42-56).
use allocative::Allocative;
use starlark::collections::StarlarkHasher;
use starlark::environment::Module;
use starlark::starlark_simple_value;
use starlark::values::{
    Heap, NoSerialize, OwnedFrozenValue, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
    Value, ValueLike, starlark_value,
};
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

use crate::platforms::{
    ConstraintCollection, ConstraintSettingInfo, ConstraintValueInfo, PlatformInfo,
};

/// Provider keys: BuiltinProvider.requirement_id of `platform_common.X`
/// (crate::language_modules).
pub(crate) const CONSTRAINT_SETTING_ID: &str = "builtin:ConstraintSettingInfo";
pub(crate) const CONSTRAINT_VALUE_ID: &str = "builtin:ConstraintValueInfo";
pub(crate) const PLATFORM_ID: &str = "builtin:PlatformInfo";

// Simple native values owning their Arc data; no Starlark heap references.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct ConstraintSettingValue {
        #[allocative(skip)]
        pub info: Arc<ConstraintSettingInfo>,
    }

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct ConstraintValueValue {
        #[allocative(skip)]
        pub info: Arc<ConstraintValueInfo>,
    }

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct PlatformValue {
        #[allocative(skip)]
        pub info: Arc<PlatformInfo>,
    }
}
use owned::{ConstraintSettingValue, ConstraintValueValue, PlatformValue};
starlark_simple_value!(ConstraintSettingValue);
starlark_simple_value!(ConstraintValueValue);
starlark_simple_value!(PlatformValue);

fn shown(label: &crate::label::Label) -> String {
    crate::rule_declarations::canonical_form(label)
}

/// ConstraintCollection.repr (analysis/platform/ConstraintCollection.java:233-247).
fn collection_repr(collection: &ConstraintCollection) -> String {
    let mut text = String::from("<");
    if let Some(parent) = &collection.parent {
        text.push_str("parent: ");
        text.push_str(&collection_repr(parent));
        text.push_str(", ");
    }
    text.push('[');
    text.push_str(
        &collection
            .values
            .iter()
            .map(|value| shown(&value.label))
            .collect::<Vec<_>>()
            .join(", "),
    );
    text.push_str("]>");
    text
}

/// ConstraintSettingInfo.repr (ConstraintSettingInfo.java:114-124).
impl fmt::Display for ConstraintSettingValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ConstraintSettingInfo({}", shown(&self.info.label))?;
        if let Some(default) = &self.info.default_value {
            write!(f, ", default_constraint_value={}", shown(default))?;
        }
        if let Some(refined) = &self.info.refined {
            write!(f, ", refines_constraint_value={}", shown(&refined.label))?;
        }
        f.write_str(")")
    }
}

/// ConstraintValueInfo.repr (ConstraintValueInfo.java:96-99).
impl fmt::Display for ConstraintValueValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ConstraintValueInfo(setting={}, {})",
            shown(&self.info.setting.label),
            shown(&self.info.label)
        )
    }
}

/// PlatformInfo.repr (PlatformInfo.java).
impl fmt::Display for PlatformValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "PlatformInfo({}, constraints={})",
            shown(&self.info.label),
            collection_repr(&self.info.constraints)
        )
    }
}

#[starlark_value(type = "ConstraintSettingInfo")]
impl<'v> StarlarkValue<'v> for ConstraintSettingValue {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        (attribute == "has_default_constraint_value")
            .then(|| heap.alloc(self.info.default_value.is_some()))
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["has_default_constraint_value".to_owned()]
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other
            .downcast_ref::<ConstraintSettingValue>()
            .is_some_and(|other| *other.info == *self.info))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.info.label.hash(hasher);
        Ok(())
    }
}

#[starlark_value(type = "ConstraintValueInfo")]
impl<'v> StarlarkValue<'v> for ConstraintValueValue {
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other
            .downcast_ref::<ConstraintValueValue>()
            .is_some_and(|other| *other.info == *self.info))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.info.setting.label.hash(hasher);
        self.info.label.hash(hasher);
        Ok(())
    }
}

#[starlark_value(type = "PlatformInfo")]
impl<'v> StarlarkValue<'v> for PlatformValue {
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other
            .downcast_ref::<PlatformValue>()
            .is_some_and(|other| *other.info == *self.info))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.info.label.hash(hasher);
        Ok(())
    }
}

/// One native platform provider of a configured target.
pub(crate) enum Provider {
    Setting(ConstraintSettingInfo),
    Value(ConstraintValueInfo),
    Platform(Arc<PlatformInfo>),
}

/// The provider key and an owned frozen Starlark value of `provider`, for
/// the target's provider map (crate::engine).
pub(crate) fn freeze(provider: Provider) -> Result<(String, OwnedFrozenValue), String> {
    Module::with_temp_heap(|module| {
        let heap = module.heap();
        let (id, value) = match provider {
            Provider::Setting(info) => (
                CONSTRAINT_SETTING_ID,
                heap.alloc(ConstraintSettingValue {
                    info: Arc::new(info),
                }),
            ),
            Provider::Value(info) => (
                CONSTRAINT_VALUE_ID,
                heap.alloc(ConstraintValueValue {
                    info: Arc::new(info),
                }),
            ),
            Provider::Platform(info) => (PLATFORM_ID, heap.alloc(PlatformValue { info })),
        };
        module.set("__info", value);
        let frozen = module
            .freeze()
            .map_err(|error| format!("cannot freeze platform provider: {error:?}"))?;
        let value = frozen.get("__info").map_err(|error| error.to_string())?;
        Ok((id.to_owned(), value))
    })
}

/// The ConstraintValueInfo of a Starlark value, if it is one.
pub(crate) fn constraint_value(value: Value<'_>) -> Option<Arc<ConstraintValueInfo>> {
    value
        .downcast_ref::<ConstraintValueValue>()
        .map(|value| value.info.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::Label;

    #[test]
    fn providers_expose_only_ungated_fields_and_bazel_reprs() {
        let label = |text: &str| Label::parse_canonical(text).unwrap();
        let setting = ConstraintSettingInfo {
            label: label("//libc:libc"),
            default_value: Some(label("//libc:glibc")),
            refined: None,
        };
        let value = ConstraintValueInfo {
            setting: Arc::new(setting.clone()),
            label: label("//libc:musl"),
        };
        let platform = crate::platforms::create_platform(
            &label("//p:p"),
            crate::platforms::PlatformAttributes {
                constraint_values: vec![value.clone()],
                ..Default::default()
            },
            &shown,
        )
        .unwrap();
        let (id, frozen) = freeze(Provider::Setting(setting)).unwrap();
        assert_eq!(id, CONSTRAINT_SETTING_ID);
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let value = heap.access_owned_frozen_value(&frozen);
            assert_eq!(value.get_type(), "ConstraintSettingInfo");
            assert_eq!(
                value.to_repr(),
                "ConstraintSettingInfo(//libc:libc, default_constraint_value=//libc:glibc)"
            );
            assert_eq!(
                value
                    .get_attr("has_default_constraint_value", heap)
                    .unwrap()
                    .unwrap()
                    .unpack_bool(),
                Some(true)
            );
            assert!(value.get_attr("label", heap).unwrap().is_none());
            assert_eq!(value.dir_attr(), ["has_default_constraint_value"]);
        });
        let (id, frozen) = freeze(Provider::Value(value.clone())).unwrap();
        assert_eq!(id, CONSTRAINT_VALUE_ID);
        Module::with_temp_heap(|module| {
            let value = module.heap().access_owned_frozen_value(&frozen);
            assert_eq!(
                value.to_repr(),
                "ConstraintValueInfo(setting=//libc:libc, //libc:musl)"
            );
            assert!(value.dir_attr().is_empty());
            assert_eq!(constraint_value(value).unwrap().label, label("//libc:musl"));
        });
        let (id, frozen) = freeze(Provider::Platform(Arc::new(platform))).unwrap();
        assert_eq!(id, PLATFORM_ID);
        Module::with_temp_heap(|module| {
            let value = module.heap().access_owned_frozen_value(&frozen);
            assert_eq!(
                value.to_repr(),
                "PlatformInfo(//p:p, constraints=<[//libc:musl]>)"
            );
        });
    }
}
