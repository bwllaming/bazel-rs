//! The configurations of one build and the keys of configured targets
//! (N18a: explicit configuration plumbing, docs/CONFIGURATION.md
//! "Configured-target keys and output roots").
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
//! Every phase-2 (analysis and action planning) value is keyed by a
//! [`ConfiguredTargetKey`] (skyframe/ConfiguredTargetKey.java): the label and
//! the identity of the configuration the target is analyzed in. One build
//! has a top-level configuration; dependency edges currently always keep
//! their parent's configuration (no transitions are modelled yet, N18b-d).
use std::cell::OnceCell;
use std::collections::BTreeMap;
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
}

impl ConfiguredTargetKey {
    pub fn new(label: Label, config: ConfigId) -> Self {
        Self { label, config }
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
    /// The exec configuration derived from `top` (N18b; unused yet).
    exec_baseline: OnceCell<ConfigId>,
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
        configurations.mnemonics.insert(id, mnemonic(&top, &top));
        configurations.by_id.insert(id, Arc::new(top));
        configurations
    }

    /// The top-level configuration's identity.
    pub fn top(&self) -> ConfigId {
        self.top
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

    /// Intern `configuration` (a configuration derived from the top-level
    /// one, N18b-d) and return its identity.
    pub fn intern(&mut self, configuration: Configuration) -> ConfigId {
        let id = configuration.id;
        if !self.by_id.contains_key(&id) {
            let mnemonic = mnemonic(&configuration, self.top_configuration());
            self.mnemonics.insert(id, mnemonic);
            self.by_id.insert(id, Arc::new(configuration));
        }
        id
    }

    /// The exec configuration once created (N18b).
    pub fn exec_baseline(&self) -> Option<ConfigId> {
        self.exec_baseline.get().copied()
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

/// `bazel-out/<mnemonic>/bin`.
pub fn output_root(mnemonic: &str) -> String {
    format!("bazel-out/{mnemonic}/bin")
}

/// OutputPathMnemonicComputer.computeMnemonic
/// (analysis/config/OutputPathMnemonicComputer.java:174-245) for the native
/// profile: `rust-{compilation_mode}[-{platform_suffix}][-ST-{12 hex}]`. The
/// CPU/platform descriptor is the profile's fixed `rust` (a documented
/// divergence from Bazel's `k8`, which marks `cpu` explicit in the output
/// path); `compilation_mode` and `platform_suffix` are explicit too. The
/// `ST-` fragment hashes every other option that differs from `baseline`
/// (computeNameFragmentWithDiff, :286-318), so the top-level configuration,
/// its own baseline, keeps the historical `rust-fastbuild`, `rust-dbg` and
/// `rust-opt` roots.
pub fn mnemonic(configuration: &Configuration, baseline: &Configuration) -> String {
    let native = &configuration.native;
    let mut mnemonic = format!("rust-{}", native.compilation_mode);
    if let Some(suffix) = native.platform_suffix.filter(|suffix| !suffix.is_empty()) {
        mnemonic.push('-');
        mnemonic.push_str(suffix);
    }
    if let Some(fragment) = name_fragment_with_diff(configuration, baseline) {
        mnemonic.push('-');
        mnemonic.push_str(&fragment);
    }
    mnemonic
}

/// Options whose value is explicit in the mnemonic
/// (markAsExplicitInOutputPathFor, OutputPathMnemonicComputer.java:187-198;
/// `cpu` is the native profile's fixed `rust`).
const EXPLICIT_IN_OUTPUT_PATH: &[&str] = &["compilation_mode", "platform_suffix"];

/// The native options in their `//command_line_option:NAME=VALUE` hash form
/// (hashChosenOptions, OutputPathMnemonicComputer.java:368-410): Java
/// `toString` of each value, `@null` for null values.
fn native_option_strings(native: &NativeOptions) -> BTreeMap<&'static str, String> {
    let mut options = BTreeMap::new();
    for option in crate::configuration::OPTIONS {
        if let Some(value) = native.value(option.name) {
            options.insert(option.name, format!("={value}"));
        }
    }
    let list = |items: &[String]| format!("=[{}]", items.join(", "));
    options.insert("extra_toolchains", list(&native.extra_toolchains));
    options.insert(
        "host_compilation_mode",
        format!("={}", native.host_compilation_mode),
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
) -> Option<String> {
    if configuration.id == baseline.id {
        return None;
    }
    let to = native_option_strings(&configuration.native);
    let from = native_option_strings(&baseline.native);
    // A TreeMap of `KEY` to its hash form.
    let mut chosen: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in &to {
        if from.get(name) != Some(value) && !EXPLICIT_IN_OUTPUT_PATH.contains(name) {
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
        if current != baseline.starlark.get(label) {
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
    fn top_level_mnemonics_keep_the_historical_roots() {
        for (args, root) in [
            (&[][..], "bazel-out/rust-fastbuild/bin"),
            (&["-c", "dbg"][..], "bazel-out/rust-dbg/bin"),
            (&["-c", "opt"][..], "bazel-out/rust-opt/bin"),
            // Options without an output-path effect keep the mode's root:
            // the top-level configuration is its own baseline.
            (
                &["--stamp", "--define=a=1"][..],
                "bazel-out/rust-fastbuild/bin",
            ),
            (
                &["--platforms=//p:x", "-c", "opt"][..],
                "bazel-out/rust-opt/bin",
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
        let top = parsed(&[]);
        let mut configurations = Configurations::new(top.clone());
        assert_eq!(configurations.exec_baseline(), None);
        // Interning is idempotent and keeps the top-level configuration.
        assert_eq!(configurations.intern(top.clone()), configurations.top());
        assert_eq!(
            configurations.mnemonic(configurations.top()),
            "rust-fastbuild"
        );
        // An exec-like configuration: suffix `exec`, mode from
        // --host_compilation_mode, `is exec configuration` in the hash.
        let mut native = top.native.clone();
        native.compilation_mode = native.host_compilation_mode;
        native.is_exec = true;
        native.platform_suffix = Some("exec");
        let exec = Configuration::new(native, BTreeMap::new());
        let id = configurations.intern(exec.clone());
        assert_ne!(id, configurations.top());
        let exec_mnemonic = configurations.mnemonic(id).to_owned();
        let hash = exec_mnemonic
            .strip_prefix("rust-opt-exec-ST-")
            .unwrap_or_else(|| panic!("{exec_mnemonic}"));
        assert_eq!(hash.len(), 12);
        assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(
            configurations.output_root(id),
            format!("bazel-out/{exec_mnemonic}/bin")
        );
        assert!(Arc::ptr_eq(configurations.get(id), configurations.get(id)));
        // Only explicit options differ: no hash.
        let mut native = top.native.clone();
        native.compilation_mode = "dbg";
        native.platform_suffix = Some("x");
        assert_eq!(
            mnemonic(&Configuration::new(native, BTreeMap::new()), &top),
            "rust-dbg-x"
        );
        // Other options (also Starlark settings) are hashed; equal diffs give
        // equal hashes, different values different hashes.
        let stamped = parsed(&["--stamp"]);
        let stamped_mnemonic = mnemonic(&stamped, &top);
        assert!(stamped_mnemonic.starts_with("rust-fastbuild-ST-"));
        assert_eq!(stamped_mnemonic, mnemonic(&parsed(&["--stamp=yes"]), &top));
        assert_ne!(stamped_mnemonic, mnemonic(&parsed(&["--define=a=1"]), &top));
        let flag = |value: bool| {
            Configuration::new(
                NativeOptions::default(),
                BTreeMap::from([(label("//:flag"), SettingValue::Bool(value))]),
            )
        };
        let set = mnemonic(&flag(true), &top);
        assert!(set.starts_with("rust-fastbuild-ST-"));
        assert_ne!(set, mnemonic(&flag(false), &top));
        // A setting removed relative to the baseline is hashed as `@null`.
        assert!(mnemonic(&top, &flag(true)).starts_with("rust-fastbuild-ST-"));
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
            name_fragment_with_diff(&parsed(&["--stamp"]), &parsed(&[])),
            Some(expected)
        );
        assert_eq!(name_fragment_with_diff(&parsed(&[]), &parsed(&[])), None);
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
