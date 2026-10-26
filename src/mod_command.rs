//! `mod` — Bazel 9.3 ModCommand (bazel/commands/ModCommand.java) with the
//! bzlmod/modcommand package: ModOptions, ModuleArg, ExtensionArg,
//! ModExecutor and the text, JSON and Graphviz output formatters
//! (docs/MOD_COMMAND.md).
//!
//! The command works on the invocation's repository set, like every other
//! command: the built-in modules (the default profile), the lockfile modes
//! and the registry session apply. The inspection graph
//! (BazelModuleInspectorValue) is built from the resolver's un-pruned
//! dependency graph ([`crate::module_graph::InspectedModule`]).
//!
//! Native difference: genuine evaluates every module extension of the graph
//! for every graph subcommand (BazelModuleInspectorFunction.computExtensionRepos)
//! even when nothing it prints needs it; native evaluates (or reuses from the
//! lockfile) only the extensions whose generated repositories the output
//! shows: `--extension_info=all`, `show_extension`, `show_repo` of a
//! generated repository and `show_repo --all_repos`. Output is the same; an
//! extension that fails is reported only when it is evaluated.
use crate::module_graph::{InspectedModule, ResolutionReason, compare_versions};
use crate::repository::RepositorySet;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

mod args;
mod executor;
mod output;
mod repos;
mod tidy;

pub use args::{ExtensionArg, ModuleArg};

/// ModOptions.OutputFormat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    Graph,
    StreamedProto,
    StreamedJsonproto,
}

impl OutputFormat {
    const ALL: [(&'static str, Self); 5] = [
        ("text", Self::Text),
        ("json", Self::Json),
        ("graph", Self::Graph),
        ("streamed_proto", Self::StreamedProto),
        ("streamed_jsonproto", Self::StreamedJsonproto),
    ];
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = Self::ALL
            .iter()
            .find(|(_, value)| value == self)
            .map_or("text", |(name, _)| name);
        f.write_str(name)
    }
}

/// ModOptions.ExtensionShow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExtensionShow {
    #[default]
    Hidden,
    Usages,
    Repos,
    All,
}

/// ModOptions.Charset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Charset {
    #[default]
    Utf8,
    Ascii,
}

/// ModOptions (bzlmod/modcommand/ModOptions.java), converted like Bazel's
/// options parser. The first conversion error is kept for the command to
/// report (`While parsing option ...`, exit 2) before anything runs.
#[derive(Clone, Debug)]
pub struct ModOptions {
    pub from: Vec<ModuleArg>,
    pub verbose: bool,
    pub include_unused: bool,
    pub extension_filter: Option<Vec<ExtensionArg>>,
    pub extension_info: ExtensionShow,
    pub base_module: ModuleArg,
    pub extension_usages: Vec<ModuleArg>,
    pub depth: i32,
    pub cycles: bool,
    pub include_builtin: bool,
    pub charset: Charset,
    pub output: OutputFormat,
    pub all_repos: bool,
    pub all_visible_repos: bool,
    error: Option<String>,
}

impl Default for ModOptions {
    fn default() -> Self {
        Self {
            from: vec![ModuleArg::root()],
            verbose: false,
            include_unused: false,
            extension_filter: None,
            extension_info: ExtensionShow::Hidden,
            base_module: ModuleArg::root(),
            extension_usages: Vec::new(),
            depth: -1,
            cycles: true,
            include_builtin: false,
            charset: Charset::Utf8,
            output: OutputFormat::Text,
            all_repos: false,
            all_visible_repos: false,
            error: None,
        }
    }
}

/// EnumConverter's message (common/options/EnumConverter.java).
fn enum_value<T: Copy>(value: &str, type_name: &str, choices: &[(&str, T)]) -> Result<T, String> {
    choices
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(value))
        .map(|(_, value)| *value)
        .ok_or_else(|| {
            let names: Vec<&str> = choices.iter().map(|(name, _)| *name).collect();
            let description = match names.split_last() {
                Some((last, [])) => (*last).to_owned(),
                Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
                None => "nothing".to_owned(),
            };
            format!("Not a valid {type_name}: '{value}' (should be {description})")
        })
}

/// Converters.IntegerConverter (Integer.decode).
fn integer(value: &str) -> Result<i32, String> {
    let error = || format!("'{value}' is not an int");
    let (negative, digits) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let (radix, digits) = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
        .or_else(|| digits.strip_prefix('#'))
    {
        (16, hex)
    } else if digits.len() > 1 && digits.starts_with('0') {
        (8, &digits[1..])
    } else {
        (10, digits)
    };
    if digits.is_empty() || digits.starts_with(['-', '+']) {
        return Err(error());
    }
    let magnitude = i64::from_str_radix(digits, radix).map_err(|_| error())?;
    let value = if negative { -magnitude } else { magnitude };
    i32::try_from(value).map_err(|_| error())
}

/// CommaSeparatedNonEmptyOptionListConverter.
fn comma_list(value: &str) -> Result<Vec<&str>, String> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let items: Vec<&str> = value.split(',').collect();
    if items.contains(&"") {
        return Err(
            "Empty values are not allowed as part of this comma-separated list of options"
                .to_owned(),
        );
    }
    Ok(items)
}

impl ModOptions {
    /// One `mod` option: `Some(used_next)` when `arg` is one (a conversion
    /// error is recorded, see [`Self::error`]).
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Option<bool> {
        for (name, slot) in [
            ("verbose", &mut self.verbose),
            ("include_unused", &mut self.include_unused),
            ("cycles", &mut self.cycles),
            ("include_builtin", &mut self.include_builtin),
            ("all_repos", &mut self.all_repos),
            ("all_visible_repos", &mut self.all_visible_repos),
        ] {
            if let Some(value) = crate::configuration::boolean_option(arg, name) {
                match value {
                    Ok(value) => *slot = value,
                    Err(error) => {
                        self.error.get_or_insert(error);
                    }
                }
                return Some(false);
            }
        }
        const VALUED: [&str; 8] = [
            "from",
            "extension_filter",
            "extension_info",
            "base_module",
            "extension_usages",
            "depth",
            "charset",
            "output",
        ];
        let rest = arg.strip_prefix("--")?;
        let (name, value, used) = match rest.split_once('=') {
            Some((name, value)) if VALUED.contains(&name) => (name, value, false),
            None if VALUED.contains(&rest) => (rest, next?, true),
            _ => return None,
        };
        if let Err(error) = self.convert(name, value) {
            self.error
                .get_or_insert(format!("While parsing option --{name}={value}: {error}"));
        }
        Some(used)
    }

    fn convert(&mut self, name: &str, value: &str) -> Result<(), String> {
        let modules = |value: &str| -> Result<Vec<ModuleArg>, String> {
            comma_list(value)?
                .into_iter()
                .map(ModuleArg::parse)
                .collect()
        };
        match name {
            "from" => self.from = modules(value)?,
            "extension_usages" => self.extension_usages = modules(value)?,
            "base_module" => self.base_module = ModuleArg::parse(value)?,
            "extension_filter" => {
                self.extension_filter = Some(
                    comma_list(value)?
                        .into_iter()
                        .map(ExtensionArg::parse)
                        .collect::<Result<_, _>>()?,
                )
            }
            "extension_info" => {
                self.extension_info = enum_value(
                    value,
                    "extension show",
                    &[
                        ("hidden", ExtensionShow::Hidden),
                        ("usages", ExtensionShow::Usages),
                        ("repos", ExtensionShow::Repos),
                        ("all", ExtensionShow::All),
                    ],
                )?
            }
            "depth" => self.depth = integer(value)?,
            "charset" => {
                self.charset = enum_value(
                    value,
                    "output charset",
                    &[("utf8", Charset::Utf8), ("ascii", Charset::Ascii)],
                )?
            }
            "output" => self.output = enum_value(value, "output format", &OutputFormat::ALL)?,
            _ => {}
        }
        Ok(())
    }

    /// The first option conversion error (Bazel's options parser fails the
    /// command with it, exit 2).
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// FailureDetails.ModCommand.Code with its exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Code {
    Unknown,
    MissingArguments,
    TooManyArguments,
    InvalidArguments,
    BuildozerFailed,
    ErrorDuringGraphInspection,
}

impl Code {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Unknown => 37,
            _ => 2,
        }
    }
}

/// A failed `mod` command: the ERROR event's message and the code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub code: Code,
}

impl Failure {
    /// ModCommand.reportAndCreateFailureResult's message.
    pub fn new(message: impl Into<String>, code: Code) -> Self {
        let message = message.into();
        let message = match code {
            Code::MissingArguments | Code::TooManyArguments | Code::InvalidArguments => format!(
                "{message}{} Type 'bazel help mod' for syntax and help.",
                if message.ends_with('.') { "" } else { "." }
            ),
            _ => message,
        };
        Self { message, code }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(message, Code::InvalidArguments)
    }

    fn unknown(message: impl Into<String>) -> Self {
        Self::new(message, Code::Unknown)
    }
}

/// ModSubcommand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Subcommand {
    Deps,
    Graph,
    AllPaths,
    Path,
    Explain,
    ShowRepo,
    ShowExtension,
    DumpRepoMapping,
    Tidy,
}

impl Subcommand {
    const ALL: [(&'static str, Self); 9] = [
        ("deps", Self::Deps),
        ("graph", Self::Graph),
        ("all_paths", Self::AllPaths),
        ("path", Self::Path),
        ("explain", Self::Explain),
        ("show_repo", Self::ShowRepo),
        ("show_extension", Self::ShowExtension),
        ("dump_repo_mapping", Self::DumpRepoMapping),
        ("tidy", Self::Tidy),
    ];

    fn is_graph(self) -> bool {
        matches!(
            self,
            Self::Deps | Self::Graph | Self::AllPaths | Self::Path | Self::Explain
        )
    }

    fn print_values() -> String {
        let names: Vec<&str> = Self::ALL.iter().map(|(name, _)| *name).collect();
        format!("({})", names.join(", "))
    }
}

impl fmt::Display for Subcommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = Self::ALL
            .iter()
            .find(|(_, value)| value == self)
            .map_or("", |(name, _)| name);
        f.write_str(name)
    }
}

/// ModuleKey: name and version (`("", "")` is the root).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ModuleKey {
    pub name: String,
    pub version: String,
}

impl ModuleKey {
    pub fn root() -> Self {
        Self {
            name: String::new(),
            version: String::new(),
        }
    }

    pub fn is_root(&self) -> bool {
        self.name.is_empty() && self.version.is_empty()
    }

    fn from_pair((name, version): &(String, String)) -> Self {
        Self {
            name: name.clone(),
            version: version.clone(),
        }
    }
}

impl Ord for ModuleKey {
    /// ModuleKey.LEXICOGRAPHIC_COMPARATOR.
    fn cmp(&self, other: &Self) -> Ordering {
        self.name
            .cmp(&other.name)
            .then_with(|| compare_versions(&self.version, &other.version))
    }
}

impl PartialOrd for ModuleKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for ModuleKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_root() {
            return f.write_str("<root>");
        }
        let version = if self.version.is_empty() {
            "_"
        } else {
            &self.version
        };
        write!(f, "{}@{version}", self.name)
    }
}

/// BazelModuleInspectorValue.AugmentedModule.
#[derive(Clone, Debug)]
pub(crate) struct Augmented {
    pub name: String,
    pub version: String,
    pub key: ModuleKey,
    pub repo_name: String,
    pub dependants: Vec<ModuleKey>,
    pub original_dependants: Vec<ModuleKey>,
    pub deps: Vec<(String, ModuleKey)>,
    pub unused_deps: Vec<(String, ModuleKey)>,
    pub dep_reasons: BTreeMap<String, ResolutionReason>,
}

impl Augmented {
    fn new(key: &ModuleKey) -> Self {
        Self {
            name: key.name.clone(),
            version: key.version.clone(),
            key: key.clone(),
            repo_name: key.name.clone(),
            dependants: Vec::new(),
            original_dependants: Vec::new(),
            deps: Vec::new(),
            unused_deps: Vec::new(),
            dep_reasons: BTreeMap::new(),
        }
    }

    pub fn is_used(&self) -> bool {
        self.key.is_root() || !self.dependants.is_empty()
    }

    /// getAllDeps: dependency key -> apparent name, sorted by key.
    pub fn all_deps(&self, unused: bool) -> BTreeMap<ModuleKey, String> {
        let mut map = BTreeMap::new();
        for (alias, key) in &self.deps {
            map.insert(key.clone(), alias.clone());
        }
        if unused {
            for (alias, key) in &self.unused_deps {
                map.insert(key.clone(), alias.clone());
            }
        }
        map
    }

    /// deps().inverse().get(key).
    pub fn dep_alias(&self, key: &ModuleKey) -> Option<&str> {
        self.deps
            .iter()
            .find(|(_, dep)| dep == key)
            .map(|(alias, _)| alias.as_str())
    }

    pub fn dep(&self, alias: &str) -> Option<&ModuleKey> {
        self.deps
            .iter()
            .find(|(name, _)| name == alias)
            .map(|(_, key)| key)
    }

    pub fn unused_dep(&self, alias: &str) -> Option<&ModuleKey> {
        self.unused_deps
            .iter()
            .find(|(name, _)| name == alias)
            .map(|(_, key)| key)
    }
}

fn push_unique(set: &mut Vec<ModuleKey>, key: &ModuleKey) {
    if !set.contains(key) {
        set.push(key.clone());
    }
}

/// BazelModuleInspectorValue: the augmented un-pruned graph and its indexes.
pub(crate) struct Inspector {
    pub graph: HashMap<ModuleKey, Augmented>,
    /// Module name -> its keys, sorted.
    pub index: BTreeMap<String, Vec<ModuleKey>>,
    /// Selected module key -> canonical repository name.
    pub canonical: HashMap<ModuleKey, String>,
}

impl Inspector {
    /// BazelModuleInspectorFunction.computeAugmentedGraph.
    fn new(inspection: &[InspectedModule], canonical: HashMap<ModuleKey, String>) -> Self {
        let modules: HashMap<ModuleKey, &InspectedModule> = inspection
            .iter()
            .map(|module| (ModuleKey::from_pair(&module.key), module))
            .collect();
        let mut graph: HashMap<ModuleKey, Augmented> = HashMap::new();
        let loaded = |graph: &mut HashMap<ModuleKey, Augmented>, key: &ModuleKey| {
            let module = modules.get(key).copied();
            graph.entry(key.clone()).or_insert_with(|| {
                let mut augmented = Augmented::new(key);
                if let Some(module) = module {
                    augmented.name = module.name.clone();
                    augmented.version = module.version.clone();
                    augmented.repo_name = module.repo_name.clone();
                }
                augmented
            });
        };
        for module in inspection {
            let parent = ModuleKey::from_pair(&module.key);
            loaded(&mut graph, &parent);
            if let Some(augmented) = graph.get_mut(&parent) {
                augmented.version = module.version.clone();
            }
            for dependency in &module.dependencies {
                let original = ModuleKey::from_pair(&dependency.original);
                let resolved = ModuleKey::from_pair(&dependency.resolved);
                graph
                    .entry(original.clone())
                    .or_insert_with(|| Augmented::new(&original));
                if let Some(found) = modules.get(&original) {
                    let entry = graph.get_mut(&original).expect("inserted above");
                    entry.name = found.name.clone();
                    entry.version = found.version.clone();
                    entry.repo_name = found.repo_name.clone();
                }
                loaded(&mut graph, &resolved);
                push_unique(
                    &mut graph
                        .get_mut(&original)
                        .expect("inserted above")
                        .original_dependants,
                    &parent,
                );
                if module.used {
                    push_unique(
                        &mut graph.get_mut(&resolved).expect("inserted above").dependants,
                        &parent,
                    );
                }
                let parent_entry = graph.get_mut(&parent).expect("inserted above");
                if dependency.reason != ResolutionReason::Original {
                    parent_entry
                        .unused_deps
                        .push((dependency.repo_name.clone(), original.clone()));
                }
                parent_entry
                    .deps
                    .push((dependency.repo_name.clone(), resolved.clone()));
                parent_entry
                    .dep_reasons
                    .insert(dependency.repo_name.clone(), dependency.reason);
            }
        }
        let mut index: BTreeMap<String, Vec<ModuleKey>> = BTreeMap::new();
        for module in graph.values() {
            index
                .entry(module.name.clone())
                .or_default()
                .push(module.key.clone());
        }
        for keys in index.values_mut() {
            keys.sort();
        }
        Self {
            graph,
            index,
            canonical,
        }
    }

    pub fn module(&self, key: &ModuleKey) -> &Augmented {
        &self.graph[key]
    }

    pub fn root(&self) -> &Augmented {
        &self.graph[&ModuleKey::root()]
    }
}

/// ModuleExtensionId: the `.bzl` file's label, the extension name and the
/// optional isolation key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ExtensionId {
    pub label: crate::label::Label,
    pub name: String,
    pub isolation: Option<String>,
}

impl ExtensionId {
    /// ModuleExtensionId.isInnate.
    pub fn is_innate(&self) -> bool {
        self.name.contains(' ')
    }
}

impl Ord for ExtensionId {
    /// ModuleExtensionId.LEXICOGRAPHIC_COMPARATOR.
    fn cmp(&self, other: &Self) -> Ordering {
        (
            &self.label.repository,
            &self.label.package,
            &self.label.name,
        )
            .cmp(&(
                &other.label.repository,
                &other.label.package,
                &other.label.name,
            ))
            .then_with(|| self.name.cmp(&other.name))
            .then_with(|| self.isolation.cmp(&other.isolation))
    }
}

impl PartialOrd for ExtensionId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for ExtensionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}%{}", self.label.canonical_starlark(), self.name)?;
        if let Some(isolation) = &self.isolation {
            write!(f, "%{isolation}")?;
        }
        Ok(())
    }
}

/// ModuleExtensionUsage.Proxy: its location and `use_repo` imports
/// (local name -> exported name, in call order).
#[derive(Clone, Debug)]
pub(crate) struct Proxy {
    /// Module-wide declaration order.
    pub ordinal: usize,
    pub file: String,
    pub line: String,
    pub imports: Vec<(String, String)>,
}

/// ModuleExtensionUsage of one module (all its proxies of one extension).
#[derive(Clone, Debug, Default)]
pub(crate) struct Usage {
    pub proxies: Vec<Proxy>,
    pub tags: Vec<crate::bzlmod::ExtensionTag>,
}

/// The extension usage table (BazelDepGraphValue.getExtensionUsagesTable):
/// extension -> module -> usage, with the extensions' unique names.
#[derive(Default)]
pub(crate) struct Usages {
    pub table: BTreeMap<ExtensionId, BTreeMap<ModuleKey, Usage>>,
    /// Row order (first use in the resolved graph's BFS order).
    pub order: Vec<ExtensionId>,
}

impl Usages {
    /// The extensions `module` uses (column(module).keySet()).
    pub fn of_module(&self, module: &ModuleKey) -> BTreeSet<&ExtensionId> {
        self.table
            .iter()
            .filter(|(_, row)| row.contains_key(module))
            .map(|(id, _)| id)
            .collect()
    }
}

/// Split a Starlark `file:line:col` location.
fn split_location(location: &str) -> (String, String) {
    let mut parts = location.rsplitn(3, ':');
    let _column = parts.next();
    let line = parts.next().unwrap_or_default().to_owned();
    let file = parts.next().unwrap_or_default().to_owned();
    (file, line)
}

/// BazelDepGraphFunction.getExtensionUsagesById over the resolved graph.
fn usages(
    graph: &crate::module_graph::ResolvedGraph,
    ignore_root_dev: bool,
) -> Result<Usages, String> {
    let mut usages = Usages::default();
    let mut owners: Vec<(&str, &crate::bzlmod::ModuleFile, &BTreeMap<String, String>)> =
        vec![("", &graph.root, &graph.root_mapping)];
    for module in graph.modules_in_bfs_order() {
        owners.push((&module.canonical_name, &module.module, &module.mapping));
    }
    for (owner, module, mapping) in owners {
        let key = if owner.is_empty() {
            ModuleKey::root()
        } else {
            let resolved = &graph.modules[owner];
            ModuleKey {
                name: resolved.name.clone(),
                version: resolved.version.clone(),
            }
        };
        let ignore_dev = !owner.is_empty() || ignore_root_dev;
        // A registry module's MODULE.bazel is its registry URL
        // (ModuleFile.create(content, url)), also for a local registry.
        let registry_file = match graph.modules.get(owner).map(|module| &module.source) {
            Some(crate::module_graph::ModuleSource::Registry {
                registry,
                name,
                version,
                ..
            }) => Some((
                registry.location(&format!("modules/{name}/{version}/MODULE.bazel")),
                registry
                    .download_location(&format!("modules/{name}/{version}/MODULE.bazel"))
                    .ok(),
            )),
            _ => None,
        };
        for usage in &module.extension_usages {
            let active: Vec<usize> = usage
                .declarations
                .iter()
                .enumerate()
                .filter(|(_, declaration)| !declaration.dev_dependency || !ignore_dev)
                .map(|(index, _)| index)
                .collect();
            if active.is_empty() {
                continue;
            }
            let label = if usage.identity.is_repo_rule() {
                crate::label::Label {
                    repository: owner.to_owned(),
                    package: String::new(),
                    name: "MODULE.bazel".to_owned(),
                }
            } else {
                let raw = &usage.identity.label;
                let apparent = if raw.starts_with("@@") {
                    &raw[1..]
                } else {
                    raw.as_str()
                };
                let mut base = mapping.clone();
                if owner.is_empty() {
                    base.insert(String::new(), String::new());
                }
                crate::label::Label::parse_in(apparent, owner, "", &base).map_err(|error| {
                    format!(
                        "invalid label for module extension found at {}: {error}",
                        usage.declarations[active[0]].location
                    )
                })?
            };
            // IsolationKey: the module key and the usage's exported proxy
            // name, which the recording evaluator does not keep (its
            // file-local index stands in).
            let isolation = usage
                .identity
                .isolation
                .map(|index| format!("{key}+{index}"));
            let id = ExtensionId {
                label,
                name: usage.identity.extension_name.clone(),
                isolation,
            };
            if !usages.table.contains_key(&id) {
                usages.order.push(id.clone());
            }
            let entry = usages
                .table
                .entry(id)
                .or_default()
                .entry(key.clone())
                .or_default();
            for &index in &active {
                let declaration = &usage.declarations[index];
                let (mut file, line) = split_location(&declaration.location);
                if let Some((path, Some(url))) = &registry_file
                    && file == *path
                {
                    file = url.clone();
                }
                entry.proxies.push(Proxy {
                    ordinal: declaration.ordinal,
                    file,
                    line,
                    imports: usage
                        .imports
                        .iter()
                        .filter(|import| {
                            import.declaration == index && (!import.dev_dependency || !ignore_dev)
                        })
                        .map(|import| (import.local_name.clone(), import.exported_name.clone()))
                        .collect(),
                });
            }
            // Several spellings of one extension merge in declaration order.
            entry.proxies.sort_by_key(|proxy| proxy.ordinal);
            entry.tags.extend(
                usage
                    .tags
                    .iter()
                    .filter(|tag| !tag.dev_dependency || !ignore_dev)
                    .cloned(),
            );
            entry.tags.sort_by_key(|tag| tag.ordinal);
        }
    }
    Ok(usages)
}

/// The data a `mod` subcommand works on.
pub(crate) struct Context<'a> {
    pub repositories: &'a RepositorySet,
    pub inspector: Inspector,
    pub usages: Usages,
}

impl Context<'_> {
    /// The generated repositories of `id` (BazelModuleInspectorValue
    /// .extensionToRepoInternalNames), evaluating the extension.
    pub fn extension_repos(&self, id: &ExtensionId) -> Result<BTreeSet<String>, String> {
        if id.is_innate() {
            let mut names = BTreeSet::new();
            if let Some(row) = self.usages.table.get(id) {
                for usage in row.values() {
                    for tag in &usage.tags {
                        if let Some(name) = tag.attributes.get("name").and_then(|v| v.as_str()) {
                            names.insert(name.to_owned());
                        }
                    }
                }
            }
            return Ok(names);
        }
        Ok(self
            .repositories
            .extension_repository_names(&id.label, &id.name)?
            .unwrap_or_default()
            .into_iter()
            .collect())
    }

    /// The canonical name prefix of `id`'s repositories
    /// (BazelDepGraphValue.getExtensionUniqueNames).
    pub fn unique_name(&self, id: &ExtensionId) -> Option<String> {
        let mut prefixes: BTreeMap<String, ()> = BTreeMap::new();
        for other in &self.usages.order {
            let extension = if other.is_innate() {
                other
                    .name
                    .split_once(' ')
                    .map_or(other.name.as_str(), |(_, rule)| rule)
            } else {
                other.name.as_str()
            };
            let mut attempt = 1;
            let name = loop {
                let disambiguator = if attempt == 1 {
                    String::new()
                } else {
                    attempt.to_string()
                };
                let candidate = format!("{}+{extension}{disambiguator}", other.label.repository);
                if !prefixes.contains_key(&candidate) {
                    break candidate;
                }
                attempt += 1;
            };
            prefixes.insert(name.clone(), ());
            if other == id {
                return Some(name);
            }
        }
        None
    }
}

/// Run `mod` on the invocation's repository set: `residue` begins with the
/// subcommand. Prints to `out`; returns the failure to report.
pub fn run(
    repositories: &RepositorySet,
    options: &ModOptions,
    residue: &[String],
    ignore_dev_dependencies: bool,
    out: &mut dyn std::io::Write,
) -> Result<(), Failure> {
    let Some(first) = residue.first() else {
        return Err(Failure::unknown(format!(
            "No subcommand specified, choose one of : {}.",
            Subcommand::print_values()
        )));
    };
    let Some(subcommand) = Subcommand::ALL
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(first))
        .map(|(_, value)| *value)
    else {
        return Err(Failure::unknown(format!(
            "Invalid subcommand, choose one from : {}.",
            Subcommand::print_values()
        )));
    };
    let args = &residue[1..];
    validate_args(subcommand, options, args)?;
    let mut buffer = Vec::new();
    let result = run_subcommand(
        repositories,
        options,
        subcommand,
        args,
        ignore_dev_dependencies,
        &mut buffer,
    );
    // The printer's charset (ModExecutor: UTF-8 or US-ASCII, unmappable
    // characters replaced by '?').
    let text = if options.charset == Charset::Ascii {
        let text = String::from_utf8_lossy(&buffer);
        text.chars()
            .map(|c| if c.is_ascii() { c } else { '?' })
            .collect::<String>()
            .into_bytes()
    } else {
        buffer
    };
    out.write_all(&text)
        .map_err(|error| Failure::unknown(error.to_string()))?;
    result
}

/// ModCommand.validateArgs.
fn validate_args(
    subcommand: Subcommand,
    options: &ModOptions,
    args: &[String],
) -> Result<(), Failure> {
    match subcommand {
        Subcommand::ShowRepo => {
            if !matches!(
                options.output,
                OutputFormat::Text | OutputFormat::StreamedJsonproto | OutputFormat::StreamedProto
            ) {
                return Err(Failure::invalid(format!(
                    "Invalid --output '{}' for the 'show_repo' subcommand. Only 'text', 'streamed_jsonproto', and 'streamed_proto' are supported.",
                    options.output
                )));
            }
        }
        Subcommand::ShowExtension => {
            if options.output != OutputFormat::Text {
                return Err(Failure::invalid(format!(
                    "Invalid --output '{}' for the 'show_extension' subcommand. Only 'text' is supported.",
                    options.output
                )));
            }
        }
        sub if sub.is_graph()
            && !matches!(
                options.output,
                OutputFormat::Text | OutputFormat::Json | OutputFormat::Graph
            ) =>
        {
            return Err(Failure::invalid(format!(
                "Invalid --output '{}' for the '{sub}' subcommand. Only 'text', 'json', and 'graph' are supported.",
                options.output
            )));
        }
        _ => {}
    }
    if subcommand == Subcommand::ShowRepo {
        let modes = [
            options.all_repos,
            options.all_visible_repos,
            !args.is_empty(),
        ];
        if modes.iter().filter(|mode| **mode).count() > 1 {
            return Err(Failure::new(
                "the 'show_repo' command requires exactly one of --all_repos, --all_visible_repos, or a list of repo arguments",
                Code::TooManyArguments,
            ));
        }
    } else if options.all_repos {
        return Err(Failure::invalid(format!(
            "the '{subcommand}' command doesn't take the --all_repos option"
        )));
    } else if options.all_visible_repos {
        return Err(Failure::invalid(format!(
            "the '{subcommand}' command doesn't take the --all_visible_repos option"
        )));
    }
    Ok(())
}

fn run_subcommand(
    repositories: &RepositorySet,
    options: &ModOptions,
    subcommand: Subcommand,
    args: &[String],
    ignore_dev_dependencies: bool,
    out: &mut Vec<u8>,
) -> Result<(), Failure> {
    if subcommand == Subcommand::DumpRepoMapping {
        return repos::dump_repo_mapping(repositories, args, out);
    }
    let graph = repositories.resolve_graph().map_err(Failure::unknown)?;
    if subcommand == Subcommand::Tidy {
        if !args.is_empty() {
            return Err(Failure::new(
                "the 'tidy' command doesn't take extra arguments",
                Code::TooManyArguments,
            ));
        }
        return tidy::run(repositories, graph, ignore_dev_dependencies, out);
    }
    let mut canonical = HashMap::from([(ModuleKey::root(), String::new())]);
    for module in graph.modules.values() {
        canonical.insert(
            ModuleKey {
                name: module.name.clone(),
                version: module.version.clone(),
            },
            module.canonical_name.clone(),
        );
    }
    let inspector = Inspector::new(&graph.inspection, canonical);
    let usages = usages(graph, ignore_dev_dependencies).map_err(Failure::unknown)?;
    let context = Context {
        repositories,
        inspector,
        usages,
    };
    executor::run(&context, options, subcommand, args, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> ModOptions {
        let mut options = ModOptions::default();
        for arg in args {
            assert!(options.parse(arg, None).is_some(), "{arg}");
        }
        options
    }

    #[test]
    fn options_convert_like_bazel() {
        let options = parsed(&[
            "--output=json",
            "--charset=ASCII",
            "--depth=0x10",
            "--extension_info=all",
            "--noverbose",
            "--include_unused",
            "--from=a,@b,@@c+,d@1.0,<root>",
        ]);
        assert_eq!(options.output, OutputFormat::Json);
        assert_eq!(options.charset, Charset::Ascii);
        assert_eq!(options.depth, 16);
        assert_eq!(options.extension_info, ExtensionShow::All);
        assert!(options.include_unused);
        assert_eq!(options.from.len(), 5);
        assert!(options.error().is_none());
        let options = parsed(&["--charset=latin1", "--output=bogus"]);
        assert_eq!(
            options.error(),
            Some(
                "While parsing option --charset=latin1: Not a valid output charset: 'latin1' (should be utf8 or ascii)"
            )
        );
        let options = parsed(&["--depth=x"]);
        assert_eq!(
            options.error(),
            Some("While parsing option --depth=x: 'x' is not an int")
        );
        let options = parsed(&["--from=a,,b"]);
        assert!(options.error().unwrap().contains("Empty values"));
        assert!(parsed(&["--from="]).from.is_empty());
        let mut options = ModOptions::default();
        assert_eq!(options.parse("--output", Some("graph")), Some(true));
        assert_eq!(options.output, OutputFormat::Graph);
        assert_eq!(options.parse("--unrelated", None), None);
    }

    #[test]
    fn failure_messages_append_the_help_hint() {
        assert_eq!(
            Failure::invalid("x").message,
            "x. Type 'bazel help mod' for syntax and help."
        );
        assert_eq!(
            Failure::invalid("x.").message,
            "x. Type 'bazel help mod' for syntax and help."
        );
        assert_eq!(Failure::unknown("y").message, "y");
        assert_eq!(Code::Unknown.exit_code(), 37);
    }

    #[test]
    fn module_keys_sort_by_name_then_version() {
        let key = |name: &str, version: &str| ModuleKey {
            name: name.into(),
            version: version.into(),
        };
        let mut keys = [
            key("b", "1.10"),
            key("b", ""),
            key("a", "2"),
            key("b", "1.2"),
            ModuleKey::root(),
        ];
        keys.sort();
        let shown: Vec<String> = keys.iter().map(ToString::to_string).collect();
        assert_eq!(shown, ["<root>", "a@2", "b@1.2", "b@1.10", "b@_"]);
    }
}
