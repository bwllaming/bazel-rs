//! Runfiles, SymlinkEntry and FilesToRunProvider analysis values and the
//! RunfilesProvider/FilesToRunProvider of configured targets (N23a,
//! docs/RUNFILES.md), and the runfiles mapping of a runfiles tree
//! (Runfiles.getRunfilesInputs, N23f, docs/RUNFILES_TREES.md). The tree,
//! manifest and repository-mapping actions are planned by crate::engine with
//! crate::runfiles_tree.
//!
//! Paths below are under src/main/java/com/google/devtools/build/lib/ of the
//! pinned Bazel 9.3 sources.

use allocative::Allocative;
use anyhow::{anyhow, bail, ensure};
use starlark::collections::StarlarkHasher;
use starlark::environment::{Methods, MethodsBuilder};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::dict::DictRef;
use starlark::values::structs::StructRef;
use starlark::values::{
    Heap, NoSerialize, OwnedFrozenValue, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
    Value, starlark_value,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use crate::starlark_types::{Depset, DepsetNode, File, Item, ItemType, Order, SymlinkEntry};

const RUNFILES_CLASS: &str = "com.google.devtools.build.lib.analysis.Runfiles";
const FILES_TO_RUN_CLASS: &str = "com.google.devtools.build.lib.analysis.FilesToRunProvider";

#[allow(unsafe_code)]
mod values {
    use super::*;

    /// analysis/Runfiles.java:70-215: artifacts (COMPILE order, :497-503),
    /// symlinks and root symlinks (STABLE order), the empty-files supplier
    /// and the conflict policy.
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub struct Runfiles {
        pub(in crate::runfiles) artifacts: Arc<DepsetNode>,
        pub(in crate::runfiles) symlinks: Arc<DepsetNode>,
        pub(in crate::runfiles) root_symlinks: Arc<DepsetNode>,
        /// The EmptyFilesSupplier (Runfiles.java:170-182).
        pub(in crate::runfiles) empty_files: EmptyFiles,
        /// ConflictPolicy.ERROR (Runfiles.java:192-203); WARN otherwise.
        pub(in crate::runfiles) conflict_error: bool,
    }

    /// Runfiles.EmptyFilesSupplier implementations: the dummy supplier
    /// (Runfiles.java:74-91) and BazelPyBuiltins.GET_INIT_PY_FILES
    /// (bazel/rules/python/BazelPyBuiltins.java:29-30, installed by
    /// `py_internal.merge_runfiles_with_generated_inits_empty_files_supplier`,
    /// rules/python/PyBuiltins.java:223-250).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Allocative)]
    pub enum EmptyFiles {
        #[default]
        None,
        PythonInit,
    }

    /// FilesToRunProvider and its two private subclasses
    /// (analysis/FilesToRunProvider.java:33-162).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Allocative)]
    pub enum FilesToRunKind {
        /// `FilesToRunProvider` itself (EMPTY or files without an executable).
        #[default]
        Base,
        /// `SingleExecutableFilesToRunProvider`.
        Single,
        /// `FullFilesToRunProvider` (runfiles support and/or executable).
        Full,
    }

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub struct FilesToRun {
        pub kind: FilesToRunKind,
        pub executable: Option<File>,
        /// `<executable>.runfiles/MANIFEST` (build_runfile_links default).
        pub runfiles_manifest: Option<File>,
        /// `<executable>.repo_mapping`.
        pub repo_mapping_manifest: Option<File>,
        /// The runfiles of the RunfilesSupport (Full only).
        pub runfiles: Option<Runfiles>,
        /// FilesToRunProvider.getFilesToRun without the runfiles tree artifact
        /// (an action tool adds them, StarlarkAction.Builder.addTool).
        pub files: Vec<File>,
        /// Bazel compares FilesToRunProvider objects by identity.
        pub(in crate::runfiles) id: u64,
        /// File targets create a new provider on each access
        /// (FileConfiguredTarget.getProvider, configuredtargets/FileConfiguredTarget.java:85-101).
        pub(in crate::runfiles) fresh: bool,
    }

    /// `ctx.outputs`: the frozen outputs struct plus, for executable and
    /// test rules, the on-demand `executable` field
    /// (analysis/starlark/StarlarkRuleContext.java:344-450).
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct Outputs {
        #[allocative(skip)]
        pub fields: OwnedFrozenValue,
        #[allocative(skip)]
        pub executable: Option<DefaultExecutable>,
        #[allocative(skip)]
        pub active: Arc<AtomicBool>,
        pub label: String,
    }

    /// `ctx.executable`: a struct with Bazel's unknown-field message
    /// (analysis/starlark/StarlarkAttributesCollection.java:82-86).
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct ExecutableStruct {
        #[allocative(skip)]
        pub fields: OwnedFrozenValue,
    }
}
pub use values::{EmptyFiles, FilesToRun, FilesToRunKind, Runfiles};
pub(crate) use values::{ExecutableStruct, Outputs};

starlark_simple_value!(Runfiles);
starlark_simple_value!(FilesToRun);
starlark_simple_value!(Outputs);
starlark_simple_value!(ExecutableStruct);

static NEXT_SYMLINK: AtomicU64 = AtomicU64::new(0);
static NEXT_FILES_TO_RUN: AtomicU64 = AtomicU64::new(1);

impl Default for FilesToRun {
    fn default() -> Self {
        Self {
            kind: FilesToRunKind::Base,
            executable: None,
            runfiles_manifest: None,
            repo_mapping_manifest: None,
            runfiles: None,
            files: Vec::new(),
            id: NEXT_FILES_TO_RUN.fetch_add(1, Ordering::Relaxed),
            fresh: false,
        }
    }
}

fn node(
    direct: Vec<Item>,
    children: Vec<Arc<DepsetNode>>,
    order: Order,
    kind: ItemType,
) -> anyhow::Result<Arc<DepsetNode>> {
    // Empty children never contribute; skipping them keeps empty depsets of
    // another element type from failing the homogeneous-type check.
    let children = children.into_iter().filter(|child| !child.empty).collect();
    Ok(Depset::compose(direct, children, order, Some(kind))?.node)
}

fn flatten(node: &Arc<DepsetNode>) -> Vec<Item> {
    Depset {
        node: Arc::clone(node),
    }
    .flatten()
    .into_iter()
    .cloned()
    .collect()
}

impl Runfiles {
    /// Runfiles.EMPTY (Runfiles.java:92): one shared identity.
    pub fn empty() -> Self {
        static EMPTY: OnceLock<Runfiles> = OnceLock::new();
        EMPTY
            .get_or_init(|| {
                RunfilesBuilder::default()
                    .build()
                    .expect("empty runfiles always compose")
            })
            .clone()
    }

    /// Runfiles.isEmpty (Runfiles.java:446-448): empty filenames do not count.
    pub fn is_empty(&self) -> bool {
        self.artifacts.empty && self.symlinks.empty && self.root_symlinks.empty
    }

    /// Bazel's Runfiles has object identity; equal nested sets and policy
    /// are the same object here.
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.artifacts, &other.artifacts)
            && Arc::ptr_eq(&self.symlinks, &other.symlinks)
            && Arc::ptr_eq(&self.root_symlinks, &other.root_symlinks)
            && self.empty_files == other.empty_files
            && self.conflict_error == other.conflict_error
    }

    pub fn conflict_error(&self) -> bool {
        self.conflict_error
    }

    /// `files.to_list()`.
    pub fn artifacts(&self) -> Vec<File> {
        flatten(&self.artifacts)
            .into_iter()
            .filter_map(|item| match item {
                Item::File(file) => Some(file),
                _ => None,
            })
            .collect()
    }

    /// `(path, target)` of `symlinks.to_list()` (`root` selects root_symlinks).
    pub fn symlink_entries(&self, root: bool) -> Vec<(String, File)> {
        flatten(if root {
            &self.root_symlinks
        } else {
            &self.symlinks
        })
        .into_iter()
        .filter_map(|item| match item {
            Item::Symlink(entry) => Some((entry.path, entry.target)),
            _ => None,
        })
        .collect()
    }

    /// Runfiles.getAllArtifacts (Runfiles.java:422-438): artifacts, then the
    /// symlink and root symlink targets, deduplicated.
    pub fn all_artifacts(&self) -> Vec<File> {
        let mut seen = BTreeSet::new();
        let mut files = Vec::new();
        let symlinks = self.symlink_entries(false);
        let roots = self.symlink_entries(true);
        for file in self
            .artifacts()
            .into_iter()
            .chain(symlinks.into_iter().map(|(_, file)| file))
            .chain(roots.into_iter().map(|(_, file)| file))
        {
            if seen.insert(file.clone()) {
                files.push(file);
            }
        }
        files
    }
}

/// The directory of the main repository in runfiles trees: the workspace
/// name, always `_main` with Bzlmod (RuleContext.getWorkspaceName;
/// LabelConstants.DEFAULT_REPOSITORY_DIRECTORY).
pub const WORKSPACE_NAME: &str = "_main";

/// PathFragment.startsWith for normalized relative paths: segment-wise.
fn starts_with_segments(path: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// PathFragment.getParentDirectory of a normalized relative path with more
/// than one segment.
fn parent(path: &str) -> Option<&str> {
    path.rfind('/').map(|index| &path[..index])
}

/// java.lang.String.hashCode over UTF-16 code units.
fn java_string_hash(text: &str) -> i32 {
    text.encode_utf16().fold(0i32, |hash, unit| {
        hash.wrapping_mul(31).wrapping_add(i32::from(unit))
    })
}

/// The iteration order of a `java.util.HashSet<PathFragment>` filled with
/// `items` in order (PathFragment.hashCode is its normalized string's,
/// vfs/PathFragment.java:500-503): HashMap buckets `(h ^ h >>> 16) & (n - 1)`
/// of the final table size `n` (16, doubled while the size exceeds 3/4 of
/// it), each bucket in insertion order (resizes keep the relative order).
fn java_hash_set_order(items: Vec<String>) -> Vec<String> {
    let mut unique: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    for item in items {
        if seen.insert(item.clone()) {
            unique.push(item);
        }
    }
    let mut capacity: usize = 16;
    while unique.len() > capacity / 4 * 3 {
        capacity *= 2;
    }
    let bucket = |item: &String| {
        let hash = java_string_hash(item) as u32;
        ((hash ^ (hash >> 16)) as usize) & (capacity - 1)
    };
    let mut indexed: Vec<(usize, usize, String)> = unique
        .into_iter()
        .enumerate()
        .map(|(index, item)| (bucket(&item), index, item))
        .collect();
    indexed.sort();
    indexed.into_iter().map(|(_, _, item)| item).collect()
}

impl EmptyFiles {
    /// EmptyFilesSupplier.getExtraPaths: PythonUtils.GetInitPyFiles
    /// (rules/python/PythonUtils.java:40-109) with BazelPyBuiltins'
    /// `source -> false` package-init predicate. `manifest` is iterated in
    /// order; the result is in HashSet order (ImmutableSet.copyOf).
    pub(crate) fn extra_paths(self, manifest: &[String]) -> Vec<String> {
        if self == EmptyFiles::None {
            return Vec::new();
        }
        let known: BTreeSet<&str> = manifest.iter().map(String::as_str).collect();
        let mut result = Vec::new();
        for source in manifest {
            // FileType.of(".py", ".so", ".pyc") (PythonUtils.java:32).
            if ![".py", ".so", ".pyc"]
                .iter()
                .any(|suffix| source.ends_with(suffix))
            {
                continue;
            }
            let mut current = source.as_str();
            while let Some(directory) = parent(current) {
                current = directory;
                let last = directory.rsplit('/').next().unwrap_or(directory);
                if last != "__pycache__" {
                    let init = format!("{directory}/__init__.py");
                    let initc = format!("{directory}/__init__.pyc");
                    if !known.contains(init.as_str()) && !known.contains(initc.as_str()) {
                        result.push(init);
                    }
                }
            }
        }
        java_hash_set_order(result)
    }
}

/// One runfiles tree entry: the artifact a path links to, or an empty file.
pub type RunfilesMapping = BTreeMap<String, Option<File>>;

impl Runfiles {
    /// Runfiles.getEmptyFilenames (Runfiles.java:255-265).
    pub fn empty_filenames(&self) -> Vec<String> {
        if self.empty_files == EmptyFiles::None {
            return Vec::new();
        }
        let mut keys: Vec<String> = Vec::new();
        let mut seen = BTreeSet::new();
        for path in self
            .symlink_entries(false)
            .into_iter()
            .map(|(path, _)| path)
            .chain(self.artifacts().into_iter().map(|file| file.short_path))
        {
            if seen.insert(path.clone()) {
                keys.push(path);
            }
        }
        self.empty_files.extra_paths(&keys)
    }

    fn empty_filenames_node(&self) -> anyhow::Result<Arc<DepsetNode>> {
        node(
            self.empty_filenames()
                .into_iter()
                .map(Item::String)
                .collect(),
            Vec::new(),
            Order::Default,
            ItemType::String,
        )
    }

    /// Runfiles.getRunfilesInputs (Runfiles.java:343-401): the runfiles tree
    /// below `<executable>.runfiles`, keyed by tree-relative path (sorted like
    /// the TreeMap of PathFragments). `None` values are empty files.
    /// Symlinks then artifacts (later entries win), obscured entries dropped
    /// (filterListForObscuringSymlinks, :267-314, each reported to
    /// `conflicts` when it does not resolve to the same file through its
    /// ancestor), the empty files, the workspace prefix (`..`-relative
    /// external paths become siblings), root symlinks, `_repo_mapping` and
    /// `_main/.runfile` when no entry is below `_main`.
    pub fn runfiles_inputs(
        &self,
        repo_mapping: Option<&File>,
        mut conflicts: Option<&mut Vec<String>>,
    ) -> RunfilesMapping {
        // LinkedHashMap: a replaced key keeps its first position.
        let mut order: Vec<String> = Vec::new();
        let mut manifest: BTreeMap<String, File> = BTreeMap::new();
        let mut put = |path: String, file: File| {
            if manifest.insert(path.clone(), file).is_none() {
                order.push(path);
            }
        };
        for (path, file) in self.symlink_entries(false) {
            put(path, file);
        }
        for file in self.artifacts() {
            put(file.short_path.clone(), file);
        }
        // filterListForObscuringSymlinks.
        let mut kept: Vec<String> = Vec::new();
        let mut clear: BTreeSet<String> = BTreeSet::new();
        'outer: for source in &order {
            let mut parents = Vec::new();
            let mut current = source.as_str();
            while let Some(prefix) = parent(current) {
                current = prefix;
                if clear.contains(prefix) {
                    break;
                }
                parents.push(prefix.to_owned());
                if let Some(ancestor) = manifest.get(prefix) {
                    if let Some(conflicts) = conflicts.as_deref_mut() {
                        let suffix = &source[prefix.len() + 1..];
                        let via_ancestor = format!("{}/{suffix}", ancestor.path);
                        let expected = &manifest[source].path;
                        if via_ancestor != *expected {
                            conflicts.push(format!(
                                "runfiles symlink {source} -> {expected} obscured by {prefix} -> {}",
                                ancestor.path
                            ));
                        }
                    }
                    continue 'outer;
                }
            }
            clear.extend(parents);
            kept.push(source.clone());
        }
        let mut entries: Vec<(String, Option<File>)> = kept
            .iter()
            .map(|path| (path.clone(), Some(manifest[path].clone())))
            .collect();
        for extra in self.empty_files.extra_paths(&kept) {
            match entries.iter_mut().find(|(path, _)| *path == extra) {
                Some(entry) => entry.1 = None,
                None => entries.push((extra, None)),
            }
        }
        let mut result = RunfilesMapping::new();
        let mut saw_workspace = false;
        for (path, file) in entries {
            if let Some(external) = path.strip_prefix("../") {
                saw_workspace |= starts_with_segments(external, WORKSPACE_NAME);
                result.insert(external.to_owned(), file);
            } else if path == ".." {
                result.insert(String::new(), file);
            } else {
                saw_workspace = true;
                result.insert(format!("{WORKSPACE_NAME}/{path}"), file);
            }
        }
        for (path, file) in self.symlink_entries(true) {
            saw_workspace |= starts_with_segments(&path, WORKSPACE_NAME);
            result.insert(path, Some(file));
        }
        if let Some(manifest) = repo_mapping {
            result.insert("_repo_mapping".to_owned(), Some(manifest.clone()));
        }
        if !saw_workspace {
            result.insert(format!("{WORKSPACE_NAME}/.runfile"), None);
        }
        result
    }
}

/// PyBuiltins.mergeRunfilesWithGeneratedInitsEmptyFilesSupplier
/// (rules/python/PyBuiltins.java:237-250): new runfiles with the Python
/// `__init__.py` supplier merged with `runfiles`, which must be non-empty.
/// `Err(None)` is a parameter type error.
pub(crate) fn merge_with_generated_inits<'v>(
    runfiles: Value<'v>,
    heap: Heap<'v>,
) -> Result<Value<'v>, Option<String>> {
    let runfiles = Runfiles::from_value(runfiles).ok_or(None)?;
    if runfiles.is_empty() {
        return Err(Some("input runfiles cannot be empty".to_owned()));
    }
    let mut builder = RunfilesBuilder::default();
    builder
        .set_empty_files(EmptyFiles::PythonInit)
        .merge(runfiles);
    let merged = builder.build().map_err(|error| Some(error.to_string()))?;
    Ok(heap.alloc(merged))
}

/// SourceManifestAction.ManifestType.SOURCE_SYMLINKS.writeEntry
/// (analysis/SourceManifestAction.java:369-404): `path target\n`, sorted by
/// path; an empty file has no target (the trailing space stays). Paths with a
/// space or newline (or a target with a newline) are escaped (`\s`, `\n`,
/// `\b`) and the line starts with a space.
pub fn manifest_line(path: &str, target: Option<&str>, out: &mut Vec<u8>) {
    let needs_escaping = path.contains(' ')
        || path.contains('\n')
        || target.is_some_and(|target| target.contains('\n'));
    let escape = |text: &str, spaces: bool| {
        let mut escaped = String::with_capacity(text.len());
        for c in text.chars() {
            match c {
                ' ' if spaces => escaped.push_str("\\s"),
                '\n' => escaped.push_str("\\n"),
                '\\' => escaped.push_str("\\b"),
                other => escaped.push(other),
            }
        }
        escaped
    };
    if needs_escaping {
        out.push(b' ');
        out.extend_from_slice(escape(path, true).as_bytes());
    } else {
        out.extend_from_slice(path.as_bytes());
    }
    out.push(b' ');
    if let Some(target) = target {
        if needs_escaping {
            out.extend_from_slice(escape(target, false).as_bytes());
        } else {
            out.extend_from_slice(target.as_bytes());
        }
    }
    out.push(b'\n');
}

impl fmt::Display for Runfiles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // StarlarkValue.repr default (net/starlark/java/eval/StarlarkValue.java:50-52).
        write!(f, "<unknown object {RUNFILES_CLASS}>")
    }
}

/// Runfiles.Builder (Runfiles.java:488-781) without workspace prefixes.
#[derive(Default)]
pub(crate) struct RunfilesBuilder {
    direct: Vec<Item>,
    transitive: Vec<Arc<DepsetNode>>,
    symlinks: Vec<Item>,
    transitive_symlinks: Vec<Arc<DepsetNode>>,
    root_symlinks: Vec<Item>,
    transitive_root_symlinks: Vec<Arc<DepsetNode>>,
    empty_files: EmptyFiles,
    conflict_error: bool,
}

impl RunfilesBuilder {
    /// Builder.setEmptyFilesSupplier (Runfiles.java:603-611).
    pub(crate) fn set_empty_files(&mut self, supplier: EmptyFiles) -> &mut Self {
        self.empty_files = supplier;
        self
    }

    pub(crate) fn add_artifact(&mut self, file: File) -> &mut Self {
        self.direct.push(Item::File(file));
        self
    }

    fn add_transitive(&mut self, node: &Arc<DepsetNode>) -> &mut Self {
        self.transitive.push(Arc::clone(node));
        self
    }

    /// addTransitiveArtifacts of a STABLE_ORDER set of `files` (a target's
    /// files to build).
    pub(crate) fn add_files_to_build(&mut self, files: &[File]) -> &mut Self {
        if !files.is_empty() {
            let set = node(
                files.iter().cloned().map(Item::File).collect(),
                Vec::new(),
                Order::Default,
                ItemType::File,
            )
            .expect("File sets always compose");
            self.transitive.push(set);
        }
        self
    }

    /// Builder.merge (Runfiles.java:640-662): the strictest conflict policy
    /// propagates even from empty runfiles.
    pub(crate) fn merge(&mut self, runfiles: &Runfiles) -> &mut Self {
        if !self.conflict_error {
            self.conflict_error = runfiles.conflict_error;
        }
        if runfiles.is_empty() {
            return self;
        }
        self.transitive.push(Arc::clone(&runfiles.artifacts));
        self.transitive_symlinks
            .push(Arc::clone(&runfiles.symlinks));
        self.transitive_root_symlinks
            .push(Arc::clone(&runfiles.root_symlinks));
        // Only one supplier exists natively, so the checkState of
        // differing suppliers (:654-660) cannot fail.
        if self.empty_files == EmptyFiles::None {
            self.empty_files = runfiles.empty_files;
        }
        self
    }

    pub(crate) fn build(self) -> anyhow::Result<Runfiles> {
        Ok(Runfiles {
            artifacts: node(
                self.direct,
                self.transitive,
                Order::Postorder,
                ItemType::File,
            )?,
            symlinks: node(
                self.symlinks,
                self.transitive_symlinks,
                Order::Default,
                ItemType::Symlink,
            )?,
            root_symlinks: node(
                self.root_symlinks,
                self.transitive_root_symlinks,
                Order::Default,
                ItemType::Symlink,
            )?,
            empty_files: self.empty_files,
            conflict_error: self.conflict_error,
        })
    }
}

fn runfiles_type_error(
    function: &str,
    parameter: &str,
    value: Value<'_>,
    want: &str,
) -> anyhow::Error {
    anyhow!(
        "in call to {function}(), parameter '{parameter}' got value of type '{}', want '{want}'",
        value.get_type()
    )
}

fn sequence_items<'v>(value: Value<'v>) -> Option<Vec<Value<'v>>> {
    crate::custom_rule::sequence_ref(value).map(<[Value<'v>]>::to_vec)
}

starlark::methods_static!(RUNFILES_METHODS = runfiles_methods);

#[starlark_module]
fn runfiles_methods(builder: &mut MethodsBuilder) {
    /// Runfiles.getArtifactsForStarlark (Runfiles.java:225-228).
    #[starlark(attribute)]
    fn files(this: &Runfiles) -> anyhow::Result<Depset> {
        Ok(Depset {
            node: Arc::clone(&this.artifacts),
        })
    }
    #[starlark(attribute)]
    fn symlinks(this: &Runfiles) -> anyhow::Result<Depset> {
        Ok(Depset {
            node: Arc::clone(&this.symlinks),
        })
    }
    #[starlark(attribute)]
    fn root_symlinks(this: &Runfiles) -> anyhow::Result<Depset> {
        Ok(Depset {
            node: Arc::clone(&this.root_symlinks),
        })
    }
    /// Runfiles.getEmptyFilenamesForStarlark (Runfiles.java:246-265): a
    /// STABLE depset of the supplier's extra paths.
    #[starlark(attribute)]
    fn empty_filenames(this: &Runfiles) -> anyhow::Result<Depset> {
        Ok(Depset {
            node: this.empty_filenames_node()?,
        })
    }
    /// Runfiles.merge (Runfiles.java:830-846): an empty side returns the
    /// other object itself.
    fn merge<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] other: Value<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let me = Runfiles::from_value(this).ok_or_else(|| anyhow!("expected runfiles"))?;
        let Some(theirs) = Runfiles::from_value(other) else {
            return Err(runfiles_type_error("merge", "other", other, "runfiles"));
        };
        if me.is_empty() {
            return Ok(other);
        }
        if theirs.is_empty() {
            return Ok(this);
        }
        let mut builder = RunfilesBuilder::default();
        builder.merge(me).merge(theirs);
        Ok(heap.alloc(builder.build()?))
    }
    /// Runfiles.mergeAll (Runfiles.java:848-885): exactly one non-empty
    /// input is returned as-is; none returns Runfiles.EMPTY.
    fn merge_all<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] other: Value<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let me = Runfiles::from_value(this).ok_or_else(|| anyhow!("expected runfiles"))?;
        let Some(items) = sequence_items(other) else {
            return Err(runfiles_type_error("merge_all", "other", other, "sequence"));
        };
        let mut builder: Option<RunfilesBuilder> = None;
        let mut unique: Option<Value<'v>> = None;
        if !me.is_empty() {
            let mut first = RunfilesBuilder::default();
            first.merge(me);
            builder = Some(first);
            unique = Some(this);
        }
        let mut values = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let Some(runfiles) = Runfiles::from_value(*item) else {
                bail!(
                    "at index {index} of param, got element of type {}, want runfiles",
                    item.get_type()
                );
            };
            values.push((*item, runfiles));
        }
        for (value, runfiles) in values {
            if runfiles.is_empty() {
                continue;
            }
            match builder.as_mut() {
                None => {
                    let mut first = RunfilesBuilder::default();
                    first.merge(runfiles);
                    builder = Some(first);
                    unique = Some(value);
                }
                Some(builder) => {
                    builder.merge(runfiles);
                    unique = None;
                }
            }
        }
        if let Some(unique) = unique {
            return Ok(unique);
        }
        Ok(match builder {
            Some(builder) => heap.alloc(builder.build()?),
            None => heap.alloc(Runfiles::empty()),
        })
    }
}

/// EvalUtils.setField for a non-Structure value
/// (net/starlark/java/eval/EvalUtils.java:529-536).
fn cannot_set(field: &str, kind: &str) -> starlark::Result<()> {
    Err(starlark::Error::new_value(anyhow!(
        "cannot set .{field} field of {kind} value"
    )))
}

/// Structure.setField (net/starlark/java/eval/Structure.java:78-81).
pub(crate) fn no_field_assignment(kind: &str) -> starlark::Result<()> {
    Err(starlark::Error::new_value(anyhow!(
        "{kind} value does not support field assignment"
    )))
}

#[starlark_value(type = "runfiles")]
impl<'v> StarlarkValue<'v> for Runfiles {
    fn get_methods() -> Option<&'static Methods> {
        Some(RUNFILES_METHODS.methods())
    }

    /// Runfiles.debugPrint (analysis/Runfiles.java:935-945): the four Starlark
    /// depsets, each printed with its repr (Depset has no debugPrint).
    fn collect_debug_print(&self, collector: &mut String, _eval: &Evaluator<'v, '_, '_>) {
        use std::fmt::Write;
        let depset = |node: &Arc<DepsetNode>| Depset {
            node: Arc::clone(node),
        };
        let empty_files = self
            .empty_filenames_node()
            .map(|node| depset(&node).to_string())
            .unwrap_or_else(|_| "depset([])".to_owned());
        write!(
            collector,
            "Runfiles(empty_files = {empty_files}, files = {}, root_symlinks = {}, symlinks = {})",
            depset(&self.artifacts),
            depset(&self.root_symlinks),
            depset(&self.symlinks),
        )
        .unwrap();
    }

    fn set_attr(&self, attribute: &str, _new_value: Value<'v>) -> starlark::Result<()> {
        cannot_set(attribute, "runfiles")
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Runfiles::from_value(other).is_some_and(|other| self.same(other)))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        // Identity hash, consistent with `same` (Runfiles.isImmutable, :216-219).
        (Arc::as_ptr(&self.artifacts) as usize).hash(hasher);
        (Arc::as_ptr(&self.symlinks) as usize).hash(hasher);
        (Arc::as_ptr(&self.root_symlinks) as usize).hash(hasher);
        Ok(())
    }
}

impl Default for Runfiles {
    fn default() -> Self {
        Self::empty()
    }
}

impl fmt::Display for SymlinkEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SymlinkEntry.repr (analysis/SymlinkEntry.java:79-86).
        write!(
            f,
            "SymlinkEntry(path = {}, target_file = {})",
            starlark::values::string::StarlarkStr::repr(&self.path),
            self.target
        )
    }
}

#[starlark_value(type = "SymlinkEntry")]
impl<'v> StarlarkValue<'v> for SymlinkEntry {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        // SymlinkEntryApi (starlarkbuildapi/SymlinkEntryApi.java:24-37).
        Some(match name {
            "path" => heap.alloc(self.path.as_str()),
            "target_file" => crate::starlark_types::alloc_file(heap, self.target.clone()),
            _ => return None,
        })
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["path".to_owned(), "target_file".to_owned()]
    }

    fn set_attr(&self, attribute: &str, _new_value: Value<'v>) -> starlark::Result<()> {
        cannot_set(attribute, "SymlinkEntry")
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(SymlinkEntry::from_value(other).is_some_and(|other| other.id == self.id))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.id.hash(hasher);
        Ok(())
    }
}

/// A new SymlinkEntry identity (`new SymlinkEntry(path, artifact)`).
fn symlink_entry(path: String, target: File) -> SymlinkEntry {
    SymlinkEntry {
        id: NEXT_SYMLINK.fetch_add(1, Ordering::Relaxed),
        path,
        target,
    }
}

/// PathFragment.create normalization (vfs/PathFragment.java): `.` and empty
/// segments drop, `..` cancels a preceding segment.
fn normalize_path(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => match segments.last() {
                Some(last) if *last != ".." => {
                    segments.pop();
                }
                _ if absolute => {}
                _ => segments.push(".."),
            },
            other => segments.push(other),
        }
    }
    let joined = segments.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

impl FilesToRun {
    /// SingleExecutableFilesToRunProvider of a file target
    /// (analysis/configuredtargets/FileConfiguredTarget.java:98-101).
    pub fn single(file: File) -> Self {
        Self {
            kind: FilesToRunKind::Single,
            executable: Some(file.clone()),
            files: vec![file],
            ..Self::default()
        }
    }

    /// The provider a read observes: a new identity for file targets.
    pub(crate) fn observed(&self) -> Self {
        if self.fresh {
            Self {
                id: NEXT_FILES_TO_RUN.fetch_add(1, Ordering::Relaxed),
                ..self.clone()
            }
        } else {
            self.clone()
        }
    }

    /// FilesToRunProvider.create (FilesToRunProvider.java:40-58).
    fn create(
        files_to_run: &[File],
        tree: bool,
        support: Option<(Runfiles, Option<File>, File)>,
        executable: Option<File>,
    ) -> Self {
        let count = files_to_run.len() + usize::from(tree);
        if count == 0 {
            return Self::default();
        }
        if support.is_none() && executable.is_none() {
            return Self {
                files: files_to_run.to_vec(),
                ..Self::default()
            };
        }
        if count == 1 && support.is_none() && executable.as_ref() == files_to_run.first() {
            return Self {
                kind: FilesToRunKind::Single,
                executable,
                files: files_to_run.to_vec(),
                ..Self::default()
            };
        }
        let (runfiles, runfiles_manifest, repo_mapping_manifest) = match support {
            Some((runfiles, manifest, mapping)) => (Some(runfiles), manifest, Some(mapping)),
            None => (None, None, None),
        };
        Self {
            kind: FilesToRunKind::Full,
            executable,
            runfiles_manifest,
            repo_mapping_manifest,
            runfiles,
            files: files_to_run.to_vec(),
            ..Self::default()
        }
    }

    /// The runfiles an action using this executable needs (attached by N23d).
    pub fn runfiles(&self) -> Option<&Runfiles> {
        self.runfiles.as_ref()
    }

    pub(crate) fn same(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl fmt::Display for FilesToRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // StarlarkValue.repr default with the runtime class name.
        match self.kind {
            FilesToRunKind::Base => write!(f, "<unknown object {FILES_TO_RUN_CLASS}>"),
            FilesToRunKind::Single => write!(
                f,
                "<unknown object {FILES_TO_RUN_CLASS}$SingleExecutableFilesToRunProvider>"
            ),
            FilesToRunKind::Full => {
                write!(
                    f,
                    "<unknown object {FILES_TO_RUN_CLASS}$FullFilesToRunProvider>"
                )
            }
        }
    }
}

#[starlark_value(type = "FilesToRunProvider")]
impl<'v> StarlarkValue<'v> for FilesToRun {
    /// FilesToRunProvider.debugPrint (analysis/FilesToRunProvider.java:112-120):
    /// the three Files (repr) or None.
    fn collect_debug_print(&self, collector: &mut String, _eval: &Evaluator<'v, '_, '_>) {
        use std::fmt::Write;
        let shown = |file: &Option<File>| {
            file.as_ref()
                .map_or_else(|| "None".to_owned(), ToString::to_string)
        };
        write!(
            collector,
            "FilesToRunProvider(executable = {}, repo_mapping_manifest = {}, runfiles_manifest = {})",
            shown(&self.executable),
            shown(&self.repo_mapping_manifest),
            shown(&self.runfiles_manifest),
        )
        .unwrap();
    }

    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        // FilesToRunProviderApi (starlarkbuildapi/FilesToRunProviderApi.java:24-58).
        let file = match name {
            "executable" => &self.executable,
            "runfiles_manifest" => &self.runfiles_manifest,
            "repo_mapping_manifest" => &self.repo_mapping_manifest,
            _ => return None,
        };
        Some(match file {
            Some(file) => crate::starlark_types::alloc_file(heap, file.clone()),
            None => Value::new_none(),
        })
    }

    fn dir_attr(&self) -> Vec<String> {
        ["executable", "repo_mapping_manifest", "runfiles_manifest"]
            .map(String::from)
            .to_vec()
    }

    fn set_attr(&self, attribute: &str, _new_value: Value<'v>) -> starlark::Result<()> {
        cannot_set(attribute, "FilesToRunProvider")
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(FilesToRun::from_value(other).is_some_and(|other| self.same(other)))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.id.hash(hasher);
        Ok(())
    }
}

/// The RunfilesProvider of a configured target (analysis/RunfilesProvider.java:27-91).
#[derive(Clone, Debug, Default)]
pub struct RunfilesProvider {
    pub default: Runfiles,
    pub data: Runfiles,
}

/// Runfiles-related providers of a configured target, owned across targets.
#[derive(Clone, Debug, Default)]
pub struct TargetRunfiles {
    /// `None` for file targets and native rules without a RunfilesProvider.
    pub provider: Option<RunfilesProvider>,
    /// `None`: FilesToRunProvider.create(filesToBuild, null, null).
    pub files_to_run: Option<FilesToRun>,
    /// What `bazel run` needs of a rule target (RunfilesSupport.getArgs and
    /// getActionEnvironment, analysis/RunfilesSupport.java:546-652).
    pub run: RunSettings,
}

/// The `run` command settings of a Starlark rule target.
#[derive(Clone, Debug, Default)]
pub struct RunSettings {
    /// The tokenized `args` attribute (RunfilesSupport.computeArgs).
    pub args: Vec<String>,
    /// RunEnvironmentInfo.environment (computeActionEnvironment, :612-619).
    pub environment: Vec<(String, String)>,
    /// RunEnvironmentInfo.inherited_environment.
    pub inherited: Vec<String>,
    /// A test rule (TargetUtils.isTestRule).
    pub test: bool,
}

impl TargetRunfiles {
    /// Input/output file targets: no RunfilesProvider, a single executable.
    pub fn file(file: File) -> Self {
        Self {
            provider: None,
            run: RunSettings::default(),
            files_to_run: Some(FilesToRun {
                fresh: true,
                ..FilesToRun::single(file)
            }),
        }
    }

    pub fn files_to_run(&self, files: &[File]) -> FilesToRun {
        self.files_to_run
            .clone()
            .unwrap_or_else(|| FilesToRun::create(files, false, None, None))
    }

    /// A rule target's FilesToRunProvider is one stored object
    /// (RuleConfiguredTargetBuilder.java:150-154).
    pub fn with_files_to_run(mut self, files: &[File]) -> Self {
        if self.files_to_run.is_none() {
            self.files_to_run = Some(FilesToRun::create(files, false, None, None));
        }
        self
    }

    /// RunfilesProvider.DEFAULT_RUNFILES (RunfilesProvider.java:48-60).
    pub fn default_runfiles(&self) -> Runfiles {
        self.provider
            .as_ref()
            .map(|provider| provider.default.clone())
            .unwrap_or_else(Runfiles::empty)
    }

    /// RunfilesProvider.DATA_RUNFILES (RunfilesProvider.java:67-79).
    pub fn data_runfiles(&self) -> Runfiles {
        self.provider
            .as_ref()
            .map(|provider| provider.data.clone())
            .unwrap_or_else(Runfiles::empty)
    }

    /// Every artifact a dependent can observe through these providers.
    pub fn files(&self) -> Vec<File> {
        let mut files = Vec::new();
        if let Some(provider) = &self.provider {
            files.extend(provider.default.all_artifacts());
            files.extend(provider.data.all_artifacts());
        }
        if let Some(files_to_run) = &self.files_to_run {
            files.extend(files_to_run.executable.iter().cloned());
            if let Some(runfiles) = &files_to_run.runfiles {
                files.extend(runfiles.all_artifacts());
            }
        }
        let mut seen = BTreeSet::new();
        files.retain(|file| seen.insert(file.clone()));
        files
    }

    /// genrule (rules/genrule/GenRuleBase.java:215-235): no default
    /// runfiles, the outputs as data runfiles; `executable` stays gated.
    pub fn genrule(outputs: &[File]) -> anyhow::Result<Self> {
        let mut data = RunfilesBuilder::default();
        data.add_files_to_build(outputs);
        Ok(Self {
            provider: Some(RunfilesProvider {
                default: Runfiles::empty(),
                data: data.build()?,
            }),
            files_to_run: Some(FilesToRun::create(outputs, false, None, None)),
            run: RunSettings::default(),
        })
    }

    /// filegroup (rules/filegroup/Filegroup.java:80-119) with
    /// --incompatible_filegroup_runfiles_for_data (CoreOptions.java:66-75)
    /// and no `data` (gated). `srcs` are (files to build, runfiles) pairs.
    pub fn filegroup(files: &[File], srcs: &[(&[File], &TargetRunfiles)]) -> anyhow::Result<Self> {
        let prerequisites = srcs
            .iter()
            .map(|(files, runfiles)| Prerequisite { files, runfiles })
            .collect::<Vec<_>>();
        let default = collect(&[], &prerequisites, Mapping::Default)?;
        let mut data = RunfilesBuilder::default();
        data.add_files_to_build(files);
        data.merge(&collect(&[], &prerequisites, Mapping::Data)?);
        let executable = (files.len() == 1).then(|| files[0].clone());
        Ok(Self {
            provider: Some(RunfilesProvider {
                default,
                data: data.build()?,
            }),
            files_to_run: Some(FilesToRun::create(files, false, None, executable)),
            run: RunSettings::default(),
        })
    }
}

/// A dependency of a runfiles collection: its files to build and providers.
pub(crate) struct Prerequisite<'a> {
    pub files: &'a [File],
    pub runfiles: &'a TargetRunfiles,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mapping {
    Data,
    Default,
}

/// Builder.addRunfiles (Runfiles.java:669-781): `data` prerequisites
/// contribute their files to build (--incompatible_always_include_files_in_data,
/// CoreOptions.java:612-624) and data runfiles; `srcs`/`deps` their mapped
/// runfiles (file targets contribute nothing there).
pub(crate) fn collect(
    data: &[Prerequisite<'_>],
    non_data: &[Prerequisite<'_>],
    mapping: Mapping,
) -> anyhow::Result<Runfiles> {
    let mut builder = RunfilesBuilder::default();
    for dependency in data {
        builder.add_files_to_build(dependency.files);
        if dependency.runfiles.provider.is_some() {
            builder.merge(&dependency.runfiles.data_runfiles());
        }
    }
    for dependency in non_data {
        builder.merge(&match mapping {
            Mapping::Data => dependency.runfiles.data_runfiles(),
            Mapping::Default => dependency.runfiles.default_runfiles(),
        });
    }
    builder.build()
}

/// The lazily created `ctx.outputs.executable` (RuleContext.createOutputArtifact).
#[derive(Clone, Debug)]
pub(crate) struct DefaultExecutable {
    pub file: File,
    created: Arc<AtomicBool>,
}

impl DefaultExecutable {
    pub(crate) fn new(file: File) -> Self {
        Self {
            file,
            created: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn created(&self) -> bool {
        self.created.load(Ordering::Acquire)
    }
}

/// Runfiles state of an analysis context.
#[derive(Debug, Default)]
pub(crate) struct ContextRunfiles {
    /// Executable and test rules only.
    pub default_executable: Option<DefaultExecutable>,
    /// StarlarkAttributesCollection.getExecutableRunfilesMap
    /// (analysis/starlark/StarlarkAttributesCollection.java:410-430).
    pub executables: RefCell<BTreeMap<File, FilesToRun>>,
    /// The dependencies' runfiles and repository-mapping manifests, which
    /// their runfiles support generates (N23f).
    pub manifests: BTreeSet<File>,
}

impl ContextRunfiles {
    /// An action executable/tool File: its FilesToRunProvider runfiles are
    /// attached to the spawn (StarlarkActionFactory.java:433-441, :771-779;
    /// crate::analysis_runtime::tool_runfiles_tree).
    /// StarlarkRuleContext.getExecutableRunfiles != null
    /// (analysis/starlark/StarlarkRuleContext.java:981-983): `file` is an
    /// executable attribute's executable.
    pub(crate) fn is_executable(&self, file: &File) -> bool {
        self.executables.borrow().contains_key(file)
    }

    /// StarlarkRuleContext.areRunfilesFromDeps (:985-992): `files_to_run` is
    /// an executable attribute's FilesToRunProvider (identity).
    pub(crate) fn is_from_dependencies(&self, files_to_run: &FilesToRun) -> bool {
        self.executables
            .borrow()
            .values()
            .any(|known| known.id == files_to_run.id)
    }
}

impl TargetRunfiles {
    /// The FilesToRunProvider manifests of this target.
    pub(crate) fn manifests(&self) -> Vec<File> {
        self.files_to_run
            .iter()
            .flat_map(|files| {
                files
                    .runfiles_manifest
                    .iter()
                    .chain(&files.repo_mapping_manifest)
                    .cloned()
            })
            .collect()
    }
}

impl Outputs {
    fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn created(&self) -> Option<&DefaultExecutable> {
        self.executable.as_ref().filter(|default| default.created())
    }
}

fn struct_names(fields: &OwnedFrozenValue) -> Vec<String> {
    StructRef::from_value(fields.value())
        .map(|structure| {
            structure
                .iter()
                .map(|(key, _)| key.as_str().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn struct_field<'v>(fields: &OwnedFrozenValue, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
    let fields = heap.access_owned_frozen_value(fields);
    StructRef::from_value(fields)?
        .iter()
        .find(|(key, _)| key.as_str() == name)
        .map(|(_, value)| value)
}

impl fmt::Display for Outputs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Outputs.repr (StarlarkRuleContext.java:410-436).
        if !self.active() {
            return write!(f, "ctx.outputs(for {})", self.label);
        }
        let mut fields = BTreeMap::new();
        if let Some(default) = self.created() {
            fields.insert("executable".to_owned(), default.file.to_string());
        }
        if let Some(structure) = StructRef::from_value(self.fields.value()) {
            for (name, value) in structure.iter() {
                fields.insert(name.as_str().to_owned(), value.to_repr());
            }
        }
        f.write_str("ctx.outputs(")?;
        for (index, (name, value)) in fields.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{name} = {value}")?;
        }
        f.write_str(")")
    }
}

#[starlark_value(type = "Outputs")]
impl<'v> StarlarkValue<'v> for Outputs {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        // Outputs.checkMutable (StarlarkRuleContext.java:440-447): no field is
        // readable after the implementation returned (unknown_attribute_message).
        if !self.active() {
            return None;
        }
        if name == "executable"
            && let Some(default) = &self.executable
        {
            if self.active() {
                default.created.store(true, Ordering::Release);
            }
            return Some(crate::starlark_types::alloc_file(
                heap,
                default.file.clone(),
            ));
        }
        struct_field(&self.fields, name, heap)
    }

    fn has_attr(&self, name: &str, heap: Heap<'v>) -> bool {
        if !self.active() {
            return false;
        }
        (name == "executable" && self.executable.is_some())
            || struct_field(&self.fields, name, heap).is_some()
    }

    fn dir_attr(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.created().is_some() {
            names.push("executable".to_owned());
        }
        names.extend(struct_names(&self.fields));
        names
    }

    fn set_attr(&self, _attribute: &str, _new_value: Value<'v>) -> starlark::Result<()> {
        no_field_assignment("Outputs")
    }

    fn unknown_attribute_message(&self, attribute: &str) -> Option<String> {
        // Outputs.checkMutable (StarlarkRuleContext.java:440-447) once the
        // implementation returned, else getErrorMessageForUnknownField (:402-408).
        if !self.active() {
            return Some(format!(
                "cannot access outputs of rule '{}' outside of its own rule implementation function",
                self.label
            ));
        }
        Some(format!(
            "No attribute '{attribute}' in outputs. Make sure you declared a rule output with this name."
        ))
    }
}

impl fmt::Display for ExecutableStruct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.fields.value())
    }
}

#[starlark_value(type = "struct")]
impl<'v> StarlarkValue<'v> for ExecutableStruct {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        struct_field(&self.fields, name, heap)
    }

    fn dir_attr(&self) -> Vec<String> {
        struct_names(&self.fields)
    }

    fn set_attr(&self, _attribute: &str, _new_value: Value<'v>) -> starlark::Result<()> {
        no_field_assignment("struct")
    }

    fn unknown_attribute_message(&self, attribute: &str) -> Option<String> {
        // StructImpl.getErrorMessageForUnknownField with allAttributesSuffix
        // (packages/StructImpl.java:86-95).
        let mut names = struct_names(&self.fields);
        names.sort();
        Some(format!(
            "No attribute '{attribute}' in executable. Make sure there is a label type attribute marked as 'executable' with this name\nAvailable attributes: {}",
            names.join(", ")
        ))
    }
}

/// `ctx.outputs` (executable/test rules carry the lazy executable).
pub(crate) fn alloc_outputs<'v>(
    heap: Heap<'v>,
    fields: OwnedFrozenValue,
    executable: Option<DefaultExecutable>,
    active: Arc<AtomicBool>,
    label: String,
) -> Value<'v> {
    heap.alloc(Outputs {
        fields,
        executable,
        active,
        label,
    })
}

/// The field struct of a `ctx.outputs` value (a provider payload walks its
/// predeclared output Files, crate::providers).
pub(crate) fn outputs_fields(value: Value<'_>) -> Option<Value<'_>> {
    Outputs::from_value(value).map(|outputs| outputs.fields.value())
}

/// `ctx.executable`.
pub(crate) fn alloc_executable_struct<'v>(heap: Heap<'v>, fields: OwnedFrozenValue) -> Value<'v> {
    heap.alloc(ExecutableStruct { fields })
}

/// BuiltinRestriction.INTERNAL_STARLARK_API_ALLOWLIST main-repository
/// entries (packages/BuiltinRestriction.java:36-92).
const PRIVATE_API_MAIN_PACKAGES: &[&str] = &[
    "test",
    "bazel_internal/test_rules",
    "tools/build_defs/build_info",
    "bazel_internal/test_rules/cc",
    "tools/build_defs/android",
    "third_party/bazel_rules/rules_android",
    "third_party/apple_crosstool",
    "third_party/cpptoolchains/portable_llvm/build_defs",
    "third_party/bazel_rules/rules_apple",
    "third_party/bazel_rules/rules_cc",
    "tools/build_defs/cc",
    "third_party/bazel_rules/rules_java/java",
    "third_party/bazel_rules/rules_rust/rust/private",
    "third_party/crubit",
    "third_party/gpus/cuda",
    "tools/build_defs/packaging",
    "tools/build_defs/go",
    "third_party/protobuf",
];

/// BuiltinRestriction.failIfCalledOutsideDefaultAllowlist for the innermost
/// `.bzl` (packages/BuiltinRestriction.java:178-205).
fn check_private_api(eval: &Evaluator<'_, '_, '_>) -> anyhow::Result<()> {
    let label = crate::loading::definition_label(eval)
        .ok_or_else(|| anyhow!("skip_conflict_checking: no calling .bzl file"))?;
    if label.repository == "_builtins" {
        return Ok(());
    }
    ensure!(
        label.repository.is_empty(),
        "skip_conflict_checking=True from {}: the private-API allowlist of external repositories (apparent-name mapping) is not supported by the native profile",
        crate::rule_declarations::canonical_form(&label)
    );
    let allowed = PRIVATE_API_MAIN_PACKAGES
        .iter()
        .any(|prefix| label.package == *prefix || label.package.starts_with(&format!("{prefix}/")));
    ensure!(
        allowed,
        "file '{}' cannot use private API",
        crate::rule_declarations::canonical_form(&label)
    );
    Ok(())
}

fn file_items(value: Value<'_>, what: &str) -> anyhow::Result<Vec<File>> {
    let items = sequence_items(value).ok_or_else(|| anyhow!("internal error: {what}"))?;
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            File::from_value(*item).cloned().ok_or_else(|| {
                anyhow!(
                    "at index {index} of {what}, got element of type {}, want File",
                    item.get_type()
                )
            })
        })
        .collect()
}

/// Depset.getSet type check (collect/nestedset/Depset.java:207-218).
fn depset_of(value: Value<'_>, kind: ItemType) -> Result<&Depset, String> {
    let depset = Depset::from_value(value).ok_or_else(|| "not a depset".to_owned())?;
    match depset.node.element_type {
        Some(found) if !depset.node.empty && found != kind => Err(format!(
            "got a depset of '{}', expected a depset of '{}'",
            found.name(),
            kind.name()
        )),
        _ => Ok(depset),
    }
}

/// Arguments of `ctx.runfiles` (StarlarkRuleContextApi.java:469-556).
pub(crate) struct RunfilesArguments<'v> {
    pub files: Option<Value<'v>>,
    pub transitive_files: Option<Value<'v>>,
    pub collect_data: Option<Value<'v>>,
    pub collect_default: Option<Value<'v>>,
    pub symlinks: Option<Value<'v>>,
    pub root_symlinks: Option<Value<'v>>,
    pub skip_conflict_checking: bool,
}

/// StarlarkRuleContext.runfiles (analysis/starlark/StarlarkRuleContext.java:1059-1125).
/// `collected` holds the precomputed collect_data/collect_default runfiles.
pub(crate) fn ctx_runfiles(
    arguments: RunfilesArguments<'_>,
    collected: (&Runfiles, &Runfiles),
    eval: &Evaluator<'_, '_, '_>,
) -> anyhow::Result<Runfiles> {
    // Parameter binding type checks, in declaration order.
    let type_error = |parameter: &str, value: Value<'_>, want: &str| {
        runfiles_type_error("runfiles", parameter, value, want)
    };
    if let Some(files) = arguments.files
        && sequence_items(files).is_none()
    {
        return Err(type_error("files", files, "sequence"));
    }
    let transitive_files = arguments.transitive_files.filter(|value| !value.is_none());
    // The Java parameter type (Boolean) is the allowed type.
    for (name, value) in [
        ("collect_data", arguments.collect_data),
        ("collect_default", arguments.collect_default),
    ] {
        if let Some(value) = value
            && value.unpack_bool().is_none()
        {
            return Err(type_error(name, value, "bool"));
        }
    }
    if let Some(value) = transitive_files
        && Depset::from_value(value).is_none()
    {
        return Err(type_error("transitive_files", value, "depset or NoneType"));
    }
    for (name, value) in [
        ("symlinks", arguments.symlinks),
        ("root_symlinks", arguments.root_symlinks),
    ] {
        if let Some(value) = value
            && DictRef::from_value(value).is_none()
            && Depset::from_value(value).is_none()
        {
            return Err(type_error(name, value, "dict or depset"));
        }
    }
    if arguments.skip_conflict_checking {
        check_private_api(eval)?;
    }
    let mut builder = RunfilesBuilder::default();
    if arguments.collect_data.and_then(Value::unpack_bool) == Some(true) {
        builder.merge(collected.0);
    }
    if arguments.collect_default.and_then(Value::unpack_bool) == Some(true) {
        builder.merge(collected.1);
    }
    if let Some(files) = arguments.files {
        for file in file_items(files, "files")? {
            builder.add_artifact(file);
        }
    }
    if let Some(value) = transitive_files {
        let depset = depset_of(value, ItemType::File)
            .map_err(|error| anyhow!("for 'transitive_files', {error}"))?;
        // Runfiles uses compile order (Order.COMPILE_ORDER.isCompatible).
        ensure!(
            Order::Postorder.compatible(depset.node.order),
            "order '{}' is invalid for transitive_files",
            depset.node.order.name()
        );
        builder.add_transitive(&depset.node);
    }
    let mut check_conflicts = false;
    for (root, value) in [(false, arguments.symlinks), (true, arguments.root_symlinks)] {
        let Some(value) = value else {
            continue;
        };
        let what = if root { "root_symlinks" } else { "symlinks" };
        if let Some(depset) = Depset::from_value(value) {
            if depset.node.empty {
                continue;
            }
            check_conflicts = true;
            let depset = depset_of(value, ItemType::Symlink).map_err(anyhow::Error::msg)?;
            let node = Arc::clone(&depset.node);
            if root {
                builder.transitive_root_symlinks.push(node);
            } else {
                builder.transitive_symlinks.push(node);
            }
        } else if let Some(dict) = DictRef::from_value(value) {
            if dict.is_empty() {
                continue;
            }
            check_conflicts = true;
            let mut entries = Vec::new();
            for (key, target) in dict.iter() {
                match (key.unpack_str(), File::from_value(target)) {
                    (Some(path), Some(file)) => entries.push((path.to_owned(), file.clone())),
                    _ => bail!(
                        "got dict<{}, {}> for '{what}', want dict<string, File>",
                        key.get_type(),
                        target.get_type()
                    ),
                }
            }
            for (path, file) in entries {
                let item = Item::Symlink(symlink_entry(normalize_path(&path), file));
                if root {
                    builder.root_symlinks.push(item);
                } else {
                    builder.symlinks.push(item);
                }
            }
        }
    }
    let mut runfiles = builder.build()?;
    if check_conflicts && !arguments.skip_conflict_checking {
        runfiles.conflict_error = true;
    }
    Ok(runfiles)
}

/// The fields of a returned DefaultInfo (or of the empty implicit one).
#[derive(Debug, Default)]
pub(crate) struct ReturnedDefaultInfo {
    pub files: Option<Vec<File>>,
    pub runfiles: Option<Runfiles>,
    pub data_runfiles: Option<Runfiles>,
    pub default_runfiles: Option<Runfiles>,
    pub executable: Option<File>,
}

/// The rule being configured.
pub(crate) struct RuleClassification<'a> {
    /// Canonical label string (an artifact owner).
    pub label: &'a str,
    pub rule_class: &'a str,
    /// RuleClass.isExecutableStarlark: executable or test (StarlarkRuleClassFunctions.java:911-913).
    pub executable: bool,
    /// TargetUtils.isTestRuleName of the rule class.
    pub test: bool,
    /// The built-in `args` attribute (binaryBaseRule/getTestBaseRule).
    pub args: Vec<String>,
}

/// Root-relative path of a generated artifact (Artifact.getRootRelativePathString):
/// the path below its own configuration's output root (`bazel-out/<mnemonic>/bin`).
pub(crate) fn root_relative(file: &File) -> &str {
    crate::label::split_generated_root(&file.path)
        .map_or(file.path.as_str(), |(_, relative)| relative)
}

fn merge_executable(runfiles: Runfiles, executable: Option<&File>) -> anyhow::Result<Runfiles> {
    // StarlarkRuleConfiguredTargetUtil.mergeFiles (:493-502).
    let Some(executable) = executable else {
        return Ok(runfiles);
    };
    let mut builder = RunfilesBuilder::default();
    builder.add_artifact(executable.clone()).merge(&runfiles);
    builder.build()
}

fn suffixed(file: &File, suffix: &str, owner: &str) -> File {
    File {
        is_directory: false,
        path: format!("{}{suffix}", file.path),
        short_path: format!("{}{suffix}", file.short_path),
        owner: owner.to_owned(),
        is_source: false,
    }
}

/// StarlarkRuleConfiguredTargetUtil.parseDefaultProviderFields and
/// addSimpleProviders (analysis/starlark/StarlarkRuleConfiguredTargetUtil.java:359-490):
/// the files to build and runfiles providers of a Starlark rule target.
pub(crate) fn configure_target(
    info: ReturnedDefaultInfo,
    rule: &RuleClassification<'_>,
    predeclared: Vec<File>,
    default_executable: Option<&DefaultExecutable>,
    options: crate::runfiles_tree::RunfilesOptions,
) -> anyhow::Result<(Vec<File>, TargetRunfiles)> {
    let rule_class = rule.rule_class;
    let mut executable = info.executable;
    if let Some(file) = &executable {
        ensure!(
            file.owner == rule.label,
            "'executable' provided by an executable rule '{rule_class}' should be created by the same rule."
        );
    }
    let created = default_executable.filter(|default| default.created());
    if let (Some(file), true, Some(default)) = (&executable, rule.executable, created) {
        ensure!(
            *file == default.file,
            "The rule '{rule_class}' both accesses 'ctx.outputs.executable' and provides a different executable '{}'. Do not use 'ctx.output.executable'.",
            root_relative(file)
        );
    }
    if executable.is_none() && rule.executable {
        match created {
            Some(default) => executable = Some(default.file.clone()),
            None => bail!(
                "The rule '{rule_class}' is executable. It needs to create an executable File and pass it as the 'executable' parameter to the DefaultInfo it returns."
            ),
        }
    }
    let files = match info.files {
        Some(files) => files,
        None => {
            let mut files = predeclared;
            if let Some(file) = &executable
                && !files.contains(file)
            {
                files.push(file.clone());
            }
            files
        }
    };
    let mut stateless = info.runfiles;
    if stateless.is_none() && info.data_runfiles.is_none() && info.default_runfiles.is_none() {
        stateless = Some(Runfiles::empty());
    }
    let executable_or_test = executable.is_some() || rule.test;
    let provider = match stateless {
        Some(runfiles) => {
            let merged = merge_executable(runfiles, executable.as_ref())?;
            RunfilesProvider {
                default: merged.clone(),
                data: merged,
            }
        }
        None => {
            let mut default = info.default_runfiles.unwrap_or_else(Runfiles::empty);
            if executable_or_test {
                default = merge_executable(default, executable.as_ref())?;
            }
            RunfilesProvider {
                default,
                data: info.data_runfiles.unwrap_or_else(Runfiles::empty),
            }
        }
    };
    if rule.test && provider.default.is_empty() {
        bail!("Test rules have to define runfiles");
    }
    let files_to_run = if executable_or_test {
        let support = if provider.default.is_empty() {
            None
        } else {
            let file = executable
                .as_ref()
                .ok_or_else(|| anyhow!("executable must not be null"))?;
            // RunfilesSupport.computeArgs (analysis/RunfilesSupport.java:602-610):
            // location/Make expansion and shell tokenization are not modelled.
            if let Some(arg) = rule
                .args
                .iter()
                .find(|arg| arg.contains(['$', '\'', '"', '\\']))
            {
                bail!(
                    "in args attribute of {rule_class} rule {}: {arg:?} needs Make-variable/location expansion or shell tokenization, which is not supported by the native profile (docs/RUNFILES.md)",
                    rule.label
                );
            }
            // RunfilesSupport.create (analysis/RunfilesSupport.java:305-325):
            // the MANIFEST in the tree with --build_runfile_links, the input
            // manifest without, none with --nobuild_runfile_manifests.
            let manifest = if options.links() {
                Some(suffixed(file, ".runfiles/MANIFEST", rule.label))
            } else if options.build_runfile_manifests {
                Some(suffixed(file, ".runfiles_manifest", rule.label))
            } else {
                None
            };
            Some((
                provider.default.clone(),
                manifest,
                suffixed(file, ".repo_mapping", rule.label),
            ))
        };
        let mut to_run = files.clone();
        if let Some(file) = &executable
            && !to_run.contains(file)
        {
            to_run.push(file.clone());
        }
        let tree = support.is_some();
        FilesToRun::create(&to_run, tree, support, executable)
    } else {
        FilesToRun::create(&files, false, None, None)
    };
    // ShellUtils.tokenize of each `args` value: the analysis gate above
    // leaves only whitespace separation (no quotes, escapes or `$`).
    let args = rule
        .args
        .iter()
        .flat_map(|arg| {
            arg.split([' ', '\t', '\n', '\r'])
                .filter(|token| !token.is_empty())
        })
        .map(str::to_owned)
        .collect();
    Ok((
        files,
        TargetRunfiles {
            provider: Some(provider),
            files_to_run: Some(files_to_run),
            run: RunSettings {
                args,
                test: rule.test,
                ..RunSettings::default()
            },
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, owner: &str) -> File {
        File {
            is_directory: false,
            path: format!("bazel-out/x/bin/{path}"),
            short_path: path.to_owned(),
            owner: owner.to_owned(),
            is_source: false,
        }
    }

    fn rule<'a>(test: bool) -> RuleClassification<'a> {
        RuleClassification {
            label: "//pkg:r",
            rule_class: "r",
            executable: true,
            test,
            args: Vec::new(),
        }
    }

    fn source(path: &str, owner: &str) -> File {
        File {
            is_directory: false,
            path: path.to_owned(),
            short_path: path
                .strip_prefix("external/")
                .map_or_else(|| path.to_owned(), |rest| format!("../{rest}")),
            owner: owner.to_owned(),
            is_source: true,
        }
    }

    #[test]
    fn runfiles_inputs_prefix_external_and_drop_obscured_entries() {
        let main = source("pkg/d.txt", "//pkg:d.txt");
        let external = source("external/ext+/e.txt", "@@ext+//:e.txt");
        let mut builder = RunfilesBuilder::default();
        builder
            .add_artifact(main.clone())
            .add_artifact(external.clone());
        builder
            .symlinks
            .push(Item::Symlink(symlink_entry("dir".into(), main.clone())));
        builder.symlinks.push(Item::Symlink(symlink_entry(
            "dir/x".into(),
            external.clone(),
        )));
        builder
            .root_symlinks
            .push(Item::Symlink(symlink_entry("ext+/r".into(), main.clone())));
        let runfiles = builder.build().unwrap();
        let mapping_file = file("pkg/b.repo_mapping", "//pkg:b");
        let mut conflicts = Vec::new();
        let mapping = runfiles.runfiles_inputs(Some(&mapping_file), Some(&mut conflicts));
        assert_eq!(
            mapping.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "_main/dir",
                "_main/pkg/d.txt",
                "_repo_mapping",
                "ext+/e.txt",
                "ext+/r"
            ]
        );
        assert_eq!(
            conflicts,
            ["runfiles symlink dir/x -> external/ext+/e.txt obscured by dir -> pkg/d.txt"]
        );
        // Only external entries: `_main/.runfile` keeps the directory.
        let mut builder = RunfilesBuilder::default();
        builder.add_artifact(external);
        let mapping = builder.build().unwrap().runfiles_inputs(None, None);
        assert_eq!(
            mapping.keys().collect::<Vec<_>>(),
            ["_main/.runfile", "ext+/e.txt"]
        );
        assert_eq!(mapping["_main/.runfile"], None);
    }

    #[test]
    fn python_init_files_follow_get_init_py_files_and_hash_set_order() {
        let manifest = [
            "py/a/b/m.py",
            "py/a/x.so",
            "py/top.py",
            "py/__pycache__/c.pyc",
            "../ext+/q.py",
        ]
        .map(str::to_owned);
        assert_eq!(
            EmptyFiles::PythonInit.extra_paths(&manifest),
            // HashMap buckets of String.hashCode (an independent computation
            // gives the same order); the first three match genuine Bazel's
            // empty_filenames for py/a/b/m.py, py/a/x.so, py/top.py.
            [
                "../ext+/__init__.py",
                "../__init__.py",
                "py/a/b/__init__.py",
                "py/a/__init__.py",
                "py/__init__.py"
            ]
        );
        assert!(EmptyFiles::None.extra_paths(&manifest).is_empty());
        // An existing __init__.py(c) suppresses the generated one.
        let manifest = ["a/m.py", "a/__init__.pyc"].map(str::to_owned);
        assert!(EmptyFiles::PythonInit.extra_paths(&manifest).is_empty());
        assert_eq!(
            java_string_hash("py/__init__.py"),
            "py/__init__.py"
                .chars()
                .fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(c as i32))
        );
        // Larger sets resize the table (capacity 32 above 12 entries).
        let many: Vec<String> = (0..13).map(|i| format!("f{i}")).collect();
        let ordered = java_hash_set_order(many.clone());
        assert_eq!(ordered.len(), 13);
        assert_ne!(ordered, many);
    }

    #[test]
    fn manifest_lines_escape_like_source_manifest_action() {
        let mut out = Vec::new();
        manifest_line("_main/a", Some("/x/a b"), &mut out);
        manifest_line("_main/e", None, &mut out);
        manifest_line("_main/s p\\q", Some("/t\\u"), &mut out);
        manifest_line("_main/n", Some("/l\nm"), &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "_main/a /x/a b\n_main/e \n _main/s\\sp\\bq /t\\bu\n _main/n /l\\nm\n"
        );
    }

    #[test]
    fn path_fragments_normalize_like_bazel() {
        for (input, expected) in [
            ("a/./b//c/", "a/b/c"),
            ("a/../b", "b"),
            ("../a", "../a"),
            ("a/../../b", "../b"),
            ("/a/../..", "/"),
            ("", ""),
        ] {
            assert_eq!(normalize_path(input), expected, "{input}");
        }
    }

    #[test]
    fn strictest_conflict_policy_propagates_even_from_empty_runfiles() {
        let strict = Runfiles {
            conflict_error: true,
            ..RunfilesBuilder::default().build().unwrap()
        };
        assert!(strict.is_empty());
        let mut builder = RunfilesBuilder::default();
        builder.merge(&Runfiles::empty()).merge(&strict);
        let merged = builder.build().unwrap();
        assert!(merged.conflict_error() && merged.is_empty());
        let mut builder = RunfilesBuilder::default();
        builder.add_artifact(file("pkg/a", "//pkg:a"));
        assert!(!builder.build().unwrap().conflict_error());
        // Runfiles.EMPTY is one identity; built empty runfiles are new ones.
        assert!(Runfiles::empty().same(&Runfiles::empty()));
        assert!(!Runfiles::empty().same(&RunfilesBuilder::default().build().unwrap()));
    }

    #[test]
    fn compile_order_puts_transitive_runfiles_before_direct_files() {
        let mut inner = RunfilesBuilder::default();
        inner
            .add_artifact(file("pkg/b", "//pkg:b"))
            .add_artifact(file("pkg/a", "//pkg:a"));
        let inner = inner.build().unwrap();
        let merged = merge_executable(inner.clone(), Some(&file("pkg/exe", "//pkg:r"))).unwrap();
        assert_eq!(
            merged
                .artifacts()
                .iter()
                .map(|file| file.short_path.as_str())
                .collect::<Vec<_>>(),
            ["pkg/b", "pkg/a", "pkg/exe"]
        );
        assert!(merge_executable(inner.clone(), None).unwrap().same(&inner));
    }

    #[test]
    fn files_to_run_provider_kinds_follow_create() {
        let a = file("pkg/a", "//pkg:a");
        let b = file("pkg/b", "//pkg:b");
        assert_eq!(
            FilesToRun::create(&[], false, None, None).kind,
            FilesToRunKind::Base
        );
        assert_eq!(
            FilesToRun::create(&[a.clone(), b.clone()], false, None, None).kind,
            FilesToRunKind::Base
        );
        let single = FilesToRun::create(std::slice::from_ref(&a), false, None, Some(a.clone()));
        assert_eq!(single.kind, FilesToRunKind::Single);
        assert_eq!(single.executable.as_ref(), Some(&a));
        let multiple = FilesToRun::create(&[a.clone(), b.clone()], false, None, Some(a.clone()));
        assert_eq!(multiple.kind, FilesToRunKind::Full);
        assert!(multiple.runfiles_manifest.is_none());
        assert_eq!(
            multiple.to_string(),
            "<unknown object com.google.devtools.build.lib.analysis.FilesToRunProvider$FullFilesToRunProvider>"
        );
    }

    #[test]
    fn executable_targets_get_runfiles_support_and_manifests() {
        let exe = file("pkg/r", "//pkg:r");
        let (files, runfiles) = configure_target(
            ReturnedDefaultInfo {
                executable: Some(exe.clone()),
                ..Default::default()
            },
            &rule(false),
            vec![file("pkg/out", "//pkg:r")],
            None,
            Default::default(),
        )
        .unwrap();
        assert_eq!(files, [file("pkg/out", "//pkg:r"), exe.clone()]);
        let files_to_run = runfiles.files_to_run.unwrap();
        assert_eq!(files_to_run.kind, FilesToRunKind::Full);
        assert_eq!(
            files_to_run.runfiles_manifest.unwrap().path,
            "bazel-out/x/bin/pkg/r.runfiles/MANIFEST"
        );
        assert_eq!(
            files_to_run.repo_mapping_manifest.unwrap().short_path,
            "pkg/r.repo_mapping"
        );
        let provider = runfiles.provider.unwrap();
        assert_eq!(provider.default.artifacts(), std::slice::from_ref(&exe));
        assert!(provider.default.same(&provider.data));
        // Test rules always have runfiles here (the executable is merged in);
        // the check (StarlarkRuleConfiguredTargetUtil.java:470-472) still holds.
        let error = configure_target(
            ReturnedDefaultInfo::default(),
            &RuleClassification {
                executable: false,
                ..rule(true)
            },
            Vec::new(),
            None,
            Default::default(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "Test rules have to define runfiles");
        // A non-executable rule may still return an executable it owns.
        let (_, runfiles) = configure_target(
            ReturnedDefaultInfo {
                executable: Some(exe.clone()),
                files: Some(Vec::new()),
                ..Default::default()
            },
            &RuleClassification {
                executable: false,
                ..rule(false)
            },
            Vec::new(),
            None,
            Default::default(),
        )
        .unwrap();
        assert_eq!(
            runfiles.files_to_run.unwrap().executable.as_ref(),
            Some(&exe)
        );
    }
}
