//! Late-bound `configuration_field(fragment, name)` attribute defaults
//! (StarlarkLateBoundDefault.resolve, analysis/starlark/StarlarkLateBoundDefault.java:57-66):
//! the value is the fragment field of the configuration the target is
//! analyzed in. Only fields whose value is fixed in every native
//! configuration are resolved; every other field stays an explicit
//! analysis-time gap (crate::custom_rule::AttrDeclaration::unanalyzed).
//!
//! The native profile models none of the command-line options these fields
//! read (crate::configuration::OPTIONS: an unmodelled option is rejected
//! before analysis), so their Bazel 9.3 defaults always apply.

use std::collections::BTreeMap;

use crate::custom_rule::{AttrSpec, AttrValue};

/// The configured value of a resolvable field: `Some(None)` is Bazel's null
/// label (the attribute is `None`, no dependency edge), `Some(Some(label))`
/// a canonical label. `None` means the field is not resolved natively.
pub(crate) fn resolve(fragment: &str, name: &str) -> Option<Option<&'static str>> {
    Some(match (fragment, name) {
        // CppConfiguration (rules/cpp/CppConfiguration.java): every field is
        // a CppOptions label whose option default is null, or derived only
        // from such options.
        //   fdo_optimize :632-635 (fdoOptimizeLabel from --fdo_optimize, :180-209)
        //   fdo_prefetch_hints :683-694, fdo_profile :696-699,
        //   cs_fdo_profile :701-706, propeller_optimize :709-717,
        //   xbinary_fdo :720-730, memprof_profile :733-738,
        //   custom_malloc :424-430 (CppOptions.java:311-320 default "null"),
        //   libc_top :756-759 (--grte_top, CppOptions.java:670-678 default "null"),
        //   proto_profile_path :958-961 (CppOptions.java:542-550 default "null"),
        //   zipper :309-319 (non-null only when an FDO option is set).
        (
            "cpp",
            "fdo_optimize" | "fdo_prefetch_hints" | "fdo_profile" | "cs_fdo_profile"
            | "propeller_optimize" | "xbinary_fdo" | "memprof_profile" | "custom_malloc"
            | "libc_top" | "proto_profile_path" | "zipper",
        ) => None,
        // CoverageConfiguration.outputGenerator (analysis/test/CoverageConfiguration.java:76-95):
        // null unless --collect_code_coverage, which the native profile never sets.
        ("coverage", "output_generator") => None,
        // AppleConfiguration.getXcodeConfigLabel (rules/apple/AppleConfiguration.java:99-100,
        // 320-327) is --xcode_version_config, default
        // "@bazel_tools//tools/cpp:host_xcodes" (rules/apple/AppleCommandLineOptions.java:293-296).
        // The annotation's defaultLabel //tools/objc:host_xcodes (:326) is only the
        // attribute's static (query-time) default, not the analysis value.
        ("apple", "xcode_config_label") => Some("@@bazel_tools//tools/cpp:host_xcodes"),
        // JavaConfiguration (rules/java/JavaConfiguration.java):
        //   launcher :236-243 (--java_launcher, JavaOptions.java:212-213 default "null"),
        //   proguard_top :246-253 (--proguard_top, JavaOptions.java:224-225 default "null"),
        //   java_toolchain_bytecode_optimizer :318-329 and
        //   local_java_optimization_configuration :332-340 are null unless
        //   --experimental_local_java_optimizations (JavaOptions.java:254-255 default false).
        //   bytecode_optimizer :304-311 is the label of the single
        //   --bytecode_optimizers entry, default "Proguard" without a label
        //   (:133-147; JavaOptions.java:239-246).
        (
            "java",
            "launcher"
            | "proguard_top"
            | "bytecode_optimizer"
            | "java_toolchain_bytecode_optimizer"
            | "local_java_optimization_configuration",
        ) => None,
        // PythonConfiguration.getNativeRulesAllowlist (rules/python/PythonConfiguration.java:60,
        // 146-153): --python_native_rules_allowlist, default null (PythonOptions.java:61-64).
        // BazelPythonConfiguration.getPythonTop always returns null
        // (bazel/rules/python/BazelPythonConfiguration.java:124-129).
        ("py", "native_rules_allowlist") | ("bazel_py", "python_top") => None,
        _ => return None,
    })
}

/// The configured values of the resolvable late-bound attributes of a
/// Starlark rule target, replacing their unconfigured (`None`) defaults.
pub(crate) fn configure(
    schema: &BTreeMap<String, AttrSpec>,
    attrs: &mut BTreeMap<String, AttrValue>,
) {
    for (name, spec) in schema {
        let Some(field) = &spec.declaration.configuration_field else {
            continue;
        };
        if let Some(value) = resolve(&field.fragment, &field.name) {
            attrs.insert(name.clone(), AttrValue::Label(value.map(str::to_owned)));
        }
    }
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
    late
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_fields_follow_bazel_option_defaults_and_others_stay_gaps() {
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
            assert_eq!(resolve("cpp", field), Some(None), "{field}");
        }
        assert_eq!(resolve("coverage", "output_generator"), Some(None));
        assert_eq!(
            resolve("apple", "xcode_config_label"),
            Some(Some("@@bazel_tools//tools/cpp:host_xcodes"))
        );
        for (fragment, field) in [
            ("java", "bytecode_optimizer"),
            ("py", "native_rules_allowlist"),
            ("bazel_py", "python_top"),
        ] {
            assert_eq!(resolve(fragment, field), Some(None), "{fragment}.{field}");
        }
        for (fragment, field) in [
            ("proto", "proto_compiler"),
            ("android", "optimizing_dexer"),
            ("j2objc", "dead_code_report"),
        ] {
            assert_eq!(resolve(fragment, field), None, "{fragment}.{field}");
        }
    }
}
