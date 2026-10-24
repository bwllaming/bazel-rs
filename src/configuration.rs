//! Native build configuration: the modelled Bazel 9.3 build options, their
//! command-line converters and a stable configuration identity.
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/`. Only the
//! options in [`OPTIONS`] are modelled; every other build option is an explicit
//! failure on the command line and in `config_setting(values=...)`
//! (docs/CONFIGURATION.md). Starlark build settings (`starlark`) are stored
//! only when different from their default (StarlarkOptionsParser.java:232-254,
//! crate::starlark_options).
use std::collections::BTreeMap;
use std::fmt;

use sha2::{Digest, Sha256};

use crate::action_env::{ActionEnvironment, EnvVar};
use crate::label::Label;

/// Value shape and converter of a modelled option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionKind {
    /// Converters.BooleanConverter (common/options/Converters.java:46-60).
    Bool,
    /// EnumConverter (common/options/EnumConverter.java:32-67): case-insensitive.
    Enum {
        type_name: &'static str,
        values: &'static [&'static str],
    },
    /// Converters.AssignmentConverter (common/options/Converters.java:483-501).
    Assignments,
    /// CoreOptionConverters.LabelListConverter (build/lib/analysis/config/CoreOptionConverters.java:143-158).
    LabelList,
    /// PlatformOptions.HostPlatformConverter (build/lib/analysis/PlatformOptions.java:218-227).
    Label,
    /// A `String` option without a converter: the default converter of
    /// `String` (Converters.StringConverter, common/options/Converters.java:69-79,
    /// 226): the value as given.
    String,
    /// DottedVersionConverter (build/lib/rules/apple/DottedVersionConverter.java:21-36),
    /// default null.
    DottedVersion,
    /// CoreOptionConverters.LabelConverter (build/lib/analysis/config/CoreOptionConverters.java:129-140):
    /// convertOptionsLabel without a default for an empty input (`//`, an
    /// invalid label).
    OptionsLabel,
    /// AutoCpuConverter (build/lib/analysis/config/AutoCpuConverter.java:28-71):
    /// an empty input is this host's CPU ([`host_cpu`]), anything else the
    /// value as given.
    AutoCpu,
    /// AppleCommandLineOptions.PlatformTypeConverter
    /// (build/lib/rules/apple/AppleCommandLineOptions.java:394-406): the
    /// input in ASCII lower case (`Ascii.toLowerCase`), never an error.
    LowerCaseString,
    /// Converters.TriStateConverter (common/options/Converters.java:133-156):
    /// `auto` or a boolean spelling, case-insensitive, stored as the
    /// TriState constant (`YES`, `NO`, `AUTO`: its `toString`); boolean
    /// option syntax (OptionDefinition.usesBooleanValueSyntax, :160-166).
    TriState,
}

impl OptionKind {
    /// OptionDefinition.usesBooleanValueSyntax (common/options/OptionDefinition.java:160-166):
    /// `--NAME` without a value is `--NAME=1`, `--noNAME` is `--NAME=0`.
    pub fn boolean_syntax(self) -> bool {
        matches!(self, Self::Bool | Self::TriState)
    }
}

/// One modelled native option (an `@Option` of a FragmentOptions class).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OptionDef {
    pub name: &'static str,
    pub old_name: Option<&'static str>,
    pub abbrev: Option<char>,
    pub kind: OptionKind,
    /// `defaultValue` as written in the annotation.
    pub default: &'static str,
    pub allow_multiple: bool,
    /// OptionMetadataTag.NON_CONFIGURABLE (BuildOptionDetails.java:188-202).
    pub non_configurable: bool,
    pub source: &'static str,
    /// `deprecationWarning` (Option.java:176-182): OptionsParserImpl adds
    /// `Option 'NAME' is deprecated: TEXT` once per invocation
    /// (OptionsParserImpl.java:336-369, a LinkedHashSet).
    pub deprecation: Option<&'static str>,
}

const COMPILATION_MODES: &[&str] = &["fastbuild", "dbg", "opt"];
const STRIP_MODES: &[&str] = &["always", "sometimes", "never"];

/// `@bazel_tools//tools:host_platform` (PlatformOptions.java:46).
pub const DEFAULT_HOST_PLATFORM: &str = "@bazel_tools//tools:host_platform";

/// The modelled options, in a fixed order that is also the identity order.
pub const OPTIONS: &[OptionDef] = &[
    OptionDef {
        name: "compilation_mode",
        old_name: None,
        abbrev: Some('c'),
        // CompilationMode enum order FASTBUILD, DBG, OPT
        // (build/lib/analysis/config/CompilationMode.java:24-51).
        kind: OptionKind::Enum {
            type_name: "compilation mode",
            values: COMPILATION_MODES,
        },
        default: "fastbuild",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/analysis/config/CoreOptions.java:396-404",
        deprecation: None,
    },
    OptionDef {
        name: "host_compilation_mode",
        old_name: None,
        abbrev: None,
        // CompilationMode.Converter, like --compilation_mode; the exec
        // transition's compilation mode (builtin_exec_platforms.bzl:221-229).
        kind: OptionKind::Enum {
            type_name: "compilation mode",
            values: COMPILATION_MODES,
        },
        default: "opt",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/analysis/config/CoreOptions.java:406-417",
        deprecation: None,
    },
    OptionDef {
        name: "stamp",
        old_name: None,
        abbrev: None,
        kind: OptionKind::Bool,
        default: "false",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/analysis/config/CoreOptions.java:349-355",
        deprecation: None,
    },
    OptionDef {
        name: "define",
        old_name: None,
        abbrev: None,
        kind: OptionKind::Assignments,
        default: "null",
        allow_multiple: true,
        non_configurable: false,
        source: "build/lib/analysis/config/CoreOptions.java:255-267",
        deprecation: None,
    },
    OptionDef {
        name: "strip",
        old_name: None,
        abbrev: None,
        // CppConfiguration.StripMode ALWAYS, SOMETIMES, NEVER
        // (build/lib/rules/cpp/CppConfiguration.java:119-134), StripModeConverter
        // (build/lib/rules/cpp/CppOptions.java:68-72).
        kind: OptionKind::Enum {
            type_name: "strip mode",
            values: STRIP_MODES,
        },
        default: "sometimes",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/rules/cpp/CppOptions.java:337-347",
        deprecation: None,
    },
    OptionDef {
        name: "force_pic",
        old_name: None,
        abbrev: None,
        kind: OptionKind::Bool,
        default: "false",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/rules/cpp/CppOptions.java:218-227",
        deprecation: None,
    },
    OptionDef {
        name: "platforms",
        old_name: Some("experimental_platforms"),
        abbrev: None,
        kind: OptionKind::LabelList,
        default: "",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/analysis/PlatformOptions.java:82-96",
        deprecation: None,
    },
    OptionDef {
        name: "host_platform",
        old_name: Some("experimental_host_platform"),
        abbrev: None,
        kind: OptionKind::Label,
        default: DEFAULT_HOST_PLATFORM,
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/analysis/PlatformOptions.java:52-64",
        deprecation: None,
    },
    OptionDef {
        name: "use_target_platform_for_tests",
        old_name: None,
        abbrev: None,
        kind: OptionKind::Bool,
        default: "false",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/analysis/config/CoreOptions.java:1110-1123",
        deprecation: Some(
            "Tests select an execution platform matching all constraints of the target platform by default. Instead of using this flag, make sure that all test target platform are registered as execution platforms.",
        ),
    },
    // JavaOptions (N28b): the Java runtime/language version strings that
    // rules_java's toolchain config_settings select on. Not CoreOptions, so
    // `query` does not accept them (QueryCommand.java:59-67).
    OptionDef {
        name: "java_runtime_version",
        old_name: None,
        abbrev: None,
        kind: OptionKind::String,
        default: DEFAULT_JAVA_RUNTIME_VERSION,
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/rules/java/JavaOptions.java:373-379",
        deprecation: None,
    },
    OptionDef {
        name: "java_language_version",
        old_name: None,
        abbrev: None,
        kind: OptionKind::String,
        default: "",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/rules/java/JavaOptions.java:389-395",
        deprecation: None,
    },
    OptionDef {
        name: "tool_java_runtime_version",
        old_name: None,
        abbrev: None,
        kind: OptionKind::String,
        default: DEFAULT_TOOL_JAVA_RUNTIME_VERSION,
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/rules/java/JavaOptions.java:381-387",
        deprecation: None,
    },
    OptionDef {
        name: "tool_java_language_version",
        old_name: None,
        abbrev: None,
        kind: OptionKind::String,
        default: "",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/rules/java/JavaOptions.java:397-403",
        deprecation: None,
    },
    OptionDef {
        name: "incompatible_auto_exec_groups",
        old_name: None,
        abbrev: None,
        kind: OptionKind::Bool,
        default: "false",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/analysis/config/CoreOptions.java:419-433",
        deprecation: None,
    },
    // AppleCommandLineOptions SDK versions (N40): DottedVersion.Option values,
    // stored as given (equality is by string, DottedVersion.java:128-139)
    // and read by ctx.fragments.apple (AppleConfiguration.java:104-107).
    OptionDef {
        name: "ios_sdk_version",
        old_name: None,
        abbrev: None,
        kind: OptionKind::DottedVersion,
        default: "null",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/rules/apple/AppleCommandLineOptions.java:59-67",
        deprecation: None,
    },
    OptionDef {
        name: "watchos_sdk_version",
        old_name: None,
        abbrev: None,
        kind: OptionKind::DottedVersion,
        default: "null",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/rules/apple/AppleCommandLineOptions.java:70-78",
        deprecation: None,
    },
    OptionDef {
        name: "tvos_sdk_version",
        old_name: None,
        abbrev: None,
        kind: OptionKind::DottedVersion,
        default: "null",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/rules/apple/AppleCommandLineOptions.java:81-89",
        deprecation: None,
    },
    OptionDef {
        name: "macos_sdk_version",
        old_name: None,
        abbrev: None,
        kind: OptionKind::DottedVersion,
        default: "null",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/rules/apple/AppleCommandLineOptions.java:92-100",
        deprecation: None,
    },
    // ConfigSetting.DEPRECATED_PRE_PLATFORMS_FLAGS (rules/config/ConfigSetting.java:95-97):
    // selectable in `config_setting(values)` with Bazel's main-repository
    // deprecation warning (crate::config_setting). `--cpu` names the output
    // directory of target configurations (crate::config_registry); the exec
    // transition sets it to `--host_cpu` (builtin_exec_platforms.bzl:225).
    OptionDef {
        name: "cpu",
        old_name: None,
        abbrev: None,
        kind: OptionKind::AutoCpu,
        default: "",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/analysis/config/CoreOptions.java:279-292",
        deprecation: None,
    },
    OptionDef {
        name: "host_cpu",
        old_name: None,
        abbrev: None,
        kind: OptionKind::AutoCpu,
        default: "",
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/analysis/config/CoreOptions.java:387-394",
        deprecation: None,
    },
    // A NO_OP option in Bazel 9.3: nothing but `config_setting` reads it.
    OptionDef {
        name: "crosstool_top",
        old_name: None,
        abbrev: None,
        kind: OptionKind::OptionsLabel,
        default: DEFAULT_CROSSTOOL_TOP,
        allow_multiple: false,
        non_configurable: false,
        source: "build/lib/rules/cpp/CppOptions.java:102-111",
        deprecation: None,
    },
];

/// The `--crosstool_top` default (rules/cpp/CppOptions.java:102-111).
pub const DEFAULT_CROSSTOOL_TOP: &str = "@bazel_tools//tools/cpp:toolchain";

/// The label of [`DEFAULT_CROSSTOOL_TOP`].
pub fn default_crosstool_top() -> Label {
    Label {
        repository: "bazel_tools".into(),
        package: "tools/cpp".into(),
        name: "toolchain".into(),
    }
}

/// `--collect_code_coverage` (CoreOptions.java:566-578): a boolean option
/// that only a Starlark transition sets natively (N47), so not in
/// [`OPTIONS`] (the command line and `config_setting` keep rejecting it).
pub const COLLECT_CODE_COVERAGE: OptionDef = OptionDef {
    name: "collect_code_coverage",
    old_name: None,
    abbrev: None,
    kind: OptionKind::Bool,
    default: "false",
    allow_multiple: false,
    non_configurable: false,
    source: "build/lib/analysis/config/CoreOptions.java:566-578",
    deprecation: None,
};

/// `--java_runtime_version` default (JavaOptions.java:373-379).
pub const DEFAULT_JAVA_RUNTIME_VERSION: &str = "local_jdk";
/// `--tool_java_runtime_version` default (JavaOptions.java:381-387).
pub const DEFAULT_TOOL_JAVA_RUNTIME_VERSION: &str = "remotejdk_11";

/// How Bazel 9.3's exec transition sets a [`ValueOption`]: the built-in
/// Starlark `bazel_exec_transition`
/// (src/main/starlark/builtins_bzl/common/builtin_exec_platforms.bzl) runs
/// over a baseline of every native option at its default
/// (FunctionTransitionUtil.maybeGetExecDefaults,
/// analysis/starlark/FunctionTransitionUtil.java:167-225).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecValue {
    /// In its fragment's `propagate` list: the parent's value.
    Propagate,
    /// Neither propagated nor written: the option default.
    Default,
    /// Written by the fragment's `func` with this unconverted value.
    Set(&'static str),
}

/// A native option modelled by its value alone (table-driven): its
/// converter and default ([`OptionDef`]), whether the command line may set
/// it, and its exec-transition rule. Its configured value is matched by
/// `config_setting(values)` (crate::config_setting) and is part of the
/// configuration identity only when it differs from the default
/// ([`NativeOptions::values`]), so a default spelled explicitly is the
/// default configuration. Options whose value has further effects the
/// native profile does not model stay unsupported on the command line
/// (`command_line: false`): their configured value is then always the one
/// the modelled options and transitions give (the default unless a typed
/// [`NativeOptions`] field holds it), and matching it is exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValueOption {
    pub option: OptionDef,
    /// Whether `--NAME` is a modelled command-line option (of the commands
    /// with configuration options; `query` registers only CoreOptions,
    /// QueryCommand.java:59-67).
    pub command_line: bool,
    pub exec: ExecValue,
    /// Where the exec rule is defined (builtin_exec_platforms.bzl line).
    pub exec_source: &'static str,
}

/// CoreOptionConverters.StrictDepsMode (build/lib/analysis/config/CoreOptionConverters.java:311-322)
/// in declaration order, spelled as its `toString` (the constant name):
/// StrictDepsConverter (:324-329) is an EnumConverter matching it
/// case-insensitively; config_setting diffs print it upper-case.
const STRICT_DEPS_MODES: &[&str] = &["OFF", "WARN", "ERROR", "STRICT", "DEFAULT"];

/// The table-driven options (docs/CONFIGURATION.md "Value options"). Each
/// row was checked against genuine Bazel 9.3.0 (paired capture
/// tests/fixtures/value_options): converter texts, case rules, defaults,
/// the exec transition, `query`'s `Unrecognized option`.
pub const VALUE_OPTIONS: &[ValueOption] = &[
    // ProtoConfiguration.Options (build/lib/rules/proto/ProtoConfiguration.java:47-154):
    // read natively only by `config_setting` (protobuf's native_bool_flag,
    // bazel/private/native_bool_flag.bzl) and the private
    // `ctx.fragments.proto` getters (crate::ctx_surface). The fragment's
    // options all propagate to exec configurations
    // (builtin_exec_platforms.bzl:422-436).
    ValueOption {
        option: OptionDef {
            name: "experimental_proto_descriptor_sets_include_source_info",
            old_name: None,
            abbrev: None,
            kind: OptionKind::Bool,
            default: "false",
            allow_multiple: false,
            non_configurable: false,
            source: "build/lib/rules/proto/ProtoConfiguration.java:58-65",
            deprecation: None,
        },
        command_line: true,
        exec: ExecValue::Propagate,
        exec_source: "builtin_exec_platforms.bzl:426",
    },
    ValueOption {
        option: OptionDef {
            name: "strict_proto_deps",
            old_name: None,
            abbrev: None,
            kind: OptionKind::Enum {
                type_name: "strict dependency checking level",
                values: STRICT_DEPS_MODES,
            },
            default: "error",
            allow_multiple: false,
            non_configurable: false,
            source: "build/lib/rules/proto/ProtoConfiguration.java:113-123",
            deprecation: None,
        },
        command_line: true,
        exec: ExecValue::Propagate,
        exec_source: "builtin_exec_platforms.bzl:431",
    },
    ValueOption {
        option: OptionDef {
            name: "strict_public_imports",
            old_name: None,
            abbrev: None,
            kind: OptionKind::Enum {
                type_name: "strict dependency checking level",
                values: STRICT_DEPS_MODES,
            },
            default: "off",
            allow_multiple: false,
            non_configurable: false,
            source: "build/lib/rules/proto/ProtoConfiguration.java:125-135",
            deprecation: None,
        },
        command_line: true,
        exec: ExecValue::Propagate,
        exec_source: "builtin_exec_platforms.bzl:432",
    },
    // CoreOptions.enableRunfiles (`--enable_runfiles`, a TriState, default
    // auto): config_setting matches the configured value, which stays AUTO
    // unless set (aspect_bazel_lib's //lib:enable_runfiles `true` does not
    // match by default; paired capture tests/fixtures/enable_runfiles_option);
    // the runfiles trees read the same flag (crate::runfiles_tree,
    // BuildConfigurationValue.runfilesEnabled: YES, or AUTO off Windows).
    // Propagated to exec configurations (builtin_exec_platforms.bzl:237).
    ValueOption {
        option: OptionDef {
            name: "enable_runfiles",
            old_name: None,
            abbrev: None,
            kind: OptionKind::TriState,
            default: "auto",
            allow_multiple: false,
            non_configurable: false,
            source: "build/lib/analysis/config/CoreOptions.java:954-961",
            deprecation: None,
        },
        command_line: true,
        exec: ExecValue::Propagate,
        exec_source: "builtin_exec_platforms.bzl:237",
    },
    // CoreOptions.collectCodeCoverage: held by the typed field
    // NativeOptions::collect_code_coverage, which only a Starlark
    // transition sets (N47); the command line (and `bazel coverage`) stays
    // unsupported because coverage instrumentation is not modelled.
    // rules_python's hermetic runtimes select on it
    // (python/private/hermetic_runtime_repo_setup.bzl, `:coverage_enabled`).
    ValueOption {
        option: COLLECT_CODE_COVERAGE,
        command_line: false,
        exec: ExecValue::Default,
        exec_source: "builtin_exec_platforms.bzl:231-287 (not propagated)",
    },
    // AppleCommandLineOptions.applePlatformType: a String option with
    // PlatformTypeConverter, "derived from other flags and configuration
    // transitions" that the native profile does not model (Apple rules,
    // ctx.fragments.apple), so not a command-line option; always `macos`
    // natively, as in every non-Apple Bazel configuration. grpc's
    // `//:macos`, `//:ios`, ... config_settings select on it.
    ValueOption {
        option: OptionDef {
            name: "apple_platform_type",
            old_name: None,
            abbrev: None,
            kind: OptionKind::LowerCaseString,
            default: "macos",
            allow_multiple: false,
            non_configurable: false,
            source: "build/lib/rules/apple/AppleCommandLineOptions.java:194-203",
            deprecation: None,
        },
        command_line: false,
        exec: ExecValue::Set("macos"),
        exec_source: "builtin_exec_platforms.bzl:185, 190",
    },
];

/// The table-driven option named `name` (its canonical name only).
pub fn value_option(name: &str) -> Option<&'static ValueOption> {
    VALUE_OPTIONS
        .iter()
        .find(|option| option.option.name == name)
}

impl ValueOption {
    /// The converted default (`defaultValue` through the converter).
    pub fn default_value(&self) -> OptionValue {
        convert(&self.option, self.option.default, &|_| {
            Err("value options have no label-typed defaults".to_owned())
        })
        .expect("value option defaults convert")
    }
}

/// The modelled option named `name` (its canonical name only): a typed
/// option of [`OPTIONS`] or a table-driven [`VALUE_OPTIONS`] row.
pub fn option(name: &str) -> Option<&'static OptionDef> {
    OPTIONS
        .iter()
        .find(|option| option.name == name)
        .or_else(|| value_option(name).map(|option| &option.option))
}

/// The names of every modelled option, typed then table-driven, for
/// explicit "not implemented" errors.
pub fn modelled_option_names() -> impl Iterator<Item = &'static str> {
    OPTIONS
        .iter()
        .map(|option| option.name)
        .chain(VALUE_OPTIONS.iter().map(|option| option.option.name))
}

/// BuildOptionDetails.getCanonicalName (BuildOptionDetails.java:174-176)
/// over every Bazel 9.3 fragment option (crate::bazel_options).
pub fn canonical_name(name: &str) -> &str {
    crate::bazel_options::FRAGMENT_OPTIONS
        .iter()
        .find(|(_, old, _, _)| !old.is_empty() && *old == name)
        .map_or(name, |(canonical, _, _, _)| canonical)
}

/// Whether `name` (exactly, not an old name) is a Bazel 9.3 fragment option
/// (BuildOptionDetails.getOptionClass, :140-143), and whether it is
/// NON_CONFIGURABLE (BuildOptionDetails.isNonConfigurable, :188-202).
pub fn fragment_option(name: &str) -> Option<bool> {
    crate::bazel_options::FRAGMENT_OPTIONS
        .iter()
        .find(|(option, _, _, _)| *option == name)
        .map(|(_, _, non_configurable, _)| *non_configurable)
}

/// SpellChecker.suggest (net/starlark/java/spelling/SpellChecker.java:83-96),
/// the vendored interpreter's port: whether some word is within
/// `min(5, (len + 1) / 2)` case-insensitive edits.
fn has_suggestion(input: &str, words: impl IntoIterator<Item = String>) -> bool {
    let words: Vec<String> = words.into_iter().collect();
    starlark::errors::bazel_suggest(input, words.iter().map(String::as_str)).is_some()
}

/// OptionsParserImpl.identifyOptionAndPossibleArgument for a `--` argument
/// that is not a native option of this profile
/// (common/options/OptionsParserImpl.java:776-824): `ARG :: Unrecognized
/// option: ARG` when the name (also without a `no` prefix) is not the name or
/// old name of any `@Option` of Bazel 9.3 and SpellChecker would suggest
/// nothing even among every option (`--NAME`, `--noNAME`; a superset of the
/// command's valid arguments). `None` otherwise: the caller keeps its
/// explicit "unsupported option" error.
pub fn unrecognized_option(arg: &str) -> Option<String> {
    let rest = arg.strip_prefix("--")?;
    let name = rest.split_once('=').map_or(rest, |(name, _)| name);
    if name.trim().is_empty() {
        return None;
    }
    let known = |name: &str| crate::bazel_options::ALL_OPTION_NAMES.contains(&name);
    if known(name) || name.strip_prefix("no").is_some_and(known) {
        return None;
    }
    let words = crate::bazel_options::ALL_OPTION_NAMES
        .iter()
        .flat_map(|name| [format!("--{name}"), format!("--no{name}")]);
    if has_suggestion(arg, words) {
        return None;
    }
    Some(invalid_argument(arg, format!("Unrecognized option: {arg}")))
}

/// SpellChecker.didYouMean(arg, getAllValidArgs()) of the `query` command's
/// options parser (OptionsParserImpl.java:812-824, 865-879): every
/// non-INTERNAL `query` option as `--NAME`, boolean-syntax ones also as
/// `--noNAME` (crate::command_options, genuine `help flags-as-proto`).
/// Candidates are in name order where Bazel iterates its option classes,
/// which can only matter for equally distant candidates.
fn query_did_you_mean(arg: &str) -> String {
    const QUERY: u8 = 2;
    let words = crate::command_options::COMMAND_OPTIONS
        .iter()
        .filter(|(_, _, _, _, _, commands)| commands & QUERY != 0)
        .flat_map(|(name, _, _, boolean, _, _)| {
            let mut forms = vec![format!("--{name}")];
            if *boolean {
                forms.push(format!("--no{name}"));
            }
            forms
        })
        .collect::<Vec<_>>();
    starlark::errors::bazel_did_you_mean(arg, words.iter().map(String::as_str))
}

/// A boolean option outside the modelled configuration (a Starlark
/// semantics option such as `--incompatible_disable_starlark_host_transitions`)
/// in OptionsParserImpl's forms (common/options/OptionsParserImpl.java:756-850):
/// `--NAME`, `--noNAME`, `--NAME=V` with BooleanConverter; `None` when `arg`
/// is not this option.
pub fn boolean_option(arg: &str, name: &str) -> Option<Result<bool, String>> {
    let rest = arg.strip_prefix("--")?;
    let (given, value) = match rest.split_once('=') {
        Some((given, value)) => (given, Some(value)),
        None => (rest, None),
    };
    if given == name {
        return Some(match value {
            None => Ok(true),
            Some(value) => match value.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "t" | "y" => Ok(true),
                "false" | "0" | "no" | "f" | "n" => Ok(false),
                lower => Err(format!(
                    "While parsing option {arg}: '{lower}' is not a boolean"
                )),
            },
        });
    }
    if given.strip_prefix("no") == Some(name) {
        return Some(match value {
            None => Ok(false),
            Some(_) => Err(invalid_argument(
                arg,
                format!("Unexpected value after boolean option: {arg}"),
            )),
        });
    }
    None
}

/// A converted option value, compared like the Java objects Bazel stores.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OptionValue {
    Bool(bool),
    Enum(&'static str),
    /// `List<Map.Entry<String, String>>`.
    Assignments(Vec<(String, String)>),
    Labels(Vec<Label>),
    Label(Label),
    String(String),
    /// A DottedVersion.Option (its string representation), `None` for null.
    DottedVersion(Option<String>),
}

impl fmt::Display for OptionValue {
    /// Java `toString` of the stored value (Boolean, the enum's mode string,
    /// `AbstractCollection.toString`, `Map.Entry` as `k=v`, Label.toString =
    /// getCanonicalForm, cmdline/Label.java:479-492).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool(value) => write!(f, "{value}"),
            Self::Enum(value) => f.write_str(value),
            Self::Assignments(entries) => {
                let items = entries
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>();
                write!(f, "[{}]", items.join(", "))
            }
            Self::Labels(labels) => {
                let items = labels
                    .iter()
                    .map(crate::rule_declarations::canonical_form)
                    .collect::<Vec<_>>();
                write!(f, "[{}]", items.join(", "))
            }
            Self::Label(label) => f.write_str(&crate::rule_declarations::canonical_form(label)),
            Self::String(value) => f.write_str(value),
            Self::DottedVersion(version) => f.write_str(version.as_deref().unwrap_or("null")),
        }
    }
}

/// DottedVersionConverter (rules/apple/DottedVersionConverter.java:21-36):
/// DottedVersion.fromString (DottedVersion.java:190-219) as a validity
/// check, through N25d2a's port (crate::apple_xcode::DottedVersion).
pub fn validate_dotted_version(version: &str) -> Result<(), String> {
    crate::apple_xcode::DottedVersion::parse(version).map(|_| ())
}

/// Resolve a label-typed option value (already `//`-prefixed per
/// CoreOptionConverters.convertOptionsLabel, :331-361).
pub type LabelParser<'a> = dyn Fn(&str) -> Result<Label, String> + 'a;

/// `Label.parseCanonical` (the null conversion context of
/// CoreOptionConverters.convertOptionsLabel, :345-350): an apparent `@name`
/// is taken as the canonical repository name.
pub fn parse_canonical_option_label(text: &str) -> Result<Label, String> {
    let canonical = if let Some(rest) = text.strip_prefix("@//") {
        format!("//{rest}")
    } else if text.starts_with('@') && !text.starts_with("@@") {
        format!("@{text}")
    } else {
        text.to_owned()
    };
    let canonical = match canonical.split_once("//") {
        // `@@repo` alone denotes `@@repo//:repo`.
        None if canonical.starts_with("@@") => {
            format!("{canonical}//:{}", &canonical[2..])
        }
        _ => canonical,
    };
    Label::parse_canonical(&canonical)
}

/// The option's converter (without the `While parsing option` wrapping of
/// ParsedOptionDescription.getConvertedValue, common/options/ParsedOptionDescription.java:204-213).
pub fn convert(
    option: &OptionDef,
    input: &str,
    labels: &LabelParser<'_>,
) -> Result<OptionValue, String> {
    // CoreOptionConverters.convertOptionsLabel (:339-344): prefix "//" unless
    // the input starts with '/' or '@'.
    let label = |text: &str| -> Result<Label, String> {
        if text.starts_with('/') || text.starts_with('@') {
            labels(text)
        } else {
            labels(&format!("//{text}"))
        }
    };
    match option.kind {
        OptionKind::Bool => {
            let lower = input.to_ascii_lowercase();
            if matches!(lower.as_str(), "true" | "1" | "yes" | "t" | "y") {
                Ok(OptionValue::Bool(true))
            } else if matches!(lower.as_str(), "false" | "0" | "no" | "f" | "n") {
                Ok(OptionValue::Bool(false))
            } else {
                Err(format!("'{lower}' is not a boolean"))
            }
        }
        // Ascii.equalsIgnoreCase against each constant's toString; the type
        // description is the list in ASCII lower case (EnumConverter.java:52-67).
        OptionKind::Enum { type_name, values } => values
            .iter()
            .find(|value| value.eq_ignore_ascii_case(input))
            .map(|value| OptionValue::Enum(value))
            .ok_or_else(|| {
                format!(
                    "Not a valid {type_name}: '{input}' (should be {})",
                    join_english_list(values).to_ascii_lowercase()
                )
            }),
        OptionKind::Assignments => match input.find('=') {
            Some(position) if position > 0 => Ok(OptionValue::Assignments(vec![(
                input[..position].to_owned(),
                input[position + 1..].to_owned(),
            )])),
            _ => Err(
                "Variable definitions must be in the form of a 'name=value' assignment".to_owned(),
            ),
        },
        // Splitter.on(",").omitEmptyStrings().
        OptionKind::LabelList => input
            .split(',')
            .filter(|part| !part.is_empty())
            .map(label)
            .collect::<Result<Vec<_>, _>>()
            .map(OptionValue::Labels),
        OptionKind::Label => label(if input.is_empty() {
            DEFAULT_HOST_PLATFORM
        } else {
            input
        })
        .map(OptionValue::Label),
        OptionKind::String => Ok(OptionValue::String(input.to_owned())),
        OptionKind::DottedVersion => validate_dotted_version(input)
            .map(|()| OptionValue::DottedVersion(Some(input.to_owned()))),
        OptionKind::OptionsLabel => label(input).map(OptionValue::Label),
        OptionKind::AutoCpu => Ok(OptionValue::String(if input.is_empty() {
            host_cpu().to_owned()
        } else {
            input.to_owned()
        })),
        OptionKind::LowerCaseString => Ok(OptionValue::String(input.to_ascii_lowercase())),
        OptionKind::TriState => {
            let lower = input.to_ascii_lowercase();
            if lower == "auto" {
                Ok(OptionValue::Enum("AUTO"))
            } else if matches!(lower.as_str(), "true" | "1" | "yes" | "t" | "y") {
                Ok(OptionValue::Enum("YES"))
            } else if matches!(lower.as_str(), "false" | "0" | "no" | "f" | "n") {
                Ok(OptionValue::Enum("NO"))
            } else {
                Err(format!("'{lower}' is not a boolean"))
            }
        }
    }
}

/// Converters.joinEnglishList (common/options/Converters.java:240-250).
fn join_english_list(values: &[&str]) -> String {
    let mut text = String::new();
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            text.push_str(if index + 1 < values.len() {
                ", "
            } else {
                " or "
            });
        }
        text.push_str(value);
    }
    if text.is_empty() {
        "nothing".to_owned()
    } else {
        text
    }
}

/// Values of the modelled native options.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct NativeOptions {
    pub compilation_mode: &'static str,
    pub stamp: bool,
    /// Last value per key wins, sorted by key (CoreOptions.getNormalized,
    /// CoreOptions.java:1245-1248; FragmentOptions.normalizeEntries, :100-113).
    pub define: Vec<(String, String)>,
    pub strip: &'static str,
    pub force_pic: bool,
    /// Only the first entry (PlatformOptions.getNormalized, :189-200); empty
    /// becomes the host platform (PlatformMappingValue.computeMapping,
    /// skyframe/config/PlatformMappingValue.java:154-158).
    #[serde(serialize_with = "serialize_labels")]
    pub platforms: Vec<Label>,
    #[serde(serialize_with = "serialize_label")]
    pub host_platform: Label,
    /// `--extra_toolchains` target patterns (PlatformOptions.java:98-116,
    /// CommaSeparatedOptionListConverter, repeatable), deduplicated keeping
    /// the last copy (PlatformOptions.getNormalized, :164-201). Omitted from
    /// the identity serialization when empty, so existing identities keep
    /// their value.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_toolchains: Vec<String>,
    /// `--host_compilation_mode` (CoreOptions.java:406-417, default `opt`):
    /// the compilation mode of the exec configuration
    /// (builtin_exec_platforms.bzl:221-229); omitted from the identity
    /// serialization at its default.
    #[serde(skip_serializing_if = "is_default_host_compilation_mode")]
    pub host_compilation_mode: &'static str,
    /// CoreOptions.isExec (`is exec configuration`, an internal option set
    /// by the exec transition, builtin_exec_platforms.bzl:224); omitted from
    /// the identity when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_exec: bool,
    /// CoreOptions.platformSuffix (`--platform_suffix`, default null; the
    /// exec transition sets `exec`, analysis/config/ExecutionTransitionFactory.java:229-258).
    /// Omitted from the identity when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_suffix: Option<&'static str>,
    /// `--use_target_platform_for_tests` (CoreOptions.java:1110-1123,
    /// default false): selects which `@bazel_tools//tools/test` toolchain
    /// the default test exec group resolves (docs/TEST_FLAGS.md). Not
    /// propagated by the exec transition (builtin_exec_platforms.bzl:231-275).
    /// Omitted from the identity when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub use_target_platform_for_tests: bool,
    /// `--incompatible_auto_exec_groups` (CoreOptions.java:419-433, default
    /// false): RuleContext.useAutoExecGroups of a rule class whose targets
    /// have no `_use_auto_exec_groups` attribute (RuleClass.AutoExecGroupsMode,
    /// packages/RuleClass.java:179-203; docs/EXEC_GROUPS.md). Propagated by
    /// the exec transition (builtin_exec_platforms.bzl:246). Omitted from the
    /// identity when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub incompatible_auto_exec_groups: bool,
    /// CoreOptions.cpu (`--cpu`, CoreOptions.java:279-292; AutoCpuConverter
    /// gives the host CPU for the default ""): `None` is the host CPU. Set
    /// on the command line or by a Starlark transition; the exec transition
    /// sets it to `--host_cpu` (builtin_exec_platforms.bzl:225). Omitted
    /// from the identity when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<String>,
    /// CoreOptions.hostCpu (`--host_cpu`, CoreOptions.java:387-394;
    /// AutoCpuConverter): `None` is the host CPU. Propagated by the exec
    /// transition (builtin_exec_platforms.bzl:241). Omitted from the
    /// identity when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_cpu: Option<String>,
    /// CppOptions.crosstoolTop (`--crosstool_top`, CppOptions.java:102-111;
    /// LabelConverter), a NO_OP option only `config_setting(values)` reads:
    /// `None` is the default [`DEFAULT_CROSSTOOL_TOP`]. Not propagated by
    /// the exec transition (builtin_exec_platforms.bzl:291-348), so exec
    /// configurations have the default. Omitted from the identity when
    /// unset.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_label"
    )]
    pub crosstool_top: Option<Label>,
    /// The table-driven [`VALUE_OPTIONS`] whose value differs from the
    /// default, by name ([`NativeOptions::value`] gives the default for the
    /// others). Serialized as `NAME: toString` and omitted from the
    /// identity when empty, so configurations at the defaults keep their
    /// identities (and action keys); a default spelled explicitly is not
    /// stored.
    #[serde(
        skip_serializing_if = "BTreeMap::is_empty",
        serialize_with = "serialize_values"
    )]
    pub values: BTreeMap<&'static str, OptionValue>,
    /// CoreOptions.collectCodeCoverage (`--collect_code_coverage`,
    /// analysis/config/CoreOptions.java:566-578, default false): a value
    /// only a Starlark transition sets natively (N47: rules_go's
    /// go_stdlib_transition writes it; the command-line option and the
    /// `coverage` command stay unsupported). Read by
    /// `ctx.configuration.coverage_enabled` (BuildConfigurationValue.isCodeCoverageEnabled,
    /// :730-733); instrumentation in such a configuration fails explicitly
    /// (docs/CONFIGURATION.md). Not propagated by the exec transition
    /// (builtin_exec_platforms.bzl:231-275). Omitted from the identity when
    /// false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub collect_code_coverage: bool,
    /// `--java_runtime_version`, `--java_language_version`,
    /// `--tool_java_runtime_version`, `--tool_java_language_version`
    /// (JavaOptions.java:373-403). Serialized separately
    /// ([`JavaVersions::identity`]): omitted from the identity at the values
    /// the configuration kind has by default, so existing identities
    /// (target and exec) are unchanged.
    #[serde(skip)]
    pub java: JavaVersions,
    /// The no-config configuration of NoConfigTransition
    /// (analysis/config/transitions/NoConfigTransition.java:52-55):
    /// CommonOptions.EMPTY_OPTIONS (analysis/config/CommonOptions.java:32-42),
    /// only CoreOptions at their defaults. The other modelled options keep
    /// their defaults here but do not exist in Bazel's configuration
    /// (selects in it fail explicitly, crate::engine). Omitted from the
    /// identity when false.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_config: bool,
    /// `--ios_sdk_version`, `--watchos_sdk_version`, `--tvos_sdk_version`,
    /// `--macos_sdk_version` (AppleCommandLineOptions.java:59-100, default
    /// null), in that order; propagated to exec configurations
    /// (builtin_exec_platforms.bzl:170-176). Omitted from the identity
    /// serialization while all are null.
    #[serde(skip_serializing_if = "AppleSdkVersions::is_unset")]
    pub apple_sdk_versions: AppleSdkVersions,
    /// `--action_env` (CoreOptions.java:525-545), normalized; the exec
    /// configuration's is the parent's `--host_action_env`
    /// (builtin_exec_platforms.bzl:227). Omitted from the identity when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub action_env: Vec<EnvVar>,
    /// `--host_action_env` (CoreOptions.java:547-564), normalized; propagates
    /// to exec configurations (builtin_exec_platforms.bzl:259).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub host_action_env: Vec<EnvVar>,
    /// `--incompatible_strict_action_env` (BazelRuleClassProvider.java:73-87,
    /// default true; propagates to exec, builtin_exec_platforms.bzl:203-207).
    /// Omitted from the identity when true.
    #[serde(skip_serializing_if = "is_true")]
    pub strict_action_env: bool,
    /// `--test_env` (TestConfiguration.java:90-107), normalized. TestOptions
    /// are trimmed from the modelled (non-test) configurations
    /// (`--trim_test_configuration`), so it is part of the untrimmed identity
    /// only (Configuration::untrimmed_id); recorded for a test command.
    #[serde(skip)]
    pub test_env: Vec<EnvVar>,
}

/// The JavaOptions version strings (N28b).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct JavaVersions {
    pub java_runtime_version: String,
    pub java_language_version: String,
    pub tool_java_runtime_version: String,
    pub tool_java_language_version: String,
}

impl Default for JavaVersions {
    fn default() -> Self {
        Self {
            java_runtime_version: DEFAULT_JAVA_RUNTIME_VERSION.to_owned(),
            java_language_version: String::new(),
            tool_java_runtime_version: DEFAULT_TOOL_JAVA_RUNTIME_VERSION.to_owned(),
            tool_java_language_version: String::new(),
        }
    }
}

impl JavaVersions {
    /// The versions in the identity serialization: `None` at the values a
    /// target configuration (the option defaults) or an exec configuration
    /// (the exec transition of the defaults: java_* = tool_java_*,
    /// builtin_exec_platforms.bzl:358-359) has by default. `is_exec` is
    /// itself part of the identity, so the serialization stays injective.
    fn identity(&self, is_exec: bool) -> Option<&Self> {
        let mut default = Self::default();
        if is_exec {
            default.java_runtime_version = default.tool_java_runtime_version.clone();
            default.java_language_version = default.tool_java_language_version.clone();
        }
        (*self != default).then_some(self)
    }
}

/// AutoCpuConverter.convert("") on Linux (analysis/config/AutoCpuConverter.java:30-62):
/// the default `--cpu` of this host.
pub fn host_cpu() -> &'static str {
    match std::env::consts::ARCH {
        "x86" => "piii",
        "x86_64" => "k8",
        "powerpc" | "powerpc64" => "ppc",
        "arm" => "arm",
        "aarch64" => "aarch64",
        "s390x" => "s390x",
        "mips64" => "mips64",
        "riscv64" => "riscv64",
        _ => "unknown",
    }
}

/// The four Apple SDK version options (DottedVersion.Option strings).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct AppleSdkVersions {
    pub ios: Option<String>,
    pub watchos: Option<String>,
    pub tvos: Option<String>,
    pub macos: Option<String>,
}

impl AppleSdkVersions {
    fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    fn slot(&mut self, name: &str) -> Option<&mut Option<String>> {
        Some(match name {
            "ios_sdk_version" => &mut self.ios,
            "watchos_sdk_version" => &mut self.watchos,
            "tvos_sdk_version" => &mut self.tvos,
            "macos_sdk_version" => &mut self.macos,
            _ => return None,
        })
    }

    /// The value of one `*_sdk_version` option.
    pub fn get(&self, name: &str) -> Option<&Option<String>> {
        Some(match name {
            "ios_sdk_version" => &self.ios,
            "watchos_sdk_version" => &self.watchos,
            "tvos_sdk_version" => &self.tvos,
            "macos_sdk_version" => &self.macos,
            _ => return None,
        })
    }
}

fn is_true(value: &bool) -> bool {
    *value
}

/// The default `--host_compilation_mode` (CoreOptions.java:406-417).
pub const DEFAULT_HOST_COMPILATION_MODE: &str = "opt";

fn is_default_host_compilation_mode(mode: &&'static str) -> bool {
    *mode == DEFAULT_HOST_COMPILATION_MODE
}

fn serialize_labels<S: serde::Serializer>(labels: &[Label], s: S) -> Result<S::Ok, S::Error> {
    s.collect_seq(labels.iter().map(crate::rule_declarations::canonical_form))
}

fn serialize_label<S: serde::Serializer>(label: &Label, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&crate::rule_declarations::canonical_form(label))
}

fn serialize_optional_label<S: serde::Serializer>(
    label: &Option<Label>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match label {
        Some(label) => serialize_label(label, s),
        None => s.serialize_none(),
    }
}

/// [`NativeOptions::values`] as `NAME: toString` (injective per option for
/// the value options' converters: booleans, enum constants, strings).
fn serialize_values<S: serde::Serializer>(
    values: &BTreeMap<&'static str, OptionValue>,
    s: S,
) -> Result<S::Ok, S::Error> {
    s.collect_map(
        values
            .iter()
            .map(|(name, value)| (*name, value.to_string())),
    )
}

/// The label of [`DEFAULT_HOST_PLATFORM`].
pub fn default_host_platform() -> Label {
    Label {
        repository: "bazel_tools".into(),
        package: "tools".into(),
        name: "host_platform".into(),
    }
}

impl Default for NativeOptions {
    fn default() -> Self {
        let host_platform = default_host_platform();
        Self {
            compilation_mode: "fastbuild",
            stamp: false,
            define: Vec::new(),
            strip: "sometimes",
            force_pic: false,
            platforms: vec![host_platform.clone()],
            host_platform,
            extra_toolchains: Vec::new(),
            host_compilation_mode: DEFAULT_HOST_COMPILATION_MODE,
            is_exec: false,
            platform_suffix: None,
            java: JavaVersions::default(),
            no_config: false,
            cpu: None,
            host_cpu: None,
            crosstool_top: None,
            values: BTreeMap::new(),
            collect_code_coverage: false,
            use_target_platform_for_tests: false,
            incompatible_auto_exec_groups: false,
            apple_sdk_versions: AppleSdkVersions::default(),
            action_env: Vec::new(),
            host_action_env: Vec::new(),
            strict_action_env: true,
            test_env: Vec::new(),
        }
    }
}

impl NativeOptions {
    /// PlatformConfiguration.getTargetPlatform: the first `--platforms`
    /// entry (PlatformOptions.computeTargetPlatform, PlatformOptions.java:205-212).
    pub fn target_platform(&self) -> &Label {
        self.platforms.first().unwrap_or(&self.host_platform)
    }

    /// The configuration's shell action environment
    /// (BuildConfigurationValue.getActionEnvironment, SHELL_ACTION_ENV;
    /// crate::action_env).
    pub fn action_environment(&self) -> ActionEnvironment {
        ActionEnvironment::shell(self.strict_action_env, &self.action_env)
    }

    /// The configuration's test environment
    /// (BuildConfigurationValue.setupTestEnvironment, :180-195).
    pub fn test_environment(&self) -> ActionEnvironment {
        ActionEnvironment::test(&self.test_env)
    }

    /// BuildOptionDetails.getOptionValue (BuildOptionDetails.java:145-158) of a
    /// modelled option.
    pub fn value(&self, name: &str) -> Option<OptionValue> {
        Some(match name {
            "compilation_mode" => OptionValue::Enum(self.compilation_mode),
            "host_compilation_mode" => OptionValue::Enum(self.host_compilation_mode),
            "stamp" => OptionValue::Bool(self.stamp),
            "define" => OptionValue::Assignments(self.define.clone()),
            "strip" => OptionValue::Enum(self.strip),
            "force_pic" => OptionValue::Bool(self.force_pic),
            "platforms" => OptionValue::Labels(self.platforms.clone()),
            "host_platform" => OptionValue::Label(self.host_platform.clone()),
            "use_target_platform_for_tests" => {
                OptionValue::Bool(self.use_target_platform_for_tests)
            }
            "incompatible_auto_exec_groups" => {
                OptionValue::Bool(self.incompatible_auto_exec_groups)
            }
            "java_runtime_version" => OptionValue::String(self.java.java_runtime_version.clone()),
            "java_language_version" => OptionValue::String(self.java.java_language_version.clone()),
            "tool_java_runtime_version" => {
                OptionValue::String(self.java.tool_java_runtime_version.clone())
            }
            "tool_java_language_version" => {
                OptionValue::String(self.java.tool_java_language_version.clone())
            }
            name if self.apple_sdk_versions.get(name).is_some() => {
                OptionValue::DottedVersion(self.apple_sdk_versions.get(name).cloned().flatten())
            }
            "cpu" => OptionValue::String(self.cpu().to_owned()),
            "host_cpu" => OptionValue::String(self.host_cpu().to_owned()),
            "crosstool_top" => OptionValue::Label(
                self.crosstool_top
                    .clone()
                    .unwrap_or_else(default_crosstool_top),
            ),
            // The typed field a Starlark transition sets (N47).
            "collect_code_coverage" => OptionValue::Bool(self.collect_code_coverage),
            name => {
                let option = value_option(name)?;
                self.values
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| option.default_value())
            }
        })
    }

    /// Records a table-driven option's converted value: stored only when
    /// it differs from the default (the identity rule of [`Self::values`]).
    pub fn set_value(&mut self, option: &'static ValueOption, value: OptionValue) {
        if value == option.default_value() {
            self.values.remove(option.option.name);
        } else {
            self.values.insert(option.option.name, value);
        }
    }

    /// The table-driven values of the exec configuration of `self`
    /// ([`ExecValue`]): propagated values are kept, the others are the
    /// default or the transition's fixed value.
    pub fn exec_values(&self) -> BTreeMap<&'static str, OptionValue> {
        let mut exec = NativeOptions::default();
        for option in VALUE_OPTIONS {
            match option.exec {
                ExecValue::Propagate => {
                    if let Some(value) = self.values.get(option.option.name) {
                        exec.set_value(option, value.clone());
                    }
                }
                ExecValue::Default => {}
                ExecValue::Set(raw) => {
                    let value = convert(&option.option, raw, &|_| {
                        Err("value options have no label-typed exec values".to_owned())
                    })
                    .expect("exec values convert");
                    exec.set_value(option, value);
                }
            }
        }
        exec.values
    }

    /// CoreOptions.cpu after AutoCpuConverter: the host CPU when unset.
    pub fn cpu(&self) -> &str {
        self.cpu.as_deref().unwrap_or(host_cpu())
    }

    /// CoreOptions.hostCpu after AutoCpuConverter: the host CPU when unset.
    pub fn host_cpu(&self) -> &str {
        self.host_cpu.as_deref().unwrap_or(host_cpu())
    }
}

/// A build setting value, compared like the Java objects Bazel stores
/// (Boolean, StarlarkInt, String, List<String>, Label).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum SettingValue {
    Bool(bool),
    Int(i64),
    String(String),
    /// A converted `List<String>` (the command line's and the default's
    /// ImmutableList).
    StringList(Vec<String>),
    Label(#[serde(serialize_with = "serialize_label")] Label),
    /// The raw Starlark list (`tuple: false`) or tuple a Starlark transition
    /// returned for a string-list setting (N47). Bazel keeps the returned
    /// object in BuildOptions (FunctionTransitionUtil.applyTransition,
    /// analysis/starlark/FunctionTransitionUtil.java:619-659;
    /// StarlarkTransition.validate converts it only to compare it with the
    /// default, StarlarkTransition.java:225-274), so it is a different
    /// configuration than the equal converted list: its `toString` is the
    /// Starlark repr (StarlarkList.java:272-274, Tuple.java:139-141) in the
    /// options checksum and the ST- hash, and StarlarkList/Tuple.equals
    /// only accept their own class (StarlarkList.java:244-251,
    /// Tuple.java:129-135). rules_go's go_transition writes
    /// `//go/config:tags` this way.
    Sequence {
        tuple: bool,
        items: Vec<String>,
    },
}

impl SettingValue {
    /// The elements of a string-list value, converted or raw.
    pub fn string_items(&self) -> Option<&[String]> {
        match self {
            Self::StringList(items) | Self::Sequence { items, .. } => Some(items),
            _ => None,
        }
    }

    /// Java `first.equals(second)` of the stored objects: a converted list
    /// (ImmutableList) equals any List with equal elements, a raw
    /// StarlarkList or Tuple only its own class (StarlarkList.java:244-251,
    /// Tuple.java:129-135).
    pub fn java_equals(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::StringList(items), _) => other.string_items() == Some(items.as_slice()),
            _ => self == other,
        }
    }
}

/// Printer.appendQuoted (net/starlark/java/eval/Printer.java:225-259).
fn starlark_quoted(text: &str, out: &mut String) {
    out.push('"');
    // Only characters below U+0020 are escaped numerically; they are single
    // UTF-16 units, so iterating scalar values is equivalent.
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 32 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

impl fmt::Display for SettingValue {
    /// Java `toString` of the stored value (Boolean, StarlarkInt,
    /// AbstractCollection.toString, Label.toString = getCanonicalForm, the
    /// Starlark repr of a raw StarlarkList/Tuple).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool(value) => write!(f, "{value}"),
            Self::Int(value) => write!(f, "{value}"),
            Self::String(value) => f.write_str(value),
            Self::StringList(values) => write!(f, "[{}]", values.join(", ")),
            Self::Label(label) => f.write_str(&crate::rule_declarations::canonical_form(label)),
            Self::Sequence { tuple, items } => {
                let quoted = items
                    .iter()
                    .map(|item| {
                        let mut out = String::new();
                        starlark_quoted(item, &mut out);
                        out
                    })
                    .collect::<Vec<_>>();
                match (tuple, quoted.as_slice()) {
                    (false, _) => write!(f, "[{}]", quoted.join(", ")),
                    (true, [single]) => write!(f, "({single},)"),
                    (true, _) => write!(f, "({})", quoted.join(", ")),
                }
            }
        }
    }
}

/// SHA-256 over the canonical serialization of a configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConfigId([u8; 32]);

impl fmt::Display for ConfigId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// One build configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Configuration {
    pub id: ConfigId,
    pub native: NativeOptions,
    /// Starlark build settings that differ from their default.
    pub starlark: BTreeMap<Label, SettingValue>,
}

impl Default for Configuration {
    fn default() -> Self {
        Self::new(NativeOptions::default(), BTreeMap::new())
    }
}

impl Configuration {
    pub fn new(native: NativeOptions, starlark: BTreeMap<Label, SettingValue>) -> Self {
        let id =
            ConfigId(Sha256::digest(canonical_serialization(&native, &starlark, false)).into());
        Self {
            id,
            native,
            starlark,
        }
    }

    /// The identity of this configuration before Bazel's test trimming: a
    /// top-level target is requested in the configuration with the
    /// TestOptions fragment (not modelled natively), and a non-test,
    /// non-alias rule's TestTrimmingTransition removes it
    /// (analysis/test/TestTrimmingTransitionFactory.java:92-135), so a cycle
    /// path through a requested rule lists both keys
    /// (crate::engine::Engine::cycle_error). The modelled configurations are
    /// the trimmed ones.
    pub fn untrimmed_id(&self) -> ConfigId {
        ConfigId(Sha256::digest(canonical_serialization(&self.native, &self.starlark, true)).into())
    }
}

fn canonical_serialization(
    native: &NativeOptions,
    starlark: &BTreeMap<Label, SettingValue>,
    test_options: bool,
) -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct Canonical<'a> {
        version: u32,
        native: &'a NativeOptions,
        /// The JavaOptions versions when not at their defaults (N28b).
        #[serde(skip_serializing_if = "Option::is_none")]
        java: Option<&'a JavaVersions>,
        starlark: Vec<(String, &'a SettingValue)>,
        /// The (unmodelled) TestOptions fragment of an untrimmed
        /// configuration; omitted otherwise, so identities are unchanged.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        test_options: bool,
        /// `--test_env` of the untrimmed TestOptions; omitted when empty.
        #[serde(skip_serializing_if = "<[EnvVar]>::is_empty")]
        test_env: &'a [EnvVar],
    }
    serde_json::to_vec(&Canonical {
        version: 1,
        native,
        java: native.java.identity(native.is_exec),
        test_options,
        test_env: if test_options { &native.test_env } else { &[] },
        starlark: starlark
            .iter()
            .map(|(label, value)| (crate::rule_declarations::canonical_form(label), value))
            .collect(),
    })
    .expect("configuration serialization")
}

/// Command-line options of one invocation, in command-line order, converted
/// once the main repository mapping is known (the second parsing round of
/// CoreOptionConverters.convertOptionsLabel, :351-357).
#[derive(Clone, Debug, Default)]
pub struct CommandLineOptions {
    /// (option, command-line form, unconverted value).
    entries: Vec<(&'static OptionDef, String, String)>,
    /// Starlark build setting arguments (skipped by the native parser), in
    /// command-line order.
    starlark: Vec<String>,
    /// `query` options: only CoreOptions (runtime/commands/QueryCommand.java:59-67,
    /// no usesConfigurationOptions, runtime/BlazeCommandUtils.java:74-98).
    query: bool,
    /// `--extra_toolchains` values in command-line order.
    extra_toolchains: Vec<String>,
    /// OptionsParserImpl warnings (deprecated options), each once, in first
    /// occurrence order (OptionsParserImpl.java:191, 336-369).
    warnings: Vec<String>,
    /// `--action_env`/`--host_action_env`/`--test_env` entries
    /// (option name, converted entry) in command-line order.
    env_vars: Vec<(&'static str, EnvVar)>,
    /// The last `--incompatible_strict_action_env` value.
    strict_action_env: Option<bool>,
}

/// The repeatable EnvVarsConverter options (docs/ACTION_ENV.md): name and
/// whether `query` registers it (CoreOptions only, QueryCommand.java:59-67).
const ENV_VAR_OPTIONS: [(&str, bool); 3] = [
    // CoreOptions.java:525-545
    ("action_env", true),
    // CoreOptions.java:547-564
    ("host_action_env", true),
    // analysis/test/TestConfiguration.java:90-107
    ("test_env", false),
];

/// `--incompatible_strict_action_env` and its old name
/// (BazelRuleClassProvider.StrictActionEnvOptions, :73-87).
const STRICT_ACTION_ENV: [&str; 2] = [
    "incompatible_strict_action_env",
    "experimental_strict_action_env",
];

/// OptionsParsingException with an invalid argument: BlazeOptionHandler prints
/// `ARG :: MESSAGE` (runtime/BlazeOptionHandler.java:104, 628-637).
fn invalid_argument(arg: &str, message: String) -> String {
    format!("{arg} :: {message}")
}

/// Result of [`CommandLineOptions::parse`] for one argument.
pub enum Parsed {
    /// Not a modelled configuration option.
    NotModelled,
    /// Consumed; the next argument was consumed too when `true`.
    Consumed(bool),
}

/// OptionsParser.STARLARK_SKIPPED_PREFIXES (common/options/OptionsParser.java:111-112):
/// the native parser skips these arguments; StarlarkOptionsParser parses them
/// for commands that analyze (runtime/BlazeOptionHandler.java:417-423).
pub const STARLARK_SKIPPED_PREFIXES: [&str; 4] = ["--//", "--no//", "--@", "--no@"];

impl CommandLineOptions {
    /// Options of the `query` command: the modelled CoreOptions
    /// (`--compilation_mode`, `--stamp`, `--define`) are accepted and validated
    /// but do not affect loading; the others remain unsupported there.
    pub fn for_query() -> Self {
        Self {
            query: true,
            ..Self::default()
        }
    }

    /// OptionsParserImpl.identifyOptionAndPossibleArgument
    /// (common/options/OptionsParserImpl.java:756-850) for the modelled options:
    /// `--name=value`, `--name value`, `-c value`, `--name`/`--noname` for
    /// booleans. Errors are Bazel's OptionsParsingException messages.
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Result<Parsed, String> {
        let query = self.query;
        let available =
            move |option: &&OptionDef| !query || option.source.contains("/CoreOptions.java");
        // OptionsParserImpl.java:515-525: a likely negative target pattern or a
        // mistyped Starlark option (no invalid-argument prefix).
        if arg.starts_with("-//") || arg.starts_with("-@") {
            return Err(format!(
                "Invalid options syntax: {arg}\nNote: Negative target patterns can only appear after the end of options marker ('--'). Flags corresponding to Starlark-defined build settings always start with '--', not '-'."
            ));
        }
        // OptionsParserImpl.java:535-550: skipped Starlark options keep their
        // order; `query` never parses them (it does not analyze).
        if STARLARK_SKIPPED_PREFIXES
            .iter()
            .any(|prefix| arg.starts_with(prefix))
        {
            if !query {
                self.starlark.push(arg.to_owned());
            }
            return Ok(Parsed::Consumed(false));
        }
        // OptionsParserImpl.java:766-808: `-x` (abbreviation), `-x-`, `--name[=value]`;
        // anything else starting with '-' is invalid syntax.
        if arg.starts_with('-')
            && !arg.starts_with("--")
            && arg.len() != 2
            && !(arg.len() == 3 && arg.ends_with('-'))
        {
            return Err(invalid_argument(
                arg,
                format!("Invalid options syntax: {arg}"),
            ));
        }
        let (option, value, form, used_next) = if let Some(rest) = arg.strip_prefix("--") {
            let (name, value) = match rest.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (rest, None),
            };
            if value.is_some() && name.trim().is_empty() {
                return Err(invalid_argument(
                    arg,
                    format!("Invalid options syntax: {arg}"),
                ));
            }
            // Table-driven options that are not command-line options are
            // still `query` options when they are CoreOptions: query
            // validates and ignores them (it never computes a
            // configuration).
            let lookup = |name: &str| {
                OPTIONS
                    .iter()
                    .chain(
                        VALUE_OPTIONS
                            .iter()
                            .filter(|option| option.command_line || query)
                            .map(|option| &option.option),
                    )
                    .filter(available)
                    .find(|option| option.name == name || option.old_name == Some(name))
            };
            // `query` registers only CoreOptions (QueryCommand.java:59-67):
            // OptionsParserImpl reports a table-driven option of another
            // fragment as unknown, with SpellChecker's suggestion among the
            // command's valid arguments (OptionsParserImpl.java:812-824;
            // paired capture value_options/query_*).
            if query {
                let other_fragment = |name: &str| {
                    VALUE_OPTIONS.iter().any(|option| {
                        (option.option.name == name || option.option.old_name == Some(name))
                            && !option.option.source.contains("/CoreOptions.java")
                    })
                };
                if other_fragment(name) || name.strip_prefix("no").is_some_and(other_fragment) {
                    return Err(invalid_argument(
                        arg,
                        format!("Unrecognized option: {arg}{}", query_did_you_mean(arg)),
                    ));
                }
            }
            // `query` registers no PlatformOptions (QueryCommand.java:59-67):
            // OptionsParserImpl reports them as unknown (paired capture
            // B1/query_platforms_mac: `--platforms=//:mac :: Unrecognized
            // option: --platforms=//:mac`). Nor JavaOptions (paired capture
            // N28 A2/query_jrv: `--java_runtime_version=remotejdk_11 ::
            // Unrecognized option: --java_runtime_version=remotejdk_11`), nor
            // CppOptions' `--crosstool_top` (tests/fixtures/pre_platforms_flags
            // query_crosstool_top).
            const JAVA_VERSIONS: [&str; 4] = [
                "java_runtime_version",
                "java_language_version",
                "tool_java_runtime_version",
                "tool_java_language_version",
            ];
            if query
                && ([
                    "platforms",
                    "experimental_platforms",
                    "host_platform",
                    "experimental_host_platform",
                    "extra_toolchains",
                    "crosstool_top",
                ]
                .contains(&name)
                    || JAVA_VERSIONS.contains(&name)
                    || name
                        .strip_prefix("no")
                        .is_some_and(|name| JAVA_VERSIONS.contains(&name)))
            {
                return Err(invalid_argument(arg, format!("Unrecognized option: {arg}")));
            }
            // `query` registers neither TestOptions nor StrictActionEnvOptions.
            if query
                && (name == "test_env"
                    || STRICT_ACTION_ENV
                        .iter()
                        .any(|option| name == *option || name.strip_prefix("no") == Some(option)))
            {
                return Err(invalid_argument(arg, format!("Unrecognized option: {arg}")));
            }
            if let Some((option, _)) = ENV_VAR_OPTIONS
                .iter()
                .find(|(option, _)| name == *option || name.strip_prefix("no") == Some(option))
            {
                if name != *option {
                    return Err(invalid_argument(
                        arg,
                        format!("Illegal use of 'no' prefix on non-boolean option: {arg}"),
                    ));
                }
                let (value, form, used_next) = match value {
                    Some(value) => (value.to_owned(), arg.to_owned(), false),
                    None => {
                        let value = next.ok_or_else(|| format!("Expected value after {arg}"))?;
                        (value.to_owned(), format!("{arg} {value}"), true)
                    }
                };
                let entry = EnvVar::parse(&value)
                    .map_err(|error| format!("While parsing option {form}: {error}"))?;
                self.env_vars.push((option, entry));
                return Ok(Parsed::Consumed(used_next));
            }
            if let Some(result) = STRICT_ACTION_ENV
                .iter()
                .find_map(|option| boolean_option(arg, option))
            {
                self.strict_action_env = Some(result?);
                return Ok(Parsed::Consumed(false));
            }
            // --extra_toolchains (PlatformOptions.java:98-116): a repeatable
            // comma-separated list of target patterns (empty values kept,
            // an empty input is no value; Converters.java:253-287).
            if name == "extra_toolchains" || name == "noextra_toolchains" {
                if name == "noextra_toolchains" {
                    return Err(invalid_argument(
                        arg,
                        format!("Illegal use of 'no' prefix on non-boolean option: {arg}"),
                    ));
                }
                let (value, used_next) = match value {
                    Some(value) => (value.to_owned(), false),
                    None => (
                        next.ok_or_else(|| format!("Expected value after {arg}"))?
                            .to_owned(),
                        true,
                    ),
                };
                if !value.is_empty() {
                    self.extra_toolchains
                        .extend(value.split(',').map(str::to_owned));
                }
                return Ok(Parsed::Consumed(used_next));
            }
            let (option, negated) = match lookup(name) {
                Some(option) => (option, false),
                None => match name.strip_prefix("no").and_then(lookup) {
                    Some(option) => (option, true),
                    None => return Ok(Parsed::NotModelled),
                },
            };
            if negated {
                if !option.kind.boolean_syntax() {
                    return Err(invalid_argument(
                        arg,
                        format!("Illegal use of 'no' prefix on non-boolean option: {arg}"),
                    ));
                }
                if value.is_some() {
                    return Err(invalid_argument(
                        arg,
                        format!("Unexpected value after boolean option: {arg}"),
                    ));
                }
                (option, "0".to_owned(), arg.to_owned(), false)
            } else if let Some(value) = value {
                (option, value.to_owned(), arg.to_owned(), false)
            } else if option.kind.boolean_syntax() {
                (option, "1".to_owned(), arg.to_owned(), false)
            } else {
                let value = next.ok_or_else(|| format!("Expected value after {arg}"))?;
                (option, value.to_owned(), format!("{arg} {value}"), true)
            }
        } else if arg.len() == 2
            && let Some(option) = OPTIONS
                .iter()
                .filter(available)
                .find(|option| option.abbrev.is_some_and(|c| arg == format!("-{c}")))
        {
            let value = next.ok_or_else(|| format!("Expected value after {arg}"))?;
            (option, value.to_owned(), format!("{arg} {value}"), true)
        } else {
            return Ok(Parsed::NotModelled);
        };
        // Non-label values convert in the first parsing round already.
        if !matches!(
            option.kind,
            OptionKind::LabelList | OptionKind::Label | OptionKind::OptionsLabel
        ) {
            convert(option, &value, &|_| unreachable!("no labels"))
                .map_err(|error| format!("While parsing option {form}: {error}"))?;
        }
        self.entries.push((option, form, value));
        if let Some(warning) = option.deprecation {
            let warning = format!("Option '{}' is deprecated: {warning}", option.name);
            if !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }
        Ok(Parsed::Consumed(used_next))
    }

    /// The parser's warnings, printed as `WARNING: ...` once the native
    /// options parsed (runtime/BlazeOptionHandler.java:536-539).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The Starlark build setting arguments, in command-line order
    /// (OptionsParserImpl.getSkippedArgs), for crate::starlark_options.
    pub fn starlark_args(&self) -> &[String] {
        &self.starlark
    }

    /// The configuration of the native options (without Starlark build
    /// settings, which need package loading: crate::starlark_options).
    /// `labels` resolves label options with the main repository mapping.
    pub fn configuration(&self, labels: &LabelParser<'_>) -> Result<Configuration, String> {
        let mut native = NativeOptions::default();
        let mut platforms = Vec::new();
        let mut defines = Vec::new();
        // Label.parseWithRepoContext keeps an apparent repository the main
        // repository does not see as `@@[unknown repo 'R' requested from @@]`
        // (cmdline/RepositoryName.java); the platform lookup then fails with
        // BuildFileNotFoundException (crate::engine, paired capture
        // B2/platforms*_nope_p).
        let platform_labels = |text: &str| {
            labels(text).or_else(|error| deferred_unknown_repository(text).ok_or(error))
        };
        for (option, form, value) in &self.entries {
            // `--crosstool_top` keeps an unresolvable repository too: the
            // NO_OP option is never resolved (paired capture
            // pre_platforms_flags/crosstool_flag_unknown_repo succeeds).
            let parser: &LabelParser<'_> =
                if matches!(option.name, "platforms" | "host_platform" | "crosstool_top") {
                    &platform_labels
                } else {
                    labels
                };
            let converted = convert(option, value, parser)
                .map_err(|error| format!("While parsing option {form}: {error}"))?;
            match (option.name, converted) {
                ("compilation_mode", OptionValue::Enum(mode)) => native.compilation_mode = mode,
                ("host_compilation_mode", OptionValue::Enum(mode)) => {
                    native.host_compilation_mode = mode
                }
                ("strip", OptionValue::Enum(mode)) => native.strip = mode,
                ("stamp", OptionValue::Bool(value)) => native.stamp = value,
                ("force_pic", OptionValue::Bool(value)) => native.force_pic = value,
                ("define", OptionValue::Assignments(entries)) => defines.extend(entries),
                ("platforms", OptionValue::Labels(labels)) => platforms = labels,
                ("host_platform", OptionValue::Label(label)) => native.host_platform = label,
                ("use_target_platform_for_tests", OptionValue::Bool(value)) => {
                    native.use_target_platform_for_tests = value
                }
                ("incompatible_auto_exec_groups", OptionValue::Bool(value)) => {
                    native.incompatible_auto_exec_groups = value
                }
                ("java_runtime_version", OptionValue::String(value)) => {
                    native.java.java_runtime_version = value
                }
                ("java_language_version", OptionValue::String(value)) => {
                    native.java.java_language_version = value
                }
                ("tool_java_runtime_version", OptionValue::String(value)) => {
                    native.java.tool_java_runtime_version = value
                }
                ("tool_java_language_version", OptionValue::String(value)) => {
                    native.java.tool_java_language_version = value
                }
                (name, OptionValue::DottedVersion(version)) => {
                    *native
                        .apple_sdk_versions
                        .slot(name)
                        .expect("an Apple SDK version option") = version;
                }
                // The host CPU and the default label are the unset values,
                // so spelling a default keeps the default configuration.
                ("cpu", OptionValue::String(value)) => {
                    native.cpu = (value != host_cpu()).then_some(value)
                }
                ("host_cpu", OptionValue::String(value)) => {
                    native.host_cpu = (value != host_cpu()).then_some(value)
                }
                ("crosstool_top", OptionValue::Label(label)) => {
                    native.crosstool_top = (label != default_crosstool_top()).then_some(label)
                }
                // Table-driven options: the last occurrence wins; a default
                // spelled explicitly is not stored.
                (name, value) if value_option(name).is_some() => {
                    native.set_value(value_option(name).expect("a value option"), value)
                }
                _ => unreachable!("converter shape of {}", option.name),
            }
        }
        native.define = normalize_defines(defines);
        // dedupeKeepingLast (PlatformOptions.java:164-185).
        let mut extra_toolchains: Vec<String> = Vec::new();
        for pattern in self.extra_toolchains.iter().rev() {
            if !extra_toolchains.contains(pattern) {
                extra_toolchains.push(pattern.clone());
            }
        }
        extra_toolchains.reverse();
        native.extra_toolchains = extra_toolchains;
        // CoreOptions/TestOptions.getNormalized (CoreOptions.java:1253-1254,
        // TestConfiguration.java:409; FragmentOptions.normalizeEnvVars).
        let entries = |name: &str| {
            crate::action_env::normalize(
                self.env_vars
                    .iter()
                    .filter(|(option, _)| *option == name)
                    .map(|(_, entry)| entry.clone())
                    .collect(),
            )
        };
        native.action_env = entries("action_env");
        native.host_action_env = entries("host_action_env");
        native.test_env = entries("test_env");
        if let Some(strict) = self.strict_action_env {
            native.strict_action_env = strict;
        }
        // PlatformOptions.getNormalized keeps only the first entry (:189-200);
        // an empty list becomes [--host_platform] (PlatformMappingValue.computeMapping,
        // skyframe/config/PlatformMappingValue.java:154-158). The platforms are
        // resolved and validated as platform targets by crate::engine
        // (docs/PLATFORMS.md); the default host platform spelled explicitly is
        // the same configuration as an unset option.
        platforms.truncate(1);
        native.platforms = if platforms.is_empty() {
            vec![native.host_platform.clone()]
        } else {
            platforms
        };
        Ok(Configuration::new(native, BTreeMap::new()))
    }
}

/// `@R//pkg:name` with an apparent repository R as an unresolvable label of
/// the main repository (`[unknown repo 'R' requested from @@]`,
/// crate::label::deferred_label). Option values are converted without a
/// RepositorySet, so no did-you-mean suffix is computed here.
pub(crate) fn deferred_unknown_repository(text: &str) -> Option<Label> {
    crate::label::deferred_label(text, "", "")
}

/// The apparent repository name of an unresolvable label
/// (crate::label::unresolvable_repository).
pub fn unknown_repository_name(label: &Label) -> Option<&str> {
    crate::label::unknown_repository_name(&label.repository)
}

/// FragmentOptions.normalizeEntries then sortEntries (CoreOptions.java:1199-1208).
fn normalize_defines(entries: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut normalized: Vec<(String, String)> = Vec::new();
    for (key, value) in entries {
        match normalized.iter_mut().find(|(seen, _)| *seen == key) {
            Some(entry) => entry.1 = value,
            None => normalized.push((key, value)),
        }
    }
    normalized.sort_by(|a, b| crate::native_rules::java_string_cmp(&a.0, &b.0));
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical(text: &str) -> Result<Label, String> {
        parse_canonical_option_label(text)
    }

    fn parsed(args: &[&str]) -> Result<Configuration, String> {
        let mut options = CommandLineOptions::default();
        let mut index = 0;
        while index < args.len() {
            match options.parse(args[index], args.get(index + 1).copied())? {
                Parsed::NotModelled => panic!("not modelled: {}", args[index]),
                Parsed::Consumed(next) => index += 1 + usize::from(next),
            }
        }
        options.configuration(&canonical)
    }

    #[test]
    fn converters_follow_bazel() {
        let stamp = option("stamp").unwrap();
        for (input, expected) in [("TRUE", true), ("yes", true), ("1", true), ("N", false)] {
            assert_eq!(
                convert(stamp, input, &canonical),
                Ok(OptionValue::Bool(expected))
            );
        }
        assert_eq!(
            convert(stamp, "Maybe", &canonical),
            Err("'maybe' is not a boolean".into())
        );
        let mode = option("compilation_mode").unwrap();
        assert_eq!(
            convert(mode, "OPT", &canonical),
            Ok(OptionValue::Enum("opt"))
        );
        assert_eq!(
            convert(mode, "fast", &canonical),
            Err("Not a valid compilation mode: 'fast' (should be fastbuild, dbg or opt)".into())
        );
        assert_eq!(
            convert(option("strip").unwrap(), "x", &canonical),
            Err("Not a valid strip mode: 'x' (should be always, sometimes or never)".into())
        );
        let define = option("define").unwrap();
        assert_eq!(
            convert(define, "a=b=c", &canonical),
            Ok(OptionValue::Assignments(vec![("a".into(), "b=c".into())]))
        );
        for bad in ["=x", "x"] {
            assert_eq!(
                convert(define, bad, &canonical),
                Err("Variable definitions must be in the form of a 'name=value' assignment".into())
            );
        }
        let platforms = option("platforms").unwrap();
        assert_eq!(
            convert(platforms, "p:x,,@r//q:y", &canonical)
                .unwrap()
                .to_string(),
            "[//p:x, @@r//q:y]"
        );
        assert_eq!(
            convert(option("host_platform").unwrap(), "", &canonical)
                .unwrap()
                .to_string(),
            "@@bazel_tools//tools:host_platform"
        );
        assert_eq!(join_english_list(&[]), "nothing");
    }

    #[test]
    fn apple_sdk_versions_are_dotted_versions() {
        let macos = option("macos_sdk_version").unwrap();
        for valid in [
            "10.13",
            "10.13.0",
            "3.3alpha2.4",
            "1a2b",
            "17.0.beta",
            "1.2.experimental.x",
            "07",
        ] {
            assert_eq!(
                convert(macos, valid, &canonical),
                Ok(OptionValue::DottedVersion(Some(valid.to_owned()))),
                "{valid}"
            );
        }
        for invalid in [
            "",
            "x",
            "beta",
            "10.",
            ".1",
            "1_0",
            "1-2",
            "99999999999",
            "1a99999999999",
            "10..1",
        ] {
            assert_eq!(
                convert(macos, invalid, &canonical),
                Err(format!(
                    "Dotted version components must all start with the form \\d+([a-z0-9]*?)?(\\d+)? but got '{invalid}'"
                )),
                "{invalid}"
            );
        }
        let set = parsed(&["--macos_sdk_version=10.13", "--ios_sdk_version", "17.0"]).unwrap();
        assert_eq!(
            set.native.apple_sdk_versions.macos.as_deref(),
            Some("10.13")
        );
        assert_eq!(set.native.apple_sdk_versions.ios.as_deref(), Some("17.0"));
        assert_eq!(
            set.native.value("tvos_sdk_version"),
            Some(OptionValue::DottedVersion(None))
        );
        assert_eq!(OptionValue::DottedVersion(None).to_string(), "null");
        // Unset versions keep the identity of the default configuration.
        assert_eq!(parsed(&[]).unwrap().id, Configuration::default().id);
        assert_ne!(set.id, Configuration::default().id);
        // query has no AppleCommandLineOptions.
        let mut query = CommandLineOptions::for_query();
        assert!(matches!(
            query.parse("--macos_sdk_version=10.13", None),
            Ok(Parsed::NotModelled)
        ));
    }

    #[test]
    fn command_line_forms_and_normalization() {
        let configuration = parsed(&[
            "-c",
            "dbg",
            "--nostamp",
            "--stamp",
            "--define",
            "b=1",
            "--define=a=2",
            "--define=b=3",
            "--platforms=@bazel_tools//tools:host_platform,//p:two",
            "--strip=ALWAYS",
            "--force_pic=yes",
        ])
        .unwrap();
        let native = &configuration.native;
        assert_eq!(native.compilation_mode, "dbg");
        assert!(native.stamp && native.force_pic);
        assert_eq!(native.strip, "always");
        assert_eq!(
            native.define,
            [("a".to_owned(), "2".to_owned()), ("b".into(), "3".into())]
        );
        assert_eq!(native.platforms, [default_host_platform()]);
        assert_eq!(
            crate::config_registry::mnemonic(&configuration, &configuration),
            Ok("k8-dbg".to_owned())
        );
        let mut options = CommandLineOptions::default();
        for (args, error) in [
            (
                &["--nostamp=1"][..],
                "--nostamp=1 :: Unexpected value after boolean option: --nostamp=1",
            ),
            (
                &["--nocompilation_mode"][..],
                "--nocompilation_mode :: Illegal use of 'no' prefix on non-boolean option: --nocompilation_mode",
            ),
            (&["-c=opt"][..], "-c=opt :: Invalid options syntax: -c=opt"),
            (&["--=x"][..], "--=x :: Invalid options syntax: --=x"),
            (&["-c"][..], "Expected value after -c"),
            (
                &["--compilation_mode", "x"][..],
                "While parsing option --compilation_mode x: Not a valid compilation mode: 'x' (should be fastbuild, dbg or opt)",
            ),
            (
                &["--stamp=maybe"][..],
                "While parsing option --stamp=maybe: 'maybe' is not a boolean",
            ),
        ] {
            assert_eq!(
                options
                    .parse(args[0], args.get(1).copied())
                    .err()
                    .as_deref(),
                Some(error)
            );
        }
        assert!(matches!(
            options.parse("--copt=-O2", None),
            Ok(Parsed::NotModelled)
        ));
        assert!(matches!(options.parse("-k", None), Ok(Parsed::NotModelled)));
        let empty = parsed(&["--platforms="]).unwrap();
        assert_eq!(empty.native.platforms, [default_host_platform()]);
        assert_eq!(empty, Configuration::default());
        // The default host platform spelled explicitly is the default
        // configuration.
        for args in [
            &["--platforms=@bazel_tools//tools:host_platform"][..],
            &["--host_platform=@@bazel_tools//tools:host_platform"][..],
            &["--host_platform="][..],
            &["--platforms=//p:x", "--platforms="][..],
        ] {
            assert_eq!(parsed(args).unwrap(), Configuration::default(), "{args:?}");
        }
        // Other platforms are configurations of their own (validated as
        // platform targets by crate::engine); without --platforms the target
        // platform is --host_platform (PlatformMappingValue.java:154-158).
        let label = |text: &str| Label::parse_canonical(text).unwrap();
        for (args, target, host) in [
            (&["--platforms=//p:x"][..], "//p:x", DEFAULT_HOST_PLATFORM),
            (&["--host_platform", "//p:h"][..], "//p:h", "//p:h"),
            (
                &["--experimental_platforms=p:x,p:y", "--host_platform=//p:h"][..],
                "//p:x",
                "//p:h",
            ),
        ] {
            let configuration = parsed(args).unwrap();
            assert_eq!(configuration.native.platforms, [label(target)], "{args:?}");
            assert_eq!(*configuration.native.target_platform(), label(target));
            assert_eq!(
                configuration.native.host_platform,
                parse_canonical_option_label(host).unwrap()
            );
            assert_ne!(configuration.id, Configuration::default().id);
        }
        // An apparent repository the main repository does not see is kept
        // as an unresolvable label for the platform lookup.
        let mut options = CommandLineOptions::default();
        options.parse("--platforms=@nope//p:x", None).unwrap();
        let configuration = options
            .configuration(&|text| {
                if text.starts_with("@nope") {
                    Err("unknown apparent repository @nope".into())
                } else {
                    canonical(text)
                }
            })
            .unwrap();
        let target = configuration.native.target_platform();
        assert_eq!(unknown_repository_name(target), Some("nope"));
        assert_eq!(
            crate::rule_declarations::canonical_form(target),
            "@@[unknown repo 'nope' requested from @@]//p:x"
        );
        assert_eq!(deferred_unknown_repository("@@nope//:x"), None);
        // Starlark options are skipped by the native parser, in order
        // (OptionsParserImpl.java:535-550); `-//` and `-@` are invalid syntax.
        let mut options = CommandLineOptions::default();
        for arg in ["--//:a=1", "--no//p:b", "--@r//:c", "--no@r//:d"] {
            assert!(matches!(
                options.parse(arg, Some("next")),
                Ok(Parsed::Consumed(false))
            ));
        }
        assert_eq!(
            options.starlark_args(),
            ["--//:a=1", "--no//p:b", "--@r//:c", "--no@r//:d"]
        );
        for arg in ["-//:a=1", "-@r//:a"] {
            assert_eq!(
                options.parse(arg, None).err().unwrap(),
                format!(
                    "Invalid options syntax: {arg}\nNote: Negative target patterns can only appear after the end of options marker ('--'). Flags corresponding to Starlark-defined build settings always start with '--', not '-'."
                )
            );
        }
        // query accepts only the modelled CoreOptions.
        let mut query = CommandLineOptions::for_query();
        // ... and ignores Starlark options (it does not analyze).
        assert!(matches!(
            query.parse("--//:a=1", None),
            Ok(Parsed::Consumed(false))
        ));
        assert!(query.starlark_args().is_empty());
        assert!(matches!(
            query.parse("-c", Some("opt")),
            Ok(Parsed::Consumed(true))
        ));
        assert!(matches!(
            query.parse("--stamp", None),
            Ok(Parsed::Consumed(false))
        ));
        assert!(matches!(
            query.parse("--define=a=1", None),
            Ok(Parsed::Consumed(false))
        ));
        for arg in ["--strip=always", "--force_pic"] {
            assert!(
                matches!(query.parse(arg, None), Ok(Parsed::NotModelled)),
                "{arg}"
            );
        }
        // query has no PlatformOptions (QueryCommand.java:59-67).
        for arg in ["--platforms=//p:x", "--host_platform=//p:h", "--platforms"] {
            assert_eq!(
                query.parse(arg, Some("next")).err(),
                Some(format!("{arg} :: Unrecognized option: {arg}"))
            );
        }
    }

    #[test]
    fn config_id_is_stable_and_value_sensitive() {
        let default = Configuration::default();
        assert_eq!(default.id, Configuration::default().id);
        // Pinned identity of the default configuration (canonical serialization v1).
        assert_eq!(
            default.id.to_string(),
            format!(
                "{:x}",
                Sha256::digest(
                    br#"{"version":1,"native":{"compilation_mode":"fastbuild","stamp":false,"define":[],"strip":"sometimes","force_pic":false,"platforms":["@@bazel_tools//tools:host_platform"],"host_platform":"@@bazel_tools//tools:host_platform"},"starlark":[]}"#
                )
            )
        );
        let opt = parsed(&["-c", "opt"]).unwrap();
        let stamped = parsed(&["--stamp"]).unwrap();
        let defined = parsed(&["--define=a=1"]).unwrap();
        let ids = [default.id, opt.id, stamped.id, defined.id];
        for (index, id) in ids.iter().enumerate() {
            assert!(!ids[index + 1..].contains(id));
        }
        // Equal option values give equal identities regardless of spelling.
        assert_eq!(parsed(&["--compilation_mode=OPT"]).unwrap().id, opt.id);
        assert_eq!(
            parsed(&["--define=a=0", "--define=a=1"]).unwrap().id,
            defined.id
        );
        assert_eq!(
            crate::config_registry::mnemonic(&opt, &opt),
            Ok("k8-opt".to_owned())
        );
        // Starlark build settings take part in the identity; only values
        // different from the default are stored (crate::starlark_options).
        let flag = Label::parse_canonical("//:flag").unwrap();
        let set = |value: SettingValue| {
            Configuration::new(
                NativeOptions::default(),
                BTreeMap::from([(flag.clone(), value)]),
            )
        };
        assert_ne!(set(SettingValue::Bool(true)).id, default.id);
        assert_ne!(
            set(SettingValue::Bool(true)).id,
            set(SettingValue::String("true".into())).id
        );
        assert_eq!(
            set(SettingValue::Label(flag.clone())).id,
            set(SettingValue::Label(flag.clone())).id
        );
    }

    /// N18a: the options added for the exec configuration (N18b) are
    /// omitted from the identity at their defaults, so the identities of
    /// existing configurations (and the action cache keys and output roots
    /// derived from them) are byte-identical. The pinned digests are those
    /// the pre-N18a binary printed in cycle reports.
    #[test]
    fn exec_options_keep_existing_identities_byte_identical() {
        for (args, mode, id) in [
            (
                &[][..],
                "fastbuild",
                "25220f3df2bbaeaabb6d1d9090e0350f71be66fbaf6c30d161dbe3a21d6e6f12",
            ),
            (
                &["-c", "dbg"][..],
                "dbg",
                "ecf996e106ce543d963831ae33f4558154fe94c658b867a943a6e965f0abe391",
            ),
            (
                &["-c", "opt"][..],
                "opt",
                "c4756fdb1db24997bd83626c4e31214644514404335a977e8351493e245b4af7",
            ),
        ] {
            let configuration = parsed(args).unwrap();
            assert_eq!(configuration.native.host_compilation_mode, "opt");
            assert!(!configuration.native.is_exec);
            assert_eq!(configuration.native.platform_suffix, None);
            let expected = format!(
                r#"{{"version":1,"native":{{"compilation_mode":"{mode}","stamp":false,"define":[],"strip":"sometimes","force_pic":false,"platforms":["@@bazel_tools//tools:host_platform"],"host_platform":"@@bazel_tools//tools:host_platform"}},"starlark":[]}}"#
            );
            assert_eq!(
                String::from_utf8(canonical_serialization(
                    &configuration.native,
                    &configuration.starlark,
                    false
                ))
                .unwrap(),
                expected
            );
            assert_eq!(configuration.id.to_string(), id, "{args:?}");
        }
        // --host_compilation_mode (CoreOptions.java:406-417): converted like
        // --compilation_mode; its default keeps the identity.
        for args in [
            &["--host_compilation_mode=opt"][..],
            &["--host_compilation_mode", "OPT"][..],
        ] {
            assert_eq!(parsed(args).unwrap().id, Configuration::default().id);
        }
        let host_dbg = parsed(&["--host_compilation_mode=dbg"]).unwrap();
        assert_eq!(host_dbg.native.host_compilation_mode, "dbg");
        assert_eq!(
            host_dbg.native.value("host_compilation_mode"),
            Some(OptionValue::Enum("dbg"))
        );
        assert_ne!(host_dbg.id, Configuration::default().id);
        assert_eq!(
            parsed(&["--host_compilation_mode=x"]).unwrap_err(),
            "While parsing option --host_compilation_mode=x: Not a valid compilation mode: 'x' (should be fastbuild, dbg or opt)"
        );
        assert_eq!(
            parsed(&["--nohost_compilation_mode"]).unwrap_err(),
            "--nohost_compilation_mode :: Illegal use of 'no' prefix on non-boolean option: --nohost_compilation_mode"
        );
        // A CoreOptions option: query accepts it (QueryCommand.java:59-67).
        assert!(matches!(
            CommandLineOptions::for_query().parse("--host_compilation_mode=dbg", None),
            Ok(Parsed::Consumed(false))
        ));
        // Non-default values are part of the identity.
        let base = NativeOptions::default();
        for native in [
            NativeOptions {
                host_compilation_mode: "dbg",
                ..base.clone()
            },
            NativeOptions {
                is_exec: true,
                ..base.clone()
            },
            NativeOptions {
                platform_suffix: Some("exec"),
                ..base.clone()
            },
        ] {
            assert_ne!(
                Configuration::new(native, BTreeMap::new()).id,
                Configuration::default().id
            );
        }
    }

    /// N28b: the JavaOptions version strings (JavaOptions.java:373-403) are
    /// String options (the value as given), settable in every command-line
    /// form, part of the identity only when not at their defaults: the
    /// pinned default identity is unchanged.
    #[test]
    fn java_versions_are_string_options_omitted_from_the_identity_at_defaults() {
        let default = Configuration::default();
        assert_eq!(
            default.id.to_string(),
            "25220f3df2bbaeaabb6d1d9090e0350f71be66fbaf6c30d161dbe3a21d6e6f12"
        );
        for args in [
            &["--java_runtime_version=local_jdk"][..],
            &["--java_runtime_version", "local_jdk"][..],
            &["--java_language_version="][..],
            &["--tool_java_runtime_version=remotejdk_11"][..],
            &["--tool_java_language_version", ""][..],
        ] {
            let configuration = parsed(args).unwrap();
            assert_eq!(configuration.id, default.id, "{args:?}");
            assert_eq!(configuration.native.java, JavaVersions::default());
        }
        let java = parsed(&[
            "--java_runtime_version=remotejdk_11",
            "--java_language_version",
            "17",
            "--tool_java_runtime_version=Remotejdk_21",
            "--tool_java_language_version=21",
        ])
        .unwrap();
        assert_eq!(
            java.native.java,
            JavaVersions {
                java_runtime_version: "remotejdk_11".into(),
                java_language_version: "17".into(),
                // No case folding (Converters.StringConverter).
                tool_java_runtime_version: "Remotejdk_21".into(),
                tool_java_language_version: "21".into(),
            }
        );
        assert_eq!(
            String::from_utf8(canonical_serialization(&java.native, &java.starlark, false))
                .unwrap(),
            r#"{"version":1,"native":{"compilation_mode":"fastbuild","stamp":false,"define":[],"strip":"sometimes","force_pic":false,"platforms":["@@bazel_tools//tools:host_platform"],"host_platform":"@@bazel_tools//tools:host_platform"},"java":{"java_runtime_version":"remotejdk_11","java_language_version":"17","tool_java_runtime_version":"Remotejdk_21","tool_java_language_version":"21"},"starlark":[]}"#
        );
        for (args, value) in [
            (&["--java_runtime_version=remotejdk_11"][..], "remotejdk_11"),
            (&["--java_language_version=11"][..], "11"),
            (&["--tool_java_runtime_version=local_jdk"][..], "local_jdk"),
            (&["--tool_java_language_version=8"][..], "8"),
        ] {
            let configuration = parsed(args).unwrap();
            assert_ne!(configuration.id, default.id, "{args:?}");
            let name = args[0][2..].split_once('=').unwrap().0;
            assert_eq!(
                configuration.native.value(name),
                Some(OptionValue::String(value.to_owned()))
            );
        }
        assert_eq!(
            default.native.value("java_runtime_version"),
            Some(OptionValue::String("local_jdk".into()))
        );
        assert_eq!(
            default.native.value("tool_java_runtime_version"),
            Some(OptionValue::String("remotejdk_11".into()))
        );
        assert_eq!(
            parsed(&["--nojava_runtime_version"]).unwrap_err(),
            "--nojava_runtime_version :: Illegal use of 'no' prefix on non-boolean option: --nojava_runtime_version"
        );
        assert_eq!(
            parsed(&["--java_runtime_version"]).unwrap_err(),
            "Expected value after --java_runtime_version"
        );
        // Not CoreOptions: `query` does not register them (QueryCommand.java:59-67).
        for arg in ["--java_runtime_version=x", "--notool_java_language_version"] {
            assert_eq!(
                CommandLineOptions::for_query().parse(arg, None).err(),
                Some(format!("{arg} :: Unrecognized option: {arg}"))
            );
        }
        // The no-config marker is part of the identity only when set.
        let no_config = NativeOptions {
            no_config: true,
            ..NativeOptions::default()
        };
        assert_ne!(
            Configuration::new(no_config, BTreeMap::new()).id,
            default.id
        );
    }

    #[test]
    fn action_environment_options_parse_normalize_and_enter_the_identity() {
        use crate::action_env::{EnvVar, STRICT_PATH};
        let default = parsed(&[]).unwrap();
        let configured = parsed(&[
            "--action_env=A=1",
            "--action_env",
            "B",
            "--action_env==C",
            "--action_env=A=2",
            "--host_action_env=H=x",
        ])
        .unwrap();
        assert_eq!(
            configured.native.action_env,
            [
                EnvVar::Set("A".into(), "2".into()),
                EnvVar::Inherit("B".into()),
                EnvVar::Unset("C".into()),
            ]
        );
        assert_eq!(
            configured.native.host_action_env,
            [EnvVar::Set("H".into(), "x".into())]
        );
        assert_ne!(configured.id, default.id);
        let env = configured.native.action_environment();
        assert_eq!(
            env.fixed,
            BTreeMap::from([
                ("A".to_owned(), "2".to_owned()),
                ("PATH".to_owned(), STRICT_PATH.to_owned()),
            ])
        );
        assert_eq!(
            env.inherited,
            std::collections::BTreeSet::from(["B".to_owned()])
        );
        // Normalized forms are one configuration.
        assert_eq!(
            parsed(&["--action_env=A=0", "--action_env=A=2"])
                .unwrap()
                .id,
            parsed(&["--action_env=A=2"]).unwrap().id
        );
        // --incompatible_strict_action_env (old name experimental_*): the
        // default spelled explicitly keeps the identity.
        assert_eq!(
            parsed(&["--incompatible_strict_action_env"]).unwrap().id,
            default.id
        );
        let loose = parsed(&["--experimental_strict_action_env=false"]).unwrap();
        assert!(!loose.native.strict_action_env);
        assert_ne!(loose.id, default.id);
        // --test_env: trimmed, so only the untrimmed identity changes.
        let test = parsed(&["--test_env=T=1", "--test_env=U"]).unwrap();
        assert_eq!(test.id, default.id);
        assert_ne!(test.untrimmed_id(), default.untrimmed_id());
        assert_eq!(
            test.native.test_environment().fixed,
            BTreeMap::from([("T".to_owned(), "1".to_owned())])
        );
        assert_eq!(
            parsed(&["--action_env="]).unwrap_err(),
            "While parsing option --action_env=: Variable definitions must be in the form of a 'name=value', 'name', or '=name' assignment"
        );
        assert_eq!(
            parsed(&["--nohost_action_env"]).unwrap_err(),
            "--nohost_action_env :: Illegal use of 'no' prefix on non-boolean option: --nohost_action_env"
        );
    }

    /// `--cpu`/`--host_cpu` (CoreOptions, AutoCpuConverter) and
    /// `--crosstool_top` (CppOptions, LabelConverter): the defaults spelled
    /// explicitly keep the default identity; other values are part of it;
    /// `query` accepts only the CoreOptions (paired captures query_cpu,
    /// query_crosstool_top in tests/fixtures/pre_platforms_flags).
    #[test]
    fn pre_platforms_flags_parse_and_enter_the_identity_when_set() {
        let default = Configuration::default();
        for args in [
            &["--cpu=k8"][..],
            &["--cpu="][..],
            &["--host_cpu", "k8"][..],
            &["--host_cpu="][..],
            &["--crosstool_top=@bazel_tools//tools/cpp:toolchain"][..],
            &["--crosstool_top", "@@bazel_tools//tools/cpp:toolchain"][..],
            &["--cpu=arm", "--cpu=k8"][..],
        ] {
            assert_eq!(parsed(args).unwrap().id, default.id, "{args:?}");
        }
        assert_eq!(
            default.native.value("cpu"),
            Some(OptionValue::String(host_cpu().into()))
        );
        assert_eq!(
            default.native.value("host_cpu"),
            Some(OptionValue::String(host_cpu().into()))
        );
        assert_eq!(
            default.native.value("crosstool_top"),
            Some(OptionValue::Label(default_crosstool_top()))
        );
        assert_eq!(
            parse_canonical_option_label(DEFAULT_CROSSTOOL_TOP),
            Ok(default_crosstool_top())
        );
        let set = parsed(&[
            "--cpu=arm",
            "--host_cpu",
            "ppc",
            "--crosstool_top=pkg:my_crosstool",
        ])
        .unwrap();
        assert_eq!(set.native.cpu.as_deref(), Some("arm"));
        assert_eq!(set.native.host_cpu(), "ppc");
        assert_eq!(
            set.native.value("crosstool_top"),
            Some(OptionValue::Label(
                Label::parse_canonical("//pkg:my_crosstool").unwrap()
            ))
        );
        assert_eq!(
            String::from_utf8(canonical_serialization(&set.native, &set.starlark, false)).unwrap(),
            r#"{"version":1,"native":{"compilation_mode":"fastbuild","stamp":false,"define":[],"strip":"sometimes","force_pic":false,"platforms":["@@bazel_tools//tools:host_platform"],"host_platform":"@@bazel_tools//tools:host_platform","cpu":"arm","host_cpu":"ppc","crosstool_top":"//pkg:my_crosstool"},"starlark":[]}"#
        );
        for args in [
            &["--cpu=arm"][..],
            &["--host_cpu=ppc"][..],
            &["--crosstool_top=//pkg:my_crosstool"][..],
        ] {
            assert_ne!(parsed(args).unwrap().id, default.id, "{args:?}");
        }
        // LabelConverter has no default for an empty value.
        assert_eq!(
            parsed(&["--crosstool_top="]).unwrap_err(),
            "While parsing option --crosstool_top=: invalid target name '': empty target name"
        );
        assert_eq!(
            parsed(&["--crosstool_top=a:b:c"]).unwrap_err(),
            "While parsing option --crosstool_top=a:b:c: invalid target name 'b:c': target names may not contain ':'"
        );
        for arg in ["--nocpu", "--nocrosstool_top"] {
            assert_eq!(
                parsed(&[arg]).unwrap_err(),
                format!("{arg} :: Illegal use of 'no' prefix on non-boolean option: {arg}")
            );
        }
        // An apparent repository the main repository does not see is kept.
        let mut options = CommandLineOptions::default();
        options.parse("--crosstool_top=@nope//:x", None).unwrap();
        let configuration = options
            .configuration(&|text| {
                if text.starts_with("@nope") {
                    Err("unknown apparent repository @nope".into())
                } else {
                    canonical(text)
                }
            })
            .unwrap();
        assert_eq!(
            configuration
                .native
                .value("crosstool_top")
                .unwrap()
                .to_string(),
            "@@[unknown repo 'nope' requested from @@]//:x"
        );
        let mut query = CommandLineOptions::for_query();
        for arg in ["--cpu=arm", "--host_cpu=ppc"] {
            assert!(
                matches!(query.parse(arg, None), Ok(Parsed::Consumed(false))),
                "{arg}"
            );
        }
        assert_eq!(
            query.parse("--crosstool_top=//pkg:t", None).err().unwrap(),
            "--crosstool_top=//pkg:t :: Unrecognized option: --crosstool_top=//pkg:t"
        );
    }

    /// Every table-driven row is a real Bazel 9.3 fragment option, its
    /// default and fixed exec value convert, and only CoreOptions rows are
    /// `query` options.
    #[test]
    fn value_option_rows_are_bazel_options_with_converting_defaults() {
        for row in VALUE_OPTIONS {
            let name = row.option.name;
            assert_eq!(
                fragment_option(name),
                Some(row.option.non_configurable),
                "{name}"
            );
            assert!(OPTIONS.iter().all(|typed| typed.name != name), "{name}");
            assert_eq!(option(name), Some(&row.option));
            assert_eq!(
                NativeOptions::default().value(name),
                Some(row.default_value())
            );
            if let ExecValue::Set(raw) = row.exec {
                assert!(convert(&row.option, raw, &canonical).is_ok(), "{name}");
            }
            assert!(!row.option.allow_multiple && row.option.abbrev.is_none());
        }
        let names = modelled_option_names().collect::<Vec<_>>();
        assert_eq!(names.len(), OPTIONS.len() + VALUE_OPTIONS.len());
        assert_eq!(
            names[OPTIONS.len()..],
            [
                "experimental_proto_descriptor_sets_include_source_info",
                "strict_proto_deps",
                "strict_public_imports",
                "enable_runfiles",
                "collect_code_coverage",
                "apple_platform_type"
            ]
        );
        let defaults = NativeOptions::default();
        assert_eq!(
            defaults.value("strict_proto_deps"),
            Some(OptionValue::Enum("ERROR"))
        );
        assert_eq!(
            defaults.value("strict_public_imports"),
            Some(OptionValue::Enum("OFF"))
        );
        assert_eq!(
            defaults.value("experimental_proto_descriptor_sets_include_source_info"),
            Some(OptionValue::Bool(false))
        );
        assert_eq!(
            defaults.value("apple_platform_type"),
            Some(OptionValue::String("macos".into()))
        );
        // CoreOptions.enableRunfiles: the TriState constant, AUTO unless set
        // (tests/fixtures/enable_runfiles_option).
        assert_eq!(
            defaults.value("enable_runfiles"),
            Some(OptionValue::Enum("AUTO"))
        );
        assert_eq!(defaults.value("not_an_option"), None);
    }

    /// ProtoConfiguration's options on the command line (paired captures
    /// tests/fixtures/value_options proto_*): BooleanConverter and the
    /// StrictDepsMode EnumConverter (case-insensitive, `toString` constants,
    /// a lower-case type description); defaults spelled explicitly keep the
    /// default identity; other values join it (`"values"`); the last
    /// occurrence wins.
    #[test]
    fn proto_value_options_parse_and_enter_the_identity_when_set() {
        let default = Configuration::default();
        for args in [
            &["--strict_proto_deps=error"][..],
            &["--strict_proto_deps", "ERROR"][..],
            &["--strict_public_imports=Off"][..],
            &["--noexperimental_proto_descriptor_sets_include_source_info"][..],
            &["--experimental_proto_descriptor_sets_include_source_info=0"][..],
            &["--strict_proto_deps=off", "--strict_proto_deps=error"][..],
        ] {
            assert_eq!(parsed(args).unwrap().id, default.id, "{args:?}");
        }
        let set = parsed(&[
            "--strict_proto_deps=OFF",
            "--experimental_proto_descriptor_sets_include_source_info",
            "--strict_public_imports",
            "strict",
        ])
        .unwrap();
        assert_eq!(
            set.native.value("strict_proto_deps"),
            Some(OptionValue::Enum("OFF"))
        );
        assert_eq!(
            set.native.value("strict_public_imports"),
            Some(OptionValue::Enum("STRICT"))
        );
        assert_eq!(
            set.native
                .value("experimental_proto_descriptor_sets_include_source_info"),
            Some(OptionValue::Bool(true))
        );
        assert_eq!(
            String::from_utf8(canonical_serialization(&set.native, &set.starlark, false)).unwrap(),
            r#"{"version":1,"native":{"compilation_mode":"fastbuild","stamp":false,"define":[],"strip":"sometimes","force_pic":false,"platforms":["@@bazel_tools//tools:host_platform"],"host_platform":"@@bazel_tools//tools:host_platform","values":{"experimental_proto_descriptor_sets_include_source_info":"true","strict_proto_deps":"OFF","strict_public_imports":"STRICT"}},"starlark":[]}"#
        );
        // `default` is the StrictDepsMode constant DEFAULT, not the default.
        let constant = parsed(&["--strict_proto_deps=default"]).unwrap();
        assert_eq!(
            constant.native.value("strict_proto_deps"),
            Some(OptionValue::Enum("DEFAULT"))
        );
        assert_ne!(constant.id, default.id);
        assert_eq!(
            parsed(&["--strict_proto_deps=off", "--strict_proto_deps=warn"])
                .unwrap()
                .native
                .values,
            BTreeMap::from([("strict_proto_deps", OptionValue::Enum("WARN"))])
        );
        for (args, error) in [
            (
                &["--strict_proto_deps=loose"][..],
                "While parsing option --strict_proto_deps=loose: Not a valid strict dependency checking level: 'loose' (should be off, warn, error, strict or default)",
            ),
            (
                &["--nostrict_proto_deps"][..],
                "--nostrict_proto_deps :: Illegal use of 'no' prefix on non-boolean option: --nostrict_proto_deps",
            ),
            (
                &["--experimental_proto_descriptor_sets_include_source_info=maybe"][..],
                "While parsing option --experimental_proto_descriptor_sets_include_source_info=maybe: 'maybe' is not a boolean",
            ),
            (
                &["--noexperimental_proto_descriptor_sets_include_source_info=false"][..],
                "--noexperimental_proto_descriptor_sets_include_source_info=false :: Unexpected value after boolean option: --noexperimental_proto_descriptor_sets_include_source_info=false",
            ),
            (
                &["--strict_public_imports"][..],
                "Expected value after --strict_public_imports",
            ),
        ] {
            assert_eq!(parsed(args).unwrap_err(), error, "{args:?}");
        }
    }

    /// Rows without a modelled command-line form stay unsupported options
    /// (`NotModelled`: main's explicit error); `query` registers only
    /// CoreOptions (QueryCommand.java:59-67): it accepts and ignores
    /// `--collect_code_coverage` and reports the other fragments' rows as
    /// unknown (paired captures value_options query cases).
    #[test]
    fn value_options_without_a_command_line_form_and_query() {
        let mut options = CommandLineOptions::default();
        for arg in [
            "--collect_code_coverage",
            "--nocollect_code_coverage",
            "--apple_platform_type=ios",
        ] {
            assert!(
                matches!(options.parse(arg, None), Ok(Parsed::NotModelled)),
                "{arg}"
            );
        }
        let mut query = CommandLineOptions::for_query();
        assert!(matches!(
            query.parse("--collect_code_coverage", None),
            Ok(Parsed::Consumed(false))
        ));
        for (arg, next) in [
            ("--strict_proto_deps=off", None),
            ("--strict_public_imports", Some("warn")),
            (
                "--experimental_proto_descriptor_sets_include_source_info",
                None,
            ),
            (
                "--noexperimental_proto_descriptor_sets_include_source_info",
                None,
            ),
            ("--apple_platform_type=ios", None),
        ] {
            assert_eq!(
                query.parse(arg, next).err().unwrap(),
                format!("{arg} :: Unrecognized option: {arg}"),
                "{arg}"
            );
        }
        // SpellChecker's suggestion among query's options when one is close.
        assert_eq!(
            query_did_you_mean("--keep_goin"),
            " (did you mean '--keep_going'?)"
        );
        assert_eq!(query_did_you_mean("--strict_proto_deps=off"), "");
    }

    #[test]
    fn extra_toolchains_are_a_repeatable_list_keeping_the_last_copy() {
        // PlatformOptions.java:98-116, 164-201; Converters.java:253-287.
        let configuration = parsed(&[
            "--extra_toolchains=//a:b,//c:d",
            "--extra_toolchains",
            "//a:b",
            "--extra_toolchains=",
        ])
        .unwrap();
        assert_eq!(configuration.native.extra_toolchains, ["//c:d", "//a:b"]);
        assert_eq!(
            parsed(&["--extra_toolchains=,x"])
                .unwrap()
                .native
                .extra_toolchains,
            ["", "x"]
        );
        // The identity of a configuration without extra toolchains is
        // unchanged; with them it differs.
        assert_eq!(parsed(&[]).unwrap().id, Configuration::default().id);
        assert_ne!(
            parsed(&["--extra_toolchains=//a:b"]).unwrap().id,
            Configuration::default().id
        );
        assert_eq!(
            parsed(&["--noextra_toolchains"]).unwrap_err(),
            "--noextra_toolchains :: Illegal use of 'no' prefix on non-boolean option: --noextra_toolchains"
        );
        assert_eq!(
            parsed(&["--extra_toolchains"]).unwrap_err(),
            "Expected value after --extra_toolchains"
        );
        let mut query = CommandLineOptions::for_query();
        assert_eq!(
            query.parse("--extra_toolchains=//a:b", None).err().unwrap(),
            "--extra_toolchains=//a:b :: Unrecognized option: --extra_toolchains=//a:b"
        );
    }

    #[test]
    fn unrecognized_options_are_certain_and_without_suggestions() {
        for arg in ["--:s=blue", "--s=blue", "--//x", "--zzzzzzzzzzzzzzzz=1"] {
            assert_eq!(
                unrecognized_option(arg),
                Some(format!("{arg} :: Unrecognized option: {arg}")),
                "{arg}"
            );
        }
        // Real Bazel options (also `no`-prefixed or old names) and arguments
        // close to one (SpellChecker would suggest it) stay undecided.
        for arg in [
            "--keep_going",
            "--nokeep_going",
            "--cpu=k8",
            "--experimental_platforms=//:p",
            "--keep-going",
            "--stmp",
            "-k",
            "--=x",
        ] {
            assert_eq!(unrecognized_option(arg), None, "{arg}");
        }
    }

    /// N47: a transition's raw StarlarkList/Tuple prints as its Starlark
    /// repr and compares like Java (an ImmutableList equals any List with
    /// equal elements; StarlarkList/Tuple only their own class).
    #[test]
    fn raw_sequences_print_and_compare_like_starlark_objects() {
        let list = SettingValue::Sequence {
            tuple: false,
            items: vec!["netgo".into(), "a\"b\\\n\u{1}é".into()],
        };
        assert_eq!(list.to_string(), r#"["netgo", "a\"b\\\n\x01é"]"#);
        let single = SettingValue::Sequence {
            tuple: true,
            items: vec!["x".into()],
        };
        assert_eq!(single.to_string(), r#"("x",)"#);
        let empty = SettingValue::Sequence {
            tuple: true,
            items: vec![],
        };
        assert_eq!(empty.to_string(), "()");
        let converted = SettingValue::StringList(vec!["x".into()]);
        assert!(converted.java_equals(&single));
        assert!(!single.java_equals(&converted));
        let list_x = SettingValue::Sequence {
            tuple: false,
            items: vec!["x".into()],
        };
        assert!(!list_x.java_equals(&single));
        assert!(list_x.java_equals(&list_x.clone()));
        assert_eq!(single.string_items(), Some(&["x".to_owned()][..]));
        // Distinct identities for equal elements.
        let id = |value: SettingValue| {
            Configuration::new(
                NativeOptions::default(),
                BTreeMap::from([(Label::parse_canonical("//f:tags").unwrap(), value)]),
            )
            .id
        };
        assert_ne!(id(converted), id(list_x));
    }

    #[test]
    fn setting_values_print_like_java_objects() {
        assert_eq!(SettingValue::Bool(true).to_string(), "true");
        assert_eq!(SettingValue::Int(-3).to_string(), "-3");
        assert_eq!(SettingValue::String("a b".into()).to_string(), "a b");
        assert_eq!(
            SettingValue::StringList(vec!["a".into(), "".into()]).to_string(),
            "[a, ]"
        );
        assert_eq!(
            SettingValue::Label(Label::parse_canonical("@@r+//p:x").unwrap()).to_string(),
            "@@r+//p:x"
        );
    }

    #[test]
    fn option_tables_know_old_and_non_configurable_names() {
        assert_eq!(canonical_name("experimental_platforms"), "platforms");
        assert_eq!(canonical_name("cpu"), "cpu");
        assert_eq!(fragment_option("cpu"), Some(false));
        assert_eq!(fragment_option("platform_mappings"), Some(true));
        assert_eq!(fragment_option("experimental_platforms"), None);
        assert_eq!(fragment_option("is exec configuration"), None);
        assert_eq!(fragment_option("no_such_option"), None);
        for option in OPTIONS {
            assert_eq!(fragment_option(option.name), Some(option.non_configurable));
            assert_eq!(
                option.old_name.map(canonical_name),
                option.old_name.map(|_| option.name)
            );
        }
    }
}
