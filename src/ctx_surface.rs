//! N23c: the rule-implementation `ctx` members genuine rule sets read during
//! analysis: `ctx.fragments` and its configuration fragments,
//! `ctx.configuration`, `ctx.bin_dir`/`ctx.genfiles_dir`, `ctx.var`,
//! `ctx.exec_groups`, Make-variable expansion, `ctx.tokenize`,
//! `ctx.check_placeholders` and the feature sets (docs/CTX_SURFACE.md). Paths
//! below are relative to `src/main/java/com/google/devtools/build/lib/` of the
//! pinned Bazel 9.3 source. Values come from the configuration the target is
//! analyzed in (crate::engine); options the native profile does not model are
//! rejected on the command line, so their Bazel defaults are the values of
//! every native configuration. Unmodelled members fail explicitly.
use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::collections::StarlarkHasher;
use starlark::environment::{Methods, MethodsBuilder};
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::AllocTuple;
use starlark::values::{
    FrozenHeap, FrozenValue, Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic,
    StarlarkValue, Value, ValueLike, starlark_value,
};
use std::fmt;
use std::sync::{Arc, Mutex};

use crate::custom_rule::CppConfigurationView;
use crate::label::Label;
use crate::platforms::TargetPlatform;

/// The configuration facts of one analyzed target (set by crate::engine).
#[derive(Clone, Debug)]
pub struct CtxSurface {
    /// BuildConfigurationValue.getBinDirectory(RepositoryName.MAIN)
    /// (analysis/config/BuildConfigurationValue.java:398-415): the
    /// configuration's output root, `bazel-out/<mnemonic>/bin` (Bazel's `k8`
    /// CPU token since N41 closed divergence D1).
    pub bin_dir: String,
    pub compilation_mode: &'static str,
    /// CoreOptions.commandLineBuildVariables (`--define`), normalized: last
    /// value per key, sorted by key.
    pub defines: Vec<(String, String)>,
    /// CoreOptions.isExec.
    pub is_exec: bool,
    /// The no-config configuration (CommonOptions.EMPTY_OPTIONS): no
    /// configuration fragments and no PlatformOptions.
    pub no_config: bool,
    /// CoreOptions.stampBinaries (`--stamp`).
    pub stamp: bool,
    /// CoreOptions.collectCodeCoverage (`--collect_code_coverage`, set
    /// natively only by a Starlark transition, N47).
    pub coverage_enabled: bool,
    /// The instrumentation options of a coverage configuration (`None`
    /// without coverage).
    pub instrumenting: Option<Arc<crate::coverage::Instrumenting>>,
    /// PlatformOptions.platforms (normalized, never empty) and host_platform.
    pub platforms: Vec<Label>,
    pub host_platform: Label,
    /// JavaConfiguration.getPlugins: labels, in option order.
    pub java_plugins: Vec<Label>,
    /// BuildOptions.shortId: the first 7 hex digits of the native
    /// configuration identity (a different hash than Bazel's checksum).
    pub short_id: String,
    pub cpp: CppConfigurationView,
    /// The configuration's target platform for `$(TARGET_CPU)`
    /// (BuildConfigurationFunction.getPlatformCpu,
    /// skyframe/config/BuildConfigurationFunction.java:96-122).
    pub target_platform: TargetPlatform,
    /// TemplateVariableInfo variables of the resolved toolchains
    /// (ResolvedToolchainContext.load, analysis/ResolvedToolchainContext.java:56-85;
    /// RuleContext.fromToolchains, analysis/RuleContext.java:914-924), in order.
    pub template_variables: Vec<Vec<(String, String)>>,
    /// RuleContext.computeFeatures (analysis/RuleContext.java:268-276): the
    /// package's and the rule's `features` merged with the configuration's
    /// `--features` (`--host_features` in exec configurations).
    pub features: FeatureSet,
    /// The configuration's disabled default features
    /// (BuildConfigurationValue.getDisabledFeatures, :965-969): the `-name`
    /// entries of `--features`, sorted.
    pub disabled_default_features: Vec<String>,
    /// JavaOptions.javacOpts (`--javacopt`), as given
    /// (`ctx.fragments.java.default_javac_flags[_depset]`).
    pub javacopts: Vec<String>,
    /// Package.Metadata buildFilename relative to its source root
    /// (StarlarkRuleContext.getBuildFileRelativePath, StarlarkRuleContext.java:1007-1016).
    pub build_file_path: String,
    /// AppleCommandLineOptions `--{ios,watchos,tvos,macos}_sdk_version`
    /// (N40; DottedVersion.Option strings, `None` for null).
    pub apple_sdk_versions: crate::configuration::AppleSdkVersions,
    /// The configured target's shell action environment, the same value its
    /// actions use (analysis_runtime::Context::action_env, crate::action_env):
    /// `ctx.configuration.default_shell_env`.
    pub action_env: Arc<crate::action_env::ActionEnvironment>,
    /// The configuration's non-default table-driven option values
    /// (crate::configuration::NativeOptions::values), read by the
    /// `ctx.fragments.proto` getters.
    pub option_values: std::collections::BTreeMap<&'static str, crate::configuration::OptionValue>,
}

/// The currently supported Java classpath option. The fragment and native
/// compilation factory share it so the factory cannot silently change modes.
pub(crate) const JAVA_CLASSPATH_MODE: &str = "BAZEL";

impl CtxSurface {
    /// The configured value of the table-driven option `name`
    /// (crate::configuration::VALUE_OPTIONS).
    fn option_value(&self, name: &str) -> crate::configuration::OptionValue {
        self.option_values.get(name).cloned().unwrap_or_else(|| {
            crate::configuration::value_option(name)
                .expect("a table-driven option")
                .default_value()
        })
    }
}

impl Default for CtxSurface {
    fn default() -> Self {
        let host = crate::configuration::default_host_platform();
        Self {
            bin_dir: crate::label::GENERATED_EXEC_ROOT.to_owned(),
            compilation_mode: "fastbuild",
            defines: Vec::new(),
            is_exec: false,
            no_config: false,
            stamp: false,
            coverage_enabled: false,
            instrumenting: None,
            platforms: vec![host.clone()],
            host_platform: host,
            java_plugins: Vec::new(),
            short_id: String::new(),
            cpp: CppConfigurationView::default(),
            target_platform: TargetPlatform::Absent,
            template_variables: Vec::new(),
            features: FeatureSet::default(),
            disabled_default_features: Vec::new(),
            javacopts: Vec::new(),
            build_file_path: String::new(),
            apple_sdk_versions: Default::default(),
            action_env: Arc::new(
                crate::configuration::NativeOptions::default().action_environment(),
            ),
            option_values: Default::default(),
        }
    }
}

/// FeatureSet (packages/FeatureSet.java): sorted enabled and disabled names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeatureSet {
    pub on: Vec<String>,
    pub off: Vec<String>,
}

impl FeatureSet {
    fn from_map(map: std::collections::BTreeMap<String, bool>) -> Self {
        let mut set = Self::default();
        for (feature, state) in map {
            if state {
                set.on.push(feature);
            } else {
                set.off.push(feature);
            }
        }
        set
    }

    /// FeatureSet.parse: `-x` disables x and always trumps `x`;
    /// `no_layering_check` disables `layering_check`.
    pub fn parse<'a>(features: impl IntoIterator<Item = &'a str>) -> Self {
        let mut map = std::collections::BTreeMap::new();
        for feature in features {
            if let Some(off) = feature.strip_prefix('-') {
                map.insert(off.to_owned(), false);
            } else if feature == "no_layering_check" {
                map.insert("layering_check".to_owned(), false);
            } else {
                map.entry(feature.to_owned()).or_insert(true);
            }
        }
        Self::from_map(map)
    }

    /// FeatureSet.merge(coarse, fine): the finer set's states win.
    pub fn merge(coarse: &Self, fine: &Self) -> Self {
        let mut map = std::collections::BTreeMap::new();
        for set in [coarse, fine] {
            for feature in &set.on {
                map.insert(feature.clone(), true);
            }
            for feature in &set.off {
                map.insert(feature.clone(), false);
            }
        }
        Self::from_map(map)
    }

    /// FeatureSet.mergeWithGlobalFeatures(base, global): global on, base on,
    /// base off, global off.
    pub fn merge_with_global(base: &Self, global: &Self) -> Self {
        let mut map = std::collections::BTreeMap::new();
        for (set, state) in [
            (&global.on, true),
            (&base.on, true),
            (&base.off, false),
            (&global.off, false),
        ] {
            for feature in set {
                map.insert(feature.clone(), state);
            }
        }
        Self::from_map(map)
    }
}

/// The per-target view of the rule context (owned by crate::analysis).
#[derive(Debug, Default)]
pub struct RuleSurface {
    pub config: Arc<CtxSurface>,
    /// The rule's existing shared schema, used by native attribute expanders
    /// to distinguish an empty string list from an empty list of another type.
    pub schema: crate::custom_rule::SharedSchema,
    pub builtin_kind: crate::native_rules::StarlarkRuleKind,
    /// RuleContext.getRuleClassNameForLogging: the rule class name.
    pub rule_class: String,
    pub label: Label,
    /// `rule(fragments = [...])` (StarlarkRuleClassFunctions.java:1051-1052).
    pub declared_fragments: Vec<String>,
    /// `KIND rule LABEL` (Rule.toString), the exec-group error prefix.
    pub target_description: String,
    /// The rule defines an `alwayslink` attribute (ObjcConfiguration.targetShouldAlwayslink).
    pub has_alwayslink_attribute: bool,
    /// JavaHelper distinguishes explicitly empty resource_strip_prefix from
    /// an omitted/default value.
    pub explicit_resource_strip_prefix: bool,
    /// A rule attribute uses a Starlark transition (`ctx.split_attr`).
    pub split_transition_attribute: bool,
    /// `rule(_skylark_testable=True)` (`ctx.created_actions()`).
    pub skylark_testable: bool,
    /// The repository mapping of the rule's repository
    /// (`ctx.package_relative_label`).
    pub mapping: Arc<std::collections::BTreeMap<String, String>>,
    /// The location map of `ctx.expand_location` without `targets`
    /// (LocationExpander.buildLocationMap with collectSrcs, analysis/LocationExpander.java:393-485).
    pub(crate) locations: crate::location_expander::LocationMap,
    /// The `data` attribute's location entries (Expander.withDataLocations,
    /// LocationExpander allowData).
    pub(crate) data_locations: Vec<(String, Vec<crate::starlark_types::File>)>,
    /// Attribute and rule errors reported during the implementation
    /// (RuleContext.attributeError/ruleError): the target fails after the
    /// implementation returns.
    pub errors: Mutex<Vec<String>>,
}

impl RuleSurface {
    fn label_text(&self) -> String {
        crate::rule_declarations::canonical_form(&self.label)
    }

    /// EventHandlingErrorReporter.completeAttributeMessage (crate::rule_errors).
    fn attribute_error(&self, attribute: &str, message: &str) {
        let label = self.label_text();
        self.push(
            crate::rule_errors::RuleEvents {
                rule_class: &self.rule_class,
                label: &label,
            }
            .attribute_message(attribute, message),
        );
    }

    /// EventHandlingErrorReporter.prefixRuleMessage (crate::rule_errors).
    fn rule_error(&self, message: &str) {
        let label = self.label_text();
        self.push(
            crate::rule_errors::RuleEvents {
                rule_class: &self.rule_class,
                label: &label,
            }
            .rule_message(message),
        );
    }

    fn push(&self, error: String) {
        if let Ok(mut errors) = self.errors.lock() {
            errors.push(error);
        }
    }

    /// The reported errors, in order.
    pub fn take_errors(&self) -> Vec<String> {
        self.errors
            .lock()
            .map(|mut errors| std::mem::take(&mut *errors))
            .unwrap_or_default()
    }
}

/// An allocation on a private frozen heap the given heap keeps alive:
/// Bazel's immutable lists, dicts and structs (`trying to mutate a frozen ...`).
fn frozen<'v>(heap: Heap<'v>, build: impl FnOnce(&FrozenHeap) -> FrozenValue) -> Value<'v> {
    let frozen_heap = FrozenHeap::new();
    let value = build(&frozen_heap);
    let owner = frozen_heap.into_ref();
    heap.add_reference(&owner);
    value.to_value()
}

fn frozen_strings<'v>(heap: Heap<'v>, items: &[&str]) -> Value<'v> {
    if items.is_empty() {
        return crate::subrule_fragments::empty_list();
    }
    frozen(heap, |h| h.alloc(AllocList(items.iter().copied())))
}

fn frozen_owned_strings<'v>(heap: Heap<'v>, items: &[String]) -> Value<'v> {
    if items.is_empty() {
        return crate::subrule_fragments::empty_list();
    }
    frozen(heap, |h| {
        h.alloc(AllocList(items.iter().map(String::as_str)))
    })
}

fn frozen_dict<'v>(heap: Heap<'v>, entries: &[(String, String)]) -> Value<'v> {
    frozen(heap, |h| {
        h.alloc(AllocDict(
            entries.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        ))
    })
}

fn label_value<'v>(heap: Heap<'v>, label: &Label) -> anyhow::Result<Value<'v>> {
    crate::starlark_types::alloc_label(heap, &crate::rule_declarations::canonical_form(label))
}

/// `'T' value has no field or method 'X'` (net/starlark/java/eval/Starlark.java
/// getattr, with SpellChecker.didYouMean over the value's fields).
pub(crate) fn no_field_message(ty: &str, name: &str, fields: &[String]) -> String {
    let fields = fields.iter().map(String::as_str).collect::<Vec<_>>();
    format!(
        "'{ty}' value has no field or method '{name}'{}",
        crate::native_rules::did_you_mean(name, &fields)
    )
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

#[allow(unsafe_code)]
mod values {
    use super::*;

    /// ArtifactRoot (actions/ArtifactRoot.java), a derived root.
    #[derive(
        Clone,
        Debug,
        PartialEq,
        Eq,
        Hash,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        StarlarkPagablePanic,
    )]
    pub struct Root {
        pub path: String,
    }

    /// BuildConfigurationValue as `ctx.configuration`.
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub struct Configuration {
        #[allocative(skip)]
        pub surface: Arc<CtxSurface>,
    }

    /// FragmentCollection (analysis/FragmentCollection.java) of a rule context.
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub struct RuleFragments {
        #[allocative(skip)]
        pub rule: Arc<RuleSurface>,
    }

    /// A configuration fragment method bound to its fragment.
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub struct FragmentMethod {
        #[allocative(skip)]
        pub kind: Kind,
        pub name: &'static str,
        #[allocative(skip)]
        pub surface: Arc<CtxSurface>,
    }

    /// ApplePlatform (rules/apple/ApplePlatform.java:41-50).
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub struct ApplePlatform {
        pub name: &'static str,
        pub name_in_plist: &'static str,
        pub platform_type: &'static str,
        pub is_device: bool,
    }

    /// `ctx.super` (StarlarkRuleContext.callParent, StarlarkRuleContext.java:615-665).
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub struct SuperMethod;

    macro_rules! fragment_types {
        ($(($name:ident, $ty:literal, $kind:ident, $class:literal)),* $(,)?) => {
            $(
                #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
                pub struct $name {
                    #[allocative(skip)]
                    pub surface: Arc<CtxSurface>,
                }
                starlark::starlark_simple_value!($name);
                impl fmt::Display for $name {
                    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str(concat!("<unknown object ", $class, ">"))
                    }
                }
                #[starlark_value(type = $ty)]
                impl<'v> StarlarkValue<'v> for $name {
                    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
                        member_value(Kind::$kind, &self.surface, name, heap)
                    }
                    fn has_attr(&self, name: &str, _heap: Heap<'v>) -> bool {
                        member(Kind::$kind, name).is_some()
                    }
                    fn dir_attr(&self) -> Vec<String> {
                        member_names(Kind::$kind)
                    }
                    fn unknown_attribute_message(&self, name: &str) -> Option<String> {
                        Some(
                            coverage_member_gap(Kind::$kind, &self.surface, name)
                                .unwrap_or_else(|| unknown_member(Kind::$kind, name)),
                        )
                    }
                }
                impl $name {
                    pub fn alloc<'v>(heap: Heap<'v>, surface: Arc<CtxSurface>) -> Value<'v> {
                        heap.alloc(Self { surface })
                    }
                }
            )*
        };
    }

    // StarlarkBuiltin names and classes (bazel/rules/BazelRuleClassProvider.java:235-304).
    fragment_types!(
        (
            PlatformFragment,
            "platform",
            Platform,
            "com.google.devtools.build.lib.analysis.PlatformConfiguration"
        ),
        (
            CoverageFragment,
            "coverage",
            Coverage,
            "com.google.devtools.build.lib.analysis.test.CoverageConfiguration"
        ),
        (
            AndroidFragment,
            "android",
            Android,
            "com.google.devtools.build.lib.rules.android.AndroidConfiguration"
        ),
        (
            BazelAndroidFragment,
            "bazel_android",
            BazelAndroid,
            "com.google.devtools.build.lib.rules.android.BazelAndroidConfiguration"
        ),
        (
            AppleFragment,
            "apple",
            Apple,
            "com.google.devtools.build.lib.rules.apple.AppleConfiguration"
        ),
        (
            JavaFragment,
            "java",
            Java,
            "com.google.devtools.build.lib.rules.java.JavaConfiguration"
        ),
        (
            J2objcFragment,
            "j2objc",
            J2objc,
            "com.google.devtools.build.lib.rules.objc.J2ObjcConfiguration"
        ),
        (
            ObjcFragment,
            "objc",
            Objc,
            "com.google.devtools.build.lib.rules.objc.ObjcConfiguration"
        ),
        (
            ProtoFragment,
            "proto",
            Proto,
            "com.google.devtools.build.lib.rules.proto.ProtoConfiguration"
        ),
    );
}
pub(crate) use values::*;

starlark::starlark_simple_value!(Root);
starlark::starlark_simple_value!(Configuration);
starlark::starlark_simple_value!(RuleFragments);
starlark::starlark_simple_value!(FragmentMethod);
starlark::starlark_simple_value!(ApplePlatform);
starlark::starlark_simple_value!(SuperMethod);

impl fmt::Display for SuperMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<built-in method super of ctx value>")
    }
}

/// Native rules have no parent (`rule(parent=...)` is rejected at load time).
#[starlark_value(type = "builtin_function_or_method")]
impl<'v> StarlarkValue<'v> for SuperMethod {
    fn invoke(
        &self,
        _me: Value<'v>,
        _args: &Arguments<'v, '_>,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Err(starlark::Error::new_other(anyhow!(
            "Can't use 'super' call, the rule has no parent."
        )))
    }
}

/// `ctx.super`, a method whose name is a Rust keyword.
pub(crate) fn ctx_super<'v>(name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
    (name == "super").then(|| heap.alloc(SuperMethod))
}

// ---------------------------------------------------------------------------
// Root
// ---------------------------------------------------------------------------

impl fmt::Display for Root {
    /// ArtifactRoot.repr (actions/ArtifactRoot.java): `<derived root>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<derived root>")
    }
}

#[starlark_value(type = "root")]
impl<'v> StarlarkValue<'v> for Root {
    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        std::hash::Hash::hash(&self.path, hasher);
        Ok(())
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Root::from_value(other).is_some_and(|other| other == self))
    }

    /// FileRootApi.getExecPathString (starlarkbuildapi/FileRootApi.java).
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        (name == "path").then(|| heap.alloc(self.path.as_str()))
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["path".to_owned()]
    }

    fn unknown_attribute_message(&self, name: &str) -> Option<String> {
        Some(no_field_message("root", name, &self.dir_attr()))
    }
}

pub(crate) fn alloc_root<'v>(heap: Heap<'v>, path: &str) -> Value<'v> {
    heap.alloc(Root {
        path: path.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// ctx.configuration (starlarkbuildapi/BuildConfigurationApi.java)
// ---------------------------------------------------------------------------

impl fmt::Display for Configuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "<unknown object com.google.devtools.build.lib.analysis.config.BuildConfigurationValue>",
        )
    }
}

starlark::methods_static!(CONFIGURATION_METHODS = configuration_methods);

/// The BuildConfigurationApi members (dir order).
const CONFIGURATION_MEMBERS: [&str; 13] = [
    "bin_dir",
    "coverage_enabled",
    "default_shell_env",
    "disabled_features",
    "genfiles_dir",
    "has_separate_genfiles_directory",
    "host_path_separator",
    "is_sibling_repository_layout",
    "is_tool_configuration",
    "runfiles_enabled",
    "short_id",
    "stamp_binaries",
    "test_env",
];

#[starlark_value(type = "configuration")]
impl<'v> StarlarkValue<'v> for Configuration {
    fn get_methods() -> Option<&'static Methods> {
        Some(CONFIGURATION_METHODS.methods())
    }

    fn unknown_attribute_message(&self, name: &str) -> Option<String> {
        let fields = CONFIGURATION_MEMBERS.map(str::to_owned);
        Some(no_field_message("configuration", name, &fields))
    }
}

fn configuration<'v>(this: Value<'v>) -> anyhow::Result<&'v CtxSurface> {
    Ok(&this
        .downcast_ref::<Configuration>()
        .ok_or_else(|| anyhow!("expected configuration"))?
        .surface)
}

#[starlark_module]
fn configuration_methods(builder: &mut MethodsBuilder) {
    /// BuildConfigurationValue.getBinDir (BuildConfigurationValue.java:396-400).
    #[starlark(attribute)]
    fn bin_dir<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        Ok(alloc_root(heap, &configuration(this)?.bin_dir))
    }
    /// `--incompatible_merge_genfiles_directory` (default true): genfiles is bin
    /// (BuildConfigurationValue.java:430-434).
    #[starlark(attribute)]
    fn genfiles_dir<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        Ok(alloc_root(heap, &configuration(this)?.bin_dir))
    }
    /// BuildConfigurationValue.java:483-485 (`:` on Unix).
    #[starlark(attribute)]
    fn host_path_separator<'v>(this: Value<'v>) -> anyhow::Result<&'static str> {
        configuration(this)?;
        Ok(":")
    }
    /// The fixed action environment (BuildConfigurationValue.getLocalShellEnvironment,
    /// BuildConfigurationValue.java:555-558) of the configured target's
    /// configuration (N42, crate::action_env: strict PATH by default,
    /// `--action_env` / `--host_action_env` in exec configurations): the
    /// same environment its `use_default_shell_env` actions get.
    #[starlark(attribute)]
    fn default_shell_env<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let surface = configuration(this)?;
        let env = surface
            .action_env
            .default_shell_env()
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        Ok(frozen_dict(heap, &env))
    }
    /// BuildConfigurationValue.getTestEnv (:709-712): `--test_env` is
    /// accepted and recorded since N42, but TestOptions are trimmed from the
    /// modelled configurations (`--trim_test_configuration`, the untrimmed
    /// identity only; docs/ACTION_ENV.md), so empty. A test rule's own
    /// (untrimmed) configuration is not modelled (no test command).
    #[starlark(attribute)]
    fn test_env<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        configuration(this)?;
        Ok(frozen_dict(heap, &[]))
    }
    /// CoreOptions.collectCodeCoverage (:730-733): true only in a
    /// configuration a Starlark transition set `--collect_code_coverage` in
    /// (N47; the command-line option is rejected natively).
    #[starlark(attribute)]
    fn coverage_enabled<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        Ok(configuration(this)?.coverage_enabled)
    }
    /// BuildOptions.shortId (:735-738): the native identity's prefix.
    #[starlark(attribute)]
    fn short_id<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        Ok(heap.alloc(configuration(this)?.short_id.as_str()))
    }
    /// BuildConfigurationValue.stampBinariesForStarlark (:677-681).
    fn stamp_binaries<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        let surface = configuration(this).map_err(starlark::Error::new_other)?;
        crate::subrule_fragments::private_api(eval)?;
        Ok(surface.stamp)
    }
    /// isToolConfiguration = isExecConfiguration (:755-758).
    fn is_tool_configuration<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        Ok(configuration(this)?.is_exec)
    }
    /// :450-455 (merged genfiles: false).
    fn has_separate_genfiles_directory<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        configuration(this).map_err(starlark::Error::new_other)?;
        crate::subrule_fragments::private_api(eval)?;
        Ok(false)
    }
    /// :538-542; `--experimental_sibling_repository_layout` (default false).
    fn is_sibling_repository_layout<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        configuration(this).map_err(starlark::Error::new_other)?;
        crate::subrule_fragments::private_api(eval)?;
        Ok(false)
    }
    /// :888-892; BuildConfigurationValue.runfilesEnabled over
    /// getRunfileSymlinksMode (:866-882): the configured `--enable_runfiles`
    /// is YES, or AUTO (the default) off Windows
    /// (crate::configuration::VALUE_OPTIONS).
    fn runfiles_enabled<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<bool> {
        let surface = configuration(this).map_err(starlark::Error::new_other)?;
        crate::subrule_fragments::private_api(eval)?;
        Ok(
            surface.option_value("enable_runfiles")
                != crate::configuration::OptionValue::Enum("NO"),
        )
    }
    /// :965-969: the configuration's disabled default features
    /// (`-name` entries of `--features`), an immutable set.
    fn disabled_features<'v>(
        this: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let surface = configuration(this).map_err(starlark::Error::new_other)?;
        crate::subrule_fragments::private_api(eval)?;
        frozen_string_set(eval.heap(), &surface.disabled_default_features)
            .map_err(starlark::Error::new_other)
    }
}

/// An immutable Starlark set of `items` (StarlarkSet.immutableCopyOf).
fn frozen_string_set<'v>(heap: Heap<'v>, items: &[String]) -> anyhow::Result<Value<'v>> {
    use starlark::environment::{GlobalsBuilder, LibraryExtension, Module};
    use starlark::syntax::{AstModule, Dialect};
    let owned = Module::with_temp_heap(|module| -> anyhow::Result<_> {
        {
            module.set(
                "items",
                module
                    .heap()
                    .alloc(AllocList(items.iter().map(String::as_str))),
            );
            let mut eval = Evaluator::new(&module);
            let globals = GlobalsBuilder::extended_by(&[LibraryExtension::SetType]).build();
            eval.eval_module(
                AstModule::parse(
                    "set.star",
                    "s = set(items)\n".to_owned(),
                    &Dialect::Standard,
                )
                .map_err(|error| anyhow!(error.to_string()))?,
                &globals,
            )
            .map_err(|error| anyhow!(error.to_string()))?;
        }
        module
            .freeze()
            .map_err(|error| anyhow!("{error:?}"))?
            .get("s")
            .map_err(|error| anyhow!(error.to_string()))
    })?;
    Ok(heap.access_owned_frozen_value(&owned))
}

// ---------------------------------------------------------------------------
// ctx.fragments
// ---------------------------------------------------------------------------

/// The Starlark-visible fragments of a Bazel 9.3 configuration in
/// BuildConfigurationValue field order: the fragment classes sorted by
/// class name (FragmentClassSet.LEXICAL_FRAGMENT_SORTER,
/// analysis/config/FragmentClassSet.java:45-53;
/// BuildConfigurationValue.buildIndexOfStarlarkVisibleFragments, :360-370).
pub(crate) const CONFIGURATION_FRAGMENTS: [&str; 10] = [
    "platform",
    "coverage",
    "android",
    "bazel_android",
    "apple",
    "cpp",
    "java",
    "j2objc",
    "objc",
    "proto",
];

/// FragmentRegistry universal fragments (BazelRuleClassProvider.java:235-237)
/// with a Starlark name: accessible without a declaration (RuleContext.isLegalFragment).
const UNIVERSAL_FRAGMENTS: [&str; 1] = ["platform"];

impl fmt::Display for RuleFragments {
    /// FragmentCollection does not override StarlarkValue.repr.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<unknown object com.google.devtools.build.lib.analysis.FragmentCollection>")
    }
}

starlark::methods_static!(RULE_FRAGMENT_METHODS = rule_fragment_methods);

#[starlark_value(type = "fragments")]
impl<'v> StarlarkValue<'v> for RuleFragments {
    fn get_methods() -> Option<&'static Methods> {
        Some(RULE_FRAGMENT_METHODS.methods())
    }

    fn unknown_attribute_message(&self, name: &str) -> Option<String> {
        Some(no_fragment_message(&self.rule, name))
    }
}

fn field_names(rule: &RuleSurface) -> &'static [&'static str] {
    if rule.config.no_config {
        &[]
    } else {
        &CONFIGURATION_FRAGMENTS
    }
}

/// FragmentCollectionApi.getErrorMessageForUnknownField
/// (starlarkbuildapi/FragmentCollectionApi.java:39-49).
fn no_fragment_message(rule: &RuleSurface, name: &str) -> String {
    format!(
        "There is no configuration fragment named '{name}'. Available fragments: '{}'",
        field_names(rule).join("', '")
    )
}

/// RuleContext.getStarlarkFragment (analysis/RuleContext.java:511-530).
fn rule_fragment<'v>(this: Value<'v>, name: &str, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let rule = &this
        .downcast_ref::<RuleFragments>()
        .ok_or_else(|| anyhow!("expected fragments"))?
        .rule;
    if !field_names(rule).contains(&name) {
        bail!("{}", no_fragment_message(rule, name));
    }
    require_declared(rule, name)?;
    let surface = rule.config.clone();
    Ok(match name {
        "cpp" => crate::subrule_fragments::alloc_cpp(heap, surface.cpp.clone()),
        "platform" => PlatformFragment::alloc(heap, surface),
        "coverage" => CoverageFragment::alloc(heap, surface),
        "android" => AndroidFragment::alloc(heap, surface),
        "bazel_android" => BazelAndroidFragment::alloc(heap, surface),
        "apple" => AppleFragment::alloc(heap, surface),
        "java" => JavaFragment::alloc(heap, surface),
        "j2objc" => J2objcFragment::alloc(heap, surface),
        "objc" => ObjcFragment::alloc(heap, surface),
        "proto" => ProtoFragment::alloc(heap, surface),
        _ => bail!("{}", no_fragment_message(rule, name)),
    })
}

fn require_declared(rule: &RuleSurface, name: &str) -> anyhow::Result<()> {
    if !UNIVERSAL_FRAGMENTS.contains(&name) && !rule.declared_fragments.iter().any(|d| d == name) {
        bail!(
            "{} has to declare '{name}' as a required fragment in order to access it. Please update the 'fragments' argument of the rule definition (for example: fragments = [\"{name}\"])",
            rule.rule_class
        );
    }
    Ok(())
}

#[starlark_module]
fn rule_fragment_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn android<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        rule_fragment(this, "android", heap)
    }
    #[starlark(attribute)]
    fn apple<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        rule_fragment(this, "apple", heap)
    }
    #[starlark(attribute)]
    fn bazel_android<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        rule_fragment(this, "bazel_android", heap)
    }
    #[starlark(attribute)]
    fn coverage<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        rule_fragment(this, "coverage", heap)
    }
    #[starlark(attribute)]
    fn cpp<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        rule_fragment(this, "cpp", heap)
    }
    #[starlark(attribute)]
    fn j2objc<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        rule_fragment(this, "j2objc", heap)
    }
    #[starlark(attribute)]
    fn java<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        rule_fragment(this, "java", heap)
    }
    #[starlark(attribute)]
    fn objc<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        rule_fragment(this, "objc", heap)
    }
    #[starlark(attribute)]
    fn platform<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        rule_fragment(this, "platform", heap)
    }
    #[starlark(attribute)]
    fn proto<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        rule_fragment(this, "proto", heap)
    }
}

/// `hasattr(ctx.fragments, F)`: Structure.getValue, which raises for an
/// undeclared fragment (RuleContext.getStarlarkFragment); `dir()` lists the
/// configuration's fragments (FragmentCollection.getFieldNames).
pub(crate) fn fragments_hasattr(value: Value<'_>, name: &str) -> Option<anyhow::Result<bool>> {
    let fragments = value.downcast_ref::<RuleFragments>()?;
    let rule = &fragments.rule;
    Some((|| {
        if !field_names(rule).contains(&name) {
            return Ok(false);
        }
        require_declared(rule, name)?;
        Ok(true)
    })())
}

pub(crate) fn fragments_dir(value: Value<'_>) -> Option<Vec<String>> {
    let fragments = value.downcast_ref::<RuleFragments>()?;
    let mut names = field_names(&fragments.rule)
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    names.sort();
    Some(names)
}

pub(crate) fn alloc_rule_fragments<'v>(heap: Heap<'v>, rule: Arc<RuleSurface>) -> Value<'v> {
    heap.alloc(RuleFragments { rule })
}

// ---------------------------------------------------------------------------
// Configuration fragments
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Platform,
    Coverage,
    Android,
    BazelAndroid,
    Apple,
    Java,
    J2objc,
    Objc,
    Proto,
}

impl Kind {
    fn type_name(self) -> &'static str {
        match self {
            Kind::Platform => "platform",
            Kind::Coverage => "coverage",
            Kind::Android => "android",
            Kind::BazelAndroid => "bazel_android",
            Kind::Apple => "apple",
            Kind::Java => "java",
            Kind::J2objc => "j2objc",
            Kind::Objc => "objc",
            Kind::Proto => "proto",
        }
    }
}

type Dynamic = for<'v> fn(&CtxSurface, Heap<'v>) -> anyhow::Result<Value<'v>>;

#[derive(Clone, Copy)]
enum V {
    Bool(bool),
    Int(i32),
    Str(&'static str),
    None,
    Strs(&'static [&'static str]),
    Dyn(Dynamic),
}

#[derive(Clone, Copy)]
enum Member {
    /// A `structField = true` member.
    Field(V),
    /// A method without parameters; `private` when it calls
    /// BuiltinRestriction.failIfCalledOutsideDefaultAllowlist.
    Method { private: bool, value: V },
    /// ObjcConfiguration.target_should_alwayslink(ctx) (private).
    AlwaysLink,
    /// Not modelled: reading it fails explicitly.
    Gap,
}

use Member::{Field, Gap, Method};

/// CoverageConfiguration.outputGenerator (analysis/test/CoverageConfiguration.java:76-95):
/// null unless coverage is enabled, then `--coverage_output_generator`
/// (default `@bazel_tools//tools/test:lcov_merger`).
fn coverage_output_generator<'v>(
    surface: &CtxSurface,
    heap: Heap<'v>,
) -> anyhow::Result<Value<'v>> {
    if surface.coverage_enabled {
        let text = crate::coverage::explicit_output_generator()
            .unwrap_or(crate::coverage::DEFAULT_OUTPUT_GENERATOR);
        let label = Label::parse_canonical(text).map_err(anyhow::Error::msg)?;
        return label_value(heap, &label);
    }
    Ok(Value::new_none())
}

fn host_platform<'v>(surface: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    label_value(heap, &surface.host_platform)
}

/// PlatformConfiguration.getTargetPlatform: the first `--platforms` entry
/// (PlatformOptions.computeTargetPlatform).
fn target_platform<'v>(surface: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    label_value(
        heap,
        surface.platforms.first().unwrap_or(&surface.host_platform),
    )
}

fn target_platforms<'v>(surface: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let labels = surface
        .platforms
        .iter()
        .map(crate::rule_declarations::canonical_form)
        .collect::<Vec<_>>();
    let mut failure = None;
    let value = frozen(heap, |h| {
        let mut items = Vec::new();
        for label in &labels {
            match crate::starlark_types::alloc_frozen_label(h, label) {
                Ok(value) => items.push(value),
                Err(error) => failure = Some(error),
            }
        }
        h.alloc(AllocList(items))
    });
    match failure {
        Some(error) => Err(error),
        None => Ok(value),
    }
}

/// ObjcConfiguration.getCoptsForCompilationMode (rules/objc/ObjcConfiguration.java:119-137)
/// with `--incompatible_avoid_hardcoded_objc_compilation_flags` (default true,
/// rules/objc/ObjcCommandLineOptions.java:105-106) and
/// `--experimental_objc_fastbuild_options` (default `-O0,-DDEBUG=1`, :61-62).
fn objc_copts<'v>(surface: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    Ok(match surface.compilation_mode {
        "fastbuild" => frozen_strings(heap, &["-O0", "-DDEBUG=1"]),
        _ => crate::subrule_fragments::empty_list(),
    })
}

/// AppleConfiguration.get{Ios,Macos,Tvos,Watchos}SdkVersionFlag
/// (rules/apple/AppleConfiguration.java:104-107): the option's
/// DottedVersion, None while unset.
fn sdk_version_flag<'v>(
    surface: &CtxSurface,
    option: &str,
    heap: Heap<'v>,
) -> anyhow::Result<Value<'v>> {
    match surface.apple_sdk_versions.get(option).cloned().flatten() {
        None => Ok(Value::new_none()),
        Some(text) => crate::apple_xcode::DottedVersion::parse(&text)
            .map(|version| heap.alloc(version))
            .map_err(anyhow::Error::msg),
    }
}

fn ios_sdk_version_flag<'v>(surface: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    sdk_version_flag(surface, "ios_sdk_version", heap)
}

fn macos_sdk_version_flag<'v>(surface: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    sdk_version_flag(surface, "macos_sdk_version", heap)
}

fn tvos_sdk_version_flag<'v>(surface: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    sdk_version_flag(surface, "tvos_sdk_version", heap)
}

fn watchos_sdk_version_flag<'v>(surface: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    sdk_version_flag(surface, "watchos_sdk_version", heap)
}

/// The host CPU's AppleCommandLineOptions defaults
/// (rules/apple/AppleCommandLineOptions.java:174-192; AppleConfiguration.java:73).
fn host_is_aarch64() -> bool {
    std::env::consts::ARCH == "aarch64"
}

fn default_macos_cpu() -> &'static str {
    if host_is_aarch64() { "arm64" } else { "x86_64" }
}

/// AppleConfiguration.getAppleCpusForStarlark (rules/apple/AppleConfiguration.java:172-182)
/// over AppleCpus.create (:119-154) with every CPU option at its default.
fn apple_cpus<'v>(_: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let arm = host_is_aarch64();
    let ios = if arm { "sim_arm64" } else { "x86_64" };
    let watchos = if arm { "arm64" } else { "x86_64" };
    let tvos = if arm { "sim_arm64" } else { "x86_64" };
    let macos = default_macos_cpu();
    Ok(frozen(heap, |h| {
        let empty = h.alloc("");
        let tuple = |cpu: &str| h.alloc(AllocTuple([cpu]));
        h.alloc(AllocStruct([
            ("apple_split_cpu", empty),
            ("catalyst_cpus", tuple("x86_64")),
            ("ios_multi_cpus", tuple(ios)),
            ("macos_cpus", tuple(macos)),
            ("tvos_cpus", tuple(tvos)),
            ("visionos_cpus", tuple("sim_arm64")),
            ("watchos_cpus", tuple(watchos)),
        ]))
    }))
}

/// AppleConfiguration.getSingleArchitecture (:212-249) for
/// `--apple_platform_type=macos` (default): the first macOS CPU.
fn single_arch_cpu<'v>(_: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    Ok(heap.alloc(default_macos_cpu()))
}

/// AppleConfiguration.getSingleArchPlatform (:256-259):
/// ApplePlatform.forTarget("macos", cpu) is MACOS for `darwin_x86_64` and
/// `darwin_arm64` (rules/apple/ApplePlatform.java:44, 71-72, 140, 154-155).
fn single_arch_platform<'v>(_: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    Ok(heap.alloc(ApplePlatform {
        name: "macos",
        name_in_plist: "MacOSX",
        platform_type: "macos",
        is_device: true,
    }))
}

/// JavaConfiguration.getDefaultJvmFlags (rules/java/JavaConfiguration.java:116, 225-228):
/// `--jvmopt`, which the exec transition sets to `-XX:ErrorFile=/dev/stderr`
/// when `--host_jvmopt` is empty (builtin_exec_platforms.bzl:350-354; both
/// options are rejected natively, so at their defaults).
fn default_jvm_opts<'v>(surface: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    Ok(if surface.is_exec {
        frozen_strings(heap, &["-XX:ErrorFile=/dev/stderr"])
    } else {
        crate::subrule_fragments::empty_list()
    })
}

/// ObjcConfiguration.useDeviceDebugEntitlements (rules/objc/ObjcConfiguration.java:150-157):
/// `--device_debug_entitlements` (default true) and a compilation mode other
/// than `opt` (an exec configuration's is `--host_compilation_mode`).
fn device_debug_entitlements<'v>(surface: &CtxSurface, _: Heap<'v>) -> anyhow::Result<Value<'v>> {
    Ok(Value::new_bool(surface.compilation_mode != "opt"))
}

/// ProtoConfiguration.experimentalProtoDescriptorSetsIncludeSourceInfo
/// (rules/proto/ProtoConfiguration.java:174-186).
fn proto_descriptor_sets_include_source_info<'v>(
    surface: &CtxSurface,
    _: Heap<'v>,
) -> anyhow::Result<Value<'v>> {
    match surface.option_value("experimental_proto_descriptor_sets_include_source_info") {
        crate::configuration::OptionValue::Bool(value) => Ok(Value::new_bool(value)),
        other => anyhow::bail!("internal error: boolean option value {other}"),
    }
}

/// ProtoConfiguration.strictProtoDepsForStarlark: StrictDepsMode.toString
/// (rules/proto/ProtoConfiguration.java:233-237).
fn strict_proto_deps<'v>(surface: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    Ok(heap.alloc(surface.option_value("strict_proto_deps").to_string()))
}

/// ProtoConfiguration.strictPublicImportsForStarlark (:239-243).
fn strict_public_imports<'v>(surface: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    Ok(heap.alloc(surface.option_value("strict_public_imports").to_string()))
}

/// JavaConfiguration.commandLineJavacFlags (rules/java/JavaConfiguration.java:107-108):
/// JavaHelper.detokenizeJavaOptions of the tokenized `--javacopt` values —
/// nothing, or one string of the ShellUtils.shellEscape'd tokens joined by
/// spaces (rules/java/JavaHelper.java:56-95).
fn command_line_javac_flags(surface: &CtxSurface) -> Option<String> {
    let mut tokens = Vec::new();
    for option in &surface.javacopts {
        // A tokenization failure keeps the tokens scanned before it, then
        // the option as given (JavaHelper.tokenizeJavaOptions :61-74).
        if tokenize_into(&mut tokens, option).is_err() {
            tokens.push(option.clone());
        }
    }
    if tokens.is_empty() {
        return None;
    }
    Some(
        tokens
            .iter()
            .map(|token| shell_utils_escape(token))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// ShellUtils.shellEscape (shell/ShellUtils.java:28-53): letters, digits
/// and `@%-_+:,./` are safe; the empty string is `''`.
fn shell_utils_escape(word: &str) -> String {
    if word.is_empty() {
        return "''".to_owned();
    }
    if word
        .chars()
        .all(|c| c.is_alphanumeric() || "@%-_+:,./".contains(c))
    {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// JavaConfiguration.getDefaultJavacFlagsStarlark (:168-172): a
/// NAIVE_LINK_ORDER (`preorder`) depset of [`command_line_javac_flags`].
fn javac_flags_depset<'v>(surface: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    match command_line_javac_flags(surface) {
        None => crate::starlark_types::empty_preorder_string_depset(heap),
        Some(flags) => Ok(heap.alloc(crate::starlark_types::Depset::compose(
            vec![crate::starlark_types::Item::String(flags)],
            Vec::new(),
            crate::starlark_types::Order::Preorder,
            Some(crate::starlark_types::ItemType::String),
        )?)),
    }
}

/// JavaConfiguration.getDefaultJavacFlagsForStarlarkAsList (:160-166): the
/// tokenized [`command_line_javac_flags`].
fn default_javac_flags<'v>(surface: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let flags = command_line_javac_flags(surface)
        .map(|flags| tokenize(&flags).unwrap_or_else(|_| vec![flags]))
        .unwrap_or_default();
    Ok(frozen_owned_strings(heap, &flags))
}

fn java_plugins<'v>(surface: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    Ok(heap.alloc(
        surface
            .java_plugins
            .iter()
            .map(|label| crate::starlark_types::alloc_label(heap, &label.to_string()))
            .collect::<anyhow::Result<Vec<_>>>()?,
    ))
}

/// AndroidConfiguration (rules/android/AndroidConfiguration.java) members:
/// not modelled.
const ANDROID_MEMBERS: [&str; 37] = [
    "apk_signing_method_v1",
    "apk_signing_method_v2",
    "apk_signing_method_v4",
    "assume_min_sdk_version",
    "break_build_on_parallel_dex2oat_failure",
    "check_desugar_deps",
    "check_for_migration_tag",
    "compress_java_resources",
    "desugar_java8",
    "desugar_java8_libs",
    "filter_library_jar_with_program_jar",
    "fixed_resource_neverlinking",
    "get_dexopts_supported_in_dex_merger",
    "get_dexopts_supported_in_dex_sharder",
    "get_dexopts_supported_in_incremental_dexing",
    "get_exports_manifest_default",
    "get_java_resources_from_optimized_jar",
    "get_one_version_enforcement_use_transitive_jars_for_binary_under_test",
    "get_output_directory_name",
    "get_target_dexopts_that_prevent_incremental_dexing",
    "incremental_dexing_after_proguard_by_default",
    "incremental_dexing_shards_after_proguard",
    "incremental_dexing_use_dex_sharder",
    "manifest_merger",
    "omit_resources_info_provider_from_android_binary",
    "persistent_aar_extractor",
    "persistent_android_dex_desugar",
    "persistent_busybox_tools",
    "persistent_multiplex_android_dex_desugar",
    "persistent_multiplex_busybox_tools",
    "use_android_resource_cycle_shrinking",
    "use_android_resource_name_obfuscation",
    "use_android_resource_path_shortening",
    "use_android_resource_shrinking",
    "use_incremental_dexing",
    "use_parallel_dex2oat",
    "use_rex_to_compress_dex_files",
];

/// The members of each fragment, in `dir()` order, with their Bazel 9.3
/// values in a native configuration (every unmodelled option at its default).
fn members(kind: Kind) -> &'static [(&'static str, Member)] {
    match kind {
        // starlarkbuildapi/platform/PlatformConfigurationApi.java.
        Kind::Platform => &[
            ("host_platform", Field(V::Dyn(host_platform))),
            ("platform", Field(V::Dyn(target_platform))),
            ("platforms", Field(V::Dyn(target_platforms))),
        ],
        // CoverageConfiguration.outputGenerator (analysis/test/CoverageConfiguration.java:76-95):
        // null without `--collect_code_coverage`; the CoverageOptions label
        // otherwise, which is an explicit gap (coverage_output_generator).
        Kind::Coverage => &[("output_generator", Field(V::Dyn(coverage_output_generator)))],
        Kind::Android => &[],
        // BazelAndroidConfiguration (rules/android/BazelAndroidConfiguration.java).
        Kind::BazelAndroid => &[("merge_android_manifest_permissions", Gap)],
        // starlarkbuildapi/apple/AppleConfigurationApi.java; AppleCommandLineOptions
        // defaults (rules/apple/AppleCommandLineOptions.java:49-196, 305-306):
        // the version flags are null, `--experimental_prefer_mutual_xcode`
        // true, `--experimental_include_xcode_execution_requirements` false.
        // The `*_sdk_version_flag`s read the configuration's
        // `--{ios,macos,tvos,watchos}_sdk_version` (N40) as DottedVersion
        // (crate::apple_xcode, N25d2a). The other version flags stay null
        // only while `--xcode_version` and `--{ios,macos,tvos,watchos}_minimum_os`
        // are rejected by the native option parser; once modelled they must
        // read the configuration's values the same way. Test hooks:
        // tests/ctx_surface.rs apple_version_flags_stay_none_while_the_options_are_rejected
        // and apple_sdk_version_flags_read_the_configuration.
        Kind::Apple => &[
            ("apple_cpus", Field(V::Dyn(apple_cpus))),
            ("apple_platform_type", Field(V::Str("macos"))),
            ("include_xcode_exec_requirements", Field(V::Bool(false))),
            ("ios_minimum_os_flag", Field(V::None)),
            ("ios_sdk_version_flag", Field(V::Dyn(ios_sdk_version_flag))),
            ("macos_minimum_os_flag", Field(V::None)),
            (
                "macos_sdk_version_flag",
                Field(V::Dyn(macos_sdk_version_flag)),
            ),
            ("prefer_mutual_xcode", Field(V::Bool(true))),
            ("single_arch_cpu", Field(V::Dyn(single_arch_cpu))),
            ("single_arch_platform", Field(V::Dyn(single_arch_platform))),
            ("tvos_minimum_os_flag", Field(V::None)),
            (
                "tvos_sdk_version_flag",
                Field(V::Dyn(tvos_sdk_version_flag)),
            ),
            ("watchos_minimum_os_flag", Field(V::None)),
            (
                "watchos_sdk_version_flag",
                Field(V::Dyn(watchos_sdk_version_flag)),
            ),
            ("xcode_version_flag", Field(V::None)),
        ],
        // starlarkbuildapi/java/JavaConfigurationApi.java over JavaConfiguration
        // (rules/java/JavaConfiguration.java:180-416) with JavaOptions defaults
        // (rules/java/JavaOptions.java); the thread-taking methods call
        // JavaStarlarkCommon.checkPrivateAccess (rules/java/JavaStarlarkCommon.java:291-293).
        Kind::Java => &[
            ("add_test_support_to_compile_deps", Field(V::Bool(true))),
            (
                "auto_create_java_test_deploy_jars",
                Method {
                    private: true,
                    value: V::Bool(false),
                },
            ),
            ("bytecode_optimization_pass_actions", Field(V::Int(1))),
            ("bytecode_optimizer_mnemonic", Field(V::Str("Proguard"))),
            ("default_javac_flags", Field(V::Dyn(default_javac_flags))),
            (
                "default_javac_flags_depset",
                Field(V::Dyn(javac_flags_depset)),
            ),
            ("default_jvm_opts", Field(V::Dyn(default_jvm_opts))),
            (
                "disallow_java_import_exports",
                Method {
                    private: true,
                    value: V::Bool(false),
                },
            ),
            (
                "enforce_explicit_java_test_deps",
                Method {
                    private: true,
                    value: V::Bool(false),
                },
            ),
            ("enforce_proguard_file_extension", Field(V::Bool(false))),
            (
                "generate_java_deps",
                Method {
                    private: true,
                    value: V::Bool(true),
                },
            ),
            ("multi_release_deploy_jars", Field(V::Bool(true))),
            ("one_version_enforcement_level", Field(V::Str("OFF"))),
            (
                "one_version_enforcement_on_java_tests",
                Field(V::Bool(true)),
            ),
            ("plugins", Field(V::Dyn(java_plugins))),
            (
                "reduce_java_classpath",
                Method {
                    private: true,
                    value: V::Str(JAVA_CLASSPATH_MODE),
                },
            ),
            ("run_android_lint", Field(V::Bool(false))),
            ("split_bytecode_optimization_pass", Field(V::Bool(false))),
            ("strict_java_deps", Field(V::Str("default"))),
            (
                "use_header_compilation",
                Method {
                    private: true,
                    value: V::Bool(true),
                },
            ),
            (
                "use_header_compilation_direct_deps",
                Method {
                    private: true,
                    value: V::Bool(true),
                },
            ),
            (
                "use_ijars",
                Method {
                    private: true,
                    value: V::Bool(true),
                },
            ),
        ],
        // J2ObjcConfiguration.getTranslationFlags (rules/objc/J2ObjcConfiguration.java):
        // the default flags.
        Kind::J2objc => &[(
            "translation_flags",
            Field(V::Strs(&[
                "-g:relative",
                "-encoding",
                "UTF-8",
                "--doc-comments",
                "-XcombineJars",
                "-XDinjectLogSites=true",
            ])),
        )],
        // starlarkbuildapi/cpp/ObjcConfigurationApi.java; ObjcCommandLineOptions defaults.
        Kind::Objc => &[
            ("alwayslink_by_default", Field(V::Bool(false))),
            ("builtin_objc_strip_action", Field(V::Bool(true))),
            (
                "copts_for_current_compilation_mode",
                Field(V::Dyn(objc_copts)),
            ),
            ("disallow_sdk_frameworks_attributes", Field(V::Bool(false))),
            ("ios_simulator_device", Field(V::None)),
            ("ios_simulator_version", Field(V::None)),
            ("run_memleaks", Field(V::Bool(false))),
            ("signing_certificate_name", Field(V::None)),
            ("strip_executable_safely", Field(V::Bool(false))),
            ("target_should_alwayslink", Member::AlwaysLink),
            (
                "uses_device_debug_entitlements",
                Field(V::Dyn(device_debug_entitlements)),
            ),
        ],
        // rules/proto/ProtoConfiguration.java:50-241: the configured
        // `--experimental_proto_descriptor_sets_include_source_info`,
        // `--strict_proto_deps` and `--strict_public_imports`
        // (StrictDepsMode.toString, `ERROR`/`OFF` by default; table-driven
        // options, crate::configuration::VALUE_OPTIONS).
        Kind::Proto => &[
            (
                "cc_proto_library_header_suffixes",
                Field(V::Strs(&[".pb.h"])),
            ),
            (
                "cc_proto_library_source_suffixes",
                Field(V::Strs(&[".pb.cc"])),
            ),
            (
                "experimental_proto_descriptorsets_include_source_info",
                Method {
                    private: true,
                    value: V::Dyn(proto_descriptor_sets_include_source_info),
                },
            ),
            ("experimental_protoc_opts", Field(V::Strs(&[]))),
            (
                "strict_proto_deps",
                Method {
                    private: true,
                    value: V::Dyn(strict_proto_deps),
                },
            ),
            (
                "strict_public_imports",
                Method {
                    private: true,
                    value: V::Dyn(strict_public_imports),
                },
            ),
        ],
    }
}

fn member(kind: Kind, name: &str) -> Option<Member> {
    if kind == Kind::Android {
        return ANDROID_MEMBERS.contains(&name).then_some(Gap);
    }
    members(kind)
        .iter()
        .find(|(member, _)| *member == name)
        .map(|(_, member)| *member)
}

fn member_names(kind: Kind) -> Vec<String> {
    if kind == Kind::Android {
        return ANDROID_MEMBERS.map(str::to_owned).to_vec();
    }
    members(kind)
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect()
}

/// The explicit failure of a fragment member whose value needs coverage
/// instrumentation options (N47), reported instead of the generic message.
fn coverage_member_gap(_kind: Kind, _surface: &CtxSurface, _name: &str) -> Option<String> {
    None
}

fn unknown_member(kind: Kind, name: &str) -> String {
    match member(kind, name) {
        Some(_) => format!(
            "configuration fragment member '{}.{name}' is not implemented by the native profile",
            kind.type_name()
        ),
        None => no_field_message(kind.type_name(), name, &member_names(kind)),
    }
}

fn value_of<'v>(value: V, surface: &CtxSurface, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    Ok(match value {
        V::Bool(value) => Value::new_bool(value),
        V::Int(value) => heap.alloc(value),
        V::Str(value) => heap.alloc(value),
        V::None => Value::new_none(),
        V::Strs(items) => frozen_strings(heap, items),
        V::Dyn(function) => function(surface, heap)?,
    })
}

fn member_value<'v>(
    kind: Kind,
    surface: &Arc<CtxSurface>,
    name: &str,
    heap: Heap<'v>,
) -> Option<Value<'v>> {
    let (name, member) = if kind == Kind::Android {
        return None;
    } else {
        members(kind).iter().find(|(member, _)| *member == name)?
    };
    match member {
        Field(value) => value_of(*value, surface, heap).ok(),
        Method { .. } | Member::AlwaysLink => Some(heap.alloc(FragmentMethod {
            kind,
            name,
            surface: surface.clone(),
        })),
        Gap => None,
    }
}

impl fmt::Display for FragmentMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "<built-in method {} of {} value>",
            self.name,
            self.kind.type_name()
        )
    }
}

#[starlark_value(type = "builtin_function_or_method")]
impl<'v> StarlarkValue<'v> for FragmentMethod {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        let positional = args.positions(heap)?.collect::<Vec<_>>();
        let named = args.names_map()?;
        let error = |message: String| starlark::Error::new_other(anyhow!(message));
        match member(self.kind, self.name) {
            Some(Method { private, value }) => {
                if !positional.is_empty() {
                    return Err(error(format!(
                        "{}() accepts no more than 0 positional arguments but got {}",
                        self.name,
                        positional.len()
                    )));
                }
                if let Some((key, _)) = named.iter().next() {
                    return Err(error(format!(
                        "{}() got unexpected keyword argument '{}'",
                        self.name,
                        key.as_str()
                    )));
                }
                if private {
                    crate::subrule_fragments::private_api(eval)?;
                }
                value_of(value, &self.surface, eval.heap()).map_err(starlark::Error::new_other)
            }
            // ObjcConfiguration.targetShouldAlwayslink (rules/objc/ObjcConfiguration.java:170-186).
            Some(Member::AlwaysLink) => {
                let ctx = match (positional.as_slice(), named.len()) {
                    ([ctx], 0) => *ctx,
                    ([], 1) => *named.get("ctx").ok_or_else(|| {
                        error(
                            "target_should_alwayslink() got unexpected keyword argument".to_owned(),
                        )
                    })?,
                    _ => {
                        return Err(error(
                            "target_should_alwayslink() missing 1 required positional argument: ctx"
                                .to_owned(),
                        ));
                    }
                };
                crate::subrule_fragments::private_api(eval)?;
                let rule = crate::analysis_ctx::rule_surface(ctx).ok_or_else(|| {
                    error(format!(
                        "in call to target_should_alwayslink(), parameter 'ctx' got value of type '{}', want 'ctx'",
                        ctx.get_type()
                    ))
                })?;
                if rule.has_alwayslink_attribute {
                    return Err(error(
                        "objc.target_should_alwayslink of a rule with an 'alwayslink' attribute is not implemented by the native profile (explicitly specified attribute values)"
                            .to_owned(),
                    ));
                }
                Ok(Value::new_bool(false))
            }
            _ => Err(error(unknown_member(self.kind, self.name))),
        }
    }
}

impl fmt::Display for ApplePlatform {
    /// ApplePlatform.repr: the lower-case enum name (ApplePlatform.java:198-201).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

#[starlark_value(type = "apple_platform")]
impl<'v> StarlarkValue<'v> for ApplePlatform {
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(ApplePlatform::from_value(other).is_some_and(|other| other.name == self.name))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        std::hash::Hash::hash(self.name, hasher);
        Ok(())
    }

    /// starlarkbuildapi/apple/ApplePlatformApi.java:45-90.
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Some(match name {
            "is_device" => Value::new_bool(self.is_device),
            "name" => heap.alloc(self.name),
            "name_in_plist" => heap.alloc(self.name_in_plist),
            "platform_type" => heap.alloc(self.platform_type),
            _ => return None,
        })
    }

    fn dir_attr(&self) -> Vec<String> {
        ["is_device", "name", "name_in_plist", "platform_type"]
            .map(str::to_owned)
            .to_vec()
    }

    fn unknown_attribute_message(&self, name: &str) -> Option<String> {
        Some(no_field_message("apple_platform", name, &self.dir_attr()))
    }
}

// ---------------------------------------------------------------------------
// ctx.var and Make variables
// ---------------------------------------------------------------------------

const CPU_CONSTRAINT_SETTING: (&str, &str, &str) = ("platforms", "cpu", "cpu");

/// ConstraintCollection.get of `@platforms//cpu:cpu` (the platform and its
/// parents, nearest first); the setting has no default value.
fn cpu_value(
    collection: &crate::platforms::ConstraintCollection,
) -> Option<crate::platforms::ConstraintValueInfo> {
    let mut current = Some(collection);
    while let Some(constraints) = current {
        if let Some(value) = constraints.values.iter().find(|value| {
            let label = &value.setting.label;
            (
                label.repository.as_str(),
                label.package.as_str(),
                label.name.as_str(),
            ) == CPU_CONSTRAINT_SETTING
        }) {
            return Some(value.clone());
        }
        current = constraints.parent.as_deref();
    }
    None
}

/// The global make environment (BuildConfigurationValue.java:322-333):
/// `TARGET_CPU` is the target platform's `@platforms//cpu:cpu` value name
/// (`--incompatible_target_cpu_from_platform`, default true,
/// CoreOptions.java:232-241; "" without one or without PlatformOptions).
fn global_make_environment(surface: &CtxSurface) -> anyhow::Result<Vec<(String, String)>> {
    let cpu = if surface.no_config {
        String::new()
    } else {
        match &surface.target_platform {
            TargetPlatform::Deferred => bail!("{}", crate::platforms::NEEDS_TARGET_PLATFORM),
            TargetPlatform::Absent => String::new(),
            TargetPlatform::Resolved(platform) => cpu_value(&platform.constraints)
                .map(|value| value.label.name)
                .unwrap_or_default(),
        }
    };
    Ok(vec![
        ("TARGET_CPU".to_owned(), cpu),
        (
            "COMPILATION_MODE".to_owned(),
            surface.compilation_mode.to_owned(),
        ),
        ("BINDIR".to_owned(), surface.bin_dir.clone()),
        ("GENDIR".to_owned(), surface.bin_dir.clone()),
    ])
}

/// TemplateVariableInfoBackedMakeVariableSupplier.getAllMakeVariables
/// (analysis/MakeVariableSupplier.java:63-70): a TreeMap, later providers win.
fn template_variables(surface: &CtxSurface) -> Vec<(String, String)> {
    let mut map = std::collections::BTreeMap::new();
    for provider in &surface.template_variables {
        for (key, value) in provider {
            map.insert(key.clone(), value.clone());
        }
    }
    map.into_iter().collect()
}

/// ConfigurationMakeVariableContext.collectMakeVariables
/// (analysis/ConfigurationMakeVariableContext.java:43-83): the suppliers in
/// reverse priority order (global, rule template variables, package `vardef`
/// (none in Bazel 9), command line); Dict.Builder keeps the last value at
/// the first position (net/starlark/java/eval/Dict.java:505-575).
pub(crate) fn make_variables(surface: &CtxSurface) -> anyhow::Result<Vec<(String, String)>> {
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut put = |key: &str, value: &str| {
        if let Some(entry) = entries.iter_mut().find(|(k, _)| k == key) {
            entry.1 = value.to_owned();
        } else {
            entries.push((key.to_owned(), value.to_owned()));
        }
    };
    for (key, value) in global_make_environment(surface)?
        .iter()
        .chain(&template_variables(surface))
        .chain(&surface.defines)
    {
        put(key, value);
    }
    Ok(entries)
}

/// ConfigurationMakeVariableContext.lookupVariable (:64-73) in priority order,
/// after `additional_substitutions` (StarlarkRuleContext.java:958-975).
pub(crate) fn lookup_variable(
    surface: &CtxSurface,
    additional: &[(String, String)],
    name: &str,
) -> anyhow::Result<Result<String, String>> {
    if let Some((_, value)) = additional.iter().find(|(key, _)| key == name) {
        return Ok(Ok(value.clone()));
    }
    if let Some((_, value)) = surface.defines.iter().find(|(key, _)| key == name) {
        return Ok(Ok(value.clone()));
    }
    for provider in &surface.template_variables {
        if let Some((_, value)) = provider.iter().find(|(key, _)| key == name) {
            return Ok(Ok(value.clone()));
        }
    }
    if let Some((_, value)) = global_make_environment(surface)?
        .into_iter()
        .find(|(key, _)| key == name)
    {
        return Ok(Ok(value));
    }
    Ok(Err(format!("$({name}) not defined")))
}

/// TemplateExpander.expand (analysis/stringtemplate/TemplateExpander.java:55-177).
/// The outer error is a native failure (the deferred target platform); the
/// inner one an ExpansionException message.
pub(crate) fn expand_template(
    expression: &str,
    lookup: &dyn Fn(&str) -> anyhow::Result<Result<String, String>>,
    depth: usize,
) -> anyhow::Result<Result<String, String>> {
    expand_template_with(expression, lookup, &undefined_function, depth)
}

/// ConfigurationMakeVariableContext.lookupFunction (:85-88): no functions.
fn undefined_function(name: &str, _param: &str) -> anyhow::Result<Result<String, String>> {
    Ok(Err(format!("$({name}) not defined")))
}

/// A variable or function lookup: the outer error is a native failure,
/// the inner one an ExpansionException message.
pub(crate) type TemplateLookup<'a> = &'a dyn Fn(&str) -> anyhow::Result<Result<String, String>>;
pub(crate) type TemplateFunction<'a> =
    &'a dyn Fn(&str, &str) -> anyhow::Result<Result<String, String>>;

/// TemplateExpander.expand with a TemplateContext's lookupVariable
/// (`lookup`) and lookupFunction (`function`, called with the name and the
/// trimmed parameter, :88-97).
pub(crate) fn expand_template_with(
    expression: &str,
    lookup: TemplateLookup<'_>,
    function: TemplateFunction<'_>,
    depth: usize,
) -> anyhow::Result<Result<String, String>> {
    // Only the public entry short-cuts a `$`-free expression (:55-61).
    if depth == 0 && !expression.contains('$') {
        return Ok(Ok(expression.to_owned()));
    }
    if depth > 10 {
        return Ok(Err(format!(
            "potentially unbounded recursion during expansion of '{expression}'"
        )));
    }
    let chars = expression.chars().collect::<Vec<_>>();
    let mut result = String::new();
    let mut offset = 0;
    while offset < chars.len() {
        let c = chars[offset];
        if c != '$' {
            result.push(c);
            offset += 1;
            continue;
        }
        offset += 1;
        if offset >= chars.len() {
            return Ok(Err("unterminated $".to_owned()));
        }
        if chars[offset] == '$' {
            result.push('$');
            offset += 1;
            continue;
        }
        // scanVariable (:120-177).
        let variable = match chars[offset] {
            '(' => {
                let start = offset + 1;
                let Some(end) = chars[start..].iter().position(|c| *c == ')') else {
                    return Ok(Err("unterminated variable reference".to_owned()));
                };
                offset = start + end;
                chars[start..start + end].iter().collect::<String>()
            }
            '{' => {
                let start = offset + 1;
                let Some(end) = chars[start..].iter().position(|c| *c == '}') else {
                    return Ok(Err("unterminated variable reference".to_owned()));
                };
                let text = chars[start..start + end].iter().collect::<String>();
                return Ok(Err(format!(
                    "'${{{text}}}' syntax is not supported; use '$({text})' instead for \"Make\" variables, or escape the '$' as '$$' if you intended this for the shell"
                )));
            }
            c @ ('@' | '<' | '^') => c.to_string(),
            _ => {
                let start = offset;
                while offset + 1 < chars.len() && java_identifier_part(chars[offset + 1]) {
                    offset += 1;
                }
                let text = chars[start..=offset].iter().collect::<String>();
                return Ok(Err(format!(
                    "'${text}' syntax is not supported; use '$({text})' instead for \"Make\" variables, or escape the '$' as '$$' if you intended this for the shell"
                )));
            }
        };
        match variable.split_once(' ') {
            None => {
                let value = match lookup(&variable)? {
                    Ok(value) => value,
                    Err(error) => return Ok(Err(error)),
                };
                if value != variable {
                    // Make's `:=` semantics: expand the value recursively.
                    match expand_template_with(&value, lookup, function, depth + 1)? {
                        Ok(expanded) => result.push_str(&expanded),
                        Err(error) => return Ok(Err(error)),
                    }
                } else {
                    result.push_str(&value);
                }
            }
            Some((name, param)) => match function(name, param.trim())? {
                Ok(value) => result.push_str(&value),
                Err(error) => return Ok(Err(error)),
            },
        }
        offset += 1;
    }
    Ok(Ok(result))
}

/// Character.isJavaIdentifierPart for the characters a Make variable reference
/// can contain.
fn java_identifier_part(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// StarlarkRuleContext.expandMakeVariables (StarlarkRuleContext.java:950-978)
/// with Expander.expand (analysis/Expander.java:159-172): an error is reported
/// as an attribute error and the command is returned unchanged.
pub(crate) fn expand_make_variables(
    rule: &RuleSurface,
    attribute: &str,
    command: &str,
    additional: &[(String, String)],
) -> anyhow::Result<String> {
    let lookup = |name: &str| lookup_variable(&rule.config, additional, name);
    match expand_template(command, &lookup, 0)? {
        Ok(expanded) => Ok(expanded),
        Err(message) => {
            rule.attribute_error(attribute, &message);
            Ok(command.to_owned())
        }
    }
}

/// StarlarkRuleContext.expandLocation (StarlarkRuleContext.java:1029-1053)
/// with LocationExpander.withExecPaths (analysis/LocationExpander.java:130-160):
/// the location map is `targets` (makeLabelMap, :1284-1313) plus the rule's
/// prerequisites and outputs (RuleSurface::locations); `short_paths` uses
/// runfiles paths for `$(location)`. A failure is a rule error
/// (RuleErrorReporter) and the input is returned unchanged.
pub(crate) fn expand_location(
    rule: &RuleSurface,
    value: &str,
    targets: &[(String, Vec<crate::starlark_types::File>)],
    short_paths: bool,
) -> anyhow::Result<String> {
    let parse = |arg: &str| {
        Label::parse_in(
            arg,
            &rule.label.repository,
            &rule.label.package,
            &rule.mapping,
        )
    };
    let resolve = |name: &str, arg: &str| {
        crate::location_expander::apply_with(
            name,
            arg,
            !short_paths,
            &parse,
            &rule.locations,
            targets,
        )
        .unwrap_or_else(|| Err(format!("$({name}) not defined")))
    };
    match crate::location_expander::expand(value, &resolve) {
        Ok(expanded) => Ok(expanded),
        Err(message) => {
            rule.rule_error(&message);
            Ok(value.to_owned())
        }
    }
}

/// RunfilesSupport.computeArgs (analysis/RunfilesSupport.java:602-610):
/// `Expander.withDataLocations().tokenized(attribute)` (analysis/Expander.java).
/// Each value goes through TemplateExpander over a LocationTemplateContext
/// (analysis/LocationTemplateContext.java) with runfiles paths for
/// `$(location)` (execPaths false) and the `data` prerequisites (`data`,
/// added to the rule's location map; allowData), delegating variables to
/// the configuration's Make variables (ConfigurationMakeVariableContext),
/// then ShellUtils.tokenize. Every failure is an attribute error; a value
/// that fails to expand is tokenized unchanged (Expander.expand returns the
/// expression), and a value that fails to tokenize adds the tokens before
/// the failure (ShellUtils.tokenize appends while scanning).
pub(crate) fn expand_args(
    rule: &RuleSurface,
    attribute: &str,
    values: &[String],
    data: &[(String, Vec<crate::starlark_types::File>)],
) -> anyhow::Result<Vec<String>> {
    expand_options(rule, attribute, values, data, false, true)
}

/// RuleContext's Expander list/tokenized operations. JavaStarlarkCommon uses
/// withDataLocations by default, or withExecLocations (without data) when
/// exec_paths is true. Callers supply the matching additional location map.
pub(crate) fn expand_options(
    rule: &RuleSurface,
    attribute: &str,
    values: &[String],
    data: &[(String, Vec<crate::starlark_types::File>)],
    exec_paths: bool,
    tokenize: bool,
) -> anyhow::Result<Vec<String>> {
    let parse = |arg: &str| {
        Label::parse_in(
            arg,
            &rule.label.repository,
            &rule.label.package,
            &rule.mapping,
        )
    };
    let lookup = |name: &str| lookup_variable(&rule.config, &[], name);
    let function = |name: &str, param: &str| -> anyhow::Result<Result<String, String>> {
        Ok(crate::location_expander::apply_with(
            name,
            param,
            exec_paths,
            &parse,
            &rule.locations,
            data,
        )
        .unwrap_or_else(|| Err(format!("$({name}) not defined"))))
    };
    let mut tokens = Vec::new();
    for value in values {
        let expanded = match expand_template_with(value, &lookup, &function, 0)? {
            Ok(expanded) => expanded,
            Err(message) => {
                rule.attribute_error(attribute, &message);
                value.clone()
            }
        };
        if tokenize {
            if let Err(message) = tokenize_into(&mut tokens, &expanded) {
                rule.attribute_error(attribute, &message);
            }
        } else {
            tokens.push(expanded);
        }
    }
    Ok(tokens)
}

/// `Expander.withDataLocations().expand(attribute, expression)`
/// (analysis/Expander.java:159-172) over RuleContext.getExpander(labelMap)
/// (PyBuiltins.expandLocationAndMakeVariables): `$(location)` is a runfiles
/// path, the location map adds `targets` and the `data` prerequisites to
/// the rule's, and other variables are the configuration's Make variables.
/// A failure is an attribute error and the expression is returned
/// unchanged.
pub(crate) fn expand_data_locations_and_make_variables(
    rule: &RuleSurface,
    attribute: &str,
    expression: &str,
    targets: &[(String, Vec<crate::starlark_types::File>)],
) -> anyhow::Result<String> {
    let parse = |arg: &str| {
        Label::parse_in(
            arg,
            &rule.label.repository,
            &rule.label.package,
            &rule.mapping,
        )
    };
    let lookup = |name: &str| lookup_variable(&rule.config, &[], name);
    let data: Vec<(String, Vec<crate::starlark_types::File>)> = targets
        .iter()
        .chain(&rule.data_locations)
        .cloned()
        .collect();
    let function =
        |name: &str, param: &str| -> anyhow::Result<Result<String, String>> {
            Ok(crate::location_expander::apply_with(
                name,
                param,
                false,
                &parse,
                &rule.locations,
                &data,
            )
            .unwrap_or_else(|| Err(format!("$({name}) not defined"))))
        };
    Ok(
        match expand_template_with(expression, &lookup, &function, 0)? {
            Ok(expanded) => expanded,
            Err(message) => {
                rule.attribute_error(attribute, &message);
                expression.to_owned()
            }
        },
    )
}

/// ShellUtils.tokenize (util/ShellUtils.java:90-143).
pub(crate) fn tokenize(input: &str) -> Result<Vec<String>, String> {
    let mut options = Vec::new();
    tokenize_into(&mut options, input)?;
    Ok(options)
}

/// ShellUtils.tokenize(List, String): the tokens are appended to `options`
/// as they are scanned, so a failure keeps the ones before it.
fn tokenize_into(options: &mut Vec<String>, input: &str) -> Result<(), String> {
    let chars = input.chars().collect::<Vec<_>>();
    let mut token = String::new();
    let mut force = false;
    let mut quotation = None;
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        match quotation {
            Some(quote) => {
                if c == quote {
                    quotation = None;
                } else if c == '\\' && quote == '"' {
                    index += 1;
                    let Some(&next) = chars.get(index) else {
                        return Err("backslash at end of string".to_owned());
                    };
                    if next != '\\' && next != '"' {
                        token.push('\\');
                    }
                    token.push(next);
                } else {
                    token.push(c);
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quotation = Some(c);
                    force = true;
                } else if c == ' ' || c == '\t' {
                    if force || !token.is_empty() {
                        options.push(std::mem::take(&mut token));
                        force = false;
                    }
                } else if c == '\\' {
                    index += 1;
                    let Some(&next) = chars.get(index) else {
                        return Err("backslash at end of string".to_owned());
                    };
                    token.push(next);
                } else {
                    token.push(c);
                }
            }
        }
        index += 1;
    }
    if quotation.is_some() {
        return Err("unterminated quotation".to_owned());
    }
    if force || !token.is_empty() {
        options.push(token);
    }
    Ok(())
}

/// ImplicitOutputsFunction.createPlaceholderSubstitutionFormatString
/// (packages/ImplicitOutputsFunction.java:416-438): the `%{name}` placeholders.
pub(crate) fn placeholders(template: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("%{") {
        let Some(end) = rest[start + 2..].find('}').map(|index| index + start + 2) else {
            break;
        };
        found.push(rest[start + 2..end].to_owned());
        rest = &rest[end + 1..];
    }
    found
}

/// Immutable Starlark helpers for crate::analysis_ctx.
pub(crate) fn frozen_string_list<'v>(heap: Heap<'v>, items: &[String]) -> Value<'v> {
    frozen_owned_strings(heap, items)
}

/// A fresh mutable Starlark list of strings, one per access (the Starlark
/// conversion of a Java method returning `List`,
/// net.starlark.java.eval.Starlark#fromJava).
pub(crate) fn mutable_string_list<'v>(heap: Heap<'v>, items: &[String]) -> Value<'v> {
    heap.alloc(AllocList(items.iter().map(String::as_str)))
}

pub(crate) fn frozen_string_dict<'v>(heap: Heap<'v>, entries: &[(String, String)]) -> Value<'v> {
    frozen_dict(heap, entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The private `ctx.fragments.proto` getters read the configured
    /// table-driven options (StrictDepsMode.toString), the defaults when
    /// unset.
    #[test]
    fn proto_fragment_getters_read_the_configured_options() {
        use crate::configuration::OptionValue;
        let mut surface = CtxSurface::default();
        let strings = |surface: &CtxSurface| {
            Heap::temp(|heap| {
                (
                    strict_proto_deps(surface, heap).unwrap().to_str(),
                    strict_public_imports(surface, heap).unwrap().to_str(),
                    proto_descriptor_sets_include_source_info(surface, heap)
                        .unwrap()
                        .to_str(),
                )
            })
        };
        assert_eq!(
            strings(&surface),
            ("ERROR".to_owned(), "OFF".to_owned(), "False".to_owned())
        );
        surface.option_values = std::collections::BTreeMap::from([
            ("strict_proto_deps", OptionValue::Enum("OFF")),
            ("strict_public_imports", OptionValue::Enum("STRICT")),
            (
                "experimental_proto_descriptor_sets_include_source_info",
                OptionValue::Bool(true),
            ),
        ]);
        assert_eq!(
            strings(&surface),
            ("OFF".to_owned(), "STRICT".to_owned(), "True".to_owned())
        );
    }

    #[test]
    fn tokenize_follows_shell_utils() {
        assert_eq!(
            tokenize("a 'b c' d\\ e \"f g\"").unwrap(),
            ["a", "b c", "d e", "f g"]
        );
        assert_eq!(tokenize("''").unwrap(), [""]);
        assert_eq!(tokenize("\"a\\x\\\"\"").unwrap(), ["a\\x\""]);
        assert_eq!(tokenize("'x").unwrap_err(), "unterminated quotation");
        assert_eq!(tokenize("x\\").unwrap_err(), "backslash at end of string");
    }

    #[test]
    fn placeholders_and_feature_sets() {
        assert_eq!(placeholders("%{a}x%{b}%{"), ["a", "b"]);
        let parsed = FeatureSet::parse(["b", "-a", "a", "no_layering_check"]);
        assert_eq!(parsed.on, ["b"]);
        assert_eq!(parsed.off, ["a", "layering_check"]);
        let merged = FeatureSet::merge(&FeatureSet::parse(["-x", "y"]), &FeatureSet::parse(["x"]));
        assert_eq!(merged.on, ["x", "y"]);
        let global = FeatureSet::merge_with_global(
            &FeatureSet::parse(["-g", "r"]),
            &FeatureSet::parse(["g", "-r"]),
        );
        assert_eq!(global.on, Vec::<String>::new());
        assert_eq!(global.off, ["g", "r"]);
    }

    fn surface(defines: &[(&str, &str)]) -> CtxSurface {
        CtxSurface {
            defines: defines
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            template_variables: vec![vec![("CC".into(), "gcc".into())]],
            ..Default::default()
        }
    }

    #[test]
    fn make_variables_order_priority_and_expansion() {
        let config = surface(&[("A", "1"), ("COMPILATION_MODE", "x"), ("R", "$(A)$(A)")]);
        assert_eq!(
            make_variables(&config).unwrap(),
            [
                ("TARGET_CPU", ""),
                ("COMPILATION_MODE", "x"),
                ("BINDIR", "bazel-out/k8-fastbuild/bin"),
                ("GENDIR", "bazel-out/k8-fastbuild/bin"),
                ("CC", "gcc"),
                ("A", "1"),
                ("R", "$(A)$(A)"),
            ]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
        );
        let rule = RuleSurface {
            config: Arc::new(config),
            rule_class: "r".into(),
            label: Label::parse_canonical("//p:t").unwrap(),
            ..Default::default()
        };
        let extra = [("F".to_owned(), "bar".to_owned())];
        assert_eq!(
            expand_make_variables(&rule, "cmd", "$(F) $$H $(R) $(CC) x$$", &extra).unwrap(),
            "bar $H 11 gcc x$"
        );
        assert!(rule.take_errors().is_empty());
        for (command, message) in [
            ("$(NOPE) a", "$(NOPE) not defined"),
            ("$(F", "unterminated variable reference"),
            ("a$", "unterminated $"),
            (
                "$Fx y",
                "'$Fx' syntax is not supported; use '$(Fx)' instead for \"Make\" variables, or escape the '$' as '$$' if you intended this for the shell",
            ),
            ("$(location :x)", "$(location) not defined"),
        ] {
            assert_eq!(
                expand_make_variables(&rule, "cmd", command, &extra).unwrap(),
                command
            );
            assert_eq!(
                rule.take_errors(),
                [format!("in cmd attribute of r rule //p:t: {message}")]
            );
        }
        // The deferred target platform is resolved on demand (crate::engine).
        let deferred = CtxSurface {
            target_platform: TargetPlatform::Deferred,
            ..Default::default()
        };
        assert!(
            make_variables(&deferred)
                .unwrap_err()
                .to_string()
                .contains(crate::platforms::NEEDS_TARGET_PLATFORM)
        );
    }

    #[test]
    fn expand_location_uses_the_location_map_and_reports_rule_errors() {
        let file = |path: &str| {
            crate::starlark_types::File::new(crate::starlark_types::FileData {
                path: path.to_owned(),
                short_path: path
                    .trim_start_matches("bazel-out/k8-fastbuild/bin/")
                    .to_owned(),
                owner: String::new(),
                is_source: false,
                is_directory: false,
                is_symlink: false,
            })
        };
        let mut locations = crate::location_expander::LocationMap::default();
        locations.add("//p:src", [file("p/src")]);
        let rule = RuleSurface {
            rule_class: "r".into(),
            label: Label::parse_canonical("//p:t").unwrap(),
            locations,
            ..Default::default()
        };
        assert_eq!(
            expand_location(&rule, "plain $$ $(FOO) $(location)", &[], false).unwrap(),
            "plain $$ $(FOO) $(location)"
        );
        let targets = [(
            "//p:tool".to_owned(),
            vec![file("bazel-out/k8-fastbuild/bin/p/tool")],
        )];
        assert_eq!(
            expand_location(&rule, "$(location :src) $(execpath :tool)", &targets, false).unwrap(),
            "p/src bazel-out/k8-fastbuild/bin/p/tool"
        );
        assert_eq!(
            expand_location(&rule, "$(location :tool)", &targets, true).unwrap(),
            "p/tool"
        );
        assert!(rule.take_errors().is_empty());
        assert_eq!(
            expand_location(&rule, "$(location :x", &[], false).unwrap(),
            "$(location :x"
        );
        assert_eq!(
            expand_location(&rule, "$(rootpath :x)", &[], false).unwrap(),
            "$(rootpath :x)"
        );
        assert_eq!(
            rule.take_errors(),
            [
                "in r rule //p:t: unterminated $(location) expression",
                "in r rule //p:t: label '//p:x' in $(location) expression is not a declared prerequisite of this rule"
            ]
        );
    }
}
