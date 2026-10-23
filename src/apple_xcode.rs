//! `apple_common.dotted_version` (Java `DottedVersion`) and the `@_builtins`
//! `XcodeVersionInfo` provider (`apple_common.XcodeVersionConfig`).
//!
//! `DottedVersion` ports rules/apple/DottedVersion.java (parsing, ordering,
//! trailing-zero normalization, equality/hash, `to_string`, `compare_to`).
//! `XcodeVersionInfo` is not re-implemented: the verbatim pinned
//! `builtins_bzl/common/xcode/providers.bzl` (resources/builtins_bzl) is
//! evaluated once under its `@@_builtins//:common/xcode/providers.bzl` label,
//! so its `init`, closures, `fail` texts and provider identity are Bazel's.

use allocative::Allocative;
use starlark::collections::StarlarkHasher;
use starlark::environment::{FrozenModule, GlobalsBuilder, LibraryExtension, Module};
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::{
    FrozenHeap, FrozenValue, Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic,
    StarlarkValue, Value, starlark_value,
};
use std::cmp::Ordering;
use std::fmt;
use std::hash::Hash;
use std::sync::{Arc, OnceLock};

use crate::language_modules::{
    Behavior, BuiltinMethod, Style, bind_styled, failure, param, sorted_struct, type_error,
};

/// One `\d+([a-z0-9]*?)?(\d+)?` component (DottedVersion.java:398-445).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Allocative)]
struct Component {
    first: i32,
    /// `NO_ALPHA_SEQUENCE` is null, which sorts last (`nullsLast`).
    alpha: Option<String>,
    second: i32,
}

const ZERO: Component = Component {
    first: 0,
    alpha: None,
    second: 0,
};

impl Component {
    /// DottedVersion.java:415-422: first number, alpha (natural, nulls last),
    /// second number. ComparisonChain.result() is -1/0/1.
    fn cmp(&self, other: &Self) -> Ordering {
        self.first
            .cmp(&other.first)
            .then_with(|| match (&self.alpha, &other.alpha) {
                (None, None) => Ordering::Equal,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                // String.compareTo on UTF-16 units; alpha is ASCII-only.
                (Some(a), Some(b)) => a.cmp(b),
            })
            .then_with(|| self.second.cmp(&other.second))
    }
}

// Only derive-generated markers require this exception. The values own plain
// Rust data and freeze unchanged.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    /// Java `DottedVersion` (`@StarlarkBuiltin(name = "DottedVersion")`).
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct DottedVersion {
        /// `stringRepresentation`: the caller's exact string.
        pub(super) text: String,
        /// Components with trailing (non-first) zero components removed.
        pub(super) components: Vec<Component>,
    }

    /// The bound `compare_to` method (a Java BuiltinFunction).
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct CompareTo(pub(super) DottedVersion);
}
use owned::CompareTo;
pub(crate) use owned::DottedVersion;

starlark_simple_value!(DottedVersion);
starlark_simple_value!(CompareTo);

/// DottedVersion.java:156-158 (`%s` is the whole version string).
fn illegal(version: &str) -> String {
    format!(
        "Dotted version components must all start with the form \\d+([a-z0-9]*?)?(\\d+)? but got '{version}'"
    )
}

/// DESCRIPTIVE_COMPONENT_PATTERN `([a-z]\w*)`, CASE_INSENSITIVE, full match
/// (DottedVersion.java:154-155, 223-225). Java `\w` is `[a-zA-Z_0-9]`.
fn is_descriptive(component: &str) -> bool {
    let bytes = component.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// COMPONENT_PATTERN `(\d+)([a-z0-9]*?)?(\d+)?`, CASE_INSENSITIVE, full match
/// (DottedVersion.java:152-153, 227-259). The greedy first group takes every
/// leading digit (the lazy second group can always absorb an alphanumeric
/// rest); the lazy second group then stops before the longest trailing digit
/// run, which the third group takes. `Integer.parseInt` overflow is illegal.
fn component(text: &str) -> Option<Component> {
    let lead = text.bytes().take_while(u8::is_ascii_digit).count();
    if lead == 0 {
        return None;
    }
    let rest = &text[lead..];
    if !rest.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let trail = rest.bytes().rev().take_while(u8::is_ascii_digit).count();
    let (alpha, second) = rest.split_at(rest.len() - trail);
    Some(Component {
        first: text[..lead].parse().ok()?,
        alpha: (!alpha.is_empty()).then(|| alpha.to_owned()),
        second: if second.is_empty() {
            0
        } else {
            second.parse().ok()?
        },
    })
}

impl DottedVersion {
    /// DottedVersion.fromString (DottedVersion.java:190-219).
    pub(crate) fn parse(version: &str) -> Result<Self, String> {
        if version.is_empty() {
            return Err(illegal(version));
        }
        let mut components = Vec::new();
        // Splitter.on('.') keeps empty pieces, which are illegal components.
        for piece in version.split('.') {
            if is_descriptive(piece) {
                break;
            }
            components.push(component(piece).ok_or_else(|| illegal(version))?);
        }
        if components.is_empty() {
            return Err(illegal(version));
        }
        while components.len() > 1 && components.last().is_some_and(|c| c.cmp(&ZERO).is_eq()) {
            components.pop();
        }
        Ok(DottedVersion {
            text: version.to_owned(),
            components,
        })
    }

    fn component_at(&self, index: usize) -> &Component {
        self.components.get(index).unwrap_or(&ZERO)
    }

    /// DottedVersion.compareTo (DottedVersion.java:277-289).
    pub(crate) fn compare(&self, other: &Self) -> Ordering {
        let n = self.components.len().max(other.components.len());
        (0..n)
            .map(|i| self.component_at(i).cmp(other.component_at(i)))
            .find(|ordering| ordering.is_ne())
            .unwrap_or(Ordering::Equal)
    }
}

/// repr and str are `stringRepresentation` (DottedVersion.java:365-367, 394-396).
impl fmt::Display for DottedVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

#[starlark_value(type = "DottedVersion")]
impl<'v> StarlarkValue<'v> for DottedVersion {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match name {
            // `to_string` is a structField (DottedVersion.java:360-367).
            "to_string" => Some(heap.alloc(self.text.as_str())),
            "compare_to" => Some(heap.alloc(CompareTo(self.clone()))),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["compare_to".to_owned(), "to_string".to_owned()]
    }

    /// equals is `compareTo == 0` (DottedVersion.java:369-379).
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(DottedVersion::from_value(other).is_some_and(|other| self.compare(other).is_eq()))
    }

    /// Starlark.compareUnchecked uses Comparable for same-type values
    /// (Starlark.java:494-517).
    fn compare(&self, other: Value<'v>) -> starlark::Result<Ordering> {
        match DottedVersion::from_value(other) {
            Some(other) => Ok(DottedVersion::compare(self, other)),
            None => Err(failure(format!(
                "unsupported comparison: DottedVersion <=> {}",
                other.get_type()
            ))),
        }
    }

    /// hashCode is over the normalized components (DottedVersion.java:381-384),
    /// consistent with equality.
    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.components.hash(hasher);
        Ok(())
    }
}

impl fmt::Display for CompareTo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<built-in method compare_to of DottedVersion value>")
    }
}

#[starlark_value(type = "builtin_function_or_method")]
impl<'v> StarlarkValue<'v> for CompareTo {
    /// DottedVersionApi.compareTo_starlark (DottedVersionApi.java:36-44):
    /// one positional-only `other` whose class is DottedVersion.
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        let bound = bind_styled(
            Style::Builtin,
            "compare_to",
            &[param("other", true, false, true)],
            args,
            heap,
        )?;
        let other = bound[0].expect("required");
        let other = DottedVersion::from_value(other)
            .ok_or_else(|| type_error("compare_to", "other", other, "DottedVersion"))?;
        Ok(heap.alloc(self.0.compare(other) as i32))
    }
}

/// AppleStarlarkCommon.dottedVersion (AppleStarlarkCommon.java:50-57): a
/// `String` @Param, then DottedVersion.fromString.
pub(crate) fn dotted_version<'v>(value: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
    let text = value
        .unpack_str()
        .ok_or_else(|| type_error("dotted_version", "version", value, "string"))?;
    DottedVersion::parse(text)
        .map(|version| heap.alloc(version))
        .map_err(failure)
}

// ---------------------------------------------------------------------------
// XcodeVersionInfo: the verbatim @_builtins provider.

pub(crate) const PROVIDERS_LABEL: &str = "@@_builtins//:common/xcode/providers.bzl";
const PROVIDERS_SOURCE: &str = include_str!("../resources/builtins_bzl/common/xcode/providers.bzl");
const PLATFORM_LOAD: &str =
    "load(\":common/objc/apple_platform.bzl\", \"PLATFORM\", \"PLATFORM_TYPE\")";

/// The `_builtins` object visible to `@_builtins` files: only
/// `internal.apple_common.dotted_version` (the Java AppleStarlarkCommon
/// method, `@Param` positional-only) is read by providers.bzl.
fn builtins_object(heap: &FrozenHeap) -> FrozenValue {
    let dotted = heap.alloc(BuiltinMethod {
        receiver: "apple_common",
        name: "dotted_version",
        qualified_owner: "apple_common",
        behavior: Behavior::DottedVersion,
    });
    let apple_common = sorted_struct(heap, vec![("dotted_version", dotted)]);
    let internal = sorted_struct(heap, vec![("apple_common", apple_common)]);
    sorted_struct(heap, vec![("internal", internal)])
}

fn evaluate_providers() -> Result<FrozenModule, String> {
    if PROVIDERS_SOURCE.matches(PLATFORM_LOAD).count() != 1 {
        return Err("embedded providers.bzl lacks its apple_platform.bzl load".to_owned());
    }
    // Blank the load (same line count) and bind its symbols as globals.
    let source = PROVIDERS_SOURCE.replacen(PLATFORM_LOAD, "", 1);
    let globals = GlobalsBuilder::extended_by(&[LibraryExtension::StructType])
        .with(crate::providers::globals)
        .with(|builder| {
            let (platform_type, platform) = {
                let heap = builder.frozen_heap();
                crate::language_modules::apple_platform_tables(heap)
            };
            let builtins = builtins_object(builder.frozen_heap());
            builder.set("PLATFORM", platform);
            builder.set("PLATFORM_TYPE", platform_type);
            builder.set("_builtins", builtins);
        })
        .build();
    let context = crate::loading::ExtensionContext {
        label: crate::label::Label::parse_canonical(PROVIDERS_LABEL)?,
        repositories: Arc::new(crate::repository::RepositorySet::new(".".into())),
        byte_strings: false,
    };
    let dialect = Dialect {
        enable_keyword_only_arguments: true,
        enable_load_reexport: false,
        ..Dialect::Standard
    };
    let ast = AstModule::parse(PROVIDERS_LABEL, source, &dialect).map_err(|e| e.to_string())?;
    Module::with_temp_heap(|module| {
        {
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            eval.eval_module(ast, &globals).map_err(|e| e.to_string())?;
        }
        module.freeze().map_err(|e| format!("{e:?}"))
    })
}

fn providers_module() -> &'static FrozenModule {
    static MODULE: OnceLock<FrozenModule> = OnceLock::new();
    MODULE.get_or_init(|| {
        evaluate_providers()
            .unwrap_or_else(|e| panic!("embedded {PROVIDERS_LABEL} failed to evaluate: {e}"))
    })
}

/// `XcodeVersionInfo` from the evaluated providers.bzl, kept alive by `heap`.
pub(crate) fn xcode_version_info(heap: &FrozenHeap) -> FrozenValue {
    providers_module()
        .get("XcodeVersionInfo")
        .expect("providers.bzl exports XcodeVersionInfo")
        .owned_value(heap)
        .unpack_frozen()
        .expect("module globals are frozen")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_modules::tests::{fails, pass};

    const FILE: &str = "@@apple_support+//xcode:xcode_config.bzl";

    fn parsed(text: &str) -> DottedVersion {
        DottedVersion::parse(text).unwrap()
    }

    #[test]
    fn components_follow_the_java_regex_groups() {
        let c = |first, alpha: Option<&str>, second| Component {
            first,
            alpha: alpha.map(str::to_owned),
            second,
        };
        assert_eq!(component("123"), Some(c(123, None, 0)));
        assert_eq!(component("5beta2"), Some(c(5, Some("beta"), 2)));
        assert_eq!(component("5beta"), Some(c(5, Some("beta"), 0)));
        assert_eq!(component("1a2b"), Some(c(1, Some("a2b"), 0)));
        assert_eq!(component("1a2b34"), Some(c(1, Some("a2b"), 34)));
        assert_eq!(component("007RC01"), Some(c(7, Some("RC"), 1)));
        assert_eq!(component("2147483647"), Some(c(i32::MAX, None, 0)));
        assert_eq!(
            component("000000000000002147483647"),
            Some(c(i32::MAX, None, 0))
        );
        for bad in [
            "",
            "a1",
            "1-2",
            "1_a",
            "2147483648",
            "1a2147483648",
            "1é",
            "١",
        ] {
            assert_eq!(component(bad), None, "{bad}");
        }
        assert!(is_descriptive("beta_1") && is_descriptive("Experimental"));
        assert!(!is_descriptive("1beta") && !is_descriptive("b-1") && !is_descriptive(""));
    }

    #[test]
    fn parse_normalizes_trailing_zeros_and_stops_at_descriptive_components() {
        assert_eq!(parsed("1.0.0").components.len(), 1);
        assert_eq!(parsed("0.0").components.len(), 1);
        assert_eq!(parsed("1.0.00.0").to_string(), "1.0.00.0");
        assert_eq!(parsed("1.0beta0").components.len(), 2);
        // Everything after the first descriptive component is ignored.
        assert_eq!(parsed("1.2.beta.!!").components.len(), 2);
        assert_eq!(parsed("1.2.beta.!!").to_string(), "1.2.beta.!!");
        let illegal_texts = [
            "", ".", "1.", ".1", "1..2", "beta", "beta.1", "1.b-1", "x1", "1.2.3-4",
        ];
        for text in illegal_texts {
            assert_eq!(
                DottedVersion::parse(text).unwrap_err(),
                illegal(text),
                "{text}"
            );
        }
        assert_eq!(
            illegal("a"),
            "Dotted version components must all start with the form \\d+([a-z0-9]*?)?(\\d+)? but got 'a'"
        );
    }

    #[test]
    fn ordering_matches_the_documented_examples() {
        // DottedVersion.java:51-75.
        let greater = [
            ("3.1.25", "3.1.1"),
            ("3.1.20", "3.1.2"),
            ("3.1.1", "3.1"),
            ("3.2", "3.1.8"),
            ("3.1.1", "3.1.1beta3"),
            ("3.1.1beta1", "3.1.0"),
            ("3.1", "3.1.0alpha1"),
            ("3.1.0beta0", "3.1.0alpha5.6"),
            ("3.4.2alpha2", "3.4.2alpha1"),
            ("3.4.2alpha2", "3.4.2alpha1.5"),
            ("3.1alpha1", "3.1alpha"),
            ("1beta", "1Beta"),
        ];
        for (a, b) in greater {
            assert_eq!(
                parsed(a).compare(&parsed(b)),
                Ordering::Greater,
                "{a} > {b}"
            );
            assert_eq!(parsed(b).compare(&parsed(a)), Ordering::Less, "{b} < {a}");
        }
        assert!(parsed("3.1").compare(&parsed("3.1.0.0")).is_eq());
        assert!(parsed("01.2").compare(&parsed("1.2.0")).is_eq());
    }

    #[test]
    fn starlark_surface() {
        pass(
            FILE,
            r#"
v = apple_common.dotted_version("1.2.0")
(type(v) == "DottedVersion" and str(v) == "1.2.0" and repr(v) == "1.2.0") or fail("shape")
(dir(v) == ["compare_to", "to_string"] and v.to_string == "1.2.0") or fail("dir")
(v == apple_common.dotted_version("1.2") and v != apple_common.dotted_version("1.2.1")) or fail("eq")
(v != "1.2.0" and bool(v)) or fail("other types")
({v: 1}[apple_common.dotted_version("01.2.0.0")] == 1) or fail("hash")
(v.compare_to(apple_common.dotted_version("1.3")) == -1) or fail("lt")
(v.compare_to(apple_common.dotted_version("1.1.9")) == 1) or fail("gt")
(v.compare_to(apple_common.dotted_version("1.2")) == 0) or fail("eq0")
(apple_common.dotted_version("1.2beta") < v and v > apple_common.dotted_version("1.2alpha")) or fail("ops")
(sorted([apple_common.dotted_version(x) for x in ["10", "9.1", "9.1beta"]]) == [apple_common.dotted_version(x) for x in ["9.1beta", "9.1", "10"]]) or fail("sorted")
(repr(v.compare_to) == "<built-in method compare_to of DottedVersion value>" and type(v.compare_to) == "builtin_function_or_method") or fail("method")
(apple_common.dotted_version(version = "7") == apple_common.dotted_version("7.0")) or fail("named")
(apple_common.dotted_version("8.4") or "default") == apple_common.dotted_version("8.4") or fail("truth")
"#,
        );
        for (call, message) in [
            (
                "apple_common.dotted_version(None)",
                "in call to dotted_version(), parameter 'version' got value of type 'NoneType', want 'string'",
            ),
            (
                "apple_common.dotted_version(1)",
                "in call to dotted_version(), parameter 'version' got value of type 'int', want 'string'",
            ),
            (
                "apple_common.dotted_version('1..2')",
                "Dotted version components must all start with the form \\d+([a-z0-9]*?)?(\\d+)? but got '1..2'",
            ),
            (
                "apple_common.dotted_version()",
                "lambda() missing 1 required positional argument: version",
            ),
            (
                "apple_common.dotted_version(versio = '1')",
                "lambda() got unexpected keyword argument: versio (did you mean 'version'?)",
            ),
            (
                "apple_common.dotted_version('1', '2')",
                "lambda() accepts no more than 1 positional argument but got 2",
            ),
            (
                "apple_common.dotted_version('1').compare_to('1')",
                "in call to compare_to(), parameter 'other' got value of type 'string', want 'DottedVersion'",
            ),
            (
                "apple_common.dotted_version('1').compare_to(other = apple_common.dotted_version('1'))",
                "compare_to() got named argument for positional-only parameter 'other'",
            ),
            (
                "apple_common.dotted_version('1').compare_to()",
                "compare_to() missing 1 required positional argument: other",
            ),
            (
                "apple_common.dotted_version('1') < 1",
                "unsupported comparison: DottedVersion <=> int",
            ),
        ] {
            fails(FILE, call, message);
        }
    }

    /// StarlarkFunction.java:389-403 / 482-485 through the `def`-style lambda.
    #[test]
    fn def_style_keyword_errors_collect_every_unknown_keyword() {
        for (call, message) in [
            (
                "apple_common.dotted_version(zeta = 1, alpha = 2)",
                "lambda() got unexpected keyword arguments: zeta, alpha",
            ),
            (
                "apple_common.dotted_version(versio = 1, versiom = 2)",
                "lambda() got unexpected keyword arguments: versio, versiom",
            ),
            (
                "apple_common.dotted_version(zzz = 1)",
                "lambda() got unexpected keyword argument: zzz",
            ),
            (
                "apple_common.dotted_version('1', version = '2')",
                "lambda() got multiple values for parameter 'version'",
            ),
            // StarlarkFunction.java: a duplicate fails as it is added, an unknown
            // keyword only after the positional count.
            (
                "apple_common.dotted_version('1', '2', version = '3')",
                "lambda() got multiple values for parameter 'version'",
            ),
            (
                "apple_common.dotted_version('1', '2', versio = '3')",
                "lambda() accepts no more than 1 positional argument but got 2",
            ),
        ] {
            let error = crate::language_modules::tests::run(FILE, call).unwrap_err();
            assert!(error.contains(message), "{call}: {error}");
            assert!(!error.contains("did you mean"), "{call}: {error}");
        }
        // Java builtins still stop at the first unknown keyword, with a hint.
        fails(
            FILE,
            "config_common.toolchain_type('//x:y', mandatroy = True, other = 1)",
            "toolchain_type() got unexpected keyword argument 'mandatroy' (did you mean 'mandatory'?)",
        );
    }

    /// The provider apple_support's xcode_config builds with no versions
    /// (xcode_config.bzl:76-113 with XcodeVersionPropertiesInfo defaults).
    const HOST_XCODES: &str = r#"
info = apple_common.XcodeVersionConfig(
    ios_sdk_version = "8.4",
    ios_minimum_os_version = "8.4",
    visionos_sdk_version = "1.0",
    visionos_minimum_os_version = "1.0",
    watchos_sdk_version = "2.0",
    watchos_minimum_os_version = "2.0",
    tvos_sdk_version = "9.0",
    tvos_minimum_os_version = "9.0",
    macos_sdk_version = "10.11",
    macos_minimum_os_version = "10.11",
    xcode_version = None,
    availability = "UNKNOWN",
    xcode_version_flag = None,
    include_xcode_execution_info = False,
)
"#;

    #[test]
    fn xcode_version_info_is_the_builtins_provider() {
        pass(
            FILE,
            &format!(
                "{HOST_XCODES}{}",
                r#"
P = apple_common.XcodeVersionConfig
(type(P) == "Provider" and repr(P) == "<provider>") or fail("provider")
(type(info) == "struct") or fail(type(info))
(dir(info) == ["availability", "execution_info", "minimum_os_for_platform_type", "sdk_version_for_platform", "xcode_version"]) or fail(str(dir(info)))
(info.xcode_version() == None and info.availability() == "unknown") or fail("version")
(info.execution_info() == {"requires-darwin": "", "supports-xcode-requirements-set": ""}) or fail("exec")
pt = apple_common.platform_type
p = apple_common.platform
mins = [str(info.minimum_os_for_platform_type(t)) for t in [pt.ios, pt.catalyst, pt.macos, pt.tvos, pt.visionos, pt.watchos]]
(mins == ["8.4", "8.4", "10.11", "9.0", "1.0", "2.0"]) or fail(str(mins))
sdks = [str(info.sdk_version_for_platform(getattr(p, n))) for n in dir(p)]
(sdks == ["10.11", "8.4", "8.4", "10.11", "9.0", "9.0", "1.0", "1.0", "2.0", "2.0"]) or fail(str(sdks))
(type(info.minimum_os_for_platform_type(pt.ios)) == "DottedVersion") or fail("dotted")
(repr(info.availability) == "<function lambda from @@_builtins//:common/xcode/providers.bzl>") or fail(repr(info.availability))
(repr(info.minimum_os_for_platform_type) == "<function _minimum_os_for_platform_type from @@_builtins//:common/xcode/providers.bzl>") or fail("def repr")
local = apple_common.XcodeVersionConfig(
    ios_sdk_version = "17.0", ios_minimum_os_version = "15.0",
    visionos_sdk_version = "1.0", visionos_minimum_os_version = "1.0",
    watchos_sdk_version = "10.0", watchos_minimum_os_version = "8.0",
    tvos_sdk_version = "17.0", tvos_minimum_os_version = "15.0",
    macos_sdk_version = "14.0", macos_minimum_os_version = "12.0",
    xcode_version = "15.0.1", availability = "LOCAL",
    xcode_version_flag = "15.0.1-local", include_xcode_execution_info = True,
)
(local.xcode_version() == apple_common.dotted_version("15.0.1") and local.availability() == "local") or fail("local")
(local.execution_info() == {"requires-darwin": "", "supports-xcode-requirements-set": "", "no-remote": "", "requires-xcode:15.0.1": "", "requires-xcode-label:local": ""}) or fail(str(local.execution_info()))
"#
            ),
        );
        for (tail, message) in [
            (
                "info.minimum_os_for_platform_type('beos')",
                "Unhandled platform type: beos",
            ),
            (
                "info.sdk_version_for_platform(struct(name = 'x'))",
                "Unhandled platform: struct(name = \"x\")",
            ),
        ] {
            fails(FILE, &format!("{HOST_XCODES}{tail}"), message);
        }
        fails(
            FILE,
            &HOST_XCODES.replace("ios_sdk_version = \"8.4\"", "ios_sdk_version = \"8..4\""),
            "but got '8..4'",
        );
        // The init closures capture `availability` and `xcode_version` raw,
        // and Bazel accepts any value there; so does the native profile since
        // no provider-file walk has to see through them (W1b/W1d: a File
        // reached through a closure resolves through the artifact index).
        for (field, value, check) in [
            (
                "availability = \"UNKNOWN\"",
                "availability = struct(lower = lambda: depset())",
                "info.availability() == depset()",
            ),
            (
                "xcode_version = None",
                "xcode_version = depset()",
                "info.xcode_version() == None",
            ),
            (
                "xcode_version_flag = None",
                "xcode_version_flag = [\"x\"]",
                "info.execution_info() == {\"requires-darwin\": \"\", \"supports-xcode-requirements-set\": \"\"}",
            ),
        ] {
            pass(
                FILE,
                &format!(
                    "{}({check}) or fail({check:?})\n",
                    HOST_XCODES.replace(field, value)
                ),
            );
        }
        fails(
            FILE,
            &HOST_XCODES.replace("ios_sdk_version = \"8.4\"", "ios_sdk_version = None"),
            "in call to dotted_version(), parameter 'version' got value of type 'NoneType', want 'string'",
        );
    }

    #[test]
    fn xcode_version_info_identity_is_the_builtins_label() {
        Module::with_temp_heap(|module| {
            let globals = crate::package::bzl_globals();
            let ast = AstModule::parse(
                "p.bzl",
                "x = apple_common.XcodeVersionConfig".to_owned(),
                &Dialect::Standard,
            )
            .unwrap();
            Evaluator::new(&module).eval_module(ast, &globals).unwrap();
            let value = module.get("x").unwrap();
            assert_eq!(
                crate::providers::provider_identity(value).as_deref(),
                Some("@@_builtins//:common/xcode/providers.bzl%XcodeVersionInfo")
            );
        });
    }
}
