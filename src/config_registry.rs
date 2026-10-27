//! The configurations of one build and the keys of configured targets
//! (N18a: explicit configuration plumbing, docs/CONFIGURATION.md
//! "Configured-target keys and output roots").
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
//! Every phase-2 (analysis and action planning) value is keyed by a
//! [`ConfiguredTargetKey`] (skyframe/ConfiguredTargetKey.java): the label and
//! the identity of the configuration the target is analyzed in. One build
//! has a top-level configuration; dependency edges keep their parent's
//! configuration except across the exec transition ([`exec_transition`],
//! N18b) and Starlark 1:1 transitions (crate::starlark_transitions, N18c;
//! [`Configurations::intern_derived`]); split transitions are N18d.
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::configuration::{ConfigId, Configuration, NativeOptions, SettingValue};
use crate::label::Label;

/// A target in a configuration (ConfiguredTargetKey).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConfiguredTargetKey {
    pub label: Label,
    pub config: ConfigId,
    /// ConfiguredTargetKey.shouldApplyRuleTransition
    /// (skyframe/ConfiguredTargetKey.java; analysis/producers/RuleTransitionApplier.java:69-110):
    /// false only for the delegate of a non-idempotent rule transition,
    /// which is analyzed in the transitioned configuration without applying
    /// the transition again. Every dependency edge requests `true`.
    pub apply_rule_transition: bool,
}

impl ConfiguredTargetKey {
    pub fn new(label: Label, config: ConfigId) -> Self {
        Self {
            label,
            config,
            apply_rule_transition: true,
        }
    }

    /// The delegate key of a non-idempotent rule transition
    /// (`shouldApplyRuleTransition = false`).
    pub fn without_rule_transition(label: Label, config: ConfigId) -> Self {
        Self {
            label,
            config,
            apply_rule_transition: false,
        }
    }

    /// The owner part of the action identifiers of this target: the label in
    /// the top-level configuration (`L`, so action ids `L#n` and the action
    /// cache keys of existing builds are unchanged), `L[<config8>]`
    /// elsewhere (the first eight hex digits of the ConfigId).
    pub fn action_owner(&self, top: ConfigId) -> String {
        if self.config == top {
            self.label.to_string()
        } else {
            format!("{}[{}]", self.label, &self.config.to_string()[..8])
        }
    }
}

impl fmt::Display for ConfiguredTargetKey {
    /// `LABEL (CONFIG)`, as cycle reports print configured target keys
    /// (skyframe/AbstractLabelCycleReporter.java:134-170; CONFIG is the full
    /// native ConfigId, docs/CONFIGURATION.md).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({})",
            crate::rule_declarations::canonical_form(&self.label),
            self.config
        )
    }
}

/// The configurations of one build, interned by identity.
#[derive(Debug)]
pub struct Configurations {
    by_id: BTreeMap<ConfigId, Arc<Configuration>>,
    /// The top-level target configuration: requested targets are analyzed
    /// in it, and it is the baseline of every output path mnemonic
    /// (`diff_against_dynamic_baseline`, analysis/BuildView.java:280).
    top: ConfigId,
    /// The baseline of exec configurations' mnemonics: `top` with the exec
    /// transition applied for its `--host_platform` (SkyframeExecutor.adjustForExec,
    /// skyframe/SkyframeExecutor.java:1525-1572; BaselineOptionsFunction,
    /// skyframe/config/BaselineOptionsFunction.java:47-95).
    exec_baseline: OnceCell<Arc<Configuration>>,
    /// The output path mnemonic of each configuration.
    mnemonics: BTreeMap<ConfigId, String>,
}

impl Default for Configurations {
    fn default() -> Self {
        Self::new(Configuration::default())
    }
}

impl Configurations {
    /// The configurations of a build whose top-level configuration is `top`.
    pub fn new(top: Configuration) -> Self {
        let id = top.id;
        let mut configurations = Self {
            by_id: BTreeMap::new(),
            top: id,
            exec_baseline: OnceCell::new(),
            mnemonics: BTreeMap::new(),
        };
        // The top-level configuration is never an exec configuration
        // (`is exec configuration` is internal), so its CPU descriptor is
        // its `--cpu`, which the caller validated ([`validate_top`]).
        let top_mnemonic =
            mnemonic(&top, &top).expect("internal error: top-level output path mnemonic");
        configurations.mnemonics.insert(id, top_mnemonic);
        configurations.by_id.insert(id, Arc::new(top));
        configurations
    }

    /// The top-level configuration's identity.
    pub fn top(&self) -> ConfigId {
        self.top
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (ConfigId, &Configuration)> {
        self.by_id
            .iter()
            .map(|(id, configuration)| (*id, configuration.as_ref()))
    }

    /// The top-level configuration.
    pub fn top_configuration(&self) -> &Arc<Configuration> {
        &self.by_id[&self.top]
    }

    /// An interned configuration. Every [`ConfigId`] handed out by this
    /// registry is interned, so an unknown identity is an internal error.
    pub fn get(&self, id: ConfigId) -> &Arc<Configuration> {
        self.by_id
            .get(&id)
            .unwrap_or_else(|| panic!("internal error: configuration {id} is not interned"))
    }

    /// Intern `configuration`, a configuration derived from the top-level
    /// one, with the output path mnemonic computed against `baseline`, and
    /// return its identity; an invalid mnemonic is Bazel's
    /// InvalidMnemonicException text.
    pub fn intern(
        &mut self,
        configuration: Configuration,
        baseline: &Configuration,
    ) -> Result<ConfigId, String> {
        let id = configuration.id;
        if !self.by_id.contains_key(&id) {
            let mnemonic = mnemonic(&configuration, baseline)?;
            self.mnemonics.insert(id, mnemonic);
            self.by_id.insert(id, Arc::new(configuration));
        }
        Ok(id)
    }

    /// Intern a configuration produced by a Starlark transition. Its output
    /// path mnemonic uses the baseline Bazel picks for it
    /// (BuildConfigurationFunction.getBaselineOptions,
    /// skyframe/config/BuildConfigurationFunction.java:133-174): the exec
    /// baseline with its own platform for an exec configuration (a
    /// transition below an exec edge keeps `is exec configuration`), the
    /// top-level configuration otherwise (`diff_against_dynamic_baseline`).
    pub fn intern_derived(
        &mut self,
        configuration: Configuration,
        target_scoped: &BTreeSet<Label>,
    ) -> Result<ConfigId, String> {
        if let Some(existing) = self.by_id.get(&configuration.id) {
            return Ok(existing.id);
        }
        if configuration.native.is_exec {
            let mut baseline = self.exec_baseline(target_scoped).as_ref().clone();
            if let [platform] = configuration.native.platforms.as_slice() {
                let mut native = baseline.native.clone();
                native.platforms = vec![platform.clone()];
                baseline = Configuration::new(native, baseline.starlark.clone());
            }
            return self.intern(configuration, &baseline);
        }
        let top = self.top_configuration().as_ref().clone();
        self.intern(configuration, &top)
    }

    /// The mnemonic baseline of exec configurations before its platform is
    /// replaced (BuildConfigurationFunction.getBaselineOptions,
    /// skyframe/config/BuildConfigurationFunction.java:133-174, and
    /// BaselineOptionsFunction, skyframe/config/BaselineOptionsFunction.java:47-95):
    /// under the default `diff_against_dynamic_baseline` scheme an exec
    /// configuration uses the exec-transitioned top-level options
    /// (SkyframeExecutor.adjustForExec, skyframe/SkyframeExecutor.java:1525-1572,
    /// whose execution platform is the top-level `--host_platform` label as
    /// given). `target_scoped` must contain every top-level Starlark flag
    /// whose scope is `target` (those the exec transition resets; callers
    /// pass a set that covers the top-level configuration's own flags, so
    /// the memoized baseline does not depend on which configuration first
    /// asks for it).
    pub fn exec_baseline(&self, target_scoped: &BTreeSet<Label>) -> &Arc<Configuration> {
        self.exec_baseline.get_or_init(|| {
            let top = self.top_configuration();
            Arc::new(exec_transition(
                top,
                top.native.host_platform.clone(),
                target_scoped,
            ))
        })
    }

    /// The exec configuration of `from` for an edge whose parent's
    /// execution platform is `execution_platform`, interned with the exec
    /// baseline.
    pub fn intern_exec(
        &mut self,
        from: ConfigId,
        execution_platform: Label,
        target_scoped: &BTreeSet<Label>,
    ) -> Result<ConfigId, String> {
        let exec = exec_transition(self.get(from), execution_platform, target_scoped);
        if let Some(existing) = self.by_id.get(&exec.id) {
            return Ok(existing.id);
        }
        // An exec configuration uses its platform in the output directory
        // (CoreOptions.usePlatformInOutputDir, analysis/config/CoreOptions.java:1146-1149:
        // `--experimental_platform_in_output_dir` defaults to AUTO), so its
        // baseline gets the configuration's own target platform
        // (BuildConfigurationFunction.java:151-160, BaselineOptionsFunction.java:72-77).
        let mut baseline = self.exec_baseline(target_scoped).as_ref().clone();
        if let [platform] = exec.native.platforms.as_slice() {
            let mut native = baseline.native.clone();
            native.platforms = vec![platform.clone()];
            baseline = Configuration::new(native, baseline.starlark.clone());
        }
        self.intern(exec, &baseline)
    }

    /// The no-config configuration (NoConfigTransition, N28c): CoreOptions
    /// defaults only and no Starlark build settings
    /// (CommonOptions.EMPTY_OPTIONS, analysis/config/CommonOptions.java:32-42),
    /// whatever the parent configuration; interned on first use.
    pub fn intern_no_config(&mut self) -> ConfigId {
        let configuration = no_config();
        let id = configuration.id;
        self.mnemonics
            .entry(id)
            .or_insert_with(|| format!("{}-noconfig", configuration.native.compilation_mode));
        self.by_id
            .entry(id)
            .or_insert_with(|| Arc::new(configuration));
        id
    }

    /// Whether `id` is the no-config configuration.
    pub fn is_no_config(&self, id: ConfigId) -> bool {
        self.get(id).native.no_config
    }

    /// The output path mnemonic of an interned configuration.
    pub fn mnemonic(&self, id: ConfigId) -> &str {
        self.mnemonics
            .get(&id)
            .unwrap_or_else(|| panic!("internal error: configuration {id} is not interned"))
    }

    /// The generated-file root of an interned configuration,
    /// `bazel-out/<mnemonic>/bin` (BuildConfigurationValue.getBinDirectory).
    pub fn output_root(&self, id: ConfigId) -> String {
        output_root(self.mnemonic(id))
    }

    /// The configured target key of `label` in the top-level configuration.
    pub fn top_key(&self, label: Label) -> ConfiguredTargetKey {
        ConfiguredTargetKey::new(label, self.top)
    }
}

/// Whether the top-level configuration has a valid output path mnemonic: a
/// `--cpu` containing `/` is Bazel's InvalidMnemonicException
/// (`CPU/Platform descriptor 'a/b' is invalid as part of a path: must not
/// contain /`; paired capture pre_platforms_flags/cpu_flag_slash). Checked
/// before [`Configurations::new`].
pub fn validate_top(top: &Configuration) -> Result<(), String> {
    mnemonic(top, top).map(|_| ())
}

/// `bazel-out/<mnemonic>/bin`.
pub fn output_root(mnemonic: &str) -> String {
    format!("bazel-out/{mnemonic}/bin")
}

/// The native profile's CPU descriptor: Bazel's `--cpu` on this Linux x86-64
/// host, `k8` (CoreOptions.java:279-282 `cpu` default "" converted by
/// AutoCpuConverter.java:52). Divergence D1 (the historical `rust`) is closed
/// (N41, docs/REMOTE_CACHE.md): output paths are part of every remote cache
/// key, so they must equal Bazel's for a shared cache.
pub const NATIVE_CPU: &str = "k8";

/// PlatformOptions.DEFAULT_PLATFORM_NAMES (analysis/PlatformOptions.java:43-50).
const DEFAULT_PLATFORM_NAMES: [&str; 5] = [
    "host",
    "host_platform",
    "target_platform",
    "default_host",
    "default_target",
];

/// The CPU/platform descriptor of `configuration`
/// (OutputPathMnemonicComputer.handlePlatformCpuDescriptor,
/// analysis/config/OutputPathMnemonicComputer.java:247-282) and whether
/// `platforms` is explicit in the output path. Only exec configurations use
/// their platform (CoreOptions.usePlatformInOutputDir, CoreOptions.java:1146-1149:
/// `--experimental_platform_in_output_dir=auto`): with one platform,
/// computePlatformName's legacy heuristic
/// (`--experimental_use_platforms_in_output_dir_legacy_heuristic`, default
/// true, CoreOptions.java:182-194; no `--experimental_override_platform_cpu_name`)
/// gives the platform's name unless it is a default name
/// (PlatformOptions.platformIsDefault), then the CPU. Target configurations
/// use the CPU (`cpu` explicit).
fn cpu_descriptor(configuration: &Configuration) -> (&str, bool) {
    let native = &configuration.native;
    // `--cpu` (the command line's, a Starlark transition's (N18c), or the
    // exec configuration's `--host_cpu`) is the descriptor itself, as in
    // Bazel (`arm-fastbuild`, `ppc-opt-exec`); the host CPU is `k8`.
    let cpu = native.cpu.as_deref().unwrap_or(NATIVE_CPU);
    if !native.is_exec {
        return (cpu, false);
    }
    match native.platforms.as_slice() {
        [platform] if DEFAULT_PLATFORM_NAMES.contains(&platform.name.as_str()) => (cpu, true),
        [platform] => (platform.name.as_str(), true),
        // Not marked explicit, so the ST- hash covers `platforms`.
        _ => ("multi-platform", false),
    }
}

/// OutputPathMnemonicComputer.computeMnemonic
/// (analysis/config/OutputPathMnemonicComputer.java:174-245) for the native
/// profile: `{cpu}-{compilation_mode}[-{platform_suffix}][-ST-{12 hex}]`.
/// The CPU descriptor ([`cpu_descriptor`]) is Bazel's `--cpu` (`k8`, giving
/// the default `k8-fastbuild` root), and the execution platform's name for
/// an exec configuration with a non-default platform name, as in Bazel
/// (`linux-opt-exec`). `compilation_mode`, `platform_suffix` and, for exec
/// configurations, `platforms` are explicit in the path. The `ST-` fragment
/// hashes every other option that differs from `baseline`
/// (computeNameFragmentWithDiff, :286-318), so the top-level configuration,
/// its own baseline, keeps Bazel's `k8-fastbuild`, `k8-dbg` and `k8-opt`
/// roots. The fragment follows Bazel's algorithm over the native
/// option set and values only: it can never equal Bazel's hash, which covers
/// every Bazel option (docs/CONFIGURATION.md). A descriptor with a path
/// separator is Bazel's InvalidMnemonicException
/// (`CPU/Platform descriptor 'D' is invalid as part of a path: must not
/// contain /`, :55-61, 134-144; vfs/PathFragment.java:948-957).
pub fn mnemonic(configuration: &Configuration, baseline: &Configuration) -> Result<String, String> {
    let native = &configuration.native;
    // BuildOptions.hasNoConfig: `<compilation_mode>-noconfig`
    // (OutputPathMnemonicComputer.java:187-190).
    if native.no_config {
        return Ok(format!("{}-noconfig", native.compilation_mode));
    }
    let (cpu, platforms_explicit) = cpu_descriptor(configuration);
    if cpu.contains('/') {
        return Err(format!(
            "CPU/Platform descriptor '{cpu}' is invalid as part of a path: must not contain /"
        ));
    }
    let mut mnemonic = format!("{cpu}-{}", native.compilation_mode);
    if let Some(suffix) = native.platform_suffix.filter(|suffix| !suffix.is_empty()) {
        mnemonic.push('-');
        mnemonic.push_str(suffix);
    }
    let explicit: &[&str] = if platforms_explicit {
        &["compilation_mode", "platform_suffix", "platforms"]
    } else {
        EXPLICIT_IN_OUTPUT_PATH
    };
    if let Some(fragment) = name_fragment_with_diff(configuration, baseline, explicit) {
        mnemonic.push('-');
        mnemonic.push_str(&fragment);
    }
    Ok(mnemonic)
}

/// Bazel's exec transition for the modelled options: the built-in Starlark
/// `bazel_exec_transition` (`--experimental_exec_config` default,
/// analysis/config/CoreOptions.java:97-108;
/// src/main/starlark/builtins_bzl/common/builtin_exec_platforms.bzl) over a
/// baseline of every native option at its DEFAULT
/// (FunctionTransitionUtil.maybeGetExecDefaults,
/// analysis/starlark/FunctionTransitionUtil.java:167-225), then the native
/// finalizer (ExecutionTransitionFactory.ExecTransitionFinalizer.patch,
/// analysis/config/ExecutionTransitionFactory.java:192-268):
///
/// * `compilation_mode` = `host_compilation_mode`, `is exec configuration`
///   = true, `stamp` = false, `action_env` = `host_action_env` (`_core_options`,
///   builtin_exec_platforms.bzl:221-229); `host_compilation_mode` and
///   `host_action_env` propagate (:231-235, 259), like
///   `incompatible_strict_action_env` (:203-207) and TestOptions (cloned,
///   FunctionTransitionUtil.java:199-207);
/// * `cpu` = `host_cpu`, which propagates (`_core_options`, :225, 241);
/// * `strip` = `always` (CppOptions, :291-348); `force_pic` and
///   `crosstool_top` are not propagated, so they are the defaults;
/// * `--define` is carried (`experimental_exclude_defines_from_exec_config`
///   is false, FunctionTransitionUtil.java:210-223);
/// * `java_runtime_version`/`java_language_version` become
///   `tool_java_runtime_version`/`tool_java_language_version`, which
///   propagate (`_java_options`, builtin_exec_platforms.bzl:350-385);
/// * `host_platform`, `extra_execution_platforms` and `extra_toolchains` propagate (PlatformOptions,
///   builtin_exec_platforms.bzl:411-419); `platforms` becomes the parent's
///   execution platform (ExecutionTransitionFactory.java:203-207);
/// * `platform_suffix` = `exec` (the default
///   `--experimental_exec_configuration_distinguisher=off`,
///   CoreOptions.java:488-505; ExecutionTransitionFactory.java:255-258);
/// * Starlark flags are carried unless their `scope` is `target`
///   (`--incompatible_exclude_starlark_flags_from_exec_config` is false,
///   FunctionTransitionUtil.getExecPropagatingStarlarkFlags, :231-251).
///   Feature flag values (ExecutionTransitionFactory.java:209-221) never
///   enter a native configuration: no native transition sets them
///   (crate::feature_flags).
///
/// The transition is idempotent: the exec configuration of an exec
/// configuration with the same execution platform is itself.
pub fn exec_transition(
    from: &Configuration,
    execution_platform: Label,
    target_scoped: &BTreeSet<Label>,
) -> Configuration {
    let defaults = NativeOptions::default();
    let native = NativeOptions {
        // Not propagated by builtin_exec_platforms.bzl CoreOptions. The
        // Starlark settings themselves follow their declared exec scope.
        flag_aliases: defaults.flag_aliases.clone(),
        compilation_mode: from.native.host_compilation_mode,
        stamp: false,
        define: from.native.define.clone(),
        strip: "always",
        force_pic: defaults.force_pic,
        platforms: vec![execution_platform],
        host_platform: from.native.host_platform.clone(),
        extra_toolchains: from.native.extra_toolchains.clone(),
        extra_execution_platforms: from.native.extra_execution_platforms.clone(),
        host_compilation_mode: from.native.host_compilation_mode,
        is_exec: true,
        platform_suffix: Some("exec"),
        // `_java_options` (builtin_exec_platforms.bzl:350-360): java_* from
        // tool_java_*; tool_java_* propagate (:362-385).
        java: crate::configuration::JavaVersions {
            java_runtime_version: from.native.java.tool_java_runtime_version.clone(),
            java_language_version: from.native.java.tool_java_language_version.clone(),
            tool_java_runtime_version: from.native.java.tool_java_runtime_version.clone(),
            tool_java_language_version: from.native.java.tool_java_language_version.clone(),
        },
        // JavaOptions.java:351-353: plugins are built in exec configuration;
        // propagating the flag would introduce cycles.
        plugins: Vec::new(),
        // An exec edge of a no-config target has no execution platform and
        // never reaches here (ExecutionTransitionFactory.java:192-197).
        no_config: false,
        // `cpu` = the parent's `--host_cpu` (builtin_exec_platforms.bzl:225),
        // which propagates (:241).
        cpu: from.native.host_cpu.clone(),
        host_cpu: from.native.host_cpu.clone(),
        // Not in CppOptions' propagate list (builtin_exec_platforms.bzl:291-348):
        // the default.
        crosstool_top: defaults.crosstool_top,
        // `_cxx_options` (builtin_exec_platforms.bzl:327-345): the target C++
        // flag lists are the host ones, with `-g0` appended to `--copt` and
        // `--cxxopt` off Windows (`py_internal.get_current_os_name()`);
        // `--host_*` themselves propagate.
        copts: {
            let mut copts = from.native.host_copts.clone();
            copts.push("-g0".to_owned());
            copts
        },
        cxxopts: {
            let mut cxxopts = from.native.host_cxxopts.clone();
            cxxopts.push("-g0".to_owned());
            cxxopts
        },
        conlyopts: from.native.host_conlyopts.clone(),
        linkopts: from.native.host_linkopts.clone(),
        per_file_copts: from.native.host_per_file_copts.clone(),
        host_copts: from.native.host_copts.clone(),
        host_cxxopts: from.native.host_cxxopts.clone(),
        host_conlyopts: from.native.host_conlyopts.clone(),
        host_per_file_copts: from.native.host_per_file_copts.clone(),
        host_linkopts: from.native.host_linkopts.clone(),
        // The table-driven options (crate::configuration::VALUE_OPTIONS):
        // each row's exec rule (propagated, default or set).
        values: from.native.exec_values(),
        // Not in CoreOptions' propagate list (builtin_exec_platforms.bzl:231-275):
        // the CoreOptions default (CoreOptions.java:566-578).
        collect_code_coverage: defaults.collect_code_coverage,
        instrumentation: defaults.instrumentation.clone(),
        // Not in CoreOptions' propagate list (builtin_exec_platforms.bzl:231-275).
        use_target_platform_for_tests: defaults.use_target_platform_for_tests,
        // In CoreOptions' propagate list (builtin_exec_platforms.bzl:246).
        incompatible_auto_exec_groups: from.native.incompatible_auto_exec_groups,
        // AppleCommandLineOptions propagates the SDK versions
        // (builtin_exec_platforms.bzl:170-176).
        apple_sdk_versions: from.native.apple_sdk_versions.clone(),
        action_env: from.native.host_action_env.clone(),
        host_action_env: from.native.host_action_env.clone(),
        strict_action_env: from.native.strict_action_env,
        // TestOptions are cloned (FunctionTransitionUtil.java:199-207).
        test_env: from.native.test_env.clone(),
    };
    let starlark = from
        .starlark
        .iter()
        .filter(|(label, _)| !target_scoped.contains(*label))
        .map(|(label, value)| (label.clone(), value.clone()))
        .collect();
    Configuration::new(native, starlark)
}

/// The no-config configuration (CommonOptions.EMPTY_OPTIONS,
/// analysis/config/CommonOptions.java:32-42; NoConfigTransition.patch,
/// analysis/config/transitions/NoConfigTransition.java:52-55).
pub fn no_config() -> Configuration {
    Configuration::new(
        NativeOptions {
            no_config: true,
            ..NativeOptions::default()
        },
        BTreeMap::new(),
    )
}

/// Options whose value is explicit in the mnemonic
/// (markAsExplicitInOutputPathFor, OutputPathMnemonicComputer.java:187-198;
/// `cpu` is the host `k8` unless `--cpu` or a transition set it).
const EXPLICIT_IN_OUTPUT_PATH: &[&str] = &["compilation_mode", "platform_suffix", "cpu"];

/// The native options in their `//command_line_option:NAME=VALUE` hash form
/// (hashChosenOptions, OutputPathMnemonicComputer.java:368-410): Java
/// `toString` of each value, `@null` for null values.
pub(crate) fn native_option_strings(native: &NativeOptions) -> BTreeMap<&'static str, String> {
    let mut options = BTreeMap::new();
    // The typed and the table-driven options (`collect_code_coverage` is a
    // table row read from its typed field).
    for name in crate::configuration::modelled_option_names() {
        match native.value(name) {
            Some(crate::configuration::OptionValue::DottedVersion(None)) => {
                options.insert(name, "@null".to_owned());
            }
            Some(value) => {
                options.insert(name, format!("={value}"));
            }
            None => {}
        }
    }
    let list = |items: &[String]| format!("=[{}]", items.join(", "));
    if !native.flag_aliases.is_empty() {
        options.insert(
            "flag_alias",
            list(
                &native
                    .flag_aliases
                    .iter()
                    .map(|(name, label)| format!("{name}={label}"))
                    .collect::<Vec<_>>(),
            ),
        );
    }
    options.insert("extra_toolchains", list(&native.extra_toolchains));
    options.insert(
        "extra_execution_platforms",
        list(&native.extra_execution_platforms),
    );
    // The CppOptions flag lists (N66): Java's List.toString is `[a, b]`, and
    // each PerLabelOptions is `regexFilter + " Options: " + optionsList`
    // (PerLabelOptions.java:120-123) — the strings the ST- hash feeds on
    // (hashChosenOptions, OutputPathMnemonicComputer.java:388-405).
    options.insert("copt", list(&native.copts));
    options.insert("cxxopt", list(&native.cxxopts));
    options.insert("conlyopt", list(&native.conlyopts));
    options.insert("linkopt", list(&native.linkopts));
    options.insert("host_copt", list(&native.host_copts));
    options.insert("host_cxxopt", list(&native.host_cxxopts));
    options.insert("host_conlyopt", list(&native.host_conlyopts));
    options.insert("host_linkopt", list(&native.host_linkopts));
    let per_label_options = |items: &[String]| {
        let entries = items
            .iter()
            .map(|entry| {
                let (filter, options) = entry.split_once('@').unwrap_or((entry.as_str(), ""));
                let mut pieces = Vec::new();
                let mut current = String::new();
                let mut previous = None;
                for character in options.chars() {
                    if character == ',' && previous != Some('\\') {
                        pieces.push(std::mem::take(&mut current));
                    } else {
                        current.push(character);
                    }
                    previous = Some(character);
                }
                pieces.push(current);
                // PerLabelOptions.toString (:120-123): `regexFilter +
                // " Options: " + optionsList` — RegexFilter prints its
                // normalized union pattern (RegexFilter.toString :133-147),
                // not the original spelling (the value a transition READS
                // shows, reverseForStarlark's toOriginalString).
                let filter = crate::coverage::RegexFilter::parse(filter)
                    .map(|filter| filter.to_string())
                    .unwrap_or_else(|_| filter.to_owned());
                format!("{filter} Options: [{}]", pieces.join(", "))
            })
            .collect::<Vec<_>>();
        format!("=[{}]", entries.join(", "))
    };
    options.insert("per_file_copt", per_label_options(&native.per_file_copts));
    options.insert(
        "host_per_file_copt",
        per_label_options(&native.host_per_file_copts),
    );
    options.insert("is exec configuration", format!("={}", native.is_exec));
    options.insert(
        "platform_suffix",
        native
            .platform_suffix
            .map_or_else(|| "@null".to_owned(), |suffix| format!("={suffix}")),
    );
    options
}

/// computeNameFragmentWithDiff (OutputPathMnemonicComputer.java:286-318):
/// `ST-` and the first 12 hex digits of the fingerprint of the sorted
/// `KEY=VALUE` strings of the options that differ from the baseline, minus
/// those explicit in the output path; `None` without such a difference.
fn name_fragment_with_diff(
    configuration: &Configuration,
    baseline: &Configuration,
    explicit: &[&str],
) -> Option<String> {
    if configuration.id == baseline.id {
        return None;
    }
    let to = native_option_strings(&configuration.native);
    let from = native_option_strings(&baseline.native);
    // A TreeMap of `KEY` to its hash form.
    let mut chosen: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in &to {
        if from.get(name) != Some(value) && !explicit.contains(name) {
            let key = format!("//command_line_option:{name}");
            chosen.insert(key.clone(), format!("{key}{value}"));
        }
    }
    // getChangedStarlarkOptions: changed, added and removed settings; a
    // setting absent from `configuration` is at its default (`@null`).
    let starlark_value = |value: Option<&SettingValue>, key: &str| match value {
        Some(value) => format!("{key}={value}"),
        None => format!("{key}@null"),
    };
    for label in configuration
        .starlark
        .keys()
        .chain(baseline.starlark.keys())
    {
        let current = configuration.starlark.get(label);
        // OptionsDiff.diff (analysis/config/OptionsDiff.java:107-118):
        // `first.equals(second)` with the transitioned options first, so a
        // raw StarlarkList differs from the baseline's equal converted list.
        let equal = match (current, baseline.starlark.get(label)) {
            (Some(first), Some(second)) => first.java_equals(second),
            (None, None) => true,
            _ => false,
        };
        if !equal {
            let key = crate::rule_declarations::canonical_form(label);
            chosen.insert(key.clone(), starlark_value(current, &key));
        }
    }
    if chosen.is_empty() {
        return None;
    }
    // transitionDirectoryNameFragment (:412-428): Fingerprint.addString is
    // protobuf's writeStringNoTag (a varint length, then the UTF-8 bytes).
    let mut hash = Sha256::new();
    let mut ordered = chosen.into_iter().collect::<Vec<_>>();
    ordered.sort_by(|a, b| crate::native_rules::java_string_cmp(&a.0, &b.0));
    for (_, text) in ordered {
        let mut length = text.len() as u64;
        loop {
            let byte = (length & 0x7f) as u8;
            length >>= 7;
            if length == 0 {
                hash.update([byte]);
                break;
            }
            hash.update([byte | 0x80]);
        }
        hash.update(text.as_bytes());
    }
    let digest = hash.finalize();
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Some(format!("ST-{}", &hex[..12]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration::{CommandLineOptions, Parsed};

    fn parsed(args: &[&str]) -> Configuration {
        let mut options = CommandLineOptions::default();
        let mut index = 0;
        while index < args.len() {
            match options
                .parse(args[index], args.get(index + 1).copied())
                .unwrap()
            {
                Parsed::NotModelled => panic!("not modelled: {}", args[index]),
                Parsed::Consumed(next) => index += 1 + usize::from(next),
            }
        }
        options
            .configuration(&crate::configuration::parse_canonical_option_label)
            .unwrap()
    }

    fn label(text: &str) -> Label {
        Label::parse_canonical(text).unwrap()
    }

    #[test]
    fn top_level_mnemonics_are_bazels_roots() {
        for (args, root) in [
            (&[][..], "bazel-out/k8-fastbuild/bin"),
            (&["-c", "dbg"][..], "bazel-out/k8-dbg/bin"),
            (&["-c", "opt"][..], "bazel-out/k8-opt/bin"),
            // Options without an output-path effect keep the mode's root:
            // the top-level configuration is its own baseline.
            (
                &["--stamp", "--define=a=1"][..],
                "bazel-out/k8-fastbuild/bin",
            ),
            (
                &["--platforms=//p:x", "-c", "opt"][..],
                "bazel-out/k8-opt/bin",
            ),
        ] {
            let configurations = Configurations::new(parsed(args));
            assert_eq!(
                configurations.output_root(configurations.top()),
                root,
                "{args:?}"
            );
        }
        assert_eq!(
            Configurations::default().output_root(Configurations::default().top()),
            crate::label::GENERATED_EXEC_ROOT
        );
    }

    #[test]
    fn derived_configurations_add_the_suffix_and_a_diff_hash() {
        let top = parsed(&["--host_platform=//p:h"]);
        let mut configurations = Configurations::new(top.clone());
        // Interning is idempotent and keeps the top-level configuration.
        assert_eq!(
            configurations.intern(top.clone(), &top),
            Ok(configurations.top())
        );
        assert_eq!(
            configurations.mnemonic(configurations.top()),
            "k8-fastbuild"
        );
        // An exec configuration uses its platform's name as the CPU
        // descriptor and `platforms` is explicit (paired capture: Bazel's
        // `linux-opt-exec` for --host_platform=//p:linux and its alias).
        let none = BTreeSet::new();
        let id = configurations
            .intern_exec(configurations.top(), label("//p:h"), &none)
            .unwrap();
        assert_ne!(id, configurations.top());
        assert_eq!(configurations.mnemonic(id), "h-opt-exec");
        assert_eq!(configurations.output_root(id), "bazel-out/h-opt-exec/bin");
        assert_eq!(configurations.exec_baseline(&none).id, id);
        assert!(Arc::ptr_eq(configurations.get(id), configurations.get(id)));
        // Exec of exec is the same configuration.
        assert_eq!(
            configurations.intern_exec(id, label("//p:h"), &none),
            Ok(id)
        );
        // Another execution platform (the end of an alias chain, or the
        // default host platform's @platforms//host:host) differs from the
        // baseline only in the explicit `platforms`: no hash
        // (paired capture: `k8-opt-exec`, `linux-opt-exec`).
        let other = configurations
            .intern_exec(configurations.top(), label("//p:other"), &none)
            .unwrap();
        assert_eq!(configurations.mnemonic(other), "other-opt-exec");
        // Default platform names use the CPU (PlatformOptions.platformIsDefault).
        for name in [
            "host",
            "host_platform",
            "target_platform",
            "default_host",
            "default_target",
        ] {
            let id = configurations
                .intern_exec(
                    configurations.top(),
                    label(&format!("@@platforms//host:{name}")),
                    &none,
                )
                .unwrap();
            assert_eq!(configurations.mnemonic(id), "k8-opt-exec", "{name}");
        }
        // A platform name with a separator is not a path component.
        assert_eq!(
            configurations.intern_exec(configurations.top(), label("//p:sub/linux"), &none),
            Err("CPU/Platform descriptor 'sub/linux' is invalid as part of a path: must not contain /".to_owned())
        );
        // Options other than the explicit ones still enter the hash of an
        // exec configuration (here a Starlark flag dropped from the
        // baseline only).
        let mut flagged = Configurations::new(Configuration::new(
            top.native.clone(),
            BTreeMap::from([(label("//:f"), SettingValue::Bool(true))]),
        ));
        let target = BTreeSet::from([label("//:f")]);
        let baseline_dropped = flagged.exec_baseline(&target).clone();
        assert!(baseline_dropped.starlark.is_empty());
        let mut native = baseline_dropped.native.clone();
        native.stamp = true;
        let id = flagged
            .intern(
                Configuration::new(native, BTreeMap::new()),
                &baseline_dropped,
            )
            .unwrap();
        assert!(flagged.mnemonic(id).starts_with("h-opt-exec-ST-"));
        // Only explicit options differ: no hash.
        let mut native = top.native.clone();
        native.compilation_mode = "dbg";
        native.platform_suffix = Some("x");
        assert_eq!(
            mnemonic(&Configuration::new(native, BTreeMap::new()), &top),
            Ok("k8-dbg-x".to_owned())
        );
        // Other options (also Starlark settings) are hashed; equal diffs give
        // equal hashes, different values different hashes.
        let stamped = parsed(&["--stamp"]);
        let stamped_mnemonic = mnemonic(&stamped, &top).unwrap();
        assert!(stamped_mnemonic.starts_with("k8-fastbuild-ST-"));
        assert_eq!(
            stamped_mnemonic,
            mnemonic(&parsed(&["--stamp=yes"]), &top).unwrap()
        );
        assert_ne!(
            stamped_mnemonic,
            mnemonic(&parsed(&["--define=a=1"]), &top).unwrap()
        );
        let flag = |value: bool| {
            Configuration::new(
                NativeOptions::default(),
                BTreeMap::from([(label("//:flag"), SettingValue::Bool(value))]),
            )
        };
        let set = mnemonic(&flag(true), &top).unwrap();
        assert!(set.starts_with("k8-fastbuild-ST-"));
        assert_ne!(set, mnemonic(&flag(false), &top).unwrap());
        // A setting removed relative to the baseline is hashed as `@null`.
        assert!(
            mnemonic(&top, &flag(true))
                .unwrap()
                .starts_with("k8-fastbuild-ST-")
        );
    }

    /// N18c: configurations produced by Starlark transitions are named
    /// against the top-level configuration (target configurations) or the
    /// exec baseline with their own platform (exec configurations), and the
    /// memoized exec baseline only depends on the top-level flags.
    #[test]
    fn starlark_transition_results_use_bazel_baselines() {
        let top = parsed(&["--host_platform=//p:h"]);
        let mut configurations = Configurations::new(top.clone());
        // Equal to the top-level configuration: its own identity and root.
        assert_eq!(
            configurations.intern_derived(top.clone(), &BTreeSet::new()),
            Ok(configurations.top())
        );
        // A transitioned target configuration: compilation_mode explicit,
        // the Starlark flag hashed against the top-level configuration.
        let mut native = top.native.clone();
        native.compilation_mode = "dbg";
        let transitioned = Configuration::new(
            native,
            BTreeMap::from([(label("//f:color"), SettingValue::String("red".into()))]),
        );
        let id = configurations
            .intern_derived(transitioned.clone(), &BTreeSet::new())
            .unwrap();
        assert_eq!(
            configurations.mnemonic(id),
            mnemonic(&transitioned, &top).unwrap()
        );
        assert!(configurations.mnemonic(id).starts_with("k8-dbg-ST-"));
        // A `--cpu` a transition set is explicit in the name (Bazel's
        // `arm-fastbuild`); the host CPU is `k8`.
        let mut native = top.native.clone();
        native.cpu = Some("arm".to_owned());
        let arm = Configuration::new(native, BTreeMap::new());
        assert_eq!(mnemonic(&arm, &top), Ok("arm-fastbuild".to_owned()));
        // A transition below an exec edge keeps `is exec configuration` and
        // is named against the exec baseline with its platform.
        let scoped = BTreeSet::from([label("//f:target")]);
        let exec = configurations
            .intern_exec(configurations.top(), label("//p:h"), &scoped)
            .unwrap();
        assert_eq!(configurations.mnemonic(exec), "h-opt-exec");
        let mut exec_flagged = configurations.get(exec).as_ref().clone();
        exec_flagged
            .starlark
            .insert(label("//f:color"), SettingValue::String("red".into()));
        let exec_flagged = Configuration::new(exec_flagged.native, exec_flagged.starlark);
        let id = configurations
            .intern_derived(exec_flagged, &scoped)
            .unwrap();
        assert!(
            configurations.mnemonic(id).starts_with("h-opt-exec-ST-"),
            "{}",
            configurations.mnemonic(id)
        );
        // The exec transition drops a target-scoped flag that a Starlark
        // transition set (it is in the caller's set even though the
        // top-level configuration does not hold it): the same exec
        // configuration as the top-level one's.
        let with_target = Configuration::new(
            top.native.clone(),
            BTreeMap::from([(label("//f:target"), SettingValue::String("x".into()))]),
        );
        let with_target = configurations.intern_derived(with_target, &scoped).unwrap();
        assert_eq!(
            configurations.intern_exec(with_target, label("//p:h"), &scoped),
            Ok(exec)
        );
    }

    #[test]
    fn exec_transition_follows_the_builtin_exec_transition() {
        let from = Configuration::new(
            parsed(&[
                "-c",
                "dbg",
                "--host_compilation_mode=fastbuild",
                "--stamp",
                "--define=a=1",
                "--strip=never",
                "--force_pic",
                "--platforms=//p:t",
                "--host_platform=//p:h",
                "--extra_toolchains=//tc:x",
                "--incompatible_auto_exec_groups",
                "--macos_sdk_version=10.13",
            ])
            .native,
            BTreeMap::from([
                (label("//f:universal"), SettingValue::Bool(true)),
                (label("//f:target"), SettingValue::Int(3)),
            ]),
        );
        let exec = exec_transition(
            &from,
            label("//p:exec"),
            &BTreeSet::from([label("//f:target")]),
        );
        let native = &exec.native;
        assert_eq!(native.compilation_mode, "fastbuild");
        assert_eq!(native.host_compilation_mode, "fastbuild");
        assert!(native.is_exec && !native.stamp && !native.force_pic);
        assert_eq!(native.strip, "always");
        assert_eq!(native.define, [("a".to_owned(), "1".to_owned())]);
        assert_eq!(native.platforms, [label("//p:exec")]);
        assert_eq!(native.host_platform, label("//p:h"));
        assert_eq!(native.extra_toolchains, ["//tc:x"]);
        assert_eq!(native.platform_suffix, Some("exec"));
        // In CoreOptions' propagate list (builtin_exec_platforms.bzl:246).
        assert!(native.incompatible_auto_exec_groups);
        // AppleCommandLineOptions propagates the SDK versions.
        assert_eq!(native.apple_sdk_versions.macos.as_deref(), Some("10.13"));
        assert_eq!(
            exec.starlark,
            BTreeMap::from([(label("//f:universal"), SettingValue::Bool(true))])
        );
        // Idempotent for the same execution platform.
        let none = BTreeSet::new();
        assert_eq!(exec_transition(&exec, label("//p:exec"), &none).id, exec.id);
        // Defaults: --host_compilation_mode is `opt`.
        let default = exec_transition(&Configuration::default(), label("//p:exec"), &none);
        assert_eq!(default.native.compilation_mode, "opt");
        assert!(!default.native.incompatible_auto_exec_groups);
        assert_eq!(mnemonic(&default, &default), Ok("exec-opt-exec".to_owned()));
    }

    /// `_core_options` (builtin_exec_platforms.bzl:221-229): the exec
    /// configuration's `--cpu` is the parent's `--host_cpu`, which
    /// propagates; `--crosstool_top` is not propagated (the default). The
    /// CPU names target roots and exec roots of default-named platforms
    /// (paired captures cpu_arm_set, exec_cpu_arm, exec_host_cpu_ppc and
    /// cpu_flag_slash in tests/fixtures/pre_platforms_flags).
    #[test]
    fn exec_transition_takes_the_host_cpu_and_resets_crosstool_top() {
        let none = BTreeSet::new();
        let host = label("@@platforms//host:host");
        let top = parsed(&["--cpu=arm", "--crosstool_top=//pkg:t"]);
        let mut configurations = Configurations::new(top.clone());
        assert_eq!(
            configurations.output_root(configurations.top()),
            "bazel-out/arm-fastbuild/bin"
        );
        let exec = configurations
            .intern_exec(configurations.top(), host.clone(), &none)
            .unwrap();
        let native = &configurations.get(exec).native;
        assert_eq!(
            (native.cpu.as_deref(), native.host_cpu.as_deref()),
            (None, None)
        );
        assert_eq!(native.crosstool_top, None);
        assert_eq!(configurations.mnemonic(exec), "k8-opt-exec");
        // The top-level --crosstool_top and --cpu leave the exec
        // configuration of the defaults unchanged.
        assert_eq!(
            exec,
            exec_transition(&Configuration::default(), host.clone(), &none).id
        );
        let top = parsed(&["--host_cpu=ppc"]);
        let mut configurations = Configurations::new(top.clone());
        assert_eq!(
            configurations.output_root(configurations.top()),
            "bazel-out/k8-fastbuild/bin"
        );
        let exec = configurations
            .intern_exec(configurations.top(), host.clone(), &none)
            .unwrap();
        let native = &configurations.get(exec).native;
        assert_eq!(native.cpu.as_deref(), Some("ppc"));
        assert_eq!(native.host_cpu(), "ppc");
        assert_eq!(configurations.mnemonic(exec), "ppc-opt-exec");
        // A non-default platform name still names the exec root.
        let named = configurations
            .intern_exec(configurations.top(), label("//p:linux"), &none)
            .unwrap();
        assert_eq!(configurations.mnemonic(named), "linux-opt-exec");
        // A `--cpu` with a separator is no output directory.
        assert_eq!(
            validate_top(&parsed(&["--cpu=a/b"])),
            Err(
                "CPU/Platform descriptor 'a/b' is invalid as part of a path: must not contain /"
                    .to_owned()
            )
        );
        assert_eq!(validate_top(&top), Ok(()));
    }

    /// The table-driven options' exec rules (crate::configuration::ExecValue):
    /// ProtoConfiguration's options propagate (builtin_exec_platforms.bzl:422-436),
    /// so `--strict_proto_deps=off` reaches exec tools while the exec root
    /// stays `k8-opt-exec` (the exec baseline has them too); defaults keep
    /// the exec identity of the defaults (paired captures proto_exec,
    /// coverage_transition, apple_defaults in tests/fixtures/value_options).
    #[test]
    fn exec_transition_applies_the_value_option_rules() {
        let none = BTreeSet::new();
        let host = label("@@platforms//host:host");
        let top = parsed(&[
            "--strict_proto_deps=off",
            "--experimental_proto_descriptor_sets_include_source_info",
            "--strict_public_imports=warn",
        ]);
        let mut configurations = Configurations::new(top.clone());
        assert_eq!(
            configurations.mnemonic(configurations.top()),
            "k8-fastbuild"
        );
        let exec = configurations
            .intern_exec(configurations.top(), host.clone(), &none)
            .unwrap();
        let native = &configurations.get(exec).native;
        assert_eq!(native.values, top.native.values);
        assert_eq!(
            native.value("strict_proto_deps"),
            Some(crate::configuration::OptionValue::Enum("OFF"))
        );
        assert_eq!(configurations.mnemonic(exec), "k8-opt-exec");
        // At the defaults nothing is stored: the exec identity is unchanged
        // (apple_platform_type is set to its default `macos`).
        let default_exec = exec_transition(&Configuration::default(), host.clone(), &none);
        assert!(default_exec.native.values.is_empty());
        assert_eq!(
            default_exec.native.value("apple_platform_type"),
            Some(crate::configuration::OptionValue::String("macos".into()))
        );
        // collect_code_coverage (a typed field) is not propagated.
        let coverage = Configuration::new(
            NativeOptions {
                collect_code_coverage: true,
                ..NativeOptions::default()
            },
            BTreeMap::new(),
        );
        let coverage_exec = exec_transition(&coverage, host, &none);
        assert_eq!(
            coverage_exec.native.value("collect_code_coverage"),
            Some(crate::configuration::OptionValue::Bool(false))
        );
        assert_eq!(coverage_exec.id, default_exec.id);
        // Genuine's root of that transitioned configuration
        // (value_options/coverage_transition): the hash of
        // `//command_line_option:collect_code_coverage=true` alone.
        assert_eq!(
            mnemonic(&coverage, &Configuration::default()),
            Ok("k8-fastbuild-ST-0a6d1b0f07f2".to_owned())
        );
    }

    /// N28b: `_java_options` (builtin_exec_platforms.bzl:350-385): the exec
    /// configuration's java_* are the parent's tool_java_*, which propagate.
    /// At the defaults the exec identity is the pre-N28b one.
    #[test]
    fn exec_transition_maps_java_versions_from_the_tool_versions() {
        let none = BTreeSet::new();
        let default = exec_transition(&Configuration::default(), label("//p:exec"), &none);
        let java = &default.native.java;
        assert_eq!(
            (
                java.java_runtime_version.as_str(),
                java.java_language_version.as_str(),
                java.tool_java_runtime_version.as_str(),
                java.tool_java_language_version.as_str()
            ),
            ("remotejdk_11", "", "remotejdk_11", "")
        );
        // The identity serialization omits the kind's default versions
        // (N28b) and the empty default C++ flag lists; N66's exec transition
        // writes `--copt`/`--cxxopt` = `--host_*` + `-g0`
        // (builtin_exec_platforms.bzl:334-342), which the exec identity
        // carries (genuine Bazel's exec BuildOptions hold the same lists).
        let n66 = br#"{"version":1,"native":{"compilation_mode":"opt","stamp":false,"define":[],"strip":"always","force_pic":false,"platforms":["//p:exec"],"host_platform":"@@bazel_tools//tools:host_platform","is_exec":true,"platform_suffix":"exec","copts":["-g0"],"cxxopts":["-g0"]},"starlark":[]}"#;
        assert_eq!(default.id.to_string(), format!("{:x}", Sha256::digest(n66)));
        let from = parsed(&[
            "--java_runtime_version=remotejdk_17",
            "--java_language_version=17",
            "--tool_java_runtime_version=remotejdk_21",
            "--tool_java_language_version=21",
        ]);
        let exec = exec_transition(&from, label("//p:exec"), &none);
        let java = &exec.native.java;
        assert_eq!(
            (
                java.java_runtime_version.as_str(),
                java.java_language_version.as_str(),
                java.tool_java_runtime_version.as_str(),
                java.tool_java_language_version.as_str()
            ),
            ("remotejdk_21", "21", "remotejdk_21", "21")
        );
        assert_ne!(exec.id, default.id);
        // The target java versions do not reach the exec configuration.
        let target_only = parsed(&["--java_runtime_version=remotejdk_17"]);
        assert_ne!(target_only.id, Configuration::default().id);
        assert_eq!(
            exec_transition(&target_only, label("//p:exec"), &none).id,
            default.id
        );
        // Against its own baseline the top-level configuration keeps its root.
        assert_eq!(
            mnemonic(&target_only, &target_only),
            Ok("k8-fastbuild".to_owned())
        );
        // Against the default baseline the version is hashed into ST-.
        assert!(
            mnemonic(&target_only, &Configuration::default())
                .unwrap()
                .starts_with("k8-fastbuild-ST-")
        );
    }

    #[test]
    fn exec_transition_clears_plugins_and_converges_with_default() {
        let default = parsed(&[]);
        let plugins = parsed(&["--plugin=//p:a,//p:b"]);
        let platform = default.native.target_platform().clone();
        let default = exec_transition(&default, platform.clone(), &BTreeSet::new());
        let plugins = exec_transition(&plugins, platform, &BTreeSet::new());
        assert!(plugins.native.plugins.is_empty());
        assert_eq!(default.id, plugins.id);
    }

    /// N28c: the no-config configuration (CommonOptions.EMPTY_OPTIONS) and
    /// its `<compilation_mode>-noconfig` mnemonic
    /// (OutputPathMnemonicComputer.java:187-190), whatever the parent.
    #[test]
    fn no_config_configuration_is_core_defaults_only() {
        let mut configurations = Configurations::new(parsed(&["-c", "opt", "--stamp"]));
        let id = configurations.intern_no_config();
        assert_eq!(configurations.intern_no_config(), id);
        assert!(configurations.is_no_config(id));
        assert!(!configurations.is_no_config(configurations.top()));
        assert_eq!(configurations.mnemonic(id), "fastbuild-noconfig");
        assert_eq!(
            configurations.output_root(id),
            "bazel-out/fastbuild-noconfig/bin"
        );
        let configuration = configurations.get(id);
        assert_eq!(configuration.native.compilation_mode, "fastbuild");
        assert!(!configuration.native.stamp && configuration.starlark.is_empty());
        assert_ne!(id, Configuration::default().id);
        assert_eq!(
            mnemonic(configuration, &Configuration::default()),
            Ok("fastbuild-noconfig".to_owned())
        );
        assert_eq!(no_config().id, id);
    }

    /// N47: a Starlark transition's `--collect_code_coverage` is hashed as
    /// `//command_line_option:collect_code_coverage=true` against the
    /// top-level baseline (computeNameFragmentWithDiff), and the exec
    /// transition takes the CoreOptions default (builtin_exec_platforms.bzl:231-275
    /// does not propagate it).
    #[test]
    fn collect_code_coverage_enters_the_st_hash_and_not_the_exec_configuration() {
        let top = parsed(&["--host_platform=//p:h"]);
        let mut native = top.native.clone();
        native.collect_code_coverage = true;
        let coverage = Configuration::new(native, BTreeMap::new());
        let text = "//command_line_option:collect_code_coverage=true";
        let mut bytes = vec![text.len() as u8];
        bytes.extend_from_slice(text.as_bytes());
        let digest = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            mnemonic(&coverage, &top),
            Ok(format!("k8-fastbuild-ST-{}", &digest[..12]))
        );
        let none = BTreeSet::new();
        assert_eq!(
            exec_transition(&coverage, label("//p:h"), &none).id,
            exec_transition(&top, label("//p:h"), &none).id
        );
    }

    /// rules_uv's transition_to_target (paired capture
    /// tests/fixtures/extra_execution_platforms, default_host: Bazel's
    /// k8-fastbuild-ST-f7ead6e6535a): `extra_execution_platforms` enters the
    /// ST- hash as its Java list form, and the exec transition propagates it.
    #[test]
    fn extra_execution_platforms_enter_the_st_hash_and_propagate_to_exec() {
        let top = Configuration::default();
        let mut native = top.native.clone();
        native.extra_execution_platforms = vec!["@@bazel_tools//tools:host_platform".into()];
        let transitioned = Configuration::new(native, BTreeMap::new());
        assert_eq!(
            mnemonic(&transitioned, &top),
            Ok("k8-fastbuild-ST-f7ead6e6535a".to_owned())
        );
        let none = BTreeSet::new();
        assert_eq!(
            exec_transition(&transitioned, label("//p:h"), &none)
                .native
                .extra_execution_platforms,
            ["@@bazel_tools//tools:host_platform"]
        );
    }

    /// N47 (paired capture S2 gobin_race): a raw list equal to the
    /// baseline's converted command-line list is still a difference, hashed
    /// with its Starlark repr (Bazel's ST-91a4ed8818d5).
    #[test]
    fn raw_lists_differ_from_the_converted_baseline_list() {
        let tags = label("//go/config:tags");
        let top = Configuration::new(
            NativeOptions::default(),
            BTreeMap::from([(
                tags.clone(),
                SettingValue::StringList(vec!["netgo".into(), "b".into()]),
            )]),
        );
        let raw = Configuration::new(
            NativeOptions::default(),
            BTreeMap::from([(
                tags,
                SettingValue::Sequence {
                    tuple: false,
                    items: vec!["netgo".into(), "b".into()],
                },
            )]),
        );
        assert_eq!(
            mnemonic(&raw, &top),
            Ok("k8-fastbuild-ST-91a4ed8818d5".to_owned())
        );
    }

    #[test]
    fn exec_transition_uses_host_action_env() {
        use crate::action_env::EnvVar;
        let from = Configuration::new(
            parsed(&[
                "--action_env=A=1",
                "--host_action_env=H",
                "--noincompatible_strict_action_env",
                "--test_env=T=1",
            ])
            .native,
            BTreeMap::new(),
        );
        let none = BTreeSet::new();
        let exec = exec_transition(&from, label("//p:exec"), &none);
        let host = [EnvVar::Inherit("H".into())];
        assert_eq!(exec.native.action_env, host);
        assert_eq!(exec.native.host_action_env, host);
        assert!(!exec.native.strict_action_env);
        assert_eq!(exec.native.test_env, [EnvVar::Set("T".into(), "1".into())]);
        assert_eq!(exec_transition(&exec, label("//p:exec"), &none).id, exec.id);
        // The exec baseline carries the same environment: no mnemonic hash.
        assert_eq!(mnemonic(&exec, &exec), Ok("exec-opt-exec".to_owned()));
        let env = exec.native.action_environment();
        assert!(env.fixed.is_empty());
        assert_eq!(
            env.inherited,
            BTreeSet::from(["H".to_owned(), "LD_LIBRARY_PATH".into(), "PATH".into()])
        );
    }

    #[test]
    fn name_fragment_hashes_protobuf_framed_sorted_strings() {
        // Fingerprint.addString("//command_line_option:stamp=true"): varint
        // length 32, then the bytes.
        let expected = {
            let text = "//command_line_option:stamp=true";
            let mut bytes = vec![text.len() as u8];
            bytes.extend_from_slice(text.as_bytes());
            let digest = Sha256::digest(&bytes);
            format!(
                "ST-{}",
                &digest
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()[..12]
            )
        };
        assert_eq!(
            name_fragment_with_diff(&parsed(&["--stamp"]), &parsed(&[]), EXPLICIT_IN_OUTPUT_PATH),
            Some(expected)
        );
        assert_eq!(
            name_fragment_with_diff(&parsed(&[]), &parsed(&[]), EXPLICIT_IN_OUTPUT_PATH),
            None
        );
    }

    #[test]
    fn configured_target_keys_order_print_and_name_actions() {
        let top = Configuration::default().id;
        let dbg = parsed(&["-c", "dbg"]).id;
        let a = ConfiguredTargetKey::new(label("//p:a"), top);
        let a_dbg = ConfiguredTargetKey::new(label("//p:a"), dbg);
        let b = ConfiguredTargetKey::new(label("//p:b"), top);
        // Label first, then configuration.
        assert!(a < b && a_dbg < b && a != a_dbg);
        assert_eq!(a.to_string(), format!("//p:a ({top})"));
        assert_eq!(
            ConfiguredTargetKey::new(label("@@r+//p:a"), top).to_string(),
            format!("@@r+//p:a ({top})")
        );
        // Action owners: unchanged in the top-level configuration.
        assert_eq!(a.action_owner(top), "//p:a");
        assert_eq!(
            a_dbg.action_owner(top),
            format!("//p:a[{}]", &dbg.to_string()[..8])
        );
        assert_eq!(a_dbg.action_owner(dbg), "//p:a");
        let configurations = Configurations::default();
        assert_eq!(configurations.top_key(label("//p:a")), a);
        assert!(Arc::ptr_eq(
            configurations.top_configuration(),
            configurations.get(top)
        ));
    }
}
