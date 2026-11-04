//! CustomCommandLine as the Java action classes build it
//! (analysis/actions/CustomCommandLine.java): literal strings, NestedSets
//! (fingerprinted by their structure, NestedSetFingerprintCache) and
//! PrefixArgs, with the action keys of the classes that use them
//! (SpawnAction.computeKey, JavaCompileAction.computeKey).
use std::sync::Arc;

use starlark::values::{Value, ValueLike};

use crate::aquery_key::Fingerprint;
use crate::starlark_types::{Depset, DepsetNode, File, Item, ItemType, Order};

/// CustomCommandLine.PrefixArg.PREFIX_UUID.
const PREFIX_UUID: &str = "a95eccdf-4f54-46fc-b925-c8c7e1f50c95";
/// CommandLines.PARAM_FILE_UUID.
const PARAM_FILE_UUID: &str = "106c1389-88d7-4cc1-8f05-f8a61fd8f7b1";
/// SpawnAction.GUID.
const SPAWN_GUID: &str = "ebd6fce3-093e-45ee-adb6-bf513b602f0d";
/// JavaCompileAction.GUID.
const JAVA_COMPILE_GUID: &str = "e423747c-2827-49e6-b961-f6c08c10bb51";

#[derive(Clone, Debug)]
pub(crate) enum Arg {
    Str(String),
    Set(Arc<DepsetNode>),
    Prefix(String, String),
}

/// One CustomCommandLine.
#[derive(Clone, Debug, Default)]
pub(crate) struct Line(pub Vec<Arg>);

fn set_strings(node: &Arc<DepsetNode>) -> Vec<String> {
    Depset {
        node: Arc::clone(node),
    }
    .flatten()
    .into_iter()
    .filter_map(item_string)
    .collect()
}

fn item_string(item: &Item) -> Option<String> {
    match item {
        Item::File(file) => Some(file.path.clone()),
        Item::String(text) => Some(text.clone()),
        _ => None,
    }
}

impl Line {
    pub(crate) fn add(&mut self, text: impl Into<String>) -> &mut Self {
        self.0.push(Arg::Str(text.into()));
        self
    }

    /// `add(arg, value)` / `addExecPath(arg, artifact)`: nothing when null.
    pub(crate) fn add_opt(&mut self, flag: &str, value: Option<&str>) -> &mut Self {
        if let Some(value) = value {
            self.add(flag).add(value);
        }
        self
    }

    /// `addExecPaths(arg, Collection)` / `addAll(arg, Collection)`: the
    /// flag only for a non-empty collection, each element a string.
    pub(crate) fn add_all(&mut self, flag: &str, values: &[String]) -> &mut Self {
        if !values.is_empty() {
            self.add(flag);
            for value in values {
                self.add(value.clone());
            }
        }
        self
    }

    pub(crate) fn add_paths(&mut self, flag: &str, files: &[File]) -> &mut Self {
        let paths: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
        self.add_all(flag, &paths)
    }

    /// `addExecPaths(arg, NestedSet)`: the flag only for a non-empty set.
    pub(crate) fn add_set(&mut self, flag: &str, node: &Arc<DepsetNode>) -> &mut Self {
        if !node.empty {
            self.add(flag);
            self.0.push(Arg::Set(Arc::clone(node)));
        }
        self
    }

    /// `addAll(NestedSet)` without a flag: kept even when empty.
    pub(crate) fn add_bare_set(&mut self, node: &Arc<DepsetNode>) -> &mut Self {
        self.0.push(Arg::Set(Arc::clone(node)));
        self
    }

    /// `addPrefixedLabel(prefix, label, mapping)`.
    pub(crate) fn add_prefixed(&mut self, prefix: &str, value: String) -> &mut Self {
        self.0.push(Arg::Prefix(prefix.to_owned(), value));
        self
    }

    /// `--target_label`: the label, or `@`-prefixed for another repository.
    pub(crate) fn add_target_label(&mut self, target: &str) -> &mut Self {
        self.add("--target_label");
        if target.starts_with('@') {
            self.add_prefixed("@", target.to_owned())
        } else {
            self.add(target)
        }
    }

    pub(crate) fn expand(&self) -> Vec<String> {
        let mut out = Vec::new();
        for arg in &self.0 {
            match arg {
                Arg::Str(text) => out.push(text.clone()),
                Arg::Set(node) => out.extend(set_strings(node)),
                Arg::Prefix(prefix, value) => out.push(format!("{prefix}{value}")),
            }
        }
        out
    }

    /// CustomCommandLine.addToFingerprint.
    pub(crate) fn fingerprint(&self, fp: &mut Fingerprint<Vec<u8>>) {
        for arg in &self.0 {
            match arg {
                Arg::Str(text) => {
                    fp.string(text);
                }
                Arg::Set(node) => {
                    let mut expand = |item: &Item, fp: &mut Fingerprint<Vec<u8>>| {
                        if let Some(text) = item_string(item) {
                            fp.string(&text);
                        }
                    };
                    fp.raw(&crate::aquery_key::nested_set(node, &mut expand));
                }
                Arg::Prefix(prefix, value) => {
                    fp.uuid(PREFIX_UUID).string(prefix).string(value);
                }
            }
        }
    }
}

/// A Starlark depset's NestedSet (`Depset.getSet`).
pub(crate) fn node(value: Value<'_>) -> Option<Arc<DepsetNode>> {
    value
        .downcast_ref::<Depset>()
        .map(|depset| Arc::clone(&depset.node))
}

/// NestedSetBuilder(order).add(direct...).addTransitive(children...).build().
pub(crate) fn build(
    direct: Vec<File>,
    children: Vec<Arc<DepsetNode>>,
    order: Order,
    files: bool,
) -> Arc<DepsetNode> {
    let mut items: Vec<Item> = Vec::new();
    for file in direct {
        let item = Item::File(file);
        if !items.contains(&item) {
            items.push(item);
        }
    }
    let mut seen = Vec::<*const DepsetNode>::new();
    let children: Vec<_> = children
        .into_iter()
        .filter(|child| {
            let ptr = Arc::as_ptr(child);
            !seen.contains(&ptr) && {
                seen.push(ptr);
                true
            }
        })
        .collect();
    Depset::compose_framework(
        items,
        children,
        order,
        if files { Some(ItemType::File) } else { None },
    )
    .map(|depset| depset.node)
    .unwrap_or_else(|_| empty(order))
}

pub(crate) fn empty(order: Order) -> Arc<DepsetNode> {
    Depset::compose_framework(Vec::new(), Vec::new(), order, None)
        .expect("empty depset")
        .node
}

/// ParamFileInfo's fingerprinted part: flag format and file type.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ParamKey {
    pub file_type: &'static str,
}

/// The fixed environment in ActionEnvironment.getFixedEnv order (the
/// configuration's, then `withAdditionalFixedVariables`) and the inherited
/// variable names.
pub(crate) struct KeyEnvironment {
    pub fixed: Vec<(String, String)>,
    pub inherited: Vec<String>,
}

impl KeyEnvironment {
    pub(crate) fn of(
        context: &crate::analysis_runtime::Context,
        extra: &[(&str, &str)],
        default_shell: bool,
    ) -> Self {
        let base = context
            .action_env
            .for_spawn(default_shell, Default::default());
        let mut fixed: Vec<(String, String)> = base
            .fixed
            .into_iter()
            .filter(|(key, _)| !extra.iter().any(|(name, _)| name == key))
            .collect();
        fixed.extend(
            extra
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
        );
        Self {
            fixed,
            inherited: base.inherited.into_iter().collect(),
        }
    }

    /// ActionEnvironment.addTo.
    fn add_to(&self, fp: &mut Fingerprint<Vec<u8>>) {
        fp.string_map(self.fixed.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        fp.strings(self.inherited.iter().map(String::as_str));
    }
}

/// SpawnAction.computeKey: the command lines (each with its parameter file
/// info), the mnemonic, the environment and the execution info
/// (`getExecutionInfo()`, in its iteration order).
pub(crate) fn spawn_key(
    lines: &[(&Line, Option<ParamKey>)],
    mnemonic: &str,
    environment: &KeyEnvironment,
    execution_info: &[(String, String)],
) -> Vec<u8> {
    let mut fp = Fingerprint::<Vec<u8>>::default();
    fp.string(SPAWN_GUID);
    for (line, param) in lines {
        line.fingerprint(&mut fp);
        if let Some(param) = param {
            fp.uuid(PARAM_FILE_UUID)
                .string("@%s")
                .string(param.file_type);
        }
    }
    fp.string(mnemonic);
    environment.add_to(&mut fp);
    fp.string_map(execution_info.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    fp.into_inner()
}

/// JavaCompileAction.computeKey (path mapping off).
pub(crate) fn java_compile_key(
    classpath_mode: i32,
    executable: &Line,
    flags: &Line,
    transitive_inputs: &Arc<DepsetNode>,
    environment: &KeyEnvironment,
    execution_info: &[(String, String)],
    in_memory_jdeps: bool,
) -> Vec<u8> {
    let mut fp = Fingerprint::<Vec<u8>>::default();
    fp.uuid(JAVA_COMPILE_GUID).int(classpath_mode);
    executable.fingerprint(&mut fp);
    flags.fingerprint(&mut fp);
    let mut expand = |item: &Item, fp: &mut Fingerprint<Vec<u8>>| {
        if let Some(text) = item_string(item) {
            fp.string(&text);
        }
    };
    fp.raw(&crate::aquery_key::nested_set(
        transitive_inputs,
        &mut expand,
    ));
    environment.add_to(&mut fp);
    fp.string_map(execution_info.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    fp.boolean(in_memory_jdeps);
    fp.into_inner()
}

/// JavaCompileAction.ProgressMessage.
pub(crate) fn progress(
    prefix: &str,
    sources: usize,
    source_jars: usize,
    processors: &[String],
) -> String {
    let mut message = format!("{prefix} %{{output}} (");
    let mut counts = Vec::new();
    for (count, name) in [(sources, "source file"), (source_jars, "source jar")] {
        if count > 0 {
            counts.push(format!(
                "{count} {name}{}",
                if count > 1 { "s" } else { "" }
            ));
        }
    }
    message.push_str(&counts.join(", "));
    message.push(')');
    if !processors.is_empty() {
        let names: Vec<&str> = processors
            .iter()
            .map(|name| {
                name.rsplit_once('.')
                    .map_or(name.as_str(), |(_, short)| short)
            })
            .collect();
        message.push_str(&format!(
            " and running annotation processors ({})",
            names.join(", ")
        ));
    }
    message
}
