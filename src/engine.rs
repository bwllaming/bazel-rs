//! A deliberately small native analysis/execution pipeline, not Skyframe.
use crate::action::{self, Action};
use crate::analysis::{self, DependencyTarget};
use crate::label::{Label, PackageId};
use crate::package::{self, Package, Rule, RuleKind};
use crate::repository::RepositorySet;
use crate::scheduler::{self, Dependency, PlannedAction};
use crate::starlark_types::File;
use crate::workspace;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug)]
struct Artifact {
    real: PathBuf,
    exec: PathBuf,
    producer: Option<(String, usize)>,
    owner: String,
}

impl Artifact {
    fn file(&self) -> File {
        let relative = self
            .exec
            .strip_prefix("bazel-out/rust-fastbuild/bin")
            .ok()
            .filter(|_| self.producer.is_some())
            .unwrap_or(&self.exec);
        // External short paths are metadata, never filesystem lookup paths.
        let short_path = if Label::parse_canonical(&self.owner)
            .is_ok_and(|label| !label.repository.is_empty())
        {
            relative
                .strip_prefix("external")
                .map(|path| PathBuf::from("..").join(path))
                .unwrap_or_else(|_| relative.to_path_buf())
        } else {
            relative.to_path_buf()
        };
        File {
            path: self.exec.to_string_lossy().into_owned(),
            short_path: short_path.to_string_lossy().into_owned(),
            owner: self.owner.clone(),
            is_source: self.producer.is_none(),
        }
    }
}

type Locations = BTreeMap<String, Vec<PathBuf>>;
type ResolvedInputs = (Vec<Artifact>, Locations);

#[derive(Clone)]
enum Target {
    Rule(Rule),
    Output(Rule, String),
    Source(PathBuf),
}

#[derive(Debug, Default)]
pub struct BuildSummary {
    pub outputs: Vec<(Label, PathBuf)>,
    pub executed: usize,
    pub cache_hits: usize,
}

pub struct Engine {
    repositories: Arc<RepositorySet>,
    packages: BTreeMap<PackageId, Package>,
    generated: BTreeMap<Label, (Label, String)>,
}

impl Engine {
    pub fn new(root: PathBuf) -> Self {
        Self::with_repositories(Arc::new(RepositorySet::new(root)))
    }

    pub fn with_repositories(repositories: Arc<RepositorySet>) -> Self {
        Self {
            repositories,
            packages: BTreeMap::new(),
            generated: BTreeMap::new(),
        }
    }

    fn load_package(&mut self, package: &PackageId) -> Result<(), String> {
        if !self.packages.contains_key(package) {
            let rules = package::evaluate_in(self.repositories.clone(), package)?;
            for rule in rules.values() {
                for out in &rule.outs {
                    self.validate_output_directory(package, out)?;
                    let output = rule.label.with_name(out)?;
                    self.generated
                        .insert(output, (rule.label.clone(), out.clone()));
                }
            }
            self.packages.insert(package.to_owned(), rules);
        }
        Ok(())
    }

    fn validate_output_directory(&self, package: &PackageId, out: &str) -> Result<(), String> {
        let mut directory = self
            .repositories
            .root(&package.repository)?
            .join(&package.package);
        let components = out.split('/').collect::<Vec<_>>();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            directory.push(component);
            match std::fs::symlink_metadata(&directory) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(format!(
                        "output {out} has an unsupported symlink source-tree parent {}",
                        directory.display()
                    ));
                }
                // Generated outputs inhabit another tree. A regular source file
                // or missing path cannot contain a source subpackage boundary.
                Ok(metadata) if !metadata.is_dir() => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(format!(
                        "cannot inspect output package boundary {}: {error}",
                        directory.display()
                    ));
                }
            }
            if directory.join("BUILD.bazel").is_file()
                || directory.join("BUILD").is_file()
                || workspace::has_marker(&directory)?
            {
                return Err(format!(
                    "output {out} crosses a package or nested workspace boundary at {}",
                    directory.display()
                ));
            }
        }
        Ok(())
    }

    fn target(&mut self, label: &Label) -> Result<Target, String> {
        self.load_package(&label.package_id())?;
        if let Some(rule) = self.packages[&label.package_id()].get(label) {
            return Ok(Target::Rule(rule.clone()));
        }
        if let Some((owner, output)) = self.generated.get(label) {
            return Ok(Target::Output(
                self.packages[&owner.package_id()][owner].clone(),
                output.clone(),
            ));
        }
        let package = &self.packages[&label.package_id()];
        let declared_source = package.exports.contains_key(label)
            || package.values().any(|rule| {
                rule.srcs
                    .iter()
                    .any(|src| self.resolve_rule_src(rule, src).as_ref() == Ok(label))
            });
        if !declared_source {
            return Err(format!(
                "source target {label} is not declared by exports_files or a rule in its package"
            ));
        }
        let package_dir = self
            .repositories
            .root(&label.repository)?
            .join(&label.package);
        let mut path = package_dir.clone();
        let parts: Vec<_> = Path::new(&label.name).components().collect();
        for (i, component) in parts.iter().enumerate() {
            if !matches!(component, Component::Normal(_)) {
                return Err(format!("unsafe source label {label}"));
            }
            path.push(component.as_os_str());
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|e| format!("no target or source {label}: {e}"))?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "source symlinks are not supported: {}",
                    path.display()
                ));
            }
            if i + 1 < parts.len()
                && (path.join("BUILD.bazel").is_file()
                    || path.join("BUILD").is_file()
                    || workspace::has_marker(&path)?)
            {
                return Err(format!(
                    "{label} crosses a package or nested workspace boundary at {}",
                    path.display()
                ));
            }
        }
        if !path.is_file() {
            return Err(format!("source is not a regular file: {label}"));
        }
        Ok(Target::Source(path))
    }

    fn resolve_rule_src(&self, rule: &Rule, text: &str) -> Result<Label, String> {
        if rule.kind == RuleKind::Custom {
            // Custom attributes were canonicalized during package evaluation.
            // In particular, //p:t here denotes main, even in an external rule.
            Label::parse_canonical(text)
        } else {
            self.repositories
                .parse(text, &rule.label.repository, &rule.label.package)
        }
    }

    /// Only literal labels, :all, //... and //package/...; not the query language.
    pub fn select(
        &mut self,
        patterns: &[String],
        current_package: &str,
    ) -> Result<Vec<Label>, String> {
        let mut selected = BTreeSet::new();
        for pattern in patterns {
            if (pattern.starts_with("//") || pattern.starts_with('@')) && pattern.ends_with("/...")
            {
                let prefix = pattern.strip_suffix("...").unwrap();
                let prefix = if prefix.ends_with("//") {
                    prefix
                } else {
                    prefix.strip_suffix('/').unwrap()
                };
                let context = self.repositories.parse(
                    &format!("{prefix}:__pattern__"),
                    "",
                    current_package,
                )?;
                let root = self.repositories.root(&context.repository)?;
                for package in workspace::discover_packages(root, &context.package)? {
                    let id = PackageId {
                        repository: context.repository.clone(),
                        package,
                    };
                    self.load_package(&id)?;
                    selected.extend(self.packages[&id].keys().cloned());
                }
            } else if let Some(prefix) = pattern.strip_suffix(":all") {
                if !prefix.is_empty() && !prefix.starts_with("//") && !prefix.starts_with('@') {
                    return Err(format!("unsupported target pattern: {pattern}"));
                }
                let id = self
                    .repositories
                    .parse(pattern, "", current_package)?
                    .package_id();
                self.load_package(&id)?;
                selected.extend(self.packages[&id].keys().cloned());
            } else {
                if pattern.contains(['(', ')', '*', ' ']) {
                    return Err(format!(
                        "unsupported query expression or target pattern: {pattern}"
                    ));
                }
                let label = self.repositories.parse(pattern, "", current_package)?;
                self.target(&label)?;
                selected.insert(label);
            }
        }
        Ok(selected.into_iter().collect())
    }

    fn analyze(
        &mut self,
        label: &Label,
        active: &mut Vec<Label>,
        done: &mut BTreeSet<Label>,
        plan: &mut Vec<Rule>,
    ) -> Result<(), String> {
        let rule = match self.target(label)? {
            Target::Source(_) => return Ok(()),
            Target::Rule(rule) | Target::Output(rule, _) => rule,
        };
        if done.contains(&rule.label) {
            return Ok(());
        }
        if active.contains(&rule.label) {
            active.push(rule.label);
            return Err(format!(
                "dependency cycle: {}",
                active
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" -> ")
            ));
        }
        if active.len() >= 256 {
            return Err("native prototype dependency depth limit (256) exceeded".into());
        }
        active.push(rule.label.clone());
        for src in &rule.srcs {
            let dependency = self.resolve_rule_src(&rule, src)?;
            if dependency.package_id() != rule.label.package_id() {
                let visible = match self.target(&dependency)? {
                    Target::Rule(dep) | Target::Output(dep, _) => dep
                        .visibility
                        .allows_in(&dep.label.package_id(), &rule.label.package_id()),
                    Target::Source(_) => {
                        let package = &self.packages[&dependency.package_id()];
                        package
                            .exports
                            .get(&dependency)
                            .unwrap_or(&package.default_visibility)
                            .allows_in(&dependency.package_id(), &rule.label.package_id())
                    }
                };
                if !visible {
                    return Err(format!("{dependency} is not visible to {}", rule.label));
                }
            }
            self.analyze(&dependency, active, done, plan)?;
        }
        active.pop();
        done.insert(rule.label.clone());
        plan.push(rule);
        Ok(())
    }

    fn artifacts(
        &mut self,
        label: &Label,
        built: &BTreeMap<Label, Vec<Artifact>>,
        declared: &BTreeMap<PathBuf, Artifact>,
    ) -> Result<Vec<Artifact>, String> {
        match self.target(label)? {
            Target::Source(real) => Ok(vec![Artifact {
                real,
                exec: label.source_exec_path(),
                producer: None,
                owner: label.to_string(),
            }]),
            Target::Rule(rule) => built
                .get(&rule.label)
                .cloned()
                .ok_or_else(|| format!("internal analysis order error: {label}")),
            Target::Output(rule, out) => {
                let exec = rule.label.with_name(&out)?.generated_exec_path();
                declared
                    .get(&exec)
                    .cloned()
                    .map(|artifact| vec![artifact])
                    .ok_or_else(|| format!("missing analyzed output {label}"))
            }
        }
    }

    fn inputs(
        &mut self,
        rule: &Rule,
        built: &BTreeMap<Label, Vec<Artifact>>,
        declared: &BTreeMap<PathBuf, Artifact>,
    ) -> Result<ResolvedInputs, String> {
        let mut inputs = Vec::new();
        let mut seen: BTreeMap<PathBuf, Artifact> = BTreeMap::new();
        let mut locations = BTreeMap::new();
        for src in &rule.srcs {
            let label = self.resolve_rule_src(rule, src)?;
            let artifacts = self.artifacts(&label, built, declared)?;
            let paths = artifacts.iter().map(|a| a.exec.clone()).collect::<Vec<_>>();
            locations.insert(label.to_string(), paths.clone());
            locations.insert(src.clone(), paths);
            for artifact in artifacts {
                if let Some(previous) = seen.get(&artifact.exec) {
                    if previous.real != artifact.real
                        || previous.producer != artifact.producer
                        || previous.owner != artifact.owner
                    {
                        return Err(format!(
                            "different artifacts map to the same execution path: {}",
                            artifact.exec.display()
                        ));
                    }
                } else {
                    seen.insert(artifact.exec.clone(), artifact.clone());
                    inputs.push(artifact);
                }
            }
        }
        Ok((inputs, locations))
    }

    pub fn build(&mut self, labels: &[Label], cache: &Path) -> Result<BuildSummary, String> {
        self.build_with_jobs(labels, cache, 1)
    }

    pub fn build_with_jobs(
        &mut self,
        labels: &[Label],
        cache: &Path,
        jobs: usize,
    ) -> Result<BuildSummary, String> {
        if jobs == 0 {
            return Err("jobs must be greater than zero".into());
        }
        let mut plan = Vec::new();
        let mut done = BTreeSet::new();
        for label in labels {
            self.analyze(label, &mut Vec::new(), &mut done, &mut plan)?;
        }
        // Validate every command before executing any action. The dry artifacts
        // carry the same exec paths as the actual generated files.
        let mut dry = BTreeMap::new();
        let mut custom_providers: BTreeMap<
            Label,
            BTreeMap<String, starlark::values::OwnedFrozenValue>,
        > = BTreeMap::new();
        let mut provider_files: BTreeMap<Label, Vec<File>> = BTreeMap::new();
        // File metadata is not a source filesystem path. Keep the real artifact
        // identity even when a custom provider forwards a non-default file.
        let mut artifact_index: BTreeMap<File, Artifact> = BTreeMap::new();
        let mut declared = BTreeMap::new();
        let mut producers: BTreeMap<PathBuf, Label> = BTreeMap::new();
        let mut nodes = Vec::new();
        for rule in &plan {
            if rule.kind == RuleKind::Custom {
                let mut dependency_targets = BTreeMap::new();
                let mut available = BTreeMap::new();
                for src in &rule.srcs {
                    let label = self.resolve_rule_src(rule, src)?;
                    let artifacts = self.artifacts(&label, &dry, &declared)?;
                    let files = artifacts.iter().map(Artifact::file).collect();
                    for artifact in artifacts {
                        artifact_index.insert(artifact.file(), artifact.clone());
                        available.insert(artifact.file(), artifact);
                    }
                    let target_provider_files =
                        provider_files.get(&label).cloned().unwrap_or_default();
                    for file in &target_provider_files {
                        let artifact = artifact_index.get(file).ok_or_else(|| {
                            format!(
                                "provider file has no declared artifact identity: {}",
                                file.path
                            )
                        })?;
                        available.insert(file.clone(), artifact.clone());
                    }
                    dependency_targets.insert(
                        label.to_string(),
                        DependencyTarget {
                            label: label.to_string(),
                            files,
                            is_source: matches!(self.target(&label)?, Target::Source(_)),
                            providers: custom_providers.get(&label).cloned().unwrap_or_default(),
                            provider_files: target_provider_files,
                        },
                    );
                }
                let analyzed =
                    analysis::analyze_in(rule, &dependency_targets, self.repositories.clone())?;
                custom_providers.insert(rule.label.clone(), analyzed.providers.clone());
                provider_files.insert(rule.label.clone(), analyzed.provider_files.clone());
                for file in &analyzed.declared {
                    let package_output = PathBuf::from("bazel-out/rust-fastbuild/bin")
                        .join(rule.label.package_exec_path());
                    let short_name = Path::new(&file.path)
                        .strip_prefix(&package_output)
                        .map_err(|_| format!("output outside declaring package: {}", file.path))?;
                    rule.label.with_name(&short_name.to_string_lossy())?;
                    self.validate_output_directory(
                        &rule.label.package_id(),
                        &short_name.to_string_lossy(),
                    )?;
                }
                // First index EVERY action's outputs, so registration order is not
                // an execution order and forward generated dependencies are legal.
                for registered in &analyzed.actions {
                    for (index, file) in registered.outputs.iter().enumerate() {
                        let exec = PathBuf::from(&file.path);
                        register_output(&mut producers, &exec, &rule.label)?;
                        let artifact = Artifact {
                            real: exec.clone(),
                            exec: exec.clone(),
                            producer: Some((registered.action.id.clone(), index)),
                            owner: file.owner.clone(),
                        };
                        available.insert(file.clone(), artifact.clone());
                        artifact_index.insert(file.clone(), artifact.clone());
                        declared.insert(exec, artifact);
                    }
                }
                for file in &analyzed.provider_files {
                    if !available.contains_key(file) {
                        return Err(format!(
                            "{}: provider file has no declared artifact identity: {}",
                            rule.label, file.path
                        ));
                    }
                }
                for registered in analyzed.actions {
                    let inputs = registered
                        .inputs
                        .iter()
                        .map(|file| {
                            available.get(file).cloned().ok_or_else(|| {
                                format!(
                                    "{}: input has no declared artifact: {}",
                                    rule.label, file.path
                                )
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    nodes.push(plan_action(registered.action, inputs)?);
                }
                let defaults = analyzed
                    .files
                    .iter()
                    .map(|file| {
                        available.get(file).cloned().ok_or_else(|| {
                            format!(
                                "{}: DefaultInfo file has no artifact: {}",
                                rule.label, file.path
                            )
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                dry.insert(rule.label.clone(), defaults);
                continue;
            }
            let (inputs, mut locations) = self.inputs(rule, &dry, &declared)?;
            for artifact in &inputs {
                artifact_index.insert(artifact.file(), artifact.clone());
            }
            let outputs = rule
                .outs
                .iter()
                .map(|o| {
                    rule.label
                        .with_name(o)
                        .map(|label| label.generated_exec_path())
                })
                .collect::<Result<Vec<_>, _>>()?;
            if rule.kind == RuleKind::Genrule {
                add_output_locations(rule, &outputs, &mut locations)?;
                let command = expand_command_in(
                    rule.cmd.as_deref().ok_or("genrule has no command")?,
                    &inputs.iter().map(|a| a.exec.clone()).collect::<Vec<_>>(),
                    &outputs,
                    &locations,
                    &rule.label,
                    &self.repositories,
                )?;
                for output in &outputs {
                    register_output(&mut producers, output, &rule.label)?;
                }
                let action = Action {
                    id: rule.label.to_string(),
                    command,
                    inputs: Vec::new(),
                    outputs: outputs.clone(),
                    write: None,
                    program: None,
                    shell_strict: true,
                    arguments: Vec::new(),
                    environment: BTreeMap::new(),
                };
                nodes.push(plan_action(action, inputs)?);
                let artifacts = outputs
                    .into_iter()
                    .enumerate()
                    .map(|(index, exec)| Artifact {
                        real: exec.clone(),
                        exec,
                        producer: Some((rule.label.to_string(), index)),
                        owner: rule.label.to_string(),
                    })
                    .collect::<Vec<_>>();
                for artifact in &artifacts {
                    artifact_index.insert(artifact.file(), artifact.clone());
                    declared.insert(artifact.exec.clone(), artifact.clone());
                }
                dry.insert(rule.label.clone(), artifacts);
            } else {
                dry.insert(rule.label.clone(), inputs);
            }
        }
        let mut requested = Vec::new();
        for label in labels {
            for artifact in self.artifacts(label, &dry, &declared)? {
                requested.push((label.clone(), artifact));
            }
        }
        // Analyze everything first, but execute only the actions reachable from
        // requested outputs. Empty DefaultInfo must not execute unused actions.
        let by_id: BTreeMap<_, _> = nodes
            .iter()
            .map(|node| (node.action.id.clone(), node))
            .collect();
        let mut todo = requested
            .iter()
            .filter_map(|(_, artifact)| artifact.producer.as_ref().map(|(id, _)| id.clone()))
            .collect::<Vec<_>>();
        let mut required = BTreeSet::new();
        while let Some(id) = todo.pop() {
            if required.insert(id.clone()) {
                let node = by_id
                    .get(&id)
                    .ok_or_else(|| format!("missing producing action: {id}"))?;
                todo.extend(
                    node.dependencies
                        .iter()
                        .map(|dependency| dependency.action_id.clone()),
                );
            }
        }
        nodes.retain(|node| required.contains(&node.action.id));
        let results = scheduler::execute_graph(cache, nodes, jobs)?;
        let mut summary = BuildSummary::default();
        for result in results.values() {
            if result.cache_hit {
                summary.cache_hits += 1;
            } else {
                summary.executed += 1;
            }
        }
        for (label, mut artifact) in requested {
            if let Some((id, index)) = &artifact.producer {
                artifact.real = results[id].outputs[*index].clone();
            }
            summary.outputs.push((label, artifact.real));
        }
        Ok(summary)
    }
}

fn register_output(
    producers: &mut BTreeMap<PathBuf, Label>,
    output: &Path,
    owner: &Label,
) -> Result<(), String> {
    for ancestor in output.ancestors() {
        if let Some(previous) = producers.get(ancestor) {
            return Err(format!(
                "overlapping output paths: {} from {previous} conflicts with {} from {owner}",
                ancestor.display(),
                output.display()
            ));
        }
    }
    if let Some((previous_path, previous)) = producers.range(output.to_path_buf()..).next()
        && previous_path.starts_with(output)
    {
        return Err(format!(
            "overlapping output paths: {} from {owner} conflicts with {} from {previous}",
            output.display(),
            previous_path.display()
        ));
    }
    producers.insert(output.to_path_buf(), owner.clone());
    Ok(())
}

fn plan_action(mut action: Action, inputs: Vec<Artifact>) -> Result<PlannedAction, String> {
    action.inputs = inputs
        .iter()
        .map(|artifact| (artifact.real.clone(), artifact.exec.clone()))
        .collect();
    action::validate_structure(&action)?;
    action.inputs.clear();
    let mut dependencies = Vec::new();
    let mut seen = BTreeMap::new();
    for artifact in inputs {
        if let Some(previous) = seen.insert(artifact.exec.clone(), artifact.file()) {
            if previous != artifact.file() {
                return Err(format!(
                    "different artifacts map to the same execution path: {}",
                    artifact.exec.display()
                ));
            }
            continue;
        }
        if let Some((action_id, output_index)) = artifact.producer {
            dependencies.push(Dependency {
                action_id,
                output_index,
                destination: artifact.exec,
            });
        } else {
            action.inputs.push((artifact.real, artifact.exec));
        }
    }
    Ok(PlannedAction {
        action,
        dependencies,
    })
}

fn add_output_locations(
    rule: &Rule,
    outputs: &[PathBuf],
    locations: &mut BTreeMap<String, Vec<PathBuf>>,
) -> Result<(), String> {
    for (name, path) in rule.outs.iter().zip(outputs) {
        locations.insert(name.clone(), vec![path.clone()]);
        locations.insert(format!(":{name}"), vec![path.clone()]);
        locations.insert(rule.label.with_name(name)?.to_string(), vec![path.clone()]);
    }
    Ok(())
}

fn paths_string(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

fn single(paths: &[PathBuf], variable: &str) -> Result<String, String> {
    if paths.len() != 1 {
        return Err(format!(
            "{variable} requires exactly one file, found {}",
            paths.len()
        ));
    }
    Ok(paths[0].to_string_lossy().into_owned())
}

#[cfg(test)]
fn expand_command(
    command: &str,
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    locations: &BTreeMap<String, Vec<PathBuf>>,
    package: &str,
) -> Result<String, String> {
    expand_command_in(
        command,
        inputs,
        outputs,
        locations,
        &Label::parse(":__command__", package)?,
        &RepositorySet::new(PathBuf::new()),
    )
}

fn expand_command_in(
    command: &str,
    inputs: &[PathBuf],
    outputs: &[PathBuf],
    locations: &BTreeMap<String, Vec<PathBuf>>,
    label: &Label,
    repositories: &RepositorySet,
) -> Result<String, String> {
    let rule_dir = PathBuf::from("bazel-out/rust-fastbuild/bin").join(label.package_exec_path());
    let mut expanded = String::new();
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        if c != '$' {
            expanded.push(c);
            continue;
        }
        match chars.next() {
            Some('$') => expanded.push('$'),
            Some('@') => expanded.push_str(&single(outputs, "$@")?),
            Some('<') => expanded.push_str(&single(inputs, "$<")?),
            Some('(') => {
                let mut variable = String::new();
                loop {
                    match chars.next() {
                        Some(')') => break,
                        Some(c) => variable.push(c),
                        None => return Err("unterminated genrule Make variable".into()),
                    }
                }
                let value = match variable.as_str() {
                    "SRCS" => paths_string(inputs),
                    "OUTS" => paths_string(outputs),
                    "RULEDIR" => rule_dir.to_string_lossy().into_owned(),
                    "@D" => {
                        if outputs.len() == 1 {
                            outputs[0]
                                .parent()
                                .ok_or("output has no parent")?
                                .to_string_lossy()
                                .into_owned()
                        } else {
                            rule_dir.to_string_lossy().into_owned()
                        }
                    }
                    _ => {
                        let (multiple, target) =
                            if let Some(t) = variable.strip_prefix("locations ") {
                                (true, t.trim())
                            } else if let Some(t) = variable.strip_prefix("location ") {
                                (false, t.trim())
                            } else {
                                return Err(format!(
                                    "unsupported genrule Make variable: $({variable})"
                                ));
                            };
                        let canonical = repositories
                            .parse(target, &label.repository, &label.package)?
                            .to_string();
                        let paths = locations
                            .get(target)
                            .or_else(|| locations.get(&canonical))
                            .ok_or_else(|| {
                                format!("location is not a declared src or out: {target}")
                            })?;
                        if multiple {
                            let mut sorted = paths.clone();
                            sorted.sort();
                            sorted.dedup();
                            paths_string(&sorted)
                        } else {
                            single(paths, &variable)?
                        }
                    }
                };
                expanded.push_str(&value);
            }
            Some(c) => {
                return Err(format!(
                    "unsupported Make variable ${c}; escape shell dollars with $$"
                ));
            }
            None => return Err("unescaped trailing $ in genrule command".into()),
        }
    }
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_artifact_short_paths_are_metadata_only() {
        for (exec, producer, expected) in [
            ("external/dep+/p/in", None, "../dep+/p/in"),
            (
                "bazel-out/rust-fastbuild/bin/external/dep+/p/out",
                Some(("action".into(), 0)),
                "../dep+/p/out",
            ),
        ] {
            let artifact = Artifact {
                real: PathBuf::from("/real/local/root/p/in"),
                exec: PathBuf::from(exec),
                producer,
                owner: "@@dep+//p:target".into(),
            };
            assert_eq!(artifact.file().short_path, expected);
            assert_eq!(artifact.file().path, exec);
        }
        let main = Artifact {
            real: PathBuf::from("/root/external/dep/in"),
            exec: PathBuf::from("external/dep/in"),
            producer: None,
            owner: "//external/dep:in".into(),
        };
        assert_eq!(main.file().short_path, "external/dep/in");
    }

    #[test]
    fn custom_canonical_main_sources_are_not_rebased_to_external() {
        let engine = Engine::new(PathBuf::new());
        let mut rule = Rule {
            label: Label::parse_canonical("@@dep+//p:target").unwrap(),
            kind: RuleKind::Custom,
            srcs: vec![],
            outs: vec![],
            cmd: None,
            visibility: Default::default(),
            custom: None,
        };
        assert_eq!(
            engine.resolve_rule_src(&rule, "//p:in").unwrap().repository,
            ""
        );
        rule.kind = RuleKind::Filegroup;
        assert_eq!(
            engine.resolve_rule_src(&rule, "//p:in").unwrap().repository,
            "dep+"
        );
    }

    #[test]
    fn external_make_locations_and_rule_directory_use_execution_paths() {
        let label = Label::parse_canonical("@@dep+//p:target").unwrap();
        let input = label.with_name("in").unwrap().source_exec_path();
        let output = label.with_name("nested/out").unwrap().generated_exec_path();
        let locations = BTreeMap::from([("@@dep+//p:in".into(), vec![input])]);
        let expanded = expand_command_in(
            "$(location //p:in) $(location :in) $(RULEDIR) $(@D)",
            &[],
            &[output],
            &locations,
            &label,
            &RepositorySet::new(PathBuf::new()),
        )
        .unwrap();
        assert_eq!(
            expanded,
            "external/dep+/p/in external/dep+/p/in bazel-out/rust-fastbuild/bin/external/dep+/p bazel-out/rust-fastbuild/bin/external/dep+/p/nested"
        );
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = PathBuf::from("/tmp/opencode").join(format!(
                "engine-repositories-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn write(&self, path: &str, contents: &str) {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn repository_patterns_and_native_build_keep_equal_packages_separate() {
        let f = Fixture::new();
        f.write("main/MODULE.bazel", "module(name='main')\nbazel_dep(name='dep',version='1',repo_name='alias')\nlocal_path_override(module_name='dep',path='../dep')\n");
        f.write("dep/MODULE.bazel", "module(name='dep')\n");
        let build =
            "genrule(name='copy',srcs=['in'],outs=['out'],cmd='cat $(location :in) > $@')\n";
        f.write("main/p/BUILD", build);
        f.write("dep/p/BUILD", build);
        f.write("main/p/in", "main contents");
        f.write("dep/p/in", "dependency contents");
        let mut engine = Engine::new(f.0.join("main"));
        let main = engine.select(&["//...".into()], "").unwrap();
        assert_eq!(
            main.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["//p:copy"]
        );
        let external = engine.select(&["@alias//p:all".into()], "").unwrap();
        assert_eq!(
            external.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["@@dep+//p:copy"]
        );
        for pattern in ["@alias//...", "@@dep+//p/..."] {
            assert_eq!(engine.select(&[pattern.into()], "").unwrap(), external);
        }
        let labels = main.into_iter().chain(external).collect::<Vec<_>>();
        let summary = engine.build(&labels, &f.0.join("cache")).unwrap();
        assert_eq!(summary.executed, 2);
        for (label, path) in summary.outputs {
            let expected = if label.repository.is_empty() {
                "main contents"
            } else {
                "dependency contents"
            };
            assert_eq!(std::fs::read_to_string(path).unwrap(), expected);
        }
    }

    #[test]
    fn make_expansion_is_strict() {
        let srcs = vec![PathBuf::from("pkg/in")];
        let outs = vec![PathBuf::from("out/file")];
        assert_eq!(
            expand_command(
                "cat $< > $@; echo $$HOME $(SRCS) $(@D)",
                &srcs,
                &outs,
                &BTreeMap::new(),
                "pkg"
            )
            .unwrap(),
            "cat pkg/in > out/file; echo $HOME pkg/in out"
        );
        for cmd in [
            "$HOME",
            "$(UNKNOWN)",
            "$(location :not_declared)",
            "$(SRCS",
            "$",
        ] {
            assert!(
                expand_command(cmd, &srcs, &outs, &BTreeMap::new(), "pkg").is_err(),
                "{cmd}"
            );
        }
        assert!(expand_command("$<", &[], &outs, &BTreeMap::new(), "pkg").is_err());
    }
}
