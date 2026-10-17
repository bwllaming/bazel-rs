//! Feature flags (increment N30b): `config_common.FeatureFlagInfo` and the
//! native `config_feature_flag` rule (docs/TEST_FLAGS.md).
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
//! The native profile has no transition that sets feature flag values
//! (`config_common.config_feature_flag_transition`, Android's
//! `feature_flags`, `--enforce_transitive_configs_for_config_feature_flag`
//! fail explicitly), so every `config_feature_flag` is in its
//! FeatureFlagValue.DefaultValue state (rules/config/FeatureFlagValue.java).
use allocative::Allocative;
use anyhow::anyhow;
use starlark::collections::StarlarkHasher;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::values::string::StarlarkStr;
use starlark::values::{
    Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value,
    starlark_value,
};
use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};

/// BuiltinProvider.requirement_id of `config_common.FeatureFlagInfo`
/// (crate::language_modules).
pub const PROVIDER_ID: &str = "builtin:FeatureFlagInfo";
static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

/// The provider's data as ConfigSetting's UserDefinedFlagMatch reads it
/// (ConfigFeatureFlagProvider.java:56-149).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeatureFlagValue {
    /// getFlagValue: null when there is an error.
    pub value: Option<String>,
    /// getError (potentialError).
    pub error: Option<String>,
    /// The validity predicate: `Predicates.in(allowed_values)` for
    /// `config_feature_flag`, `alwaysTrue` (None) for the Starlark constructor.
    pub allowed: Option<Vec<String>>,
}

impl FeatureFlagValue {
    /// isValidValue (:129-133).
    pub fn is_valid_value(&self, value: &str) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|allowed| allowed == value))
    }

    /// getFlagValue (:110-119): null when the error is non-empty.
    pub fn flag_value(&self) -> Option<&str> {
        if self.error.as_deref().is_some_and(|error| !error.is_empty()) {
            None
        } else {
            self.value.as_deref()
        }
    }
}

// Only derive-generated marker implementations need the exception. These
// simple values own all their data and freeze unchanged.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    /// ConfigFeatureFlagProvider: unique per instance (equals/hashCode are
    /// identity, :135-148).
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct FeatureFlagInfo {
        pub id: u64,
        #[allocative(skip)]
        pub data: FeatureFlagValue,
    }

    /// The bound `is_valid_value` method (a BuiltinFunction).
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct IsValidValue {
        #[allocative(skip)]
        pub data: FeatureFlagValue,
    }
}
use owned::{FeatureFlagInfo, IsValidValue};

starlark_simple_value!(FeatureFlagInfo);
starlark_simple_value!(IsValidValue);

fn error(message: String) -> starlark::Error {
    starlark::Error::new_other(anyhow!(message))
}

/// Allocate a provider instance.
pub fn alloc<'v>(heap: Heap<'v>, data: FeatureFlagValue) -> Value<'v> {
    heap.alloc(FeatureFlagInfo {
        id: NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
        data,
    })
}

pub fn is_instance(value: Value<'_>) -> bool {
    FeatureFlagInfo::from_value(value).is_some()
}

/// The data of a FeatureFlagInfo instance.
pub fn value_of(value: Value<'_>) -> Option<FeatureFlagValue> {
    FeatureFlagInfo::from_value(value).map(|info| info.data.clone())
}

/// A frozen provider instance for a native `config_feature_flag` target.
pub fn freeze(data: FeatureFlagValue) -> Result<starlark::values::OwnedFrozenValue, String> {
    starlark::environment::Module::with_temp_heap(|module| {
        module.set("__info", alloc(module.heap(), data));
        let frozen = module
            .freeze()
            .map_err(|error| format!("cannot freeze FeatureFlagInfo: {error:?}"))?;
        frozen.get("__info").map_err(|error| error.to_string())
    })
}

/// Java Starlark's BuiltinFunction argument binding for one `String value`
/// parameter (positional or named, no default; net/starlark/java/eval/BuiltinFunction.java:140-162,
/// 325-386, 470-499).
fn single_string_argument<'v>(
    method: &str,
    args: &Arguments<'v, '_>,
    heap: Heap<'v>,
) -> starlark::Result<String> {
    let positional = args.positions(heap)?.collect::<Vec<_>>();
    if positional.len() > 1 {
        return Err(error(format!(
            "{method}() accepts no more than 1 positional argument but got {}",
            positional.len()
        )));
    }
    let check = |value: Value<'v>| -> starlark::Result<String> {
        value.unpack_str().map(str::to_owned).ok_or_else(|| {
            error(format!(
                "in call to {method}(), parameter 'value' got value of type '{}', want 'string'",
                value.get_type()
            ))
        })
    };
    let mut bound = match positional.first() {
        Some(value) => Some(check(*value)?),
        None => None,
    };
    for (name, value) in args.names_map()?.iter() {
        let name = name.as_str();
        if name != "value" {
            return Err(error(format!(
                "{method}() got unexpected keyword argument '{name}'{}",
                crate::extension_ctx::did_you_mean(name, &["value"])
            )));
        }
        let value = check(*value)?;
        if bound.replace(value).is_some() {
            return Err(error(format!(
                "{method}() got multiple values for argument 'value'"
            )));
        }
    }
    bound.ok_or_else(|| {
        error(format!(
            "{method}() missing 1 required positional argument: value"
        ))
    })
}

/// ConfigFeatureFlagProvider.Constructor.selfcall (:84-90): `value` only,
/// no error, every value valid.
pub(crate) fn construct<'v>(
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let value = single_string_argument("FeatureFlagInfo", args, eval.heap())?;
    Ok(alloc(
        eval.heap(),
        FeatureFlagValue {
            value: Some(value),
            error: None,
            allowed: None,
        },
    ))
}

/// NativeInfo.repr over the struct fields `error` and `value` (sorted; the
/// `is_valid_value` method is not a field, packages/NativeInfo.java:125-138).
impl fmt::Display for FeatureFlagInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shown = |value: &Option<String>| match value {
            Some(text) => StarlarkStr::repr(text).to_string(),
            None => "None".to_owned(),
        };
        write!(
            f,
            "struct(error = {}, value = {})",
            shown(&self.data.error),
            shown(&self.data.flag_value().map(str::to_owned))
        )
    }
}

#[starlark_value(type = "FeatureFlagInfo")]
impl<'v> StarlarkValue<'v> for FeatureFlagInfo {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let text = |value: Option<&str>| match value {
            Some(text) => heap.alloc(text),
            None => Value::new_none(),
        };
        match name {
            "value" => Some(text(self.data.flag_value())),
            "error" => Some(text(self.data.error.as_deref())),
            "is_valid_value" => Some(heap.alloc(IsValidValue {
                data: self.data.clone(),
            })),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "error".to_owned(),
            "is_valid_value".to_owned(),
            "value".to_owned(),
        ]
    }

    /// Structs are immutable (Starlark `cannot set .F field of T value`).
    fn set_attr(&self, attribute: &str, _new_value: Value<'v>) -> starlark::Result<()> {
        Err(error(format!(
            "cannot set .{attribute} field of FeatureFlagInfo value"
        )))
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(FeatureFlagInfo::from_value(other).is_some_and(|other| other.id == self.id))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        PROVIDER_ID.hash(hasher);
        self.id.hash(hasher);
        Ok(())
    }
}

impl fmt::Display for IsValidValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<built-in method is_valid_value of FeatureFlagInfo value>")
    }
}

#[starlark_value(type = "builtin_function_or_method")]
impl<'v> StarlarkValue<'v> for IsValidValue {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let value = single_string_argument("is_valid_value", args, eval.heap())?;
        Ok(Value::new_bool(self.data.is_valid_value(&value)))
    }
}

/// `Starlark.repr` of a list of strings (`["a", "b"]`).
fn repr_list(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|value| StarlarkStr::repr(value).to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// ConfigFeatureFlag.create (rules/config/ConfigFeatureFlag.java:109-169)
/// with constructProvider (:183-235) for FeatureFlagValue.DefaultValue (no
/// native transition sets feature flags, and `--//flag=v` is not an option for
/// them). `Err` holds the attribute errors after the rule-context prefix
/// (`in ATTR attribute of config_feature_flag rule L: ...`), all reported.
pub fn analyze(
    shown: &str,
    allowed_values: &[String],
    default_value: Option<&str>,
) -> Result<FeatureFlagValue, Vec<String>> {
    let mut values: Vec<String> = Vec::new();
    for value in allowed_values {
        if !values.contains(value) {
            values.push(value.clone());
        }
    }
    let mut errors = Vec::new();
    if values.len() != allowed_values.len() {
        // ImmutableMultiset.copyOf keeps first-occurrence order.
        let duplicates = values
            .iter()
            .filter(|value| allowed_values.iter().filter(|v| v == value).count() > 1)
            .cloned()
            .collect::<Vec<_>>();
        errors.push(format!(
            "in allowed_values attribute of config_feature_flag rule {shown}: cannot contain duplicates, but contained multiple of {}",
            repr_list(&duplicates)
        ));
    }
    if let Some(default) = default_value
        && !values.iter().any(|value| value == default)
    {
        errors.push(format!(
            "in default_value attribute of config_feature_flag rule {shown}: must be one of {}, but was {}",
            repr_list(&values),
            StarlarkStr::repr(default)
        ));
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(match default_value {
        Some(default) => FeatureFlagValue {
            value: Some(default.to_owned()),
            error: None,
            allowed: Some(values),
        },
        // Deferred: a later transition could still set the value (:203-213).
        None => FeatureFlagValue {
            value: None,
            error: Some(format!(
                "Feature flag {shown} has no default but no value was explicitly specified."
            )),
            allowed: Some(values),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_feature_flag_create_errors_and_default_state() {
        let strings = |items: &[&str]| items.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            analyze("//ff:dup", &strings(&["a", "b", "a", "b", "c"]), Some("a")).unwrap_err(),
            vec![
                "in allowed_values attribute of config_feature_flag rule //ff:dup: cannot contain duplicates, but contained multiple of [\"a\", \"b\"]"
            ]
        );
        assert_eq!(
            analyze("//ff:both", &strings(&["a", "a"]), Some("q")).unwrap_err(),
            vec![
                "in allowed_values attribute of config_feature_flag rule //ff:both: cannot contain duplicates, but contained multiple of [\"a\"]",
                "in default_value attribute of config_feature_flag rule //ff:both: must be one of [\"a\"], but was \"q\"",
            ]
        );
        let ok = analyze("//ff:ff", &strings(&["a", "b"]), Some("a")).unwrap();
        assert_eq!(ok.flag_value(), Some("a"));
        assert!(ok.is_valid_value("b") && !ok.is_valid_value("zz"));
        let none = analyze("//ff:nodef", &strings(&["a", "b"]), None).unwrap();
        assert_eq!(none.flag_value(), None);
        assert_eq!(
            none.error.as_deref(),
            Some("Feature flag //ff:nodef has no default but no value was explicitly specified.")
        );
        let starlark = FeatureFlagValue {
            value: Some("x".into()),
            error: None,
            allowed: None,
        };
        assert!(starlark.is_valid_value("anything"));
    }
}
