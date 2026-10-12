//! Local-only Bzlmod discovery and minimal version selection.
//!
//! Discovery reads every reachable requested MODULE version before selection;
//! selection upgrades by module name (Bazel 9.3 compatibility levels are no-ops), rewrites edges,
//! and prunes unreachable modules. It never materializes repositories or executes
//! extensions. Nodep discovery reaches a fixed point before MVS and multiple-
//! version snapping; nodep-only reachability validates but does not retain repos.
//! Registry source.json/downloads and patches are unsupported. Implicit built-in
//! bazel_tools is not injected or fabricated: this graph covers explicit deps.
//!
//! Semantics: https://bazel.build/external/module#version-selection
//! Relaxed versions: https://bazel.build/external/module#version_format
use crate::bzlmod::{self, Dependency, ModuleFile, ModuleOverride};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

const MAX_DISCOVERED_MODULES: usize = 10_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolverOptions {
    /// Local registry roots, searched in order. No implicit remote registry.
    pub registries: Vec<PathBuf>,
    pub ignore_dev_dependencies: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedGraph {
    pub root: ModuleFile,
    /// Only reachable selected modules; keys are opaque canonical repo names.
    /// The root is separate and has canonical repo name "".
    pub modules: BTreeMap<String, ResolvedModule>,
    pub root_mapping: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedModule {
    pub name: String,
    /// Normalized selected version. Local overrides have the empty version;
    /// their declared version is retained in module.module instead.
    pub version: String,
    pub compatibility_level: i32,
    pub canonical_name: String,
    pub module: ModuleFile,
    /// Direct dependency aliases plus the module's own repo name. Only the
    /// root mapping exposes the main repository under the empty apparent name.
    /// Extension imports are NOT repositories until extensions are evaluated.
    pub mapping: BTreeMap<String, String>,
    pub source: ModuleSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModuleSource {
    LocalPath(PathBuf),
    Registry {
        registry: PathBuf,
        name: String,
        version: String,
    },
}

/// Resolve a workspace directory's MODULE.bazel using only local files.
pub fn resolve(root: &Path, options: &ResolverOptions) -> Result<ResolvedGraph, String> {
    let root_path = std::fs::canonicalize(root).map_err(|e| {
        format!(
            "cannot resolve root module directory {}: {e}",
            root.display()
        )
    })?;
    if !root_path.is_dir() {
        return Err(format!(
            "root module path is not a directory: {}",
            root_path.display()
        ));
    }
    let root = bzlmod::evaluate_for_resolution(
        &root_path.join("MODULE.bazel"),
        options.ignore_dev_dependencies,
    )?;
    let no_overrides = BTreeMap::new();
    // Bazel 9.3 ModuleThreadContext.addOverride ignores all MODULE overrides
    // when --ignore_dev_dependency is set, not just development dependencies.
    let overrides = if options.ignore_dev_dependencies {
        &no_overrides
    } else {
        &root.overrides
    };
    for (name, value) in overrides {
        if root
            .module
            .as_ref()
            .is_some_and(|info| !info.name.is_empty() && info.name == *name)
        {
            return Err(format!(
                "overriding the root module {name:?} is unsupported"
            ));
        }
        if let ModuleOverride::SingleVersion { version, .. } = value {
            // Match Bazel 9.3 ModuleFileFunction: this check looks up the root's
            // apparent dependency name, not the dependency's module name.
            if let Some(dep) = root
                .dependencies
                .iter()
                .find(|d| d.effective_repo_name() == Some(name.as_str()))
            {
                let requested = Version::parse(&dep.version)?;
                if !requested.normalized.is_empty() && Version::parse(version)? < requested {
                    return Err(format!(
                        "module {name:?} is overridden to version {version:?}, lower than {:?} requested by the root module",
                        dep.version
                    ));
                }
            }
        }
    }
    let discovery = Discovery {
        root_path,
        options,
        root: &root,
        overrides,
    };
    let nodes = discovery.collect()?;
    for name in overrides.keys() {
        if !nodes
            .keys()
            .any(|key| matches!(key, Key::Module { name: found, .. } if found == name))
        {
            return Err(format!(
                "root module specifies an override on nonexistent module {name:?}"
            ));
        }
    }
    let selected = select_versions(&nodes, overrides)?;
    // Selection.java:322-335: nodep-only nodes can invalidate selection, even
    // though those nodes must not appear in the final repository graph.
    selected_reachable(&nodes, &selected, false)?;
    let reachable = selected_reachable(&nodes, &selected, true)?;
    let canonical_names = canonical_names(&reachable)?;
    let mut modules = BTreeMap::new();
    for key in &reachable {
        let Key::Module { name, version } = key else {
            continue;
        };
        let node = &nodes[key];
        let level = node.compatibility_level();
        let source = node.source.clone().expect("discovered non-root source");
        check_yanked(&source)?;
        let canonical_name = canonical_names[key].clone();
        modules.insert(
            canonical_name.clone(),
            ResolvedModule {
                name: name.clone(),
                version: version.normalized.clone(),
                compatibility_level: level,
                canonical_name,
                module: node.module.clone(),
                source,
                mapping: mapping(key, &nodes, &selected, &canonical_names)?,
            },
        );
    }
    let root_mapping = mapping(&Key::Root, &nodes, &selected, &canonical_names)?;
    Ok(ResolvedGraph {
        root,
        modules,
        root_mapping,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Root,
    Module { name: String, version: Version },
}

#[derive(Debug)]
struct Node {
    module: ModuleFile,
    source: Option<ModuleSource>,
    /// Apparent repository name -> originally requested (override-adjusted) key.
    dependencies: BTreeMap<String, Key>,
    /// Fulfilled nodep requests influence selection but never create mappings.
    nodep_dependencies: Vec<Key>,
}
impl Node {
    fn compatibility_level(&self) -> i32 {
        // ModuleFileGlobals.module() stores zero in Bazel 9.3, regardless of
        // the declared value retained by our recording evaluator.
        0
    }
}

struct Discovery<'a> {
    root_path: PathBuf,
    options: &'a ResolverOptions,
    root: &'a ModuleFile,
    overrides: &'a BTreeMap<String, ModuleOverride>,
}
impl Discovery<'_> {
    fn collect(&self) -> Result<BTreeMap<Key, Node>, String> {
        // Discovery.java:62-77. Freeze the name gate for an entire BFS round;
        // newly discovered names can fulfill nodep edges only in a later round.
        // Cache evaluated files, analogous to Bazel's ModuleFileValue cache.
        let mut loaded = BTreeMap::new();
        let mut previous_names = BTreeSet::from([self
            .root
            .module
            .as_ref()
            .map_or("", |info| info.name.as_str())
            .to_owned()]);
        loop {
            let (mut nodes, unfulfilled_names) =
                self.collect_round(&previous_names, &mut loaded)?;
            let names: BTreeSet<_> = nodes
                .values()
                .filter_map(|node| node.module.module.as_ref())
                .map(|info| info.name.clone())
                .collect();
            // Discovery.java:146-163: unfulfilled nodeps disappear from the
            // semantic graph, even though original declarations remain metadata.
            let keys: BTreeSet<_> = nodes.keys().cloned().collect();
            for node in nodes.values_mut() {
                node.nodep_dependencies.retain(|key| keys.contains(key));
            }
            if unfulfilled_names.is_disjoint(&names) {
                return Ok(nodes);
            }
            previous_names = names;
        }
    }

    fn collect_round(
        &self,
        previous_names: &BTreeSet<String>,
        loaded: &mut BTreeMap<Key, (ModuleFile, Option<ModuleSource>)>,
    ) -> Result<(BTreeMap<Key, Node>, BTreeSet<String>), String> {
        let mut nodes = BTreeMap::new();
        let mut unfulfilled_names = BTreeSet::new();
        let mut enqueued = BTreeSet::from([Key::Root]);
        let mut queue = VecDeque::from([Key::Root]);
        while let Some(key) = queue.pop_front() {
            if !loaded.contains_key(&key) {
                let entry = match &key {
                    Key::Root => (self.root.clone(), None),
                    Key::Module { name, version } => {
                        let (module, source) = self.load(name, version)?;
                        (module, Some(source))
                    }
                };
                loaded.insert(key.clone(), entry);
            }
            let (module, source) = loaded[&key].clone();
            let mut dependencies = BTreeMap::new();
            let mut nodep_dependencies = Vec::new();
            for dep in &module.dependencies {
                if dep.dev_dependency && (key != Key::Root || self.options.ignore_dev_dependencies)
                {
                    continue;
                }
                let target = self.dependency_key(dep)?;
                if let Some(alias) = dep.effective_repo_name() {
                    dependencies.insert(alias.to_owned(), target);
                } else {
                    nodep_dependencies.push(target);
                }
            }
            // Ordinary deps always introduce requested versions. A nodep may
            // introduce a version only when its name existed in the prior round.
            for (target, is_nodep) in dependencies
                .values()
                .map(|key| (key, false))
                .chain(nodep_dependencies.iter().map(|key| (key, true)))
            {
                if is_nodep
                    && !enqueued.contains(target)
                    && let Key::Module { name, .. } = target
                    && !previous_names.contains(name)
                {
                    unfulfilled_names.insert(name.clone());
                    continue;
                }
                if enqueued.insert(target.clone()) {
                    if enqueued.len() > MAX_DISCOVERED_MODULES {
                        return Err(format!(
                            "module discovery exceeded {MAX_DISCOVERED_MODULES} unique requested versions"
                        ));
                    }
                    queue.push_back(target.clone());
                }
            }
            nodes.insert(
                key,
                Node {
                    module,
                    source,
                    dependencies,
                    nodep_dependencies,
                },
            );
        }
        Ok((nodes, unfulfilled_names))
    }

    fn dependency_key(&self, dep: &Dependency) -> Result<Key, String> {
        // Dependencies back to the root never require a registry version of it.
        if self
            .root
            .module
            .as_ref()
            .is_some_and(|info| !info.name.is_empty() && info.name == dep.name)
        {
            return Ok(Key::Root);
        }
        let version = match self.overrides.get(&dep.name) {
            Some(
                ModuleOverride::LocalPath { .. }
                | ModuleOverride::Git { .. }
                | ModuleOverride::Archive { .. },
            ) => Version::default(),
            Some(ModuleOverride::SingleVersion { version, .. }) => {
                Version::parse(if version.is_empty() {
                    &dep.version
                } else {
                    version
                })?
            }
            Some(ModuleOverride::MultipleVersion { .. }) | None => Version::parse(&dep.version)?,
        };
        Ok(Key::Module {
            name: dep.name.clone(),
            version,
        })
    }

    fn load(&self, name: &str, version: &Version) -> Result<(ModuleFile, ModuleSource), String> {
        if name == "bazel_tools" {
            return Err("built-in bazel_tools is unavailable in the local module resolver; no built-in repository is fabricated".to_owned());
        }
        match self.overrides.get(name) {
            Some(ModuleOverride::Git { .. }) => {
                return Err(format!(
                    "git_override for module {name:?} is unsupported: no repository fetching"
                ));
            }
            Some(ModuleOverride::Archive { .. }) => {
                return Err(format!(
                    "archive_override for module {name:?} is unsupported: no archive fetching/extraction"
                ));
            }
            Some(ModuleOverride::SingleVersion { patches, .. })
                if !patches.patches.is_empty()
                    || !patches.patch_cmds.is_empty()
                    || patches.patch_strip != 0 =>
            {
                return Err(format!(
                    "patch options for module {name:?} are unsupported; MODULE patches cannot be silently ignored"
                ));
            }
            _ => {}
        }
        if let Some(ModuleOverride::LocalPath { path }) = self.overrides.get(name) {
            // Root-relative, not relative to whichever transitive module uses it.
            let path = std::fs::canonicalize(self.root_path.join(path)).map_err(|e| {
                format!("cannot resolve local override for {name:?} at {path:?}: {e}")
            })?;
            let module = bzlmod::evaluate_for_resolution(&path.join("MODULE.bazel"), true)?;
            validate_declared_module(&module, name, None, &path)?;
            return Ok((module, ModuleSource::LocalPath(path)));
        }
        if version.normalized.is_empty() {
            return Err(format!(
                "dependency {name:?} has no registry version and no local_path_override"
            ));
        }
        let override_registry = match self.overrides.get(name) {
            Some(
                ModuleOverride::SingleVersion { registry, .. }
                | ModuleOverride::MultipleVersion { registry, .. },
            ) if !registry.is_empty() => Some(vec![local_registry_url(registry)?]),
            _ => None,
        };
        let registries = override_registry
            .as_deref()
            .unwrap_or(&self.options.registries);
        for registry in registries {
            let Some(registry) = canonical_registry(registry)? else {
                continue;
            };
            let path = registry
                .join("modules")
                .join(name)
                .join(&version.normalized)
                .join("MODULE.bazel");
            let Some(source) = read_registry_file(&path)? else {
                continue;
            };
            let module =
                bzlmod::evaluate_source_for_resolution(&path.to_string_lossy(), source, true)?;
            validate_declared_module(&module, name, Some(version), &path)?;
            return Ok((
                module,
                ModuleSource::Registry {
                    registry,
                    name: name.to_owned(),
                    version: version.normalized.clone(),
                },
            ));
        }
        Err(format!(
            "module {name}@{} was not found in local registries [{}]; remote fetching is unsupported",
            version.normalized,
            registries
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

fn validate_declared_module(
    module: &ModuleFile,
    name: &str,
    version: Option<&Version>,
    path: &Path,
) -> Result<(), String> {
    let info = module.module.as_ref().ok_or_else(|| {
        format!(
            "dependency module {name:?} at {} must declare module()",
            path.display()
        )
    })?;
    if info.name != name {
        return Err(format!(
            "module name mismatch at {}: requested {name:?}, declared {:?}",
            path.display(),
            info.name
        ));
    }
    if let Some(version) = version {
        let declared = Version::parse(&info.version)?;
        if declared != *version {
            return Err(format!(
                "module version mismatch at {}: requested {:?}, declared {:?}",
                path.display(),
                version.normalized,
                info.version
            ));
        }
    }
    Ok(())
}

struct Selection {
    /// Resolution for every discovered key, including unselected versions.
    replacements: BTreeMap<Key, Key>,
    /// Above-ceiling groups are errors only if reached in the validation walk.
    forbidden: BTreeSet<Key>,
}
fn select_versions(
    nodes: &BTreeMap<Key, Node>,
    overrides: &BTreeMap<String, ModuleOverride>,
) -> Result<Selection, String> {
    let mut allowed = BTreeMap::new();
    for (name, value) in overrides {
        if let ModuleOverride::MultipleVersion { versions, .. } = value {
            if versions.len() < 2 {
                return Err(format!(
                    "multiple_version_override for {name:?} requires at least 2 versions"
                ));
            }
            let mut versions_set = BTreeSet::new();
            for text in versions {
                let version = Version::parse(text)?;
                if !nodes.contains_key(&Key::Module {
                    name: name.clone(),
                    version: version.clone(),
                }) {
                    return Err(format!(
                        "multiple_version_override for {name:?} contains version {:?}, but it does not exist in the discovered dependency graph",
                        version.normalized
                    ));
                }
                versions_set.insert(version);
            }
            allowed.insert(name.clone(), versions_set);
        }
    }
    // All compatibility levels and maximum compatibility levels are no-ops in
    // Bazel 9.3, so every DepSpec has exactly one possible resolution. This is
    // Selection.java's group selection without its obsolete CL cartesian product.
    let mut highest = BTreeMap::new();
    for key in nodes.keys() {
        let Key::Module { name, version } = key else {
            continue;
        };
        let replace = match highest.get(name) {
            Some(Key::Module { version: old, .. }) => version > old,
            _ => true,
        };
        if replace {
            highest.insert(name.clone(), key.clone());
        }
    }
    let mut replacements = BTreeMap::from([(Key::Root, Key::Root)]);
    let mut forbidden = BTreeSet::new();
    for key in nodes.keys() {
        let Key::Module { name, version } = key else {
            continue;
        };
        let replacement = if let Some(versions) = allowed.get(name) {
            if let Some(ceiling) = versions.range(version.clone()..).next() {
                // The ceiling itself is discovered, so it is the highest
                // version in this allowed-version selection group.
                Key::Module {
                    name: name.clone(),
                    version: ceiling.clone(),
                }
            } else {
                // Selection.java:174-179,285-290: all above-ceiling versions
                // share the EMPTY sentinel group, selecting its maximum.
                forbidden.insert(key.clone());
                highest[name].clone()
            }
        } else {
            highest[name].clone()
        };
        replacements.insert(key.clone(), replacement);
    }
    Ok(Selection {
        replacements,
        forbidden,
    })
}

fn selected_key<'a>(key: &Key, selected: &'a Selection) -> Result<&'a Key, String> {
    selected
        .replacements
        .get(key)
        .ok_or_else(|| format!("internal error: missing selection for {key:?}"))
}

fn selected_reachable(
    nodes: &BTreeMap<Key, Node>,
    selected: &Selection,
    ignore_nodeps: bool,
) -> Result<BTreeSet<Key>, String> {
    let mut reachable = BTreeSet::new();
    let mut queue = VecDeque::from([Key::Root]);
    while let Some(key) = queue.pop_front() {
        if !reachable.insert(key.clone()) {
            continue;
        }
        if selected.forbidden.contains(&key) {
            return Err(format!(
                "reachable module {key:?} is not allowed by the multiple_version_override (above the allowed version ceiling)"
            ));
        }
        let node = &nodes[&key];
        // Selection.java:454-471 checks ordinary dependencies, even on nodes
        // reachable only through nodep edges. Nodeps themselves have no aliases.
        let mut dependency_targets = BTreeMap::new();
        for (alias, target) in &node.dependencies {
            let target = selected_key(target, selected)?;
            if let Some(previous) = dependency_targets.insert(target, alias) {
                return Err(format!(
                    "duplicate resolved dependency {target:?} under apparent repository names {previous:?} and {alias:?}"
                ));
            }
            queue.push_back(target.clone());
        }
        if !ignore_nodeps {
            for target in &node.nodep_dependencies {
                queue.push_back(selected_key(target, selected)?.clone());
            }
        }
    }
    Ok(reachable)
}

fn canonical_names(reachable: &BTreeSet<Key>) -> Result<BTreeMap<Key, String>, String> {
    let mut counts = BTreeMap::new();
    for key in reachable {
        if let Key::Module { name, .. } = key {
            *counts.entry(name.clone()).or_insert(0usize) += 1;
        }
    }
    let mut names = BTreeMap::new();
    let mut unique = BTreeSet::new();
    for key in reachable {
        let canonical = match key {
            Key::Root => String::new(),
            // ModuleKey.java:45-55,97-127: well-known repos keep fixed names.
            Key::Module { name, .. } if name == "platforms" => name.clone(),
            Key::Module { name, version } if counts[name] > 1 => {
                format!("{name}+{}", version.normalized)
            }
            Key::Module { name, .. } => format!("{name}+"),
        };
        if !unique.insert(canonical.clone()) {
            return Err(format!(
                "canonical repository name collision {canonical:?}: multiple versions of a well-known repository cannot coexist"
            ));
        }
        names.insert(key.clone(), canonical);
    }
    Ok(names)
}

fn mapping(
    key: &Key,
    nodes: &BTreeMap<Key, Node>,
    selected: &Selection,
    canonical_names: &BTreeMap<Key, String>,
) -> Result<BTreeMap<String, String>, String> {
    let canonical = canonical_names[key].clone();
    let node = &nodes[key];
    let mut mapping = BTreeMap::new();
    // Module.java:49-67: only the root can see the main repository as `@`.
    // External modules refer to themselves by their declared repository name.
    if *key == Key::Root {
        mapping.insert(String::new(), String::new());
    }
    if let Some(info) = &node.module.module
        && !info.effective_repo_name().is_empty()
    {
        mapping.insert(info.effective_repo_name().to_owned(), canonical);
    }
    for (alias, target) in &node.dependencies {
        let target = canonical_names[selected_key(target, selected)?].clone();
        if mapping.insert(alias.clone(), target).is_some() {
            return Err(format!(
                "repository alias {alias:?} conflicts with the module's own repository name"
            ));
        }
    }
    Ok(mapping)
}

fn canonical_registry(path: &Path) -> Result<Option<PathBuf>, String> {
    if !no_symlinks(path)? {
        return Ok(None);
    }
    let canonical = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(format!(
                "cannot open local registry {}: {e}",
                path.display()
            ));
        }
    };
    if !canonical.is_dir() {
        return Err(format!(
            "local registry is not a directory: {}",
            canonical.display()
        ));
    }
    Ok(Some(canonical))
}

/// Refuse symlinks in all path components, not just the final MODULE file.
/// Registry trees are trusted not to be changed concurrently with these reads.
fn no_symlinks(path: &Path) -> Result<bool, String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "symlink registry paths are unsupported: {}",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => {
                return Err(format!(
                    "cannot inspect registry path {}: {e}",
                    current.display()
                ));
            }
        }
    }
    Ok(true)
}

fn read_registry_file(path: &Path) -> Result<Option<String>, String> {
    if !no_symlinks(path)? {
        return Ok(None);
    }
    let metadata =
        std::fs::metadata(path).map_err(|e| format!("cannot inspect {}: {e}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "registry entry is not a regular file: {}",
            path.display()
        ));
    }
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))
}

fn local_registry_url(url: &str) -> Result<PathBuf, String> {
    let Some(rest) = url.strip_prefix("file://") else {
        return Err(format!(
            "remote registry override {url:?} is unsupported: only local file:// registries can be read"
        ));
    };
    let rest = rest
        .strip_prefix("localhost/")
        .map_or_else(|| rest.to_owned(), |p| format!("/{p}"));
    if !rest.starts_with('/') || rest.contains(['?', '#']) {
        return Err(format!(
            "unsupported file registry URL {url:?}: expected an absolute local path without query or fragment"
        ));
    }
    let mut bytes = Vec::new();
    let mut input = rest.bytes();
    while let Some(b) = input.next() {
        if b == b'%' {
            let hi = input.next().and_then(|c| char::from(c).to_digit(16));
            let lo = input.next().and_then(|c| char::from(c).to_digit(16));
            match (hi, lo) {
                (Some(hi), Some(lo)) => bytes.push((hi * 16 + lo) as u8),
                _ => return Err(format!("invalid percent encoding in registry URL {url:?}")),
            }
        } else {
            bytes.push(b);
        }
    }
    if bytes.contains(&0) {
        return Err("NUL in registry URL is unsupported".to_owned());
    }
    let path =
        String::from_utf8(bytes).map_err(|_| "non-UTF-8 registry URL is unsupported".to_owned())?;
    Ok(PathBuf::from(path))
}

fn check_yanked(source: &ModuleSource) -> Result<(), String> {
    let ModuleSource::Registry {
        registry,
        name,
        version,
    } = source
    else {
        return Ok(());
    };
    let path = registry.join("modules").join(name).join("metadata.json");
    let Some(text) = read_registry_file(&path)? else {
        return Ok(());
    };
    // IndexRegistry.grabJson treats blank documents as absent metadata.
    if text.trim().is_empty() {
        return Ok(());
    }
    let metadata: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("invalid registry metadata {}: {e}", path.display()))?;
    let object = metadata
        .as_object()
        .ok_or_else(|| format!("registry metadata must be an object: {}", path.display()))?;
    let Some(yanked) = object.get("yanked_versions") else {
        return Ok(());
    };
    // IndexRegistry.getYankedVersions treats a null field as an empty map.
    if yanked.is_null() {
        return Ok(());
    }
    let yanked = yanked
        .as_object()
        .ok_or_else(|| format!("yanked_versions must be an object: {}", path.display()))?;
    let selected = Version::parse(version)?;
    for (v, reason) in yanked {
        let reason = reason
            .as_str()
            .ok_or_else(|| format!("yanked version reason must be a string: {}", path.display()))?;
        if Version::parse(v)? == selected {
            return Err(format!(
                "selected module {name}@{version} is yanked: {reason}; yanked-version allowlists are unsupported"
            ));
        }
    }
    Ok(())
}

/// Bazel Version/Identifier ordering, not SemVer's numeric-only release rules:
/// release identifiers may contain letters and the release length is unbounded.
/// Build metadata is ignored; numeric identifier ties compare original spelling.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Version {
    normalized: String,
    release: Vec<Identifier>,
    prerelease: Vec<Identifier>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Identifier {
    spelling: String,
    number: Option<u64>,
}
impl Identifier {
    fn parse(s: &str) -> Result<Self, String> {
        if s.is_empty() {
            return Err("empty version identifier".to_owned());
        }
        let number = if s.bytes().all(|b| b.is_ascii_digit()) {
            Some(
                s.parse::<u64>()
                    .map_err(|_| format!("numeric version identifier is too large: {s}"))?,
            )
        } else {
            None
        };
        Ok(Self {
            spelling: s.to_owned(),
            number,
        })
    }
}
impl Ord for Identifier {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.number, other.number) {
            (Some(a), Some(b)) => a.cmp(&b).then_with(|| self.spelling.cmp(&other.spelling)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => self.spelling.cmp(&other.spelling),
        }
    }
}
impl PartialOrd for Identifier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Version {
    fn parse(s: &str) -> Result<Self, String> {
        if s.is_empty() {
            return Ok(Self::default());
        }
        let (main, build) = s.split_once('+').map_or((s, None), |(a, b)| (a, Some(b)));
        let (release, prerelease) = main
            .split_once('-')
            .map_or((main, None), |(a, b)| (a, Some(b)));
        let valid = |part: &str, hyphens: bool| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || (hyphens && b == b'-'))
        };
        if !valid(release, false)
            || prerelease.is_some_and(|p| !valid(p, true))
            || build.is_some_and(|p| !valid(p, true))
        {
            return Err(format!("invalid module version {s:?}"));
        }
        let identifiers = |part: &str| {
            part.split('.')
                .map(Identifier::parse)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("invalid module version {s:?}: {e}"))
        };
        Ok(Self {
            normalized: main.to_owned(),
            release: identifiers(release)?,
            prerelease: prerelease.map(identifiers).transpose()?.unwrap_or_default(),
        })
    }
}
impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.normalized
            .is_empty()
            .cmp(&other.normalized.is_empty())
            .then_with(|| self.release.cmp(&other.release))
            .then_with(|| self.prerelease.is_empty().cmp(&other.prerelease.is_empty()))
            .then_with(|| self.prerelease.cmp(&other.prerelease))
    }
}
impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
        registry: PathBuf,
    }
    impl Fixture {
        fn new(root_source: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "bazel-rs-module-graph-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, AtomicOrdering::Relaxed)
            ));
            let root = base.join("root");
            let registry = base.join("registry");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(&registry).unwrap();
            std::fs::write(root.join("MODULE.bazel"), root_source).unwrap();
            Self {
                base,
                root,
                registry,
            }
        }
        fn module(&self, name: &str, version: &str, rest: &str) {
            self.raw_module(
                name,
                version,
                &format!("module(name={name:?},version={version:?})\n{rest}"),
            );
        }
        fn raw_module(&self, name: &str, version: &str, source: &str) {
            let path = self.registry.join("modules").join(name).join(version);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("MODULE.bazel"), source).unwrap();
        }
        fn options(&self) -> ResolverOptions {
            ResolverOptions {
                registries: vec![self.registry.clone()],
                ..Default::default()
            }
        }
        fn resolve(&self) -> ResolvedGraph {
            resolve(&self.root, &self.options()).unwrap()
        }
        fn fails(&self, expected: &str) {
            let error = resolve(&self.root, &self.options()).unwrap_err();
            assert!(
                error.contains(expected),
                "expected {expected:?}, got {error}"
            );
        }
        fn local(&self, path: &str, source: &str) -> PathBuf {
            let path = self.root.join(path);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("MODULE.bazel"), source).unwrap();
            std::fs::canonicalize(path).unwrap()
        }
        fn metadata(&self, name: &str, text: &str) {
            std::fs::write(
                self.registry
                    .join("modules")
                    .join(name)
                    .join("metadata.json"),
                text,
            )
            .unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn diamond_upgrades_and_repository_mappings_are_strict() {
        let f = Fixture::new(
            "module(name='root',repo_name='main')\nbazel_dep(name='a',version='1',repo_name='first')\nbazel_dep(name='b',version='1')",
        );
        f.module("a", "1", "bazel_dep(name='c',version='1',repo_name='see')");
        f.module("b", "1", "bazel_dep(name='c',version='2')");
        f.module("c", "1", "");
        f.raw_module(
            "c",
            "2",
            "module(name='c',version='2',repo_name='legacy_c')",
        );
        let graph = f.resolve();
        assert_eq!(graph.modules.len(), 3);
        assert_eq!(graph.modules["c+"].version, "2");
        assert_eq!(
            graph.root_mapping,
            BTreeMap::from([
                ("".to_owned(), "".to_owned()),
                ("main".to_owned(), "".to_owned()),
                ("first".to_owned(), "a+".to_owned()),
                ("b".to_owned(), "b+".to_owned())
            ])
        );
        assert_eq!(graph.modules["a+"].mapping["see"], "c+");
        assert!(!graph.modules["a+"].mapping.contains_key("b"));
        assert!(!graph.root_mapping.contains_key("c"));
        assert_eq!(graph.modules["c+"].mapping["legacy_c"], "c+");
        assert_eq!(graph.modules["c+"].mapping.get(""), None);
        assert!(!graph.modules["c+"].mapping.contains_key("c"));
    }

    #[test]
    fn prunes_old_version_edges_but_uses_full_preselection_versions() {
        let f = Fixture::new("bazel_dep(name='a',version='1')\nbazel_dep(name='b',version='1')");
        f.module(
            "a",
            "1",
            "bazel_dep(name='removed',version='1')\nbazel_dep(name='c',version='9')",
        );
        f.module(
            "a",
            "2",
            "bazel_dep(name='added',version='1')\nbazel_dep(name='c',version='1')",
        );
        f.module("b", "1", "bazel_dep(name='a',version='2')");
        f.module("removed", "1", "");
        f.module("added", "1", "");
        f.module("c", "1", "");
        f.module("c", "9", "");
        let graph = f.resolve();
        assert_eq!(graph.modules["a+"].version, "2");
        assert!(!graph.modules.contains_key("removed+"));
        assert!(graph.modules.contains_key("added+"));
        // c@9 was introduced by the now-pruned a@1, but still wins MVS.
        assert_eq!(graph.modules["c+"].version, "9");
        assert!(!graph.modules["a+"].mapping.contains_key("removed"));
    }

    #[test]
    fn root_local_override_applies_transitively_and_nonroot_overrides_are_ignored() {
        let f = Fixture::new(
            "bazel_dep(name='a',version='1')\nlocal_path_override(module_name='c',path='overrides/c')",
        );
        f.module("a", "1", "bazel_dep(name='c',version='123')\nlocal_path_override(module_name='c',path='/not/used')");
        let path = f.local(
            "overrides/c",
            "module(name='c',version='99')\nbazel_dep(name='d',version='1')",
        );
        f.module("d", "1", "");
        let graph = f.resolve();
        assert_eq!(graph.modules["c+"].source, ModuleSource::LocalPath(path));
        assert_eq!(graph.modules["c+"].version, "");
        assert_eq!(
            graph.modules["c+"].module.module.as_ref().unwrap().version,
            "99"
        );
        assert_eq!(graph.modules["c+"].mapping["d"], "d+");
        assert!(graph.modules["a+"].module.overrides.is_empty());
    }

    #[test]
    fn pin_redirects_requests_before_loading_and_matches_alias_sensitive_downgrade_check() {
        let f = Fixture::new(
            "bazel_dep(name='a',version='1')\nbazel_dep(name='c',version='900',repo_name='see')\nsingle_version_override(module_name='c',version='2')",
        );
        f.module("a", "1", "bazel_dep(name='c',version='999')");
        f.module("c", "2", "");
        let graph = f.resolve();
        assert_eq!(graph.modules["c+"].version, "2");
        assert_eq!(graph.modules["a+"].mapping["c"], "c+");
        std::fs::write(f.root.join("MODULE.bazel"), "bazel_dep(name='c',version='900')\nsingle_version_override(module_name='c',version='2')").unwrap();
        f.fails("lower than");
    }

    #[test]
    fn root_development_dependencies_are_optional_and_nonroot_ones_are_excluded() {
        let f = Fixture::new(
            "bazel_dep(name='a',version='1')\nbazel_dep(name='dev',version='1',dev_dependency=True)",
        );
        f.module(
            "a",
            "1",
            "bazel_dep(name='missing',version='1',dev_dependency=True)",
        );
        f.module("dev", "1", "");
        let graph = f.resolve();
        assert!(graph.modules.contains_key("dev+"));
        assert!(!graph.modules["a+"].mapping.contains_key("missing"));
        let mut opts = f.options();
        opts.ignore_dev_dependencies = true;
        let graph = resolve(&f.root, &opts).unwrap();
        assert!(!graph.modules.contains_key("dev+"));
        assert!(!graph.root_mapping.contains_key("dev"));
        assert_eq!(graph.root.dependencies.len(), 2);
        assert_eq!(graph.modules["a+"].module.dependencies.len(), 1);
    }

    #[test]
    fn root_and_module_cycles_terminate_without_fetching_root() {
        let f = Fixture::new("module(name='root',version='1')\nbazel_dep(name='a',version='1')");
        f.module("a", "1", "bazel_dep(name='root',version='987',repo_name='back')\nbazel_dep(name='b',version='1')");
        f.module("b", "1", "bazel_dep(name='a',version='1')");
        let graph = f.resolve();
        assert_eq!(graph.modules.len(), 2);
        assert_eq!(graph.modules["a+"].mapping["back"], "");
        assert_eq!(graph.modules["b+"].mapping["a"], "a+");
    }

    #[test]
    fn declared_compatibility_levels_and_maxima_are_noops_in_bazel_9_3() {
        let f = Fixture::new("bazel_dep(name='a',version='1')\nbazel_dep(name='c',version='2')");
        f.module(
            "a",
            "1",
            "bazel_dep(name='c',version='1',max_compatibility_level=-123)",
        );
        f.module("c", "1", "");
        f.raw_module(
            "c",
            "2",
            "module(name='c',version='2',compatibility_level=1)",
        );
        let graph = f.resolve();
        assert_eq!(graph.modules["c+"].version, "2");
        assert_eq!(graph.modules["c+"].compatibility_level, 0);
        assert_eq!(
            graph.modules["c+"]
                .module
                .module
                .as_ref()
                .unwrap()
                .compatibility_level,
            1
        );
        std::fs::write(
            f.root.join("MODULE.bazel"),
            "bazel_dep(name='a',version='1')\nbazel_dep(name='b',version='1')",
        )
        .unwrap();
        f.module("b", "1", "bazel_dep(name='a',version='2')");
        f.module("a", "2", "bazel_dep(name='c',version='2')");
        let graph = f.resolve();
        assert_eq!(graph.modules["c+"].compatibility_level, 0);
        assert_eq!(graph.modules["c+"].version, "2");
    }

    #[test]
    fn relaxed_versions_follow_bazel_identifier_order_and_ignore_build_metadata() {
        let ordered = [
            "1.0-pre.2",
            "1.0-pre.10",
            "1.0-pre.alpha",
            "1.0",
            "1.0.1",
            "1.1",
            "1.10",
            "1.a",
            "2",
            "10",
            "a",
            "",
        ];
        for pair in ordered.windows(2) {
            assert!(
                Version::parse(pair[0]).unwrap() < Version::parse(pair[1]).unwrap(),
                "{pair:?}"
            );
        }
        assert_eq!(
            Version::parse("1.0+foo").unwrap(),
            Version::parse("1.0+bar").unwrap()
        );
        assert!(Version::parse("1.01").unwrap() < Version::parse("1.1").unwrap());
        assert!(Version::parse("18446744073709551615").is_ok());
        for invalid in [
            "1..0",
            "1_2",
            "1-",
            "1+",
            "1/2",
            "1-rc..1",
            "18446744073709551616",
            "1+a+b",
        ] {
            assert!(Version::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn prerelease_selection_and_normalized_registry_paths() {
        let f = Fixture::new(
            "bazel_dep(name='a',version='1')\nbazel_dep(name='c',version='1.0-rc.2+build')",
        );
        f.module("a", "1", "bazel_dep(name='c',version='1.0-rc.10')");
        f.module("c", "1.0-rc.2", "");
        f.module("c", "1.0-rc.10", "");
        assert_eq!(f.resolve().modules["c+"].version, "1.0-rc.10");
    }

    #[test]
    fn registry_override_and_ordered_fallback_are_local_only() {
        let f = Fixture::new("bazel_dep(name='a',version='1')");
        f.module("a", "1", "");
        let empty = f.base.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let opts = ResolverOptions {
            registries: vec![empty.clone(), f.registry.clone()],
            ignore_dev_dependencies: false,
        };
        assert!(resolve(&f.root, &opts).unwrap().modules.contains_key("a+"));
        std::fs::write(f.root.join("MODULE.bazel"), format!("bazel_dep(name='a',version='1')\nsingle_version_override(module_name='a',registry={:?})", format!("file://{}", f.registry.display()))).unwrap();
        let graph = resolve(
            &f.root,
            &ResolverOptions {
                registries: vec![empty],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(graph.modules["a+"].version, "1");
        assert_eq!(
            local_registry_url("file:///tmp/a%20b").unwrap(),
            PathBuf::from("/tmp/a b")
        );
        assert!(
            local_registry_url("https://example.invalid")
                .unwrap_err()
                .contains("remote registry")
        );
        assert!(local_registry_url("file://remote.example/x").is_err());
        assert!(local_registry_url("file:///tmp/%xx").is_err());
    }

    #[test]
    fn missing_malformed_and_mismatched_module_files_are_errors() {
        let f = Fixture::new("bazel_dep(name='a',version='1')");
        f.fails("was not found");
        f.raw_module("a", "1", "module(name='wrong',version='1')");
        f.fails("module name mismatch");
        f.raw_module("a", "1", "module(name='a',version='2')");
        f.fails("module version mismatch");
        f.raw_module("a", "1", "bazel_dep(name='b',version='1')");
        f.fails("must declare module()");
        f.raw_module("a", "1", "module(");
        assert!(resolve(&f.root, &f.options()).is_err());
        std::fs::write(f.root.join("MODULE.bazel"), "bazel_dep(name='a')").unwrap();
        f.fails("no registry version");
    }

    #[test]
    fn yanked_selected_versions_fail_but_pruned_versions_do_not() {
        let f = Fixture::new("bazel_dep(name='a',version='1')\nbazel_dep(name='b',version='1')");
        f.module("a", "1", "");
        f.module("a", "2", "");
        f.module("b", "1", "bazel_dep(name='a',version='2')");
        f.metadata("a", r#"{"yanked_versions":{"1":"old security bug"}}"#);
        assert_eq!(f.resolve().modules["a+"].version, "2");
        f.metadata("a", r#"{"yanked_versions":{"2":"security bug"}}"#);
        f.fails("is yanked: security bug");
        f.metadata("a", "not json");
        f.fails("invalid registry metadata");
        f.metadata("a", r#"{"yanked_versions":[]}"#);
        f.fails("yanked_versions must be an object");
    }

    #[test]
    fn unsupported_fetching_and_patches_are_never_silently_ignored() {
        for (source, expected) in [
            (
                "bazel_dep(name='a',version='1')\nsingle_version_override(module_name='a',patches=['//:fix.patch'])",
                "patch options",
            ),
            (
                "bazel_dep(name='a',version='1')\ngit_override(module_name='a',remote='r',tag='v1')",
                "git_override",
            ),
            (
                "bazel_dep(name='a',version='1')\narchive_override(module_name='a',urls='https://invalid.invalid/a.zip')",
                "archive_override",
            ),
            (
                "bazel_dep(name='a',version='1')\nsingle_version_override(module_name='a',registry='https://invalid.invalid')",
                "remote registry",
            ),
        ] {
            let f = Fixture::new(source);
            f.fails(expected);
        }
        let f = Fixture::new("bazel_dep(name='a',version='1',repo_name=None,dev_dependency=True)");
        let graph = resolve(
            &f.root,
            &ResolverOptions {
                ignore_dev_dependencies: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(graph.modules.is_empty());
    }

    #[test]
    fn extensions_and_registrations_are_retained_without_creating_repositories() {
        let f = Fixture::new(
            "x=use_extension('//:missing.bzl','x')\nx.tag(path='/not/read')\nuse_repo(x,'generated')\nregister_toolchains('@generated//:all')",
        );
        let graph = f.resolve();
        assert!(graph.modules.is_empty());
        assert!(!graph.root_mapping.contains_key("generated"));
        assert_eq!(
            graph.root.extension_usages[0].tags[0].attributes["path"],
            "/not/read"
        );
        assert_eq!(graph.root.registrations[0].labels, ["@generated//:all"]);
    }

    #[test]
    fn ignore_dev_dependencies_also_disables_root_overrides() {
        let f = Fixture::new(
            "bazel_dep(name='a',version='1')\nlocal_path_override(module_name='a',path='local')\nmultiple_version_override(module_name='unused',versions=['1','2'])",
        );
        f.module("a", "1", "");
        let mut options = f.options();
        options.ignore_dev_dependencies = true;
        let graph = resolve(&f.root, &options).unwrap();
        assert_eq!(graph.modules["a+"].version, "1");
        assert!(matches!(
            graph.modules["a+"].source,
            ModuleSource::Registry { .. }
        ));
        // Contextually ignored overrides are not recorded as active metadata.
        assert!(graph.root.overrides.is_empty());
    }

    #[test]
    fn nonexistent_and_self_overrides_and_duplicate_selected_aliases_fail() {
        let f = Fixture::new("local_path_override(module_name='absent',path='not-read')");
        f.fails("override on nonexistent module");
        std::fs::write(
            f.root.join("MODULE.bazel"),
            "module(name='root')\nlocal_path_override(module_name='root',path='not-read')",
        )
        .unwrap();
        f.fails("root module");
        std::fs::write(f.root.join("MODULE.bazel"), "bazel_dep(name='a',version='1',repo_name='first')\nbazel_dep(name='a',version='2',repo_name='second')").unwrap();
        f.module("a", "1", "");
        f.module("a", "2", "");
        f.fails("duplicate resolved dependency");
    }

    #[test]
    fn explicit_builtin_dependency_reports_missing_capability() {
        let f = Fixture::new("bazel_dep(name='bazel_tools')");
        f.fails("built-in bazel_tools is unavailable");
    }

    #[test]
    fn absent_nodep_does_not_introduce_a_name_or_require_a_registry_version() {
        let f = Fixture::new(
            "bazel_dep(name='absent',repo_name=None)\nbazel_dep(name='also_absent',version='99',repo_name=None)",
        );
        let graph = f.resolve();
        assert!(graph.modules.is_empty());
        assert_eq!(
            graph.root_mapping,
            BTreeMap::from([(String::new(), String::new())])
        );
        assert_eq!(graph.root.dependencies.len(), 2);
        // Even an available MODULE is not read merely because a nodep names it.
        f.raw_module("also_absent", "99", "this is not valid Starlark (");
        assert!(f.resolve().modules.is_empty());
    }

    #[test]
    fn fulfilled_nodep_introduces_a_higher_version_and_its_dependencies() {
        let f = Fixture::new(
            "bazel_dep(name='a',version='1',repo_name='normal')\nbazel_dep(name='a',version='2',repo_name=None)",
        );
        f.module("a", "1", "");
        f.module("a", "2", "bazel_dep(name='b',version='1')");
        f.module("b", "1", "");
        let graph = f.resolve();
        assert_eq!(graph.modules["a+"].version, "2");
        assert_eq!(graph.modules["a+"].mapping["b"], "b+");
        assert_eq!(graph.root_mapping["normal"], "a+");
        assert!(!graph.root_mapping.contains_key("a"));
        assert!(!graph.root_mapping.contains_key("b"));
        assert_eq!(graph.modules.len(), 2);
    }

    #[test]
    fn fulfilled_nodep_requires_loading_its_requested_version() {
        let f = Fixture::new(
            "bazel_dep(name='a',version='1')\nbazel_dep(name='a',version='2',repo_name=None)",
        );
        f.module("a", "1", "");
        f.fails("module a@2 was not found");
    }

    #[test]
    fn nodep_discovery_repeats_until_newly_introduced_names_reach_fixed_point() {
        let f = Fixture::new(
            "bazel_dep(name='a',version='1')\nbazel_dep(name='b',version='2',repo_name=None)\nbazel_dep(name='c',version='2',repo_name=None)",
        );
        f.module("a", "1", "bazel_dep(name='b',version='1')");
        f.module("b", "1", "");
        // b@2 is introduced in round 2, introducing c for the first time.
        f.module("b", "2", "bazel_dep(name='c',version='1')");
        f.module("c", "1", "");
        // The root's c@2 nodep is not enabled until round 3.
        f.module("c", "2", "bazel_dep(name='d',version='1')");
        f.module("d", "1", "");
        let graph = f.resolve();
        assert_eq!(graph.modules["b+"].version, "2");
        assert_eq!(graph.modules["c+"].version, "2");
        assert_eq!(graph.modules["c+"].mapping["d"], "d+");
        assert_eq!(graph.modules.len(), 4);
        assert_eq!(graph.root_mapping.len(), 2); // self + ordinary a only
    }

    #[test]
    fn final_walk_prunes_nodep_only_modules_and_their_ordinary_dependencies() {
        // Selection.java:329-333, extended with a transitive dependency of e.
        let f = Fixture::new("bazel_dep(name='a',version='1')\nbazel_dep(name='b',version='1')");
        f.module("a", "1", "bazel_dep(name='d',version='1')");
        f.module("b", "1", "bazel_dep(name='d',version='2')");
        f.module("d", "1", "bazel_dep(name='e',version='1')");
        f.module("d", "2", "bazel_dep(name='e',version='1',repo_name=None)");
        f.module("e", "1", "bazel_dep(name='f',version='1')");
        f.module("f", "1", "");
        let graph = f.resolve();
        assert_eq!(graph.modules["d+"].version, "2");
        assert!(!graph.modules.contains_key("e+"));
        assert!(!graph.modules.contains_key("f+"));
        assert!(!graph.modules["d+"].mapping.contains_key("e"));
        assert_eq!(graph.modules.len(), 3);
        assert_eq!(graph.modules["d+"].module.dependencies.len(), 1);
    }

    #[test]
    fn nodep_root_cycles_and_development_filtering_use_ordinary_dependency_rules() {
        let f = Fixture::new(
            "module(name='root')\nbazel_dep(name='a',version='1')\nbazel_dep(name='a',version='2',repo_name=None,dev_dependency=True)",
        );
        f.module("a", "1", "bazel_dep(name='root',version='999',repo_name=None)\nbazel_dep(name='a',version='777',repo_name=None,dev_dependency=True)");
        f.module("a", "2", "");
        assert_eq!(f.resolve().modules["a+"].version, "2");
        let mut options = f.options();
        options.ignore_dev_dependencies = true;
        let graph = resolve(&f.root, &options).unwrap();
        assert_eq!(graph.modules["a+"].version, "1");
        assert!(!graph.modules["a+"].mapping.contains_key("root"));
        assert_eq!(graph.modules.len(), 1);
    }

    #[test]
    fn multiple_override_requires_all_allowed_versions_in_discovery_not_just_on_disk() {
        let f = Fixture::new(
            "bazel_dep(name='c',version='1')\nmultiple_version_override(module_name='c',versions=['1','2'])",
        );
        f.module("c", "1", "");
        // Existence in the registry is insufficient: the override must not load
        // or inject allowed versions. A malformed file makes this observable.
        f.raw_module("c", "2", "module(");
        f.fails("does not exist in the discovered dependency graph");
        std::fs::write(
            f.root.join("MODULE.bazel"),
            "multiple_version_override(module_name='c',versions=['1'])",
        )
        .unwrap();
        f.fails("at least 2 versions");
    }

    #[test]
    fn multiple_override_snaps_up_to_nearest_allowed_version_and_maps_each_version() {
        let f = Fixture::new(
            "bazel_dep(name='a',version='1')\nbazel_dep(name='b',version='1')\nmultiple_version_override(module_name='c',versions=['3','2'])",
        );
        f.module("a", "1", "bazel_dep(name='c',version='1')");
        f.module("b", "1", "bazel_dep(name='c',version='2',repo_name='low')\nbazel_dep(name='c',version='3',repo_name='high')");
        for version in ["1", "2", "3"] {
            f.module("c", version, "");
        }
        f.module("c", "1", "bazel_dep(name='old',version='1')");
        f.module("c", "2", "bazel_dep(name='new',version='1')");
        f.module("old", "1", "");
        f.module("new", "1", "");
        let graph = f.resolve();
        assert!(!graph.modules.contains_key("c+"));
        assert!(!graph.modules.contains_key("c+1"));
        assert_eq!(graph.modules["c+2"].version, "2");
        assert_eq!(graph.modules["c+3"].version, "3");
        assert_eq!(graph.modules["a+"].mapping["c"], "c+2");
        assert_eq!(graph.modules["b+"].mapping["low"], "c+2");
        assert_eq!(graph.modules["b+"].mapping["high"], "c+3");
        assert_eq!(graph.modules["c+2"].mapping["c"], "c+2");
        assert_eq!(graph.modules["c+3"].mapping.get(""), None);
        assert!(!graph.modules.contains_key("old+"));
        assert_eq!(graph.modules["c+2"].mapping["new"], "new+");
        assert!(
            matches!(&graph.modules["c+2"].source, ModuleSource::Registry { version, .. } if version == "2")
        );
    }

    #[test]
    fn canonical_version_suffix_depends_on_final_ordinary_reachability_not_nodeps() {
        let f = Fixture::new(
            "bazel_dep(name='c',version='1')\nbazel_dep(name='c',version='2',repo_name=None)\nmultiple_version_override(module_name='c',versions=['1','2'])",
        );
        f.module("c", "1", "");
        f.module("c", "2", "");
        let graph = f.resolve();
        assert_eq!(graph.modules.len(), 1);
        assert_eq!(graph.modules["c+"].version, "1");
        assert_eq!(graph.root_mapping["c"], "c+");
        assert_eq!(graph.modules["c+"].mapping["c"], "c+");
        std::fs::write(f.root.join("MODULE.bazel"), "bazel_dep(name='c',version='1',repo_name='low')\nbazel_dep(name='c',version='2',repo_name='high')\nmultiple_version_override(module_name='c',versions=['1','2'])").unwrap();
        let graph = f.resolve();
        assert_eq!(graph.modules.len(), 2);
        assert_eq!(graph.root_mapping["low"], "c+1");
        assert_eq!(graph.root_mapping["high"], "c+2");
    }

    #[test]
    fn above_ceiling_versions_fail_only_when_reachable_after_rewriting() {
        let f = Fixture::new(
            "bazel_dep(name='a',version='1')\nbazel_dep(name='b',version='1')\nbazel_dep(name='c',version='1',repo_name='low')\nbazel_dep(name='c',version='2',repo_name='high')\nmultiple_version_override(module_name='c',versions=['1','2'])",
        );
        f.module("a", "1", "bazel_dep(name='c',version='9')");
        f.module("a", "2", "");
        f.module("b", "1", "bazel_dep(name='a',version='2')");
        for version in ["1", "2", "9"] {
            f.module("c", version, "");
        }
        let graph = f.resolve();
        assert_eq!(graph.modules["a+"].version, "2");
        assert!(!graph.modules.contains_key("c+9"));
        assert_eq!(graph.root_mapping["low"], "c+1");
        // Now the selected a@2 also requires the invalid above-ceiling group.
        f.module("a", "2", "bazel_dep(name='c',version='9')");
        f.fails("not allowed by the multiple_version_override");
    }

    #[test]
    fn above_ceiling_nodep_must_validate_before_nodep_only_pruning() {
        let f = Fixture::new(
            "bazel_dep(name='c',version='1')\nbazel_dep(name='c',version='2',repo_name=None)\nbazel_dep(name='c',version='9',repo_name=None)\nmultiple_version_override(module_name='c',versions=['1','2'])",
        );
        for version in ["1", "2", "9"] {
            f.module("c", version, "");
        }
        // c@9 would disappear from a normal-edge-only graph, but the stricter
        // first walk must still reject its fulfilled nodep edge.
        f.fails("not allowed by the multiple_version_override");
    }

    #[test]
    fn duplicate_aliases_after_multiple_override_snapping_are_rejected() {
        let f = Fixture::new(
            "bazel_dep(name='c',version='1',repo_name='first')\nbazel_dep(name='c',version='2',repo_name='second')\nbazel_dep(name='c',version='3',repo_name='third')\nmultiple_version_override(module_name='c',versions=['2','3'])",
        );
        for version in ["1", "2", "3"] {
            f.module("c", version, "");
        }
        f.fails("duplicate resolved dependency");
    }

    #[test]
    fn nodep_only_module_duplicate_dependencies_are_validated_before_pruning() {
        let f = Fixture::new(
            "bazel_dep(name='c',version='1')\nbazel_dep(name='c',version='2',repo_name=None)\nmultiple_version_override(module_name='c',versions=['1','2'])",
        );
        f.module("c", "1", "");
        f.module("c", "2", "bazel_dep(name='z',version='1',repo_name='first')\nbazel_dep(name='z',version='2',repo_name='second')");
        f.module("z", "1", "");
        f.module("z", "2", "");
        // c@2 and z are eventually nodep-only, but c@2's duplicate ordinary
        // aliases make the intermediate validated graph illegal.
        f.fails("duplicate resolved dependency");
    }

    #[test]
    fn multiple_override_deduplicates_normalized_allowed_versions_and_honors_registry() {
        let f = Fixture::new("");
        f.module("c", "1", "");
        std::fs::write(f.root.join("MODULE.bazel"), format!("bazel_dep(name='c',version='1+request')\nmultiple_version_override(module_name='c',versions=['1+one','1+two'],registry={:?})", format!("file://{}", f.registry.display()))).unwrap();
        // Two entries are required, not two distinct normalized versions.
        let graph = resolve(&f.root, &ResolverOptions::default()).unwrap();
        assert_eq!(graph.modules.len(), 1);
        assert_eq!(graph.modules["c+"].version, "1");
        assert!(
            matches!(&graph.modules["c+"].source, ModuleSource::Registry { registry, version, .. } if registry == &f.registry && version == "1")
        );
    }

    #[test]
    fn well_known_platforms_repo_keeps_its_official_canonical_name() {
        let f = Fixture::new("bazel_dep(name='platforms',version='1',repo_name='constraints')");
        f.module("platforms", "1", "");
        let graph = f.resolve();
        assert_eq!(graph.modules["platforms"].canonical_name, "platforms");
        assert_eq!(graph.root_mapping["constraints"], "platforms");
        assert_eq!(graph.modules["platforms"].mapping["platforms"], "platforms");
    }

    #[test]
    fn duplicate_root_overrides_are_ignored_only_in_ignore_dev_context() {
        let f = Fixture::new(
            "bazel_dep(name='a',version='1')\nlocal_path_override(module_name='a',path='unused-one')\nlocal_path_override(module_name='a',path='unused-two')",
        );
        f.module("a", "1", "");
        f.fails("duplicate override");
        assert!(
            bzlmod::evaluate(&f.root.join("MODULE.bazel"))
                .unwrap_err()
                .contains("duplicate override")
        );
        let mut options = f.options();
        options.ignore_dev_dependencies = true;
        let graph = resolve(&f.root, &options).unwrap();
        assert!(graph.root.overrides.is_empty());
        assert_eq!(graph.modules["a+"].version, "1");
        assert!(matches!(
            graph.modules["a+"].source,
            ModuleSource::Registry { .. }
        ));
    }

    #[test]
    fn only_root_exposes_empty_apparent_repository_name() {
        let f = Fixture::new("module(name='root')\nbazel_dep(name='a',version='1')");
        f.raw_module("a", "1", "module(name='a',version='1',repo_name='self_alias')\nbazel_dep(name='root',version='999',repo_name='back')");
        let graph = f.resolve();
        assert_eq!(graph.root_mapping.get("").map(String::as_str), Some(""));
        let mapping = &graph.modules["a+"].mapping;
        assert_eq!(mapping.get(""), None);
        assert_eq!(mapping.get("self_alias").map(String::as_str), Some("a+"));
        assert_eq!(mapping.get("back").map(String::as_str), Some(""));
    }

    #[test]
    fn duplicate_nonroot_overrides_are_ignored_for_registry_and_local_modules() {
        let f = Fixture::new("bazel_dep(name='a',version='1')");
        let declarations = "local_path_override(module_name='unused',path='first')\nlocal_path_override(module_name='unused',path='second')";
        f.module("a", "1", declarations);
        let graph = f.resolve();
        assert!(graph.modules["a+"].module.overrides.is_empty());
        assert_eq!(graph.modules.len(), 1);
        f.local("local-a", &format!("module(name='a')\n{declarations}"));
        std::fs::write(
            f.root.join("MODULE.bazel"),
            "bazel_dep(name='a')\nlocal_path_override(module_name='a',path='local-a')",
        )
        .unwrap();
        let graph = f.resolve();
        assert!(graph.modules["a+"].module.overrides.is_empty());
        assert!(matches!(
            graph.modules["a+"].source,
            ModuleSource::LocalPath(_)
        ));
    }

    #[test]
    fn blank_metadata_and_null_yanked_versions_are_empty_but_bad_shapes_fail() {
        let f = Fixture::new("bazel_dep(name='a',version='1')");
        f.module("a", "1", "");
        for text in ["", " \t\n\r ", "{}", r#"{"yanked_versions":null}"#] {
            f.metadata("a", text);
            assert_eq!(f.resolve().modules["a+"].version, "1");
        }
        for (text, expected) in [
            ("{", "invalid registry metadata"),
            ("[]", "registry metadata must be an object"),
            ("null", "registry metadata must be an object"),
            (
                r#"{"yanked_versions":[]}"#,
                "yanked_versions must be an object",
            ),
            (
                r#"{"yanked_versions":{"1":null}}"#,
                "reason must be a string",
            ),
        ] {
            f.metadata("a", text);
            f.fails(expected);
        }
    }

    #[test]
    fn missing_registry_root_falls_back_but_non_directory_registry_is_an_error() {
        let f = Fixture::new("bazel_dep(name='a',version='1')");
        f.module("a", "1", "");
        let missing = f.base.join("does-not-exist").join("registry");
        let options = ResolverOptions {
            registries: vec![missing.clone(), f.registry.clone()],
            ..Default::default()
        };
        assert!(!missing.exists());
        assert_eq!(
            resolve(&f.root, &options).unwrap().modules["a+"].version,
            "1"
        );
        let file = f.base.join("not-a-directory");
        std::fs::write(&file, "registry cannot be a file").unwrap();
        let options = ResolverOptions {
            registries: vec![file, f.registry.clone()],
            ..Default::default()
        };
        assert!(
            resolve(&f.root, &options)
                .unwrap_err()
                .contains("not a directory")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_first_registry_is_not_hidden_by_a_valid_fallback() {
        let f = Fixture::new("bazel_dep(name='a',version='1')");
        f.module("a", "1", "");
        let link = f.base.join("linked-registry");
        std::os::unix::fs::symlink(&f.registry, &link).unwrap();
        let options = ResolverOptions {
            registries: vec![link, f.registry.clone()],
            ..Default::default()
        };
        assert!(
            resolve(&f.root, &options)
                .unwrap_err()
                .contains("symlink registry paths")
        );
    }

    #[cfg(unix)]
    #[test]
    fn permission_denied_registry_is_not_hidden_by_a_valid_fallback() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new("bazel_dep(name='a',version='1')");
        f.module("a", "1", "");
        let denied = f.base.join("denied-registry");
        std::fs::create_dir(&denied).unwrap();
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o0)).unwrap();
        let probe = std::fs::read_dir(&denied);
        let options = ResolverOptions {
            registries: vec![denied.clone(), f.registry.clone()],
            ..Default::default()
        };
        let result = resolve(&f.root, &options);
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Privileged test runners can bypass Unix permissions; assert the error
        // on every runner where the OS actually denies access to this fixture.
        if let Err(error) = probe {
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            assert!(result.unwrap_err().contains("Permission denied"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn registry_symlink_components_and_metadata_are_rejected() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new("bazel_dep(name='a',version='1')");
        let outside = f.local("outside", "module(name='a',version='1')");
        let parent = f.registry.join("modules/a");
        std::fs::create_dir_all(&parent).unwrap();
        symlink(&outside, parent.join("1")).unwrap();
        f.fails("symlink registry paths");
        std::fs::remove_file(parent.join("1")).unwrap();
        f.module("a", "1", "");
        std::fs::write(outside.join("metadata.json"), "{}").unwrap();
        symlink(outside.join("metadata.json"), parent.join("metadata.json")).unwrap();
        f.fails("symlink registry paths");
    }
}
