//! ModExecutor (bzlmod/modcommand/ModExecutor.java) and the argument
//! resolution half of ModCommand.execInternal.
use super::{
    Code, Context, ExtensionId, ExtensionShow, Failure, ModOptions, ModuleKey, Subcommand,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

/// ResultNode.NodeMetadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Edge {
    pub expanded: bool,
    pub indirect: bool,
    pub cycle: bool,
}

/// ResultNode: whether the module is a target, and its children (a
/// SetMultimap: keys in first-insertion order, each with its distinct edges).
#[derive(Clone, Debug, Default)]
pub(crate) struct ResultNode {
    pub target: bool,
    pub children: Vec<(ModuleKey, Vec<Edge>)>,
}

impl ResultNode {
    fn put(&mut self, key: &ModuleKey, edge: Edge) {
        match self.children.iter_mut().find(|(child, _)| child == key) {
            Some((_, edges)) => {
                if !edges.contains(&edge) {
                    edges.push(edge);
                }
            }
            None => self.children.push((key.clone(), vec![edge])),
        }
    }

    fn add_child(&mut self, key: &ModuleKey, expanded: bool, indirect: bool) {
        self.put(
            key,
            Edge {
                expanded,
                indirect,
                cycle: false,
            },
        );
    }

    fn add_cycle(&mut self, key: &ModuleKey) {
        self.put(
            key,
            Edge {
                expanded: false,
                indirect: false,
                cycle: true,
            },
        );
    }

    fn add_children(&mut self, other: &ResultNode) {
        for (key, edges) in &other.children {
            for edge in edges {
                self.put(key, *edge);
            }
        }
    }

    /// The number of (child, edge) entries.
    pub fn size(&self) -> usize {
        self.children.iter().map(|(_, edges)| edges.len()).sum()
    }

    fn entries(&self) -> impl Iterator<Item = (&ModuleKey, Edge)> {
        self.children
            .iter()
            .flat_map(|(key, edges)| edges.iter().map(move |edge| (key, *edge)))
    }

    /// getChildrenSortedByKey: an ImmutableSortedSet by key only, so only
    /// the first edge of each child survives.
    pub fn sorted_by_key(&self) -> Vec<(ModuleKey, Edge)> {
        let mut children: Vec<(ModuleKey, Edge)> = self
            .children
            .iter()
            .map(|(key, edges)| (key.clone(), edges[0]))
            .collect();
        children.sort_by(|a, b| a.0.cmp(&b.0));
        children
    }

    /// getChildrenSortedByEdgeType: cycles first, then unexpanded, then
    /// direct, then by key.
    pub fn sorted_by_edge_type(&self) -> Vec<(ModuleKey, Edge)> {
        let mut children: Vec<(ModuleKey, Edge)> = self
            .entries()
            .map(|(key, edge)| (key.clone(), edge))
            .collect();
        children.sort_by(|(a, x), (b, y)| {
            y.cycle
                .cmp(&x.cycle)
                .then(x.expanded.cmp(&y.expanded))
                .then(x.indirect.cmp(&y.indirect))
                .then_with(|| a.cmp(b))
        });
        children.dedup();
        children
    }
}

pub(crate) type ResultGraph = BTreeMap<ModuleKey, ResultNode>;

/// Extension -> exported repository name -> the modules importing it.
pub(crate) type RepoImports = BTreeMap<ExtensionId, BTreeMap<String, BTreeSet<ModuleKey>>>;

/// `None`: no filter; `Some(None)`: every extension; `Some(Some(set))`.
pub(crate) type ExtensionFilter = Option<Option<BTreeSet<ExtensionId>>>;

pub(crate) struct Executor<'a> {
    pub context: &'a Context<'a>,
    pub options: &'a ModOptions,
    pub filter: ExtensionFilter,
    pub repo_imports: RepoImports,
    /// Generated repositories of the extensions the output shows.
    pub extension_repos: BTreeMap<ExtensionId, BTreeSet<String>>,
}

impl<'a> Executor<'a> {
    fn new(context: &'a Context<'a>, options: &'a ModOptions, filter: ExtensionFilter) -> Self {
        let mut executor = Self {
            context,
            options,
            filter,
            repo_imports: BTreeMap::new(),
            extension_repos: BTreeMap::new(),
        };
        let present: HashSet<ModuleKey> = context.inspector.graph.keys().cloned().collect();
        executor.repo_imports = executor.compute_repo_imports(&present);
        executor
    }

    fn filter_contains(&self, id: &ExtensionId) -> bool {
        match &self.filter {
            None | Some(None) => true,
            Some(Some(set)) => set.contains(id),
        }
    }

    /// computeRepoImportsTable.
    fn compute_repo_imports(&self, present: &HashSet<ModuleKey>) -> RepoImports {
        let mut result = BTreeMap::new();
        for (id, row) in &self.context.usages.table {
            if !self.filter_contains(id) {
                continue;
            }
            let mut imports: BTreeMap<String, BTreeSet<ModuleKey>> = BTreeMap::new();
            for (module, usage) in row {
                if !present.contains(module) {
                    continue;
                }
                for proxy in &usage.proxies {
                    for (_, exported) in &proxy.imports {
                        imports
                            .entry(exported.clone())
                            .or_default()
                            .insert(module.clone());
                    }
                }
            }
            result.insert(id.clone(), imports);
        }
        result
    }

    fn filter_builtin(&self, key: &ModuleKey) -> bool {
        self.options.include_builtin || !is_builtin(key)
    }

    fn filter_unused(&self, key: &ModuleKey) -> bool {
        self.options.include_unused || self.context.inspector.module(key).is_used()
    }

    fn dependencies(&self, key: &ModuleKey) -> Vec<ModuleKey> {
        self.context
            .inspector
            .module(key)
            .all_deps(self.options.include_unused)
            .into_keys()
            .filter(|key| self.filter_builtin(key))
            .collect()
    }

    fn root_direct_children(&self) -> BTreeSet<ModuleKey> {
        self.context
            .inspector
            .root()
            .all_deps(self.options.include_unused)
            .into_keys()
            .collect()
    }

    /// computeExtensionFilterTargets.
    fn extension_filter_targets(&self) -> Vec<ModuleKey> {
        let Some(filter) = &self.filter else {
            return Vec::new();
        };
        let mut keys: Vec<ModuleKey> = self
            .context
            .inspector
            .graph
            .keys()
            .filter(|key| self.filter_unused(key) && self.filter_builtin(key))
            .filter(|key| {
                let used = self.context.usages.of_module(key);
                match filter {
                    None => !used.is_empty(),
                    Some(set) => used.iter().any(|id| set.contains(*id)),
                }
            })
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    pub fn graph(&mut self, from: &[ModuleKey]) -> ResultGraph {
        let targets = self.extension_filter_targets();
        if targets.is_empty() {
            self.expand_and_prune(from)
        } else {
            self.expand_paths_to_targets(from, &targets, false)
        }
    }

    /// path / all_paths: `None` prints the given message instead.
    pub fn paths(
        &mut self,
        from: &[ModuleKey],
        to: &[ModuleKey],
        single: bool,
    ) -> Result<ResultGraph, &'static str> {
        let mut targets = self.extension_filter_targets();
        for key in to {
            if !targets.contains(key) {
                targets.push(key.clone());
            }
        }
        if targets.is_empty() {
            return Err("No target modules specified.");
        }
        let result = self.expand_paths_to_targets(from, &targets, single);
        if result.is_empty() {
            return Err("No path found to the specified target modules.");
        }
        Ok(result)
    }

    /// addPathToResultGraph.
    fn add_path(
        &self,
        result: &mut HashMap<ModuleKey, ResultNode>,
        parents: &HashMap<ModuleKey, Option<ModuleKey>>,
        parent: Option<ModuleKey>,
        child: &ModuleKey,
    ) {
        let mut node = ResultNode::default();
        if let Some(existing) = result.get(child) {
            node.add_children(existing);
        }
        node.target = true;
        result.insert(child.clone(), node);
        let root_direct = self.root_direct_children();
        let mut current_child = child.clone();
        let mut current_parent = parent;
        while let Some(parent) = current_parent {
            let mut node = ResultNode::default();
            if let Some(existing) = result.get(&parent) {
                node.add_children(existing);
                node.target = existing.target;
            }
            let indirect = parent.is_root() && !root_direct.contains(&current_child);
            node.add_child(&current_child, true, indirect);
            result.insert(parent.clone(), node);
            current_child = parent;
            current_parent = parents.get(&current_child).cloned().flatten();
        }
    }

    /// expandPathsToTargets.
    fn expand_paths_to_targets(
        &mut self,
        from: &[ModuleKey],
        targets: &[ModuleKey],
        single: bool,
    ) -> ResultGraph {
        let targets_set: HashSet<&ModuleKey> = targets.iter().collect();
        let mut parents: HashMap<ModuleKey, Option<ModuleKey>> = HashMap::new();
        let mut sorted: Vec<&ModuleKey> =
            from.iter().filter(|key| self.filter_builtin(key)).collect();
        sorted.sort();
        for key in sorted {
            parents.insert(key.clone(), Some(ModuleKey::root()));
        }
        parents.insert(ModuleKey::root(), None);
        let mut result: HashMap<ModuleKey, ResultNode> = HashMap::new();
        let mut queue: VecDeque<ModuleKey> = from.iter().cloned().collect();
        let mut found: HashSet<ModuleKey> = HashSet::new();
        let all_found =
            |found: &HashSet<ModuleKey>| targets.iter().all(|target| found.contains(target));
        while let Some(current) = queue.front().cloned() {
            if single && all_found(&found) {
                break;
            }
            queue.pop_front();
            if targets_set.contains(&current) && !(single && found.contains(&current)) {
                let parent = parents.get(&current).cloned().flatten();
                self.add_path(&mut result, &parents, parent, &current);
                found.insert(current.clone());
                if single && all_found(&found) {
                    break;
                }
            }
            if !self.context.inspector.graph.contains_key(&current) {
                continue;
            }
            for dependency in self.dependencies(&current) {
                if targets_set.contains(&dependency) && !(single && found.contains(&dependency)) {
                    self.add_path(&mut result, &parents, Some(current.clone()), &dependency);
                    found.insert(dependency.clone());
                }
                if !parents.contains_key(&dependency) {
                    parents.insert(dependency.clone(), Some(current.clone()));
                    queue.push_back(dependency);
                }
            }
        }
        let targets = Some(targets_set.into_iter().cloned().collect::<HashSet<_>>());
        self.prune(targets, result.into_iter().collect())
    }

    /// expandAndPrune.
    fn expand_and_prune(&mut self, from: &[ModuleKey]) -> ResultGraph {
        let root_direct = self.root_direct_children();
        let mut pinned: BTreeSet<ModuleKey> = from.iter().cloned().collect();
        if pinned.remove(&ModuleKey::root()) {
            pinned.extend(root_direct.iter().cloned());
        }
        let pinned: Vec<ModuleKey> = pinned
            .into_iter()
            .filter(|key| self.filter_builtin(key))
            .collect();
        let mut full: HashMap<ModuleKey, ResultNode> = HashMap::new();
        let mut root = ResultNode::default();
        for child in &pinned {
            root.add_child(child, true, !root_direct.contains(child));
        }
        full.insert(ModuleKey::root(), root);
        let mut visited: HashSet<ModuleKey> = pinned.iter().cloned().collect();
        visited.insert(ModuleKey::root());
        let mut queue: VecDeque<ModuleKey> = pinned.into_iter().collect();
        while let Some(current) = queue.pop_front() {
            let mut node = ResultNode::default();
            for dependency in self.dependencies(&current) {
                if visited.contains(&dependency) {
                    node.add_child(&dependency, false, false);
                } else {
                    node.add_child(&dependency, true, false);
                    visited.insert(dependency.clone());
                    queue.push_back(dependency);
                }
            }
            full.insert(current, node);
        }
        self.prune(None, full)
    }

    /// ResultGraphPruner.pruneByDepth; `targets` None is the complete set.
    fn prune(
        &mut self,
        targets: Option<HashSet<ModuleKey>>,
        old: HashMap<ModuleKey, ResultNode>,
    ) -> ResultGraph {
        if old.is_empty() {
            return ResultGraph::new();
        }
        let mut pruner = Pruner {
            old: &old,
            result: HashMap::new(),
            stack: HashSet::new(),
            targets: &targets,
            depth: self.options.depth,
            cycles: self.options.cycles,
        };
        let root_key = ModuleKey::root();
        pruner
            .result
            .insert(root_key.clone(), ResultNode::default());
        pruner.stack.insert(root_key.clone());
        if let Some(root) = old.get(&root_key) {
            for (child, edge) in root.sorted_by_key() {
                pruner.result.get_mut(&root_key).expect("root").add_child(
                    &child,
                    true,
                    edge.indirect,
                );
                pruner.visit_visible(&child, 1, &root_key, true);
            }
        }
        let result: ResultGraph = pruner.result.into_iter().collect();
        let present: HashSet<ModuleKey> = result.keys().cloned().collect();
        self.repo_imports = self.compute_repo_imports(&present);
        result
    }
}

struct Pruner<'o> {
    old: &'o HashMap<ModuleKey, ResultNode>,
    result: HashMap<ModuleKey, ResultNode>,
    stack: HashSet<ModuleKey>,
    targets: &'o Option<HashSet<ModuleKey>>,
    depth: i32,
    cycles: bool,
}

impl Pruner<'_> {
    fn old_node(&self, key: &ModuleKey) -> ResultNode {
        self.old.get(key).cloned().unwrap_or_default()
    }

    fn visit_visible(&mut self, key: &ModuleKey, depth: i32, parent: &ModuleKey, expanded: bool) {
        self.stack.insert(key.clone());
        let old = self.old_node(key);
        self.result.entry(key.clone()).or_default().target = old.target;
        if depth > 1 {
            self.result
                .entry(parent.clone())
                .or_default()
                .add_child(key, expanded, false);
        }
        if !expanded {
            self.stack.remove(key);
            return;
        }
        for (child, edge) in old.sorted_by_key() {
            if !self.stack.contains(&child) {
                if depth < self.depth {
                    self.visit_visible(&child, depth + 1, key, edge.expanded);
                } else if self.targets.is_some() {
                    self.visit_detached(&child, key, key, edge.expanded);
                }
            } else if self.cycles {
                self.result
                    .entry(key.clone())
                    .or_default()
                    .add_cycle(&child);
            }
        }
        self.stack.remove(key);
    }

    fn is_target_parent(&self, node: &ResultNode) -> bool {
        let Some(targets) = self.targets else {
            return true;
        };
        node.children
            .iter()
            .filter(|(key, _)| !self.stack.contains(key))
            .any(|(key, _)| targets.contains(key))
    }

    fn visit_detached(
        &mut self,
        key: &ModuleKey,
        parent: &ModuleKey,
        last_visible: &ModuleKey,
        expanded: bool,
    ) {
        self.stack.insert(key.clone());
        let old = self.old_node(key);
        let mut local = ResultNode {
            target: old.target,
            children: Vec::new(),
        };
        let mut in_result = false;
        let mut last_visible = last_visible.clone();
        if old.target || self.is_target_parent(&old) {
            let indirect = last_visible != *parent;
            self.result
                .entry(last_visible.clone())
                .or_default()
                .add_child(key, expanded, indirect);
            self.result.insert(key.clone(), std::mem::take(&mut local));
            in_result = true;
            last_visible = key.clone();
        }
        if !expanded {
            self.stack.remove(key);
            return;
        }
        for (child, edge) in old.sorted_by_key() {
            if !self.stack.contains(&child) {
                self.visit_detached(&child, key, &last_visible, edge.expanded);
            } else if self.cycles {
                if in_result {
                    self.result
                        .entry(key.clone())
                        .or_default()
                        .add_cycle(&child);
                } else {
                    local.add_cycle(&child);
                }
            }
        }
        self.stack.remove(key);
    }
}

/// ModExecutor.isBuiltin.
pub(crate) fn is_builtin(key: &ModuleKey) -> bool {
    key.name == "bazel_tools" && key.version.is_empty()
}

/// The second half of ModCommand.execInternal: arguments, then the query.
pub(crate) fn run(
    context: &Context<'_>,
    options: &ModOptions,
    subcommand: Subcommand,
    args: &[String],
    out: &mut Vec<u8>,
) -> Result<(), Failure> {
    let inspector = &context.inspector;
    let root = inspector.root();
    let base_keys = options
        .base_module
        .resolve_to_module_keys(inspector, root, false, false)
        .and_then(|keys| {
            if keys.len() > 1 {
                let shown: Vec<String> = keys.iter().map(ToString::to_string).collect();
                Err(Failure {
                    message: format!(
                        "The --base_module option can only specify exactly one module version, choose one of: {}.",
                        shown.join(", ")
                    ),
                    code: Code::InvalidArguments,
                })
            } else {
                Ok(keys)
            }
        })
        .map_err(|error| {
            Failure::invalid(format!(
                "In --base_module {} option: {} (Note that unused modules cannot be used here)",
                options.base_module, error.message
            ))
        })?;
    let base_key = base_keys[0].clone();
    let base = inspector.module(&base_key);
    let base_canonical = inspector
        .canonical
        .get(&base_key)
        .cloned()
        .unwrap_or_default();
    let mut modules: Vec<ModuleKey> = Vec::new();
    let mut extensions: BTreeSet<ExtensionId> = BTreeSet::new();
    let mut repos: Vec<(String, String)> = Vec::new();
    match subcommand {
        Subcommand::Graph => {
            if !args.is_empty() {
                return Err(Failure::new(
                    "the 'graph' command doesn't take extra arguments",
                    Code::TooManyArguments,
                ));
            }
        }
        Subcommand::ShowRepo => {
            repos = super::repos::repos_to_show(context, options, &base_canonical, args)?;
        }
        Subcommand::ShowExtension => {
            for arg in args {
                let resolved = super::ExtensionArg::parse(arg)
                    .map_err(Failure::invalid)
                    .and_then(|extension| extension.resolve(inspector, base));
                match resolved {
                    Ok(id) => {
                        extensions.insert(id);
                    }
                    Err(error) => {
                        return Err(Failure::invalid(format!(
                            "In extension argument {arg}: {}",
                            strip_hint(&error.message)
                        )));
                    }
                }
            }
        }
        _ => {
            for arg in args {
                let keys = super::ModuleArg::parse(arg)
                    .map_err(Failure::invalid)
                    .and_then(|module| {
                        module.resolve_to_module_keys(inspector, base, options.include_unused, true)
                    })
                    .map_err(|error| {
                        Failure::invalid(format!(
                            "In module argument {arg}: {}",
                            strip_hint(&error.message)
                        ))
                    })?;
                for key in keys {
                    if !modules.contains(&key) {
                        modules.push(key);
                    }
                }
            }
        }
    }
    let list = |arguments: &[super::ModuleArg]| -> Result<Vec<ModuleKey>, Failure> {
        let mut keys = Vec::new();
        for argument in arguments {
            for key in
                argument.resolve_to_module_keys(inspector, base, options.include_unused, true)?
            {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        Ok(keys)
    };
    let joined = |arguments: &[super::ModuleArg]| {
        let shown: Vec<String> = arguments.iter().map(ToString::to_string).collect();
        format!("[{}]", shown.join(", "))
    };
    let from = list(&options.from).map_err(|error| {
        Failure::invalid(format!(
            "In --from {} option: {}",
            joined(&options.from),
            strip_hint(&error.message)
        ))
    })?;
    let usage_keys = list(&options.extension_usages).map_err(|error| {
        Failure::invalid(format!(
            "In --extension_usages {} option: {} (Note that unused modules cannot be used here)",
            joined(&options.extension_usages),
            strip_hint(&error.message)
        ))
    })?;
    let mut filter: ExtensionFilter = None;
    if subcommand.is_graph()
        && let Some(arguments) = &options.extension_filter
    {
        if arguments.is_empty() {
            filter = Some(None);
        } else {
            let mut ids = BTreeSet::new();
            for argument in arguments {
                ids.insert(argument.resolve(inspector, base).map_err(|error| {
                    let shown: Vec<String> = arguments.iter().map(ToString::to_string).collect();
                    Failure::invalid(format!(
                        "In --extension_filter [{}] option: {}",
                        shown.join(", "),
                        strip_hint(&error.message)
                    ))
                })?);
            }
            filter = Some(Some(ids));
        }
    }
    let mut definitions = Vec::new();
    if subcommand == Subcommand::ShowRepo {
        for (key, canonical) in &repos {
            match super::repos::definition(context, canonical) {
                Ok(Some(definition)) => definitions.push((key.clone(), definition)),
                Ok(None) => {
                    return Err(Failure::invalid(format!(
                        "In repo argument {key}: no such repo"
                    )));
                }
                Err(error) => return Err(Failure::invalid(error)),
            }
        }
    }
    let mut options = options.clone();
    if options.depth < 1 {
        options.depth = match subcommand {
            Subcommand::Explain => 1,
            Subcommand::Deps => 2,
            _ => i32::MAX,
        };
    }
    let mut executor = Executor::new(context, &options, filter);
    let text = |out: &mut Vec<u8>, message: &str| {
        out.extend_from_slice(message.as_bytes());
        out.push(b'\n');
    };
    match subcommand {
        Subcommand::Graph | Subcommand::Deps => {
            let start = if subcommand == Subcommand::Graph {
                &from
            } else {
                &modules
            };
            let result = executor.graph(start);
            output(&mut executor, &result, out)?;
        }
        Subcommand::Path | Subcommand::AllPaths | Subcommand::Explain => {
            match executor.paths(&from, &modules, subcommand == Subcommand::Path) {
                Ok(result) => output(&mut executor, &result, out)?,
                Err(message) => text(out, message),
            }
        }
        Subcommand::ShowRepo => {
            super::repos::show_repo(&options, &definitions, out)?;
        }
        Subcommand::ShowExtension => {
            for id in &extensions {
                super::repos::show_extension(&mut executor, id, &usage_keys, out)?;
            }
        }
        Subcommand::DumpRepoMapping | Subcommand::Tidy => {}
    }
    Ok(())
}

/// Messages nested in another argument error carry no help hint of their own.
fn strip_hint(message: &str) -> &str {
    message
        .strip_suffix(" Type 'bazel help mod' for syntax and help.")
        .map(|message| message.strip_suffix('.').unwrap_or(message))
        .unwrap_or(message)
}

/// The formatter of `--output`, after evaluating the extensions whose
/// generated repositories it shows.
fn output(
    executor: &mut Executor<'_>,
    result: &ResultGraph,
    out: &mut Vec<u8>,
) -> Result<(), Failure> {
    if executor.options.extension_info == ExtensionShow::All {
        // Only the extensions of the displayed modules show repositories.
        let ids: Vec<ExtensionId> = executor
            .repo_imports
            .iter()
            .filter(|(_, imports)| !imports.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        let mut failed = 0;
        for id in ids {
            match executor.context.extension_repos(&id) {
                Ok(names) => {
                    executor.extension_repos.insert(id, names);
                }
                Err(error) => {
                    crate::ui::note(&format!("ERROR: {error}"));
                    failed += 1;
                }
            }
        }
        super::output::format(executor, result, out);
        if failed > 0 {
            return Err(Failure::new(
                format!(
                    "Results may be incomplete as {failed} extension{} failed.",
                    if failed == 1 { "" } else { "s" }
                ),
                Code::ErrorDuringGraphInspection,
            ));
        }
        return Ok(());
    }
    super::output::format(executor, result, out);
    Ok(())
}
