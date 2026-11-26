//! Late-bound `configuration_field(fragment, name)` attribute defaults
//! (StarlarkLateBoundDefault.resolve, analysis/starlark/StarlarkLateBoundDefault.java:57-66):
//! the value is the fragment field of the configuration the target is
//! analyzed in. The CppConfiguration fields follow the modelled CppOptions
//! of that configuration (crate::configuration::VALUE_OPTIONS: the FDO,
//! Propeller, malloc and libc options, OSS corpus B32); every other
//! resolved field is fixed in every native configuration; the rest stay an
//! explicit analysis-time gap (crate::custom_rule::AttrDeclaration::unanalyzed).

use std::collections::BTreeMap;

use crate::configuration::NativeOptions;
use crate::custom_rule::{AttrSpec, AttrValue};

/// Whether `fragment.name` is a field the native profile resolves.
pub(crate) fn resolvable(fragment: &str, name: &str) -> bool {
    resolve(fragment, name, &NativeOptions::default()).is_some()
}

/// The configured value of a resolvable field in the configuration of
/// `native`: `Some(None)` is Bazel's null label (the attribute is `None`, no
/// dependency edge), `Some(Some(label))` a canonical label. `None` means
/// the field is not resolved natively.
pub(crate) fn resolve(
    fragment: &str,
    name: &str,
    native: &NativeOptions,
) -> Option<Option<String>> {
    let label = |option: &str| native.nullable_label(option).map(|label| label.to_string());
    Some(match (fragment, name) {
        // CppConfiguration (rules/cpp/CppConfiguration.java).
        // fdo_optimize :632-635: the `//` label of --fdo_optimize (:180-209).
        ("cpp", "fdo_optimize") => native
            .nullable_string("fdo_optimize")
            .filter(|text| text.starts_with("//"))
            .and_then(|text| crate::label::Label::parse_canonical(&text).ok())
            .map(|label| label.to_string()),
        // :683-694, :696-699, :701-706, :732-738, :422-430, :756-759.
        ("cpp", "fdo_prefetch_hints") => label("fdo_prefetch_hints"),
        ("cpp", "fdo_profile") => label("fdo_profile"),
        ("cpp", "cs_fdo_profile") => label("cs_fdo_profile"),
        ("cpp", "memprof_profile") => label("memprof_profile"),
        ("cpp", "custom_malloc") => label("custom_malloc"),
        ("cpp", "libc_top") => label("grte_top"),
        // :708-717: null under --fdo_instrument or --cs_fdo_instrument.
        ("cpp", "propeller_optimize") => {
            if native.nullable_string("fdo_instrument").is_some()
                || native.nullable_string("cs_fdo_instrument").is_some()
            {
                None
            } else {
                label("propeller_optimize")
            }
        }
        // :719-730: null under --fdo_optimize, --fdo_instrument,
        // --fdo_profile or code coverage.
        ("cpp", "xbinary_fdo") => {
            if native.nullable_string("fdo_optimize").is_some()
                || native.nullable_string("fdo_instrument").is_some()
                || native.nullable_label("fdo_profile").is_some()
                || native.collect_code_coverage
            {
                None
            } else {
                label("xbinary_fdo")
            }
        }
        // :308-319: the unzip tool whenever an FDO profile is set.
        ("cpp", "zipper") => {
            let fdo_optimize = native.nullable_string("fdo_optimize").is_some();
            let xbinary = resolve("cpp", "xbinary_fdo", native).flatten().is_some();
            (fdo_optimize
                || native.nullable_label("fdo_profile").is_some()
                || native.nullable_label("memprof_profile").is_some()
                || xbinary)
                .then(|| "@@bazel_tools//tools/zip:unzip_fdo".to_owned())
        }
        // proto_profile_path :958-961 (CppOptions.java:542-550 default "null").
        ("cpp", "proto_profile_path") => None,
        // CoverageConfiguration.outputGenerator (analysis/test/CoverageConfiguration.java:76-95):
        // null unless --collect_code_coverage, which the native profile never sets.
        ("coverage", "output_generator") => None,
        // AppleConfiguration.getXcodeConfigLabel (rules/apple/AppleConfiguration.java:99-100,
        // 320-327) is --xcode_version_config, default
        // "@bazel_tools//tools/cpp:host_xcodes" (rules/apple/AppleCommandLineOptions.java:293-296).
        // The annotation's defaultLabel //tools/objc:host_xcodes (:326) is only the
        // attribute's static (query-time) default, not the analysis value.
        ("apple", "xcode_config_label") => Some("@@bazel_tools//tools/cpp:host_xcodes".to_owned()),
        // JavaConfiguration (rules/java/JavaConfiguration.java):
        //   launcher :236-243 (--java_launcher, JavaOptions.java:212-213 default "null";
        //   a configuration option, crate::configuration::VALUE_OPTIONS),
        //   proguard_top :246-253 (--proguard_top, JavaOptions.java:224-225 default "null"),
        //   java_toolchain_bytecode_optimizer :318-329 and
        //   local_java_optimization_configuration :332-340 are null unless
        //   --experimental_local_java_optimizations (JavaOptions.java:254-255 default false).
        //   bytecode_optimizer :304-311 is the label of the single
        //   --bytecode_optimizers entry, default "Proguard" without a label
        //   (:133-147; JavaOptions.java:239-246).
        ("java", "launcher") => label("java_launcher"),
        (
            "java",
            "proguard_top"
            | "bytecode_optimizer"
            | "java_toolchain_bytecode_optimizer"
            | "local_java_optimization_configuration",
        ) => None,
        // PythonConfiguration.getNativeRulesAllowlist (rules/python/PythonConfiguration.java:60,
        // 146-153): --python_native_rules_allowlist, default null (PythonOptions.java:61-64).
        // BazelPythonConfiguration.getPythonTop always returns null
        // (bazel/rules/python/BazelPythonConfiguration.java:124-129).
        ("py", "native_rules_allowlist") | ("bazel_py", "python_top") => None,
        // ProtoConfiguration (rules/proto/ProtoConfiguration.java:188-231):
        // `--proto_compiler` and `--proto_toolchain_for_*`, which the native
        // option parser accepts only at their defaults
        // (ProtoConstants.java:44-58; crate::option_defaults). protobuf's
        // rules read them through their legacy attributes under
        // `--noincompatible_enable_proto_toolchain_resolution`.
        ("proto", "proto_compiler") => Some("@@bazel_tools//tools/proto:protoc".to_owned()),
        ("proto", "proto_toolchain_for_java") => {
            Some("@@bazel_tools//tools/proto:java_toolchain".to_owned())
        }
        ("proto", "proto_toolchain_for_java_lite") => {
            Some("@@bazel_tools//tools/proto:javalite_toolchain".to_owned())
        }
        ("proto", "proto_toolchain_for_cc") => {
            Some("@@bazel_tools//tools/proto:cc_toolchain".to_owned())
        }
        ("proto", "proto_toolchain_for_j2objc") => {
            Some("@@bazel_tools//tools/j2objc:j2objc_proto_toolchain".to_owned())
        }
        _ => return None,
    })
}

/// The configured values of the resolvable late-bound attributes of a
/// Starlark rule target in the configuration of `native`, replacing their
/// unconfigured (`None`) defaults.
pub(crate) fn configured_values<'a>(
    schema: &'a BTreeMap<String, AttrSpec>,
    native: &'a NativeOptions,
) -> impl Iterator<Item = (&'a String, AttrValue)> + 'a {
    schema.iter().filter_map(move |(name, spec)| {
        let field = spec.declaration.configuration_field.as_ref()?;
        let value = resolve(&field.fragment, &field.name, native)?;
        Some((name, AttrValue::Label(value)))
    })
}

/// `aspect` with the resolvable late-bound defaults of its attributes
/// replaced by their values in the base target's configuration `native`
/// (OSS corpus B87). An aspect attribute named `_x` with a
/// `configuration_field` default is analysis-dependent (`:x`), so
/// DependencyResolutionHelpers.resolveAttribute (:437-486) resolves it like a
/// rule's late-bound attribute, through resolveLateBoundDefault with
/// `ruleConfig`, the configuration of the target the aspect applies to,
/// whatever the attribute's `cfg` (a genuine 9.3.1 capture: a `cfg = "exec"`
/// one too). The shared definition is returned when nothing changes.
pub(crate) fn configured_aspect(
    aspect: &std::sync::Arc<crate::aspects::AspectDefinition>,
    native: &NativeOptions,
) -> std::sync::Arc<crate::aspects::AspectDefinition> {
    let resolved: Vec<(usize, AttrValue)> = aspect
        .declaration
        .attrs
        .iter()
        .enumerate()
        .filter_map(|(index, (_, spec))| {
            let field = spec.declaration.configuration_field.as_ref()?;
            let value = AttrValue::Label(resolve(&field.fragment, &field.name, native)?);
            (spec.default != value).then_some((index, value))
        })
        .collect();
    if resolved.is_empty() {
        return aspect.clone();
    }
    let mut configured = crate::aspects::AspectDefinition::clone(aspect);
    for (index, value) in resolved {
        configured.declaration.attrs[index].1.default = value;
    }
    std::sync::Arc::new(configured)
}

/// A failed dependency edge whose label is the configured value of a
/// late-bound default: the target's own error, then the native gap. In the
/// explicit-module profile (`--experimental-builtin-modules=false`) there is
/// no `@bazel_tools`, so
/// `apple.xcode_config_label` (an alias to the xcode_configure extension's
/// `@local_config_xcode//:host_xcodes`, tools/cpp/BUILD.tools:124-128) cannot
/// be analyzed.
pub(crate) fn edge_error(
    rule: &crate::label::Label,
    dependency: &crate::label::Label,
    error: String,
) -> String {
    format!(
        "{error}
{}: the late-bound configuration_field default resolves to {}, which the native profile cannot analyze here{}",
        crate::rule_declarations::canonical_form(rule),
        crate::rule_declarations::canonical_form(dependency),
        if dependency.repository == "bazel_tools" {
            " (@bazel_tools is not provided with --experimental-builtin-modules=false)"
        } else {
            ""
        }
    )
}

/// Labels that only late-bound attributes depend on. Their edges skip the
/// visibility check (CommonPrerequisiteValidator.validateDirectPrerequisiteVisibility,
/// analysis/CommonPrerequisiteValidator.java:156-159: Attribute.isAnalysisDependent).
pub(crate) fn analysis_dependent_labels(
    schema: &BTreeMap<String, AttrSpec>,
    attrs: &BTreeMap<String, AttrValue>,
) -> Vec<String> {
    let mut late = Vec::new();
    let mut ordinary = Vec::new();
    for (name, value) in attrs {
        let labels = crate::package::custom_dependency_labels(&BTreeMap::from([(
            name.clone(),
            value.clone(),
        )]));
        if schema
            .get(name)
            .is_some_and(|spec| spec.declaration.late_bound)
        {
            late.extend(labels);
        } else {
            ordinary.extend(labels);
        }
    }
    late.retain(|label| !ordinary.contains(label));
    late.into_iter().map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_fields_follow_bazel_option_defaults_and_others_stay_gaps() {
        let defaults = NativeOptions::default();
        for field in [
            "fdo_optimize",
            "fdo_prefetch_hints",
            "fdo_profile",
            "cs_fdo_profile",
            "propeller_optimize",
            "xbinary_fdo",
            "memprof_profile",
            "custom_malloc",
            "libc_top",
            "proto_profile_path",
            "zipper",
        ] {
            assert_eq!(resolve("cpp", field, &defaults), Some(None), "{field}");
        }
        assert_eq!(
            resolve("coverage", "output_generator", &defaults),
            Some(None)
        );
        assert_eq!(
            resolve("apple", "xcode_config_label", &defaults),
            Some(Some("@@bazel_tools//tools/cpp:host_xcodes".to_owned()))
        );
        for (fragment, field) in [
            ("java", "bytecode_optimizer"),
            ("py", "native_rules_allowlist"),
            ("bazel_py", "python_top"),
        ] {
            assert_eq!(
                resolve(fragment, field, &defaults),
                Some(None),
                "{fragment}.{field}"
            );
        }
        assert_eq!(
            resolve("proto", "proto_compiler", &defaults),
            Some(Some("@@bazel_tools//tools/proto:protoc".to_owned()))
        );
        for (fragment, field) in [
            ("android", "optimizing_dexer"),
            ("j2objc", "dead_code_report"),
        ] {
            assert_eq!(
                resolve(fragment, field, &defaults),
                None,
                "{fragment}.{field}"
            );
        }
    }

    /// The CppConfiguration fields of a configuration with the FDO and
    /// malloc options set (CppConfiguration.java:180-209, 308-319, 708-730).
    #[test]
    fn cpp_fields_follow_the_configuration() {
        use crate::configuration::{OptionValue, value_option};
        let set = |native: &mut NativeOptions, name: &str, value: OptionValue| {
            native.set_value(value_option(name).unwrap(), value);
        };
        let label =
            |text: &str| OptionValue::Label(crate::label::Label::parse_canonical(text).unwrap());
        let mut native = NativeOptions::default();
        set(&mut native, "custom_malloc", label("//m:malloc"));
        set(&mut native, "xbinary_fdo", label("//p:x.xfdo"));
        set(&mut native, "propeller_optimize", label("//p:prop"));
        assert_eq!(
            resolve("cpp", "custom_malloc", &native),
            Some(Some("//m:malloc".to_owned()))
        );
        assert_eq!(
            resolve("cpp", "xbinary_fdo", &native),
            Some(Some("//p:x.xfdo".to_owned()))
        );
        assert_eq!(
            resolve("cpp", "zipper", &native),
            Some(Some("@@bazel_tools//tools/zip:unzip_fdo".to_owned()))
        );
        assert_eq!(
            resolve("cpp", "propeller_optimize", &native),
            Some(Some("//p:prop".to_owned()))
        );
        set(
            &mut native,
            "fdo_optimize",
            OptionValue::String("//p:profile.afdo".to_owned()),
        );
        set(
            &mut native,
            "fdo_instrument",
            OptionValue::String("dir".to_owned()),
        );
        assert_eq!(
            resolve("cpp", "fdo_optimize", &native),
            Some(Some("//p:profile.afdo".to_owned()))
        );
        assert_eq!(resolve("cpp", "xbinary_fdo", &native), Some(None));
        assert_eq!(resolve("cpp", "propeller_optimize", &native), Some(None));
        set(
            &mut native,
            "fdo_optimize",
            OptionValue::String("/abs/profile.afdo".to_owned()),
        );
        assert_eq!(resolve("cpp", "fdo_optimize", &native), Some(None));
    }
}
