//! The analysis scope of `bazel-rs affected` (docs/AFFECTED_TARGETS.md §8):
//! the requested targets whose loading-phase dependency closure in the main
//! repository reaches a changed package. A requested target outside it has
//! the same analysis at both revisions, so only the scope is analysed.
//!
//! The orchestrator (crate::affected) writes the input file
//! ([`crate::affected::SCOPE_VAR`]): the changed packages, the changed
//! `.bzl` files and the *roots* — the main-repository labels the
//! configuration names outside the loading graph (flags, registered
//! toolchains and execution platforms, transition outputs, module
//! extension and repository attributes). After the target pattern phase
//! this module
//!
//! 1. walks the main-repository dependency graph forward from the selected
//!    targets and the roots — every label an attribute names (all
//!    `select()` branches and keys, implicit and default attributes,
//!    `nodep` labels, visibility), toolchain types, attribute aspects'
//!    dependencies, an output file's generating rule, a package group's
//!    includes; external repositories are leaves (not loaded: nothing is
//!    fetched for them);
//! 2. marks every rule of a package with rule changes (its BUILD file, its
//!    globs or its subpackages changed) or whose BUILD file transitively
//!    loads a changed `.bzl` file, and every changed source file, and
//!    propagates the marks to the dependants;
//! 3. when a root reaches a mark, the change can reach any target through
//!    toolchain resolution or the configuration: the scope is everything
//!    (`full`); otherwise the selected targets are restricted to the marked
//!    ones (`restrict`) or only reported (`report`).
//!
//! The result is written next to the digests file (`<digests>.scope`).

use super::*;
use crate::custom_rule::{AttrValue, ConfigurablePart};
use std::collections::{HashMap, HashSet, VecDeque};

/// What the orchestrator asked for.
#[derive(Debug, Default)]
struct Spec {
    /// Packages whose rules may differ: their BUILD file changed, a file
    /// was added or removed in them (globs), or a subpackage appeared or
    /// disappeared below them.
    packages: HashSet<String>,
    /// Changed `.bzl` files (workspace-relative paths).
    bzl: HashSet<String>,
    /// Changed files, and every directory holding one.
    files: HashSet<String>,
    roots: Vec<String>,
    restrict: bool,
}

fn read_spec(path: &std::path::Path) -> Result<Spec, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut lines = text.lines();
    if lines.next() != Some(crate::affected::SCOPE_HEADER) {
        return Err(format!("{}: not an affected scope file", path.display()));
    }
    let mut spec = Spec::default();
    for line in lines {
        match line.split_once('\t') {
            Some(("package", value)) => {
                spec.packages.insert(value.to_owned());
            }
            Some(("bzl", value)) => {
                spec.bzl.insert(value.to_owned());
            }
            Some(("file", value)) => {
                let mut path = value;
                spec.files.insert(path.to_owned());
                while let Some((parent, _)) = path.rsplit_once('/') {
                    spec.files.insert(parent.to_owned());
                    path = parent;
                }
            }
            Some(("root", value)) => spec.roots.push(value.to_owned()),
            Some(("mode", "restrict")) => spec.restrict = true,
            Some(("mode", "report")) => spec.restrict = false,
            _ if line.is_empty() => {}
            _ => return Err(format!("{}: bad line {line:?}", path.display())),
        }
    }
    Ok(spec)
}

/// Every label an attribute value names, whatever the attribute's kind
/// (conservative: `nodep` labels too).
fn value_labels(value: &AttrValue, out: &mut Vec<String>) {
    match value {
        AttrValue::Label(Some(label)) => out.push(label.clone()),
        AttrValue::Labels(labels) => out.extend(labels.iter().cloned()),
        AttrValue::LabelKeyedStringDict(entries) => {
            out.extend(entries.iter().map(|(label, _)| label.clone()))
        }
        AttrValue::StringKeyedLabelDict(entries) => {
            out.extend(entries.iter().map(|(_, label)| label.clone()))
        }
        AttrValue::LabelListDict(entries) => out.extend(
            entries
                .iter()
                .flat_map(|(_, labels)| labels.iter().cloned()),
        ),
        _ => {}
    }
}

/// The workspace path of a main-repository source file label.
fn scope_path(label: &Label) -> String {
    if label.package.is_empty() {
        label.name.clone()
    } else {
        format!("{}/{}", label.package, label.name)
    }
}

/// The scope's outcome.
pub(crate) enum Outcome {
    /// Scoped: the selected labels in the scope.
    Scoped(BTreeSet<Label>),
    /// A root reaches a change: everything is in scope.
    Full(String),
}

impl Engine {
    /// Applies the scope of `bazel-rs affected` to a target pattern phase
    /// when [`crate::affected::SCOPE_VAR`] is set (nothing otherwise).
    pub(super) fn apply_affected_scope(
        &mut self,
        mut phase: TargetPatternPhase,
    ) -> Result<TargetPatternPhase, String> {
        let Some(input) = std::env::var_os(crate::affected::SCOPE_VAR) else {
            return Ok(phase);
        };
        let started = std::time::Instant::now();
        let spec = read_spec(std::path::Path::new(&input))?;
        let mut selected: Vec<Label> = phase.build.clone();
        selected.extend(phase.tests.iter().flatten().cloned());
        let outcome = self.affected_scope(&spec, &selected);
        let mut text = String::new();
        let (kept, total) = (
            match &outcome {
                Outcome::Scoped(scope) => scope.len(),
                Outcome::Full(_) => selected.len(),
            },
            selected.len(),
        );
        match &outcome {
            Outcome::Full(reason) => {
                text.push_str(&format!("full\t{reason}\n"));
            }
            Outcome::Scoped(scope) => {
                text.push_str("scoped\n");
                for label in scope {
                    text.push_str(&format!("{label}\n"));
                }
                if spec.restrict {
                    phase.build.retain(|label| scope.contains(label));
                    if let Some(tests) = &mut phase.tests {
                        tests.retain(|label| scope.contains(label));
                    }
                }
            }
        }
        if let Some(digests) = std::env::var_os(crate::affected::DIGESTS_VAR) {
            let mut path = std::path::PathBuf::from(digests).into_os_string();
            path.push(".scope");
            std::fs::write(&path, text).map_err(|error| {
                format!(
                    "cannot write {}: {error}",
                    std::path::Path::new(&path).display()
                )
            })?;
        }
        eprintln!(
            "bazel-rs affected: scope {} of {total} selected target(s){} in {:.1} s",
            kept,
            match &outcome {
                Outcome::Full(reason) => format!(" (full: {reason})"),
                Outcome::Scoped(_) if spec.restrict => String::new(),
                Outcome::Scoped(_) => " (reported, not restricted)".to_owned(),
            },
            started.elapsed().as_secs_f64()
        );
        Ok(phase)
    }

    /// The forward edges of a main-repository label (see the module docs),
    /// and whether it is a source file (or a missing target: a leaf).
    fn scope_edges(&mut self, label: &Label) -> (bool, Vec<Label>) {
        let id = label.package_id();
        if self.load_package(&id).is_err() {
            return (false, Vec::new());
        }
        let parse = |engine: &Engine, text: &str| {
            Label::parse_canonical(text).or_else(|_| {
                engine
                    .repositories
                    .parse(text, &label.repository, &label.package)
            })
        };
        let package = &self.packages[&id];
        if let Some(rule) = package.rules.get(label) {
            let rule = rule.clone();
            let mut texts = Vec::new();
            let mut edges: Vec<Label> = rule.visibility.group_labels().cloned().collect();
            if let crate::package::RuleKind::PackageGroup(group) = &rule.kind {
                edges.extend(group.includes.iter().cloned());
            }
            for attribute in query_attrs::rule_attributes(&rule, package) {
                if let Some(value) = &attribute.value {
                    value_labels(value, &mut texts);
                }
                if let Some(default) = &attribute.default {
                    value_labels(default, &mut texts);
                }
                for part in attribute.select.iter().flatten() {
                    match part {
                        ConfigurablePart::Plain(value) => value_labels(value, &mut texts),
                        ConfigurablePart::Select { branches, .. } => {
                            for (key, value) in branches {
                                if !query_attrs::is_default_condition(key) {
                                    texts.push(key.clone());
                                }
                                if let Some(value) = value {
                                    value_labels(value, &mut texts);
                                }
                            }
                        }
                    }
                }
                for aspect in &attribute.aspects {
                    texts.extend(
                        query_attrs::aspect_labels(aspect)
                            .into_iter()
                            .map(|(text, _)| text),
                    );
                }
            }
            texts.extend(query_attrs::toolchain_types(&rule));
            for text in texts {
                if let Ok(target) = parse(self, &text) {
                    edges.push(target);
                }
            }
            return (false, edges);
        }
        if let Some((owner, _)) = self.generated.get(label) {
            return (false, vec![owner.clone()]);
        }
        (true, Vec::new())
    }

    /// Whether the BUILD file of the loaded package `id` loads a changed
    /// `.bzl` file, transitively.
    fn scope_loads_changed(&self, id: &PackageId, bzl: &HashSet<String>) -> bool {
        if bzl.is_empty() {
            return false;
        }
        let Some(package) = self.packages.get(id) else {
            return false;
        };
        let path = |load: &Label| {
            if load.package.is_empty() {
                load.name.clone()
            } else {
                format!("{}/{}", load.package, load.name)
            }
        };
        let mut stack = package.direct_loads.clone();
        let mut visited = HashSet::new();
        while let Some(load) = stack.pop() {
            if !visited.insert(load.clone()) {
                continue;
            }
            if load.repository.is_empty() && bzl.contains(&path(&load)) {
                return true;
            }
            if let Some(loads) = package.load_graph.get(&load) {
                stack.extend(loads.iter().cloned());
            }
        }
        false
    }

    fn affected_scope(&mut self, spec: &Spec, selected: &[Label]) -> Outcome {
        // The roots: labels and patterns (`:all`, `/...`), main repository.
        let mut roots: Vec<Label> = Vec::new();
        for root in &spec.roots {
            if root.ends_with(":all")
                || root.ends_with(":*")
                || root.ends_with(":all-targets")
                || root.ends_with("/...")
                || root == "//..."
            {
                // A pattern naming nothing names no configuration value.
                if let Ok(labels) = self.select(std::slice::from_ref(root), "") {
                    roots.extend(labels);
                }
            } else if let Ok(label) = self.repositories.parse(root, "", "") {
                // A literal that is no label is no configuration value.
                roots.push(label);
            }
        }
        roots.retain(|label| label.repository.is_empty());
        // 1. The forward graph from the selected targets and the roots.
        let mut index: HashMap<Label, usize> = HashMap::new();
        let mut nodes: Vec<Label> = Vec::new();
        let mut parents: Vec<Vec<usize>> = Vec::new();
        let mut sources: HashSet<usize> = HashSet::new();
        let mut queue = VecDeque::new();
        let add = |label: &Label,
                   index: &mut HashMap<Label, usize>,
                   nodes: &mut Vec<Label>,
                   parents: &mut Vec<Vec<usize>>,
                   queue: &mut VecDeque<usize>|
         -> usize {
            if let Some(&at) = index.get(label) {
                return at;
            }
            let at = nodes.len();
            index.insert(label.clone(), at);
            nodes.push(label.clone());
            parents.push(Vec::new());
            queue.push_back(at);
            at
        };
        for label in selected.iter().chain(&roots) {
            if label.repository.is_empty() {
                add(label, &mut index, &mut nodes, &mut parents, &mut queue);
            }
        }
        while let Some(at) = queue.pop_front() {
            let label = nodes[at].clone();
            let (source, edges) = self.scope_edges(&label);
            if source {
                sources.insert(at);
            }
            for edge in edges {
                // External repositories are leaves.
                if !edge.repository.is_empty() {
                    continue;
                }
                let to = add(&edge, &mut index, &mut nodes, &mut parents, &mut queue);
                parents[to].push(at);
            }
        }
        // 2. The marks: the targets of changed packages (and of those loading
        // a changed .bzl), changed source files (a source directory holding
        // one), propagated to the dependants. A modified file changes no
        // rule: rules depend on file names (globs), never on contents.
        let mut changed_packages: HashMap<PackageId, bool> = HashMap::new();
        let mut marked = vec![false; nodes.len()];
        let mut stack = Vec::new();
        for (at, label) in nodes.iter().enumerate() {
            let id = label.package_id();
            let changed = match changed_packages.get(&id) {
                Some(&changed) => changed,
                None => {
                    let changed = spec.packages.contains(&id.package)
                        || self.scope_loads_changed(&id, &spec.bzl);
                    changed_packages.insert(id.clone(), changed);
                    changed
                }
            };
            let changed =
                changed || (sources.contains(&at) && spec.files.contains(&scope_path(label)));
            if changed {
                marked[at] = true;
                stack.push(at);
            }
        }
        while let Some(at) = stack.pop() {
            for &parent in &parents[at] {
                if !marked[parent] {
                    marked[parent] = true;
                    stack.push(parent);
                }
            }
        }
        // 3. A root reaching a change: everything.
        // A root that is a source file is a repository input or a flag
        // value read by path: only its own change matters (and a global
        // one at that, crate::affected_changes); its package's rule changes
        // do not.
        for root in &roots {
            if let Some(&at) = index.get(root)
                && marked[at]
                && (!sources.contains(&at) || spec.files.contains(&scope_path(root)))
            {
                return Outcome::Full(format!(
                    "{root} (a flag, toolchain, platform or transition root) depends on a change"
                ));
            }
        }
        // A selected target of another repository is kept (conservative).
        Outcome::Scoped(
            selected
                .iter()
                .filter(|label| {
                    !label.repository.is_empty() || index.get(*label).is_some_and(|&at| marked[at])
                })
                .cloned()
                .collect(),
        )
    }
}
