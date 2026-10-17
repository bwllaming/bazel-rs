//! Owned, immutable analysis values. Depsets retain a shared DAG, homogeneous
//! element types, and Bazel's default/postorder/preorder/topological traversal.

use allocative::Allocative;
use anyhow::{anyhow, bail};
use starlark::collections::StarlarkHasher;
use starlark::environment::{GlobalsBuilder, Methods, MethodsBuilder};
use starlark::eval::{Arguments, Evaluator};
use starlark::values::list::ListRef;
use starlark::values::starlark_value;
use starlark::values::tuple::TupleRef;
use starlark::values::type_repr::StarlarkTypeRepr;
use starlark::values::{
    Heap, NoSerialize, OwnedFrozenValue, ProvidesStaticType, StarlarkPagable, StarlarkPagablePanic,
    StarlarkValue, UnpackValue, Value, ValueTyped,
};
use starlark::{starlark_module, starlark_simple_value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

// Only upstream derives generate unsafe marker implementations. All values own
// their data, contain no heap references, and are safe to freeze unchanged.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    #[derive(
        Clone,
        Debug,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        StarlarkPagable,
    )]
    pub struct File {
        pub path: String,
        pub short_path: String,
        pub owner: String,
        pub is_source: bool,
        /// A tree artifact (ctx.actions.declare_directory).
        pub is_directory: bool,
    }

    #[derive(
        Clone,
        Debug,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        StarlarkPagable,
    )]
    pub(crate) struct LabelValue {
        pub repository: String,
        pub package: String,
        pub name: String,
    }

    /// A runfiles symlink entry (analysis/SymlinkEntry.java:44-87). Bazel
    /// deliberately has no `equals` (identity semantics, :52-66): `id` keeps
    /// separately created entries distinct in depsets.
    #[derive(
        Clone,
        Debug,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        StarlarkPagable,
    )]
    pub struct SymlinkEntry {
        pub id: u64,
        pub path: String,
        pub target: File,
    }

    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Allocative)]
    pub(crate) enum Item {
        File(File),
        String(String),
        Label(LabelValue),
        Int(i32),
        Bool(bool),
        Tuple(Vec<Item>),
        Symlink(SymlinkEntry),
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Allocative)]
    pub(crate) enum Order {
        Default,
        Postorder,
        Preorder,
        Topological,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Allocative)]
    pub(crate) enum ItemType {
        File,
        String,
        Label,
        Int,
        Bool,
        Tuple,
        Symlink,
    }

    #[derive(Debug, Allocative, StarlarkPagable)]
    pub(crate) struct DepsetNode {
        pub direct: Vec<Item>,
        pub children: Vec<Arc<DepsetNode>>,
        pub order: Order,
        pub element_type: Option<ItemType>,
        pub depth: usize,
        pub empty: bool,
    }

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
    pub(crate) struct Depset {
        pub node: Arc<DepsetNode>,
    }

    /// A DefaultInfo instance: a freshly constructed one
    /// (DefaultInfo.DefaultDefaultInfo, analysis/DefaultInfo.java:84-145) or
    /// the dependency view of a configured target (DelegatingDefaultInfo,
    /// :148-196; `view` is set).
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct DefaultInfoValue {
        #[allocative(skip)]
        pub files: Option<Depset>,
        #[allocative(skip)]
        pub runfiles: Option<crate::runfiles::Runfiles>,
        #[allocative(skip)]
        pub data_runfiles: Option<crate::runfiles::Runfiles>,
        #[allocative(skip)]
        pub default_runfiles: Option<crate::runfiles::Runfiles>,
        pub executable: Option<File>,
        #[allocative(skip)]
        pub files_to_run: Option<crate::runfiles::FilesToRun>,
    }

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
    pub(super) struct DefaultInfoConstructor;

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct Target {
        pub label: LabelValue,
        pub files: Vec<File>,
        pub providers: BTreeMap<String, OwnedFrozenValue>,
        #[allocative(skip)]
        pub kind: TargetKind,
        /// Display names of the declared providers a rule target returned,
        /// in return order (RuleConfiguredTarget.debugPrint).
        pub provider_keys: Vec<String>,
        /// RunfilesProvider and FilesToRunProvider of the configured target
        /// (`target[DefaultInfo]`, docs/RUNFILES.md).
        #[allocative(skip)]
        pub runfiles: crate::runfiles::TargetRunfiles,
        /// FileProvider.getFilesToBuild: one nested set per target.
        pub files_depset: Depset,
    }
}
use owned::{DefaultInfoConstructor, DefaultInfoValue, LabelValue, Target};
pub(crate) use owned::{Depset, DepsetNode, Item, ItemType, Order};
pub use owned::{File, SymlinkEntry};

/// The configured-target class behind a `Target` value; it selects only the
/// repr (and str) of the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TargetKind {
    /// RuleConfiguredTarget.repr (configuredtargets/RuleConfiguredTarget.java:284-286).
    Rule,
    /// A rule target whose single file is its executable, so its
    /// FilesToRunProvider is single-executable: a one-file filegroup
    /// (rules/filegroup/Filegroup.java:107,117-119). Prints like `Rule`.
    SingleExecutableRule,
    /// InputFileConfiguredTarget.repr (configuredtargets/InputFileConfiguredTarget.java:100-102).
    InputFile,
    /// OutputFileConfiguredTarget.repr (configuredtargets/OutputFileConfiguredTarget.java:149-151).
    OutputFile,
    /// AbstractConfiguredTarget.repr (configuredtargets/AbstractConfiguredTarget.java:243-245),
    /// e.g. a package group.
    Unknown,
    /// AliasConfiguredTarget.repr (rules/AliasConfiguredTarget.java:246-249):
    /// the alias's own canonical label; `label` and the boxed kind describe
    /// the actual target.
    Alias(String, Box<TargetKind>),
}

starlark_simple_value!(File);
starlark_simple_value!(LabelValue);
starlark_simple_value!(Depset);
starlark_simple_value!(DefaultInfoConstructor);
starlark_simple_value!(DefaultInfoValue);
starlark_simple_value!(Target);
starlark_simple_value!(SymlinkEntry);

impl File {
    /// Artifact.getRootRelativePathString (actions/Artifact.java:530-532):
    /// a source artifact's path below its (possibly external) source root
    /// (Artifact.java:773-775), a derived artifact's path below its output
    /// root (Artifact.java:307-309), so external generated files keep the
    /// `external/<repo>/` prefix of the non-sibling layout.
    fn root_relative_path(&self) -> String {
        match self.short_path.strip_prefix("../") {
            Some(external) if self.is_source => external
                .split_once('/')
                .map_or(external, |(_, path)| path)
                .to_owned(),
            Some(external) => format!("external/{external}"),
            None => self.short_path.clone(),
        }
    }
}

// Artifact.repr (actions/Artifact.java:1256-1262); str() is repr().
impl fmt::Display for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.is_source {
            "source"
        } else {
            "generated"
        };
        write!(f, "<{kind} file {}>", self.root_relative_path())
    }
}

impl LabelValue {
    fn parse(input: &str) -> anyhow::Result<Self> {
        if !input.starts_with("//") && !input.starts_with("@@") {
            bail!("Label requires an absolute canonical label: {input:?}");
        }
        let label = crate::label::Label::parse_canonical(input).map_err(anyhow::Error::msg)?;
        Ok(Self {
            repository: label.repository,
            package: label.package,
            name: label.name,
        })
    }
}

impl LabelValue {
    /// The getters Label.checkRepoVisibilityForStarlark guards
    /// (cmdline/Label.java:393,421,554,567) on a non-visible repository.
    fn invalid_repository_getter(&self, name: &str) -> bool {
        matches!(
            name,
            "workspace_root" | "repo_root" | "workspace_name" | "repo_name"
        ) && crate::label::unresolvable_repository(&self.repository).is_some()
    }

    /// Label.getCanonicalForm (cmdline/Label.java:490-492,
    /// PackageIdentifier.java:214-216, RepositoryName.java:284-286): no
    /// repository prefix in the main repository, `@@repo` otherwise.
    pub(crate) fn canonical_form(&self) -> String {
        if self.repository.is_empty() {
            format!("//{}:{}", self.package, self.name)
        } else {
            self.unambiguous_canonical_form()
        }
    }

    /// Label.getUnambiguousCanonicalForm (cmdline/Label.java:499-501).
    fn unambiguous_canonical_form(&self) -> String {
        format!("@@{}//{}:{}", self.repository, self.package, self.name)
    }

    /// Label.getDisplayForm (cmdline/Label.java:511-513 via
    /// PackageIdentifier.getDisplayForm).
    fn display_form(&self, main_mapping: Option<&BTreeMap<String, String>>) -> String {
        format!(
            "{}//{}:{}",
            repository_display_form(&self.repository, main_mapping),
            self.package,
            self.name
        )
    }

    /// Label.getShorthandDisplayForm (cmdline/Label.java:527-539).
    fn shorthand_display_form(&self, main_mapping: Option<&BTreeMap<String, String>>) -> String {
        let repository = repository_display_form(&self.repository, main_mapping);
        let basename = self.package.rsplit('/').next().unwrap_or("");
        if basename == self.name {
            return format!("{repository}//{}", self.package);
        }
        if basename.is_empty()
            && (repository == format!("@{}", self.name) || repository == format!("@@{}", self.name))
        {
            return repository;
        }
        self.display_form(main_mapping)
    }
}

/// RepositoryName.getDisplayForm (cmdline/RepositoryName.java:305-324): empty
/// for the main repository; otherwise the first apparent name the main
/// repository's mapping has for it (RepositoryMapping.getInverse,
/// RepositoryMapping.java:145-150), else the canonical `@@name`, also when no
/// mapping is known. Bazel's "first" follows mapping insertion order; the
/// native mapping is ordered by apparent name (docs/REPR.md).
fn repository_display_form(
    repository: &str,
    main_mapping: Option<&BTreeMap<String, String>>,
) -> String {
    if repository.is_empty() {
        return String::new();
    }
    main_mapping
        .and_then(|mapping| {
            mapping
                .iter()
                .find(|(_, canonical)| canonical.as_str() == repository)
        })
        .map_or_else(
            || format!("@@{repository}"),
            |(apparent, _)| format!("@{apparent}"),
        )
}

/// The main repository's mapping, when the evaluation has repositories.
fn main_repository_mapping(eval: &Evaluator<'_, '_, '_>) -> Option<BTreeMap<String, String>> {
    crate::loading::repositories(eval)
        .and_then(|repositories| repositories.mapping("").ok().cloned())
}

/// A Label's command-line form: StarlarkCustomCommandLine.expandToCommandLine
/// (analysis/starlark/StarlarkCustomCommandLine.java:967-978) prints
/// Label.getDisplayForm with the main repository mapping.
pub(crate) fn label_command_line_form(
    value: Value<'_>,
    eval: &Evaluator<'_, '_, '_>,
) -> Option<String> {
    let label = LabelValue::from_value(value)?;
    Some(label.display_form(main_repository_mapping(eval).as_ref()))
}

/// Label.repr (cmdline/Label.java:718-723): `Label("<canonical form>")`,
/// the canonical form quoted by Printer.repr.
impl fmt::Display for LabelValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Label({})",
            starlark::values::string::StarlarkStr::repr(&self.canonical_form())
        )
    }
}

/// Return the canonical string only for a genuine Label value.
pub fn unpack_label(value: Value<'_>) -> Option<String> {
    let label = LabelValue::from_value(value)?;
    Some(
        crate::label::Label {
            repository: label.repository.clone(),
            package: label.package.clone(),
            name: label.name.clone(),
        }
        .to_string(),
    )
}

/// Label.getCanonicalForm of a genuine Label value (`//pkg:x` in the main
/// repository).
pub(crate) fn label_canonical_form(value: Value<'_>) -> Option<String> {
    LabelValue::from_value(value).map(LabelValue::canonical_form)
}

/// Borrow components so bounded host consumers can charge before formatting.
pub(crate) fn label_parts(value: Value<'_>) -> Option<(&str, &str, &str)> {
    let label = LabelValue::from_value(value)?;
    Some((&label.repository, &label.package, &label.name))
}

/// Allocate a canonical label without target analysis (for repository attributes).
pub fn alloc_label<'v>(heap: Heap<'v>, canonical: &str) -> anyhow::Result<Value<'v>> {
    Ok(heap.alloc(LabelValue::parse(canonical)?))
}

/// An empty `preorder` depset of strings (e.g. JavaConfiguration
/// `default_javac_flags_depset`, rules/java/JavaConfiguration.java).
pub(crate) fn empty_preorder_string_depset<'v>(heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    Ok(heap.alloc(Depset::compose(
        Vec::new(),
        Vec::new(),
        Order::Preorder,
        Some(ItemType::String),
    )?))
}

/// [`alloc_label`] on a frozen heap (immutable containers of labels).
pub(crate) fn alloc_frozen_label(
    heap: &starlark::values::FrozenHeap,
    canonical: &str,
) -> anyhow::Result<starlark::values::FrozenValue> {
    Ok(heap.alloc(LabelValue::parse(canonical)?))
}

#[starlark_value(type = "Label")]
impl<'v> StarlarkValue<'v> for LabelValue {
    /// Label.str (cmdline/Label.java:740-754) under Bazel 9.3's default
    /// `--incompatible_unambiguous_label_stringification`
    /// (packages/semantics/BuildLanguageOptions.java:546-548): `@@//pkg:x`.
    fn collect_str(&self, collector: &mut String) {
        collector.push_str(&self.unambiguous_canonical_form());
    }

    /// Label.debugPrint (cmdline/Label.java:725-737): the shorthand display
    /// form relative to the main repository mapping (print, fail).
    fn collect_debug_print(&self, collector: &mut String, eval: &Evaluator<'v, '_, '_>) {
        collector.push_str(&self.shorthand_display_form(main_repository_mapping(eval).as_ref()));
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.hash(hasher);
        Ok(())
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other) == Some(self))
    }

    /// Label.compareTo (cmdline/Label.java:677-685): the package identifier
    /// (repository name, then package path, PackageIdentifier.java:295-307),
    /// then the target name; Starlark orders Comparable values of one class
    /// with it (`sorted`, `<`).
    fn compare(&self, other: Value<'v>) -> starlark::Result<std::cmp::Ordering> {
        match Self::from_value(other) {
            // PackageIdentifier.compareTo orders by RepositoryName.getName():
            // a non-visible repository by its apparent name only.
            Some(other) => Ok((
                crate::label::repository_sort_name(&self.repository),
                &self.package,
                &self.name,
            )
                .cmp(&(
                    crate::label::repository_sort_name(&other.repository),
                    &other.package,
                    &other.name,
                ))),
            None => starlark::values::ValueError::unsupported_with(self, "compare", other),
        }
    }

    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        if self.invalid_repository_getter(name) {
            return None;
        }
        Some(match name {
            "name" => heap.alloc(self.name.as_str()),
            "package" => heap.alloc(self.package.as_str()),
            "repo_name" | "workspace_name" => heap.alloc(self.repository.as_str()),
            "workspace_root" => heap.alloc(if self.repository.is_empty() {
                String::new()
            } else {
                format!("external/{}", self.repository)
            }),
            _ => return None,
        })
    }

    /// Label.checkRepoVisibilityForStarlark (cmdline/Label.java:762-766):
    /// `'M' is not allowed on invalid Label L` for a label whose repository
    /// its context repository does not see (crate::label::unresolvable_repository).
    fn unknown_attribute_message(&self, attribute: &str) -> Option<String> {
        self.invalid_repository_getter(attribute).then(|| {
            format!(
                "'{attribute}' is not allowed on invalid Label {}",
                self.canonical_form()
            )
        })
    }

    fn dir_attr(&self) -> Vec<String> {
        [
            "name",
            "package",
            "repo_name",
            "workspace_name",
            "workspace_root",
        ]
        .map(String::from)
        .to_vec()
    }
}

#[starlark_value(type = "File")]
impl<'v> StarlarkValue<'v> for File {
    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        self.hash(hasher);
        Ok(())
    }

    /// Artifact is Comparable by exec path (actions/Artifact.java:119-145,
    /// 197-199); Starlark orders Comparable values of one type with it
    /// (net/starlark/java/eval/Starlark.java:492-518).
    fn compare(&self, other: Value<'v>) -> starlark::Result<std::cmp::Ordering> {
        match Self::from_value(other) {
            Some(other) => Ok(self.path.cmp(&other.path)),
            None => starlark::values::ValueError::unsupported_with(self, "compare", other),
        }
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other) == Some(self))
    }

    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let basename = self.path.rsplit('/').next().unwrap_or("");
        Some(match name {
            "path" => heap.alloc(self.path.as_str()),
            "short_path" => heap.alloc(self.short_path.as_str()),
            "basename" => heap.alloc(basename),
            "dirname" => heap.alloc(self.path.rsplit_once('/').map_or("", |(dir, _)| dir)),
            "extension" => heap.alloc(basename.rsplit_once('.').map_or("", |(_, ext)| ext)),
            "is_source" => heap.alloc(self.is_source),
            "is_directory" => heap.alloc(self.is_directory),
            "owner" => heap.alloc(
                LabelValue::parse(&self.owner).expect("File owner must be an absolute valid label"),
            ),
            _ => return None,
        })
    }

    fn dir_attr(&self) -> Vec<String> {
        [
            "path",
            "short_path",
            "basename",
            "dirname",
            "extension",
            "is_source",
            "is_directory",
            "owner",
        ]
        .map(String::from)
        .to_vec()
    }
}

pub fn alloc_file<'v>(heap: Heap<'v>, file: File) -> Value<'v> {
    heap.alloc(file)
}

pub fn unpack_file(value: Value<'_>) -> anyhow::Result<File> {
    File::from_value(value)
        .cloned()
        .ok_or_else(|| anyhow!("expected File, got {}", value.get_type()))
}

/// JSON supports only consistent main-repository artifacts. Validate metadata
/// without deriving a root from dirname or changing any artifact paths. Callers
/// must charge the owner-label parse before invoking this adapter.
///
/// A generated file's root is the output root of the configuration it was
/// generated in (`bazel-out/<mnemonic>/bin`, crate::label::split_generated_root).
pub(crate) fn file_json_root(file: &File) -> anyhow::Result<&str> {
    let owner = crate::label::Label::parse_canonical(&file.owner)
        .map_err(|_| anyhow!("unsupported File JSON profile: invalid canonical owner"))?;
    if !owner.repository.is_empty() {
        bail!("unsupported File JSON profile: external repository");
    }
    // Any label-derived path: no empty, '.' or '..' component and none of
    // the characters no package or target name has (crate::bazel_label).
    if !file.short_path.split('/').all(|part| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && !part
                .bytes()
                .any(|b| b < 0x20 || b == 0x7f || b == b':' || b == b'\\')
    }) {
        bail!("unsupported File JSON profile: noncanonical short_path");
    }
    let relative = if owner.package.is_empty() {
        Some(file.short_path.as_str())
    } else {
        file.short_path
            .strip_prefix(owner.package.as_str())
            .and_then(|suffix| suffix.strip_prefix('/'))
            .filter(|suffix| !suffix.is_empty())
    }
    .ok_or_else(|| anyhow!("unsupported File JSON profile: owner package mismatch"))?;
    if file.is_source {
        if file.path != file.short_path || relative != owner.name {
            bail!("unsupported File JSON profile: inconsistent source metadata");
        }
        Ok("")
    } else {
        match crate::label::split_generated_root(&file.path) {
            Some((root, relative)) if relative == file.short_path => Ok(root),
            _ => bail!("unsupported File JSON profile: inconsistent generated metadata"),
        }
    }
}

/// A Bazel `Sequence` value (net/starlark/java/eval/Sequence.java): a list, a
/// tuple or a range (RangeList), as its elements; `None` for other values.
pub(crate) fn bazel_sequence<'v>(value: Value<'v>, heap: Heap<'v>) -> Option<Vec<Value<'v>>> {
    if let Some(list) = ListRef::from_value(value) {
        Some(list.iter().collect())
    } else if let Some(tuple) = TupleRef::from_value(value) {
        Some(tuple.iter().collect())
    } else if starlark::values::ValueLike::downcast_ref::<starlark::values::range::Range>(value)
        .is_some()
    {
        value.iterate(heap).ok().map(Iterator::collect)
    } else {
        None
    }
}

fn sequence(value: Value<'_>) -> anyhow::Result<Vec<Value<'_>>> {
    if let Some(list) = ListRef::from_value(value) {
        Ok(list.iter().collect())
    } else if let Some(tuple) = TupleRef::from_value(value) {
        Ok(tuple.iter().collect())
    } else {
        bail!("expected list or tuple, got {}", value.get_type())
    }
}

pub fn file_list(value: Value<'_>) -> anyhow::Result<Vec<File>> {
    if Depset::from_value(value).is_some() {
        unpack_depset(value)
    } else {
        sequence(value)?.into_iter().map(unpack_file).collect()
    }
}

impl Item {
    fn unpack(value: Value<'_>) -> anyhow::Result<Self> {
        Self::unpack_at_depth(value, 0)
    }

    fn unpack_at_depth(value: Value<'_>, depth: usize) -> anyhow::Result<Self> {
        if depth > 128 {
            bail!("depset tuple element exceeds nesting depth limit 128");
        }
        if let Some(file) = File::from_value(value) {
            Ok(Self::File(file.clone()))
        } else if let Some(entry) = SymlinkEntry::from_value(value) {
            Ok(Self::Symlink(entry.clone()))
        } else if let Some(s) = value.unpack_str() {
            Ok(Self::String(s.to_owned()))
        } else if let Some(label) = LabelValue::from_value(value) {
            Ok(Self::Label(label.clone()))
        } else if let Some(boolean) = value.unpack_bool() {
            Ok(Self::Bool(boolean))
        } else if let Some(integer) = value.unpack_i32() {
            Ok(Self::Int(integer))
        } else if value.get_type() == "int" {
            bail!("depset integers outside the signed 32-bit range are unsupported");
        } else if let Some(tuple) = TupleRef::from_value(value) {
            Ok(Self::Tuple(
                tuple
                    .iter()
                    .map(|item| Self::unpack_at_depth(item, depth + 1))
                    .collect::<anyhow::Result<_>>()?,
            ))
        } else {
            bail!(
                "depset elements must be immutable File, SymlinkEntry, string, Label, int, bool or tuple values; {} is unsupported",
                value.get_type()
            )
        }
    }

    pub(crate) fn alloc<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        match self {
            Self::File(file) => alloc_file(heap, file.clone()),
            Self::Symlink(entry) => heap.alloc(entry.clone()),
            Self::String(s) => heap.alloc(s.as_str()),
            Self::Label(label) => heap.alloc(label.clone()),
            Self::Int(integer) => heap.alloc(*integer),
            Self::Bool(boolean) => heap.alloc(*boolean),
            Self::Tuple(items) => heap.alloc(starlark::values::tuple::AllocTuple(
                items.iter().map(|item| item.alloc(heap)),
            )),
        }
    }

    fn kind(&self) -> ItemType {
        match self {
            Self::File(_) => ItemType::File,
            Self::String(_) => ItemType::String,
            Self::Label(_) => ItemType::Label,
            Self::Int(_) => ItemType::Int,
            Self::Bool(_) => ItemType::Bool,
            Self::Tuple(_) => ItemType::Tuple,
            Self::Symlink(_) => ItemType::Symlink,
        }
    }
}

impl Order {
    fn parse(order: &str) -> anyhow::Result<Self> {
        match order {
            "default" => Ok(Self::Default),
            "postorder" => Ok(Self::Postorder),
            "preorder" => Ok(Self::Preorder),
            "topological" => Ok(Self::Topological),
            _ => bail!(
                "depset order {order:?} is unsupported; expected default, postorder, preorder or topological"
            ),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Postorder => "postorder",
            Self::Preorder => "preorder",
            Self::Topological => "topological",
        }
    }

    pub(crate) fn compatible(self, other: Self) -> bool {
        self == other || self == Self::Default || other == Self::Default
    }
}

impl ItemType {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::File => "File",
            Self::Symlink => "SymlinkEntry",
            Self::String => "string",
            Self::Label => "Label",
            Self::Int => "int",
            Self::Bool => "bool",
            Self::Tuple => "tuple",
        }
    }

    fn from_tag(tag: u8) -> anyhow::Result<Self> {
        match tag {
            0 => Ok(Self::File),
            1 => Ok(Self::String),
            2 => Ok(Self::Label),
            3 => Ok(Self::Int),
            4 => Ok(Self::Bool),
            5 => Ok(Self::Tuple),
            6 => Ok(Self::Symlink),
            _ => bail!("invalid serialized depset element type {tag}"),
        }
    }
}

impl starlark::pagable::StarlarkSerialize for Order {
    fn starlark_serialize(
        &self,
        ctx: &mut dyn starlark::pagable::StarlarkSerializeContext,
    ) -> starlark::Result<()> {
        self.name().to_owned().starlark_serialize(ctx)
    }
}

impl starlark::pagable::StarlarkDeserialize for Order {
    fn starlark_deserialize(
        ctx: &mut dyn starlark::pagable::StarlarkDeserializeContext<'_>,
    ) -> starlark::Result<Self> {
        Self::parse(&String::starlark_deserialize(ctx)?).map_err(starlark::Error::new_other)
    }
}

impl starlark::pagable::StarlarkSerialize for ItemType {
    fn starlark_serialize(
        &self,
        ctx: &mut dyn starlark::pagable::StarlarkSerializeContext,
    ) -> starlark::Result<()> {
        (*self as u8).starlark_serialize(ctx)
    }
}

impl starlark::pagable::StarlarkDeserialize for ItemType {
    fn starlark_deserialize(
        ctx: &mut dyn starlark::pagable::StarlarkDeserializeContext<'_>,
    ) -> starlark::Result<Self> {
        Self::from_tag(u8::starlark_deserialize(ctx)?).map_err(starlark::Error::new_other)
    }
}

// The 0.14.2 enum derive refers to the private pagable dependency directly.
// Use the public Starlark paging traits instead, without adding a dependency.
impl starlark::pagable::StarlarkSerialize for Item {
    fn starlark_serialize(
        &self,
        ctx: &mut dyn starlark::pagable::StarlarkSerializeContext,
    ) -> starlark::Result<()> {
        self.kind().starlark_serialize(ctx)?;
        match self {
            Self::File(file) => file.starlark_serialize(ctx),
            Self::String(s) => s.starlark_serialize(ctx),
            Self::Label(label) => label.starlark_serialize(ctx),
            Self::Int(integer) => integer.starlark_serialize(ctx),
            Self::Bool(boolean) => boolean.starlark_serialize(ctx),
            Self::Tuple(items) => items.starlark_serialize(ctx),
            Self::Symlink(entry) => entry.starlark_serialize(ctx),
        }
    }
}

impl starlark::pagable::StarlarkDeserialize for Item {
    fn starlark_deserialize(
        ctx: &mut dyn starlark::pagable::StarlarkDeserializeContext<'_>,
    ) -> starlark::Result<Self> {
        Ok(match ItemType::starlark_deserialize(ctx)? {
            ItemType::File => Self::File(File::starlark_deserialize(ctx)?),
            ItemType::String => Self::String(String::starlark_deserialize(ctx)?),
            ItemType::Label => Self::Label(LabelValue::starlark_deserialize(ctx)?),
            ItemType::Int => Self::Int(i32::starlark_deserialize(ctx)?),
            ItemType::Bool => Self::Bool(bool::starlark_deserialize(ctx)?),
            ItemType::Tuple => Self::Tuple(Vec::starlark_deserialize(ctx)?),
            ItemType::Symlink => Self::Symlink(SymlinkEntry::starlark_deserialize(ctx)?),
        })
    }
}

impl fmt::Display for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(file) => write!(f, "{file}"),
            Self::Symlink(entry) => write!(f, "{entry}"),
            Self::String(s) => f.write_str(&starlark::values::string::StarlarkStr::repr(s)),
            Self::Label(label) => write!(f, "{label}"),
            Self::Int(integer) => write!(f, "{integer}"),
            Self::Bool(boolean) => f.write_str(if *boolean { "True" } else { "False" }),
            Self::Tuple(items) => {
                f.write_str("(")?;
                for (i, item) in items.iter().enumerate() {
                    if i != 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                if items.len() == 1 {
                    f.write_str(",")?;
                }
                f.write_str(")")
            }
        }
    }
}

/// Depset.repr (collect/nestedset/Depset.java:319-328): the flattened
/// elements, then `, order = "<name>"` unless the order is "default".
impl fmt::Display for Depset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("depset([")?;
        for (i, item) in self.flatten().iter().enumerate() {
            if i != 0 {
                f.write_str(", ")?;
            }
            write!(f, "{item}")?;
        }
        f.write_str("]")?;
        if self.node.order != Order::Default {
            write!(f, ", order = \"{}\"", self.node.order.name())?;
        }
        f.write_str(")")
    }
}

impl Depset {
    pub(crate) fn compose(
        direct: Vec<Item>,
        children: Vec<Arc<DepsetNode>>,
        order: Order,
        mut element_type: Option<ItemType>,
    ) -> anyhow::Result<Self> {
        const MAX_DEPTH: usize = 1024;
        let mut merge_type = |kind: ItemType| -> anyhow::Result<()> {
            if let Some(expected) = element_type {
                if kind != expected {
                    bail!(
                        "depset elements must have the same type; cannot mix {} and {}",
                        expected.name(),
                        kind.name()
                    );
                }
            } else {
                element_type = Some(kind);
            }
            Ok(())
        };
        for item in &direct {
            merge_type(item.kind())?;
        }
        for child in &children {
            // Bazel 9.3 accepts incompatible orders on empty children. Type
            // metadata, when present, must still survive empty native depsets.
            if !child.empty && !order.compatible(child.order) {
                bail!(
                    "depset order {:?} is incompatible with order {:?}",
                    order.name(),
                    child.order.name()
                );
            }
            if let Some(kind) = child.element_type {
                merge_type(kind)?;
            }
        }
        let children: Vec<_> = children.into_iter().filter(|child| !child.empty).collect();
        let depth = children.iter().map(|child| child.depth).max().unwrap_or(0) + 1;
        if depth > MAX_DEPTH {
            bail!("depset exceeds DAG depth limit {MAX_DEPTH}");
        }
        Ok(Self {
            node: Arc::new(DepsetNode {
                empty: direct.is_empty() && children.is_empty(),
                direct: dedup(direct),
                children,
                order,
                element_type,
                depth,
            }),
        })
    }

    /// Iterative traversal visits shared nodes once and deduplicates values at
    /// their first encounter. For topological order, reverse postorder visits
    /// rightmost children first and reverses the final unique sequence. This
    /// keeps shared descendants behind all their parents, not just the first.
    pub(crate) fn flatten(&self) -> Vec<&Item> {
        enum Visit<'a> {
            Node(&'a DepsetNode),
            Item(&'a Item),
        }
        let reverse = self.node.order == Order::Topological;
        let mut stack = vec![Visit::Node(&self.node)];
        let mut seen_nodes = HashSet::new();
        let mut seen_items = BTreeSet::new();
        let mut result = Vec::new();
        while let Some(visit) = stack.pop() {
            match visit {
                Visit::Item(item) => {
                    if seen_items.insert(item) {
                        result.push(item);
                    }
                }
                Visit::Node(node) => {
                    if !seen_nodes.insert(node as *const DepsetNode) {
                        continue;
                    }
                    let mut next = Vec::with_capacity(node.direct.len() + node.children.len());
                    if node.order == Order::Preorder {
                        next.extend(node.direct.iter().map(Visit::Item));
                        next.extend(node.children.iter().map(|child| Visit::Node(child)));
                    } else {
                        next.extend(node.children.iter().map(|child| Visit::Node(child)));
                        next.extend(node.direct.iter().map(Visit::Item));
                    }
                    // Topological traversal reverses siblings separately, not
                    // the direct-vs-transitive grouping of each DAG node.
                    if reverse {
                        next.clear();
                        if node.order == Order::Preorder {
                            next.extend(node.direct.iter().rev().map(Visit::Item));
                            next.extend(node.children.iter().rev().map(|child| Visit::Node(child)));
                        } else {
                            next.extend(node.children.iter().rev().map(|child| Visit::Node(child)));
                            next.extend(node.direct.iter().rev().map(Visit::Item));
                        }
                    }
                    stack.extend(next.into_iter().rev());
                }
            }
        }
        if reverse {
            result.reverse();
        }
        result
    }
}

starlark::methods_static!(DEPSET_METHODS = depset_methods);

#[starlark_module]
fn depset_methods(builder: &mut MethodsBuilder) {
    fn to_list<'v>(this: &Depset, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        Ok(heap.alloc(
            this.flatten()
                .into_iter()
                .map(|item| item.alloc(heap))
                .collect::<Vec<_>>(),
        ))
    }
}

#[starlark_value(type = "depset")]
impl<'v> StarlarkValue<'v> for Depset {
    fn get_methods() -> Option<&'static Methods> {
        Some(DEPSET_METHODS.methods())
    }

    fn to_bool(&self) -> bool {
        !self.node.empty
    }
}

fn dedup(items: impl IntoIterator<Item = Item>) -> Vec<Item> {
    let mut seen = BTreeSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect()
}

impl Depset {
    /// A stable-order depset of owned files.
    pub(crate) fn from_files(files: Vec<File>) -> Self {
        Depset::compose(
            files.into_iter().map(Item::File).collect(),
            vec![],
            Order::Default,
            Some(ItemType::File),
        )
        .expect("owned File depset always has a valid homogeneous type and bounded depth")
    }
}

pub fn alloc_depset<'v>(heap: Heap<'v>, files: Vec<File>) -> Value<'v> {
    heap.alloc(Depset::from_files(files))
}

pub fn unpack_depset(value: Value<'_>) -> anyhow::Result<Vec<File>> {
    let depset = Depset::from_value(value)
        .ok_or_else(|| anyhow!("expected depset of File, got {}", value.get_type()))?;
    if depset
        .node
        .element_type
        .is_some_and(|kind| kind != ItemType::File)
    {
        bail!(
            "expected depset of File, got {} element",
            depset.node.element_type.unwrap().name()
        );
    }
    depset
        .flatten()
        .into_iter()
        .map(|item| match item {
            Item::File(file) => Ok(file.clone()),
            _ => bail!(
                "expected depset of File, got {} element",
                item.kind().name()
            ),
        })
        .collect()
}

/// Scalar metadata depsets contain no File dependencies. Tuples may contain
/// File values and must not hide those dependencies from provider traversal.
pub(crate) fn contained_depset_files(value: Value<'_>) -> Option<Vec<File>> {
    Some({
        let mut stack = Depset::from_value(value)?.flatten();
        stack.reverse();
        let mut files = BTreeSet::new();
        let mut result = Vec::new();
        while let Some(item) = stack.pop() {
            match item {
                Item::File(file) if files.insert(file.clone()) => result.push(file.clone()),
                Item::Symlink(entry) if files.insert(entry.target.clone()) => {
                    result.push(entry.target.clone())
                }
                Item::Tuple(items) => stack.extend(items.iter().rev()),
                _ => {}
            }
        }
        result
    })
}

/// BuiltinProvider.repr (packages/BuiltinProvider.java:94-97).
impl fmt::Display for DefaultInfoConstructor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<function DefaultInfo>")
    }
}

/// DefaultInfoApi.DefaultInfoApiProvider.constructor parameter types
/// (starlarkbuildapi/DefaultInfoApi.java:73-160).
const DEFAULT_INFO_PARAMETERS: [(&str, &str); 5] = [
    ("files", "depset or NoneType"),
    ("runfiles", "runfiles or NoneType"),
    ("data_runfiles", "runfiles or NoneType"),
    ("default_runfiles", "runfiles or NoneType"),
    ("executable", "File or NoneType"),
];

// BuiltinProvider (DefaultInfo.PROVIDER) is `@StarlarkBuiltin(name = "Provider")`
// (starlarkbuildapi/DefaultInfoApi.java:97-98).
#[starlark_value(type = "Provider")]
impl<'v> StarlarkValue<'v> for DefaultInfoConstructor {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        args.no_positional_args(eval.heap())?;
        let mut info = DefaultInfoValue {
            files: None,
            runfiles: None,
            data_runfiles: None,
            default_runfiles: None,
            executable: None,
            files_to_run: None,
        };
        for (name, value) in args.names_map()?.iter() {
            let name = name.as_str();
            let Some((_, want)) = DEFAULT_INFO_PARAMETERS
                .iter()
                .find(|(parameter, _)| *parameter == name)
            else {
                return Err(starlark::Error::new_other(anyhow!(
                    "DefaultInfo() got unexpected keyword argument '{name}'"
                )));
            };
            if value.is_none() {
                continue;
            }
            let mismatch = || {
                starlark::Error::new_other(anyhow!(
                    "in call to DefaultInfo(), parameter '{name}' got value of type '{}', want '{want}'",
                    value.get_type()
                ))
            };
            let runfiles = || {
                crate::runfiles::Runfiles::from_value(*value)
                    .cloned()
                    .ok_or_else(mismatch)
            };
            match name {
                "files" => {
                    info.files = Some(Depset::from_value(*value).ok_or_else(mismatch)?.clone())
                }
                "runfiles" => info.runfiles = Some(runfiles()?),
                "data_runfiles" => info.data_runfiles = Some(runfiles()?),
                "default_runfiles" => info.default_runfiles = Some(runfiles()?),
                _ => info.executable = Some(File::from_value(*value).ok_or_else(mismatch)?.clone()),
            }
        }
        // DefaultInfo.java:212-218.
        if info.runfiles.is_some()
            && (info.data_runfiles.is_some() || info.default_runfiles.is_some())
        {
            return Err(starlark::Error::new_other(anyhow!(
                "Cannot specify the provider 'runfiles' together with 'data_runfiles' or 'default_runfiles'"
            )));
        }
        Ok(eval.heap().alloc(info))
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other).is_some())
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        "DefaultInfo".hash(hasher);
        Ok(())
    }
}

impl DefaultInfoValue {
    /// DefaultDefaultInfo.getDefaultRunfiles (DefaultInfo.java:126-135).
    fn effective_default_runfiles(&self) -> Option<&crate::runfiles::Runfiles> {
        if self.data_runfiles.is_none() && self.default_runfiles.is_none() {
            self.runfiles.as_ref()
        } else {
            self.default_runfiles.as_ref()
        }
    }
}

impl fmt::Display for DefaultInfoValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // NativeInfo.repr: sorted Starlark fields (packages/NativeInfo.java:119-135).
        fn shown(value: Option<String>) -> String {
            value.unwrap_or_else(|| "None".to_owned())
        }
        write!(
            f,
            "struct(data_runfiles = {}, default_runfiles = {}, files = {}, files_to_run = {})",
            shown(self.data_runfiles.as_ref().map(ToString::to_string)),
            shown(self.effective_default_runfiles().map(ToString::to_string)),
            shown(self.files.as_ref().map(ToString::to_string)),
            shown(self.files_to_run.as_ref().map(ToString::to_string)),
        )
    }
}

#[starlark_value(type = "DefaultInfo")]
impl<'v> StarlarkValue<'v> for DefaultInfoValue {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let none = Value::new_none;
        Some(match name {
            "files" => self
                .files
                .as_ref()
                .map_or_else(none, |files| heap.alloc(files.clone())),
            "files_to_run" => self
                .files_to_run
                .as_ref()
                .map_or_else(none, |files| heap.alloc(files.observed())),
            "data_runfiles" => self
                .data_runfiles
                .as_ref()
                .map_or_else(none, |runfiles| heap.alloc(runfiles.clone())),
            "default_runfiles" => self
                .effective_default_runfiles()
                .map_or_else(none, |runfiles| heap.alloc(runfiles.clone())),
            _ => return None,
        })
    }

    fn dir_attr(&self) -> Vec<String> {
        ["data_runfiles", "default_runfiles", "files", "files_to_run"]
            .map(String::from)
            .to_vec()
    }

    /// NativeInfo.equals: the same provider and equal Starlark fields
    /// (packages/NativeInfo.java:96-112); depsets, runfiles and
    /// FilesToRunProviders compare by identity.
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        let Some(other) = DefaultInfoValue::from_value(other) else {
            return Ok(false);
        };
        fn same<T>(a: Option<&T>, b: Option<&T>, eq: impl Fn(&T, &T) -> bool) -> bool {
            match (a, b) {
                (None, None) => true,
                (Some(a), Some(b)) => eq(a, b),
                _ => false,
            }
        }
        let runfiles = |a: &crate::runfiles::Runfiles, b: &crate::runfiles::Runfiles| a.same(b);
        Ok(same(self.files.as_ref(), other.files.as_ref(), |a, b| {
            Arc::ptr_eq(&a.node, &b.node)
        }) && same(
            self.data_runfiles.as_ref(),
            other.data_runfiles.as_ref(),
            runfiles,
        ) && same(
            self.effective_default_runfiles(),
            other.effective_default_runfiles(),
            runfiles,
        ) && same(
            self.files_to_run.as_ref(),
            other.files_to_run.as_ref(),
            // Each read of a file target's provider is a new object.
            |a, b| a.same(b) && a.observed().same(b),
        ))
    }

    fn set_attr(&self, _attribute: &str, _new_value: Value<'v>) -> starlark::Result<()> {
        crate::runfiles::no_field_assignment("DefaultInfo")
    }
}

/// Depset.cast(files, Artifact.class, "files") of a returned DefaultInfo
/// (collect/nestedset/Depset.java:262-272).
fn default_info_files(files: &Depset) -> anyhow::Result<Vec<File>> {
    if let Some(kind) = files.node.element_type
        && kind != ItemType::File
        && !files.node.empty
    {
        bail!(
            "for 'files', got a depset of '{}', expected a depset of 'File'",
            kind.name()
        );
    }
    Ok(files
        .flatten()
        .into_iter()
        .filter_map(|item| match item {
            Item::File(file) => Some(file.clone()),
            _ => None,
        })
        .collect())
}

/// The returned DefaultInfo, if any (a missing one means the implicit empty
/// DefaultInfo, StarlarkRuleConfiguredTargetUtil.java:325-335).
pub(crate) fn returned_default_info(
    result: Value<'_>,
) -> anyhow::Result<Option<crate::runfiles::ReturnedDefaultInfo>> {
    let provider = if result.is_none() {
        None
    } else if let Some(provider) = DefaultInfoValue::from_value(result) {
        Some(provider)
    } else if crate::providers::instance_identity(result).is_some() {
        None
    } else {
        let mut found = None;
        for value in sequence(result)? {
            if crate::providers::instance_identity(value).is_some() {
                continue;
            }
            let provider = DefaultInfoValue::from_value(value)
                .ok_or_else(|| anyhow!("unsupported returned provider {}; expected DefaultInfo, TemplateVariableInfo, or a custom provider instance", value.get_type()))?;
            if found.is_some() {
                // addProviders (StarlarkRuleConfiguredTargetUtil.java:268-270).
                bail!("Multiple conflicting returned providers with key DefaultInfo");
            }
            found = Some(provider);
        }
        found
    };
    let Some(provider) = provider else {
        return Ok(None);
    };
    Ok(Some(crate::runfiles::ReturnedDefaultInfo {
        files: provider
            .files
            .as_ref()
            .map(default_info_files)
            .transpose()?,
        runfiles: provider.runfiles.clone(),
        data_runfiles: provider.data_runfiles.clone(),
        default_runfiles: provider.default_runfiles.clone(),
        executable: provider.executable.clone(),
    }))
}

/// Missing DefaultInfo (including TemplateVariableInfo-only returns) or an
/// omitted/None files field requests the caller's implicit predeclared
/// outputs; an explicitly empty depset requests no default outputs.
pub fn default_files(result: Value<'_>) -> anyhow::Result<Option<Vec<File>>> {
    Ok(returned_default_info(result)?.and_then(|info| info.files))
}

/// Files reachable from a DefaultInfo, Runfiles, SymlinkEntry or
/// FilesToRunProvider payload value (provider-file traversal).
pub(crate) fn runfiles_payload_files(value: Value<'_>) -> Option<Vec<File>> {
    use crate::runfiles::{FilesToRun, Runfiles};
    if let Some(runfiles) = Runfiles::from_value(value) {
        return Some(runfiles.all_artifacts());
    }
    if let Some(entry) = SymlinkEntry::from_value(value) {
        return Some(vec![entry.target.clone()]);
    }
    let files_to_run = |files_to_run: &FilesToRun| {
        let mut files = files_to_run.executable.iter().cloned().collect::<Vec<_>>();
        if let Some(runfiles) = files_to_run.runfiles() {
            files.extend(runfiles.all_artifacts());
        }
        files
    };
    if let Some(provider) = FilesToRun::from_value(value) {
        return Some(files_to_run(provider));
    }
    let info = DefaultInfoValue::from_value(value)?;
    let mut files = Vec::new();
    if let Some(depset) = &info.files {
        files.extend(default_info_files(depset).unwrap_or_default());
    }
    for runfiles in [&info.runfiles, &info.data_runfiles, &info.default_runfiles]
        .into_iter()
        .flatten()
    {
        files.extend(runfiles.all_artifacts());
    }
    files.extend(info.executable.iter().cloned());
    if let Some(provider) = &info.files_to_run {
        files.extend(files_to_run(provider));
    }
    let mut seen = BTreeSet::new();
    files.retain(|file| seen.insert(file.clone()));
    Some(files)
}

pub(crate) fn is_depset(value: Value<'_>) -> bool {
    Depset::from_value(value).is_some()
}

/// The element type name of a non-empty depset (Depset.getElementType).
pub(crate) fn depset_element_type(value: Value<'_>) -> Option<&'static str> {
    let depset = Depset::from_value(value)?;
    (!depset.node.empty)
        .then_some(depset.node.element_type)
        .flatten()
        .map(ItemType::name)
}

pub(crate) fn is_default_info(value: Value<'_>) -> bool {
    DefaultInfoValue::from_value(value).is_some()
}

pub(crate) fn is_default_info_constructor(value: Value<'_>) -> bool {
    DefaultInfoConstructor::from_value(value).is_some()
}

/// The configured-target reprs print Label.toString, the canonical form
/// (cmdline/Label.java:478-481); str() is repr().
impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = self.label.canonical_form();
        match &self.kind {
            TargetKind::Rule | TargetKind::SingleExecutableRule => write!(f, "<target {label}>"),
            TargetKind::InputFile => write!(f, "<input file target {label}>"),
            TargetKind::OutputFile => write!(f, "<output file target {label}>"),
            TargetKind::Unknown => write!(f, "<unknown target {label}>"),
            TargetKind::Alias(alias, _) => write!(f, "<alias target {alias} of {label}>"),
        }
    }
}

impl Target {
    /// `target[DefaultInfo]`: DefaultInfo.DelegatingDefaultInfo
    /// (analysis/DefaultInfo.java:148-196) over the target's providers.
    fn default_info(&self) -> DefaultInfoValue {
        let files_to_run = self.runfiles.files_to_run(&self.files);
        DefaultInfoValue {
            files: Some(self.files_depset.clone()),
            runfiles: None,
            data_runfiles: Some(self.runfiles.data_runfiles()),
            default_runfiles: Some(self.runfiles.default_runfiles()),
            executable: files_to_run.executable.clone(),
            files_to_run: Some(files_to_run),
        }
    }
}

#[starlark_value(type = "Target")]
impl<'v> StarlarkValue<'v> for Target {
    /// RuleConfiguredTarget.debugPrint (configuredtargets/RuleConfiguredTarget.java:288-303):
    /// the declared provider keys in insertion order, the returned providers
    /// then the framework's OutputGroupInfo (RuleConfiguredTargetBuilder.java:158-172,211-213).
    /// Other configured targets print their repr (StarlarkValue.java:77-79).
    fn collect_debug_print(&self, collector: &mut String, _eval: &Evaluator<'v, '_, '_>) {
        use std::fmt::Write;
        if !matches!(
            self.kind,
            TargetKind::Rule | TargetKind::SingleExecutableRule
        ) {
            write!(collector, "{self}").unwrap();
            return;
        }
        write!(collector, "<target {}, keys:[", self.label.canonical_form()).unwrap();
        for key in &self.provider_keys {
            collector.push_str(key);
            collector.push_str(", ");
        }
        collector.push_str("OutputGroupInfo]>");
    }

    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Some(match name {
            "files" => alloc_depset(heap, self.files.clone()),
            "label" => heap.alloc(self.label.clone()),
            _ => return None,
        })
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["files".to_owned(), "label".to_owned()]
    }

    fn at(&self, index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        if DefaultInfoConstructor::from_value(index).is_some() {
            return Ok(heap.alloc(self.default_info()));
        }
        let identity = crate::providers::provider_identity(index).ok_or_else(|| {
            starlark::Error::new_other(anyhow!(
                "Target indexing requires an exported provider constructor"
            ))
        })?;
        let provider = self.providers.get(&identity).ok_or_else(|| {
            starlark::Error::new_other(anyhow!(
                "Target {} does not provide {identity}",
                self.label.unambiguous_canonical_form()
            ))
        })?;
        Ok(heap.access_owned_frozen_value(provider))
    }

    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        if DefaultInfoConstructor::from_value(other).is_some() {
            return Ok(true);
        }
        let identity = crate::providers::provider_identity(other).ok_or_else(|| {
            starlark::Error::new_other(anyhow!(
                "Target membership requires an exported provider constructor"
            ))
        })?;
        Ok(self.providers.contains_key(&identity))
    }
}

/// The `attr` module object (StarlarkAttrModule): repr `<attr>`
/// (analysis/starlark/StarlarkAttrModule.java:660-662), type `attr`
/// (starlarkbuildapi/StarlarkAttrModuleApi.java:40-57). Its members are the
/// frozen struct of `attr.*` builtin methods.
pub(crate) use builtin_objects::AttrModule;
use builtin_objects::StructProvider;

// Only the upstream derives generate unsafe marker implementations; both
// values own no heap references beyond a frozen globals value.
#[allow(unsafe_code)]
mod builtin_objects {
    use super::*;
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct AttrModule(pub starlark::values::FrozenValue);

    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct StructProvider;
}
starlark_simple_value!(AttrModule);

impl fmt::Display for AttrModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<attr>")
    }
}

#[starlark_value(type = "attr")]
impl<'v> StarlarkValue<'v> for AttrModule {
    fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        self.0.to_value().get_attr(name, heap).ok().flatten()
    }

    fn dir_attr(&self) -> Vec<String> {
        self.0.to_value().dir_attr()
    }
}

// The `struct` builtin is the StructProvider (packages/StructProvider.java:30-44):
// a BuiltinProvider with repr `<function struct>` (BuiltinProvider.java:94-97)
// and type `Provider` (starlarkbuildapi/core/StructApi.java:38-39). Calling it
// creates a struct from keyword arguments only (StructApi.java:41-53).
starlark_simple_value!(StructProvider);

impl fmt::Display for StructProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<function struct>")
    }
}

#[starlark_value(type = "Provider")]
impl<'v> StarlarkValue<'v> for StructProvider {
    fn invoke(
        &self,
        _me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let heap = eval.heap();
        if args.positions(heap)?.next().is_some() {
            // BuiltinFunction argument binding (net/starlark/java/eval/BuiltinFunction.java:152-154).
            return Err(starlark::Error::new_other(anyhow!(
                "struct() got unexpected positional argument"
            )));
        }
        let fields = args.names_map()?;
        Ok(heap.alloc(starlark::values::structs::AllocStruct(
            fields.iter().map(|(name, value)| (name.as_str(), *value)),
        )))
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Self::from_value(other).is_some())
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        "struct".hash(hasher);
        Ok(())
    }
}

/// Replace the generic `struct` builtin with Bazel's StructProvider.
pub(crate) fn install_struct_provider(builder: &mut GlobalsBuilder) {
    builder.set("struct", StructProvider);
}

/// `label` is a canonical absolute label supplied by the analysis engine.
pub fn alloc_target<'v>(heap: Heap<'v>, label: &str, files: Vec<File>) -> Value<'v> {
    alloc_target_with_providers(heap, label, files, BTreeMap::new())
}

/// Owned frozen providers retain their heaps. Indexing additionally attaches
/// that owner to the receiving heap before exposing any borrowed Value.
pub fn alloc_target_with_providers<'v>(
    heap: Heap<'v>,
    label: &str,
    files: Vec<File>,
    providers: BTreeMap<String, OwnedFrozenValue>,
) -> Value<'v> {
    alloc_target_of_kind(
        heap,
        label,
        files,
        providers,
        TargetKind::Rule,
        Vec::new(),
        Default::default(),
    )
}

/// As [`alloc_target_with_providers`], for a dependency of a known
/// configured-target class, with its RunfilesProvider/FilesToRunProvider.
pub(crate) fn alloc_target_of_kind<'v>(
    heap: Heap<'v>,
    label: &str,
    files: Vec<File>,
    providers: BTreeMap<String, OwnedFrozenValue>,
    kind: TargetKind,
    provider_keys: Vec<String>,
    runfiles: crate::runfiles::TargetRunfiles,
) -> Value<'v> {
    heap.alloc(Target {
        label: LabelValue::parse(label).expect("Target label must be an absolute valid label"),
        files_depset: Depset::from_files(files.clone()),
        files,
        providers,
        kind,
        provider_keys,
        runfiles,
    })
}

// Typed unpacking keeps the builtin's argument errors in the macro binder.
// ValueTyped retains the original (possibly frozen) Label without allocating.
enum LabelInput<'v> {
    Label(ValueTyped<'v, LabelValue>),
    String(&'v str),
}

impl StarlarkTypeRepr for LabelInput<'_> {
    type Canonical = Self;

    fn starlark_type_repr() -> starlark::typing::Ty {
        starlark::typing::Ty::union2(
            LabelValue::starlark_type_repr(),
            <&str>::starlark_type_repr(),
        )
    }
}

impl<'v> UnpackValue<'v> for LabelInput<'v> {
    type Error = std::convert::Infallible;

    fn unpack_value_impl(value: Value<'v>) -> Result<Option<Self>, Self::Error> {
        Ok(if let Some(label) = ValueTyped::new(value) {
            Some(Self::Label(label))
        } else {
            value.unpack_str().map(Self::String)
        })
    }
}

#[starlark_module]
pub fn analysis_globals(builder: &mut GlobalsBuilder) {
    const DefaultInfo: DefaultInfoConstructor = DefaultInfoConstructor;

    fn Label<'v>(
        #[starlark(require = pos)] input: LabelInput<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let label_string = match input {
            LabelInput::Label(label) => return Ok(label.to_value()),
            LabelInput::String(text) => text,
        };
        if crate::loading::repositories(eval).is_some() {
            // Label() is Label.parseWithPackageContext in the innermost .bzl's
            // package context (StarlarkRuleClassFunctions.label, :2124-2143):
            // an apparent repository that repository does not see yields an
            // invalid Label, not an error.
            // LabelSyntaxException -> `invalid label in Label(): MSG` (:2140-2142)
            // for Bazel-worded syntax errors (RepositoryName.validate).
            let label =
                crate::loading::resolve_label_deferred(eval, label_string).map_err(|e| {
                    let text = e.to_string();
                    if text.starts_with("invalid repository name '") {
                        anyhow::anyhow!("invalid label in Label(): {text}")
                    } else {
                        e
                    }
                })?;
            return Ok(eval.heap().alloc(LabelValue::parse(&label.to_string())?));
        }
        Ok(eval.heap().alloc(LabelValue::parse(
            label_string
                .strip_prefix("@//")
                .map_or(label_string, |_| &label_string[1..]),
        )?))
    }

    fn depset<'v>(
        #[starlark(default = NoneType)] direct: Value<'v>,
        #[starlark(default = "default")] order: &str,
        #[starlark(require = named, default = NoneType)] transitive: Value<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        // Depset.depset's `Sequence or NoneType` parameter checks
        // (collect/nestedset/Depset.java:572-627; BuiltinFunction.java:404-410);
        // a range is a Sequence (RangeList).
        let mut items = [Vec::new(), Vec::new()];
        for (slot, (param, value)) in [("direct", direct), ("transitive", transitive)]
            .into_iter()
            .enumerate()
        {
            if value.is_none() {
                continue;
            }
            items[slot] = bazel_sequence(value, heap).ok_or_else(|| {
                anyhow!(
                    "in call to depset(), parameter '{param}' got value of type '{}', want 'sequence or NoneType'",
                    value.get_type()
                )
            })?;
        }
        let [direct_items, transitive_items] = items;
        let order = Order::parse(order)?;
        let mut children = Vec::new();
        if !transitive.is_none() {
            for child in transitive_items {
                let child = Depset::from_value(child)
                    .ok_or_else(|| anyhow!("depset transitive entries must be depsets"))?;
                children.push(Arc::clone(&child.node));
            }
        }
        let direct = direct_items
            .into_iter()
            .map(Item::unpack)
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(heap.alloc(Depset::compose(direct, children, order, None)?))
    }
}

use starlark::values::none::NoneType;

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{Globals, Module};
    use starlark::syntax::{AstModule, Dialect};

    fn file(path: &str) -> File {
        File {
            is_directory: false,
            path: path.to_owned(),
            short_path: path.to_owned(),
            owner: "//pkg:rule".to_owned(),
            is_source: true,
        }
    }

    fn globals() -> Globals {
        GlobalsBuilder::standard().with(analysis_globals).build()
    }

    fn eval(source: &str) -> Result<String, String> {
        Module::with_temp_heap(|module| {
            module.set("a", alloc_file(module.heap(), file("pkg/a.txt")));
            module.set("a2", alloc_file(module.heap(), file("pkg/a.txt")));
            module.set("b", alloc_file(module.heap(), file("pkg/b")));
            module.set(
                "target",
                alloc_target(module.heap(), "//pkg:rule", vec![file("pkg/a.txt")]),
            );
            let ast = AstModule::parse("test.star", source.to_owned(), &Dialect::Standard).unwrap();
            let mut evaluator = Evaluator::new(&module);
            evaluator
                .eval_module(ast, &globals())
                .map(|v| v.to_repr())
                .map_err(|e| e.to_string())
        })
    }

    fn pass(source: &str) {
        assert_eq!(eval(source).unwrap(), "True", "{source}");
    }

    fn fail(source: &str, message: &str) {
        let error = eval(source).unwrap_err();
        assert!(error.contains(message), "expected {message:?} in {error}");
    }

    #[test]
    fn file_fields_identity_hash_and_label() {
        pass(
            r#"a.path == "pkg/a.txt" and a.short_path == "pkg/a.txt" and a.basename == "a.txt" and a.dirname == "pkg" and a.extension == "txt" and b.extension == "" and a.is_source and not a.is_directory"#,
        );
        pass(r#"a == a2 and a != b and {a: 42}[a2] == 42 and len(dict([(a, 1), (a2, 2)])) == 1"#);
        pass(
            r#"type(a) == "File" and type(a.owner) == "Label" and a.owner == Label("//pkg:rule") and str(a.owner) == "@@//pkg:rule" and a.owner.name == "rule" and a.owner.package == "pkg""#,
        );
        pass(
            r#"Label("//pkg") == Label("//pkg:pkg") and {Label("//pkg"): 7}[Label("//pkg:pkg")] == 7"#,
        );
        // Artifact orders by exec path (Artifact.java:134-145, 197-199).
        pass("a < b and not (a < a2) and sorted([b, a]) == [a, b] and max([a, b]) == b");
        fail("a < 1", "unsupported comparison: File <=> int");
        // Other values Bazel does not order (Starlark.java:492-518).
        for (expression, types) in [
            ("depset() < depset()", "depset <=> depset"),
            ("None < None", "NoneType <=> NoneType"),
            ("True < 1", "bool <=> int"),
            (
                "len < len",
                "builtin_function_or_method <=> builtin_function_or_method",
            ),
            ("{} < {}", "dict <=> dict"),
        ] {
            fail(expression, &format!("unsupported comparison: {types}"));
        }
        // Bazel 9.3 Starlark.getattr / EvalUtils.setField texts.
        fail("a.missing", "'File' value has no field or method 'missing'");
        fail("a.path = 'changed'", "cannot set .path field of File value");
        fail("Label(':relative')", "absolute");
        fail("Label('@external//p:x')", "absolute");
    }

    #[test]
    fn label_repo_name_surface_preserves_existing_fields() {
        for (input, repository) in [
            ("//pkg:rule", ""),
            ("@@//pkg:rule", ""),
            ("@@dep+//pkg:rule", "dep+"),
            ("@@dep++observe+made//pkg:rule", "dep++observe+made"),
        ] {
            pass(&format!(
                r#"
x = Label({input:?})
(type(x.repo_name) == "string" and x.repo_name == {repository:?} and
 x.repo_name == x.workspace_name and getattr(x, "repo_name", "DEFAULT") == {repository:?} and
 hasattr(x, "repo_name") and "repo_name" in dir(x) and
 all([field in dir(x) for field in ["name", "package", "workspace_name", "workspace_root"]]) and
 x.name == "rule" and x.package == "pkg" and
 x.workspace_root == ("external/" + {repository:?} if {repository:?} else "") and
 getattr(x, "missing_field", "DEFAULT") == "DEFAULT" and not hasattr(x, "missing_field"))
"#
            ));
        }
        pass(r#"a.owner.repo_name == "" and target.label.repo_name == """#);
    }

    #[test]
    fn label_repo_name_is_immutable_and_unknown_fields_still_fail() {
        for field in ["repo_name", "workspace_name"] {
            fail(
                &format!("x = Label('//pkg:rule'); x.{field} = 'changed'"),
                &format!("cannot set .{field} field of Label value"),
            );
        }
        fail(
            "Label('//pkg:rule').missing_field",
            "'Label' value has no field or method 'missing_field'",
        );
    }

    #[test]
    fn label_repo_name_does_not_change_string_equality_or_hash() {
        pass(
            r#"
main = Label("//pkg:rule")
external = Label("@@dep+//pkg:rule")
keys = {main: "main", external: "external"}
names = [main.repo_name, external.repo_name]
(names == ["", "dep+"] and str(main) == "@@//pkg:rule" and
 str(external) == "@@dep+//pkg:rule" and main != external and
 main == Label("@@//pkg:rule") and keys[Label("@@//pkg:rule")] == "main" and
 keys[Label("@@" + external.repo_name + "//pkg:rule")] == "external" and len(keys) == 2)
"#,
        );
        Module::with_temp_heap(|module| {
            let label = alloc_label(module.heap(), "@@dep++observe+made//pkg:rule").unwrap();
            let before = label.get_hashed().unwrap().hash();
            let value = LabelValue::from_value(label).unwrap();
            let mut legacy = StarlarkHasher::default();
            value.repository.hash(&mut legacy);
            value.package.hash(&mut legacy);
            value.name.hash(&mut legacy);
            let mut actual = StarlarkHasher::default();
            value.write_hash(&mut actual).unwrap();
            assert_eq!(
                std::hash::Hasher::finish(&actual),
                std::hash::Hasher::finish(&legacy)
            );
            assert_eq!(
                value
                    .get_attr("repo_name", module.heap())
                    .unwrap()
                    .unpack_str(),
                Some("dep++observe+made")
            );
            assert_eq!(label.get_hashed().unwrap().hash(), before);
        });
    }

    #[test]
    fn label_passthrough_preserves_pointer_hash_and_heap_usage() {
        Module::with_temp_heap(|module| {
            let globals = globals();
            let mut evaluator = Evaluator::new(&module);
            let constructor = evaluator
                .eval_module(
                    AstModule::parse("ctor.star", "Label".into(), &Dialect::Standard).unwrap(),
                    &globals,
                )
                .unwrap();
            for canonical in [
                "//pkg:main",
                "@@dep+//pkg:external",
                "@@dep++extension+made//pkg:generated",
            ] {
                let original = alloc_label(module.heap(), canonical).unwrap();
                let copied = evaluator
                    .eval_function(constructor, &[original], &[])
                    .unwrap();
                assert!(original.ptr_eq(copied), "{canonical}");
                assert_eq!(
                    original.get_hashed().unwrap().hash(),
                    copied.get_hashed().unwrap().hash()
                );
                let before = (
                    module.heap().allocated_bytes(),
                    module.heap().available_bytes(),
                );
                for _ in 0..32 {
                    assert!(
                        evaluator
                            .eval_function(constructor, &[original], &[])
                            .unwrap()
                            .ptr_eq(original)
                    );
                }
                assert_eq!(
                    before,
                    (
                        module.heap().allocated_bytes(),
                        module.heap().available_bytes(),
                    ),
                    "passthrough must not consume Starlark heap space"
                );
                module.set("original", original);
                module.set("copied", copied);
                let result = evaluator
                    .eval_module(
                        AstModule::parse(
                            "keys.star",
                            "d = {original: 1}\nd[copied] = 2\nlen(d) == 1 and d[original] == 2 and str(original) == str(copied)".into(),
                            &Dialect::Standard,
                        )
                        .unwrap(),
                        &globals,
                    )
                    .unwrap();
                assert_eq!(result.unpack_bool(), Some(true));
            }
        });
    }

    #[test]
    fn label_repo_name_getters_survive_freeze_and_owner_drop() {
        for repository in ["", "dep+", "dep++observe+made"] {
            let canonical = format!("@@{repository}//pkg:rule");
            let (label, file, target, hash) = Module::with_temp_heap(|module| {
                let label = alloc_label(module.heap(), &canonical).unwrap();
                let hash = label.get_hashed().unwrap().hash();
                let mut owned_file = file("pkg/input");
                owned_file.owner = canonical.clone();
                module.set("label", label);
                module.set("file", alloc_file(module.heap(), owned_file.clone()));
                module.set(
                    "target",
                    alloc_target(module.heap(), &canonical, vec![owned_file]),
                );
                let frozen = module.freeze().unwrap();
                (
                    frozen.get("label").unwrap(),
                    frozen.get("file").unwrap(),
                    frozen.get("target").unwrap(),
                    hash,
                )
            });
            Module::with_temp_heap(|module| {
                for (name, owned) in [("label", label), ("file", file), ("target", target)] {
                    module.set(name, module.heap().access_owned_frozen_value(&owned));
                    drop(owned);
                }
                assert_eq!(
                    module.get("label").unwrap().get_hashed().unwrap().hash(),
                    hash
                );
                let ast = AstModule::parse("owner-drop.star", format!(
                    "label.repo_name == {repository:?} and file.owner.repo_name == {repository:?} and target.label.repo_name == {repository:?} and label.workspace_name == label.repo_name and label == file.owner and label == target.label and {{label: 7}}[file.owner] == 7"
                ), &Dialect::Standard).unwrap();
                let result = Evaluator::new(&module)
                    .eval_module(ast, &globals())
                    .unwrap();
                assert_eq!(result.unpack_bool(), Some(true));
            });
        }
    }

    #[test]
    fn label_passthrough_frozen_helper_and_input_outlive_original_owners() {
        let origin = Module::with_temp_heap(|module| {
            module.set(
                "original",
                alloc_label(module.heap(), "@@dep++extension+made//pkg:item").unwrap(),
            );
            module.freeze().unwrap()
        });
        let helper = Module::with_temp_heap(|module| {
            Evaluator::new(&module)
                .eval_module(
                    AstModule::parse(
                        "helper.star",
                        "def copy(value):\n    return Label(value)\n".into(),
                        &Dialect::Standard,
                    )
                    .unwrap(),
                    &globals(),
                )
                .unwrap();
            module.freeze().unwrap()
        });
        let original = origin.get("original").unwrap();
        let copy = helper.get("copy").unwrap();
        drop(origin);
        drop(helper);
        let caller = Module::with_temp_heap(|module| {
            let value = module.heap().access_owned_frozen_value(&original);
            let function = module.heap().access_owned_frozen_value(&copy);
            drop(original);
            drop(copy);
            let result = Evaluator::new(&module)
                .eval_function(function, &[value], &[])
                .unwrap();
            assert!(value.ptr_eq(result));
            assert_eq!(
                value.get_hashed().unwrap().hash(),
                result.get_hashed().unwrap().hash()
            );
            module.set("result", result);
            module.freeze().unwrap()
        });
        let result = caller.get("result").unwrap();
        drop(caller);
        Module::with_temp_heap(|module| {
            let value = module.heap().access_owned_frozen_value(&result);
            drop(result);
            module.set("value", value);
            let ast = AstModule::parse(
                "reader.star",
                "copy = Label(value)\ncopy == value and {value: 7}[copy] == 7 and str(copy) == '@@dep++extension+made//pkg:item' and copy.workspace_name == 'dep++extension+made'".into(),
                &Dialect::Standard,
            )
            .unwrap();
            let copied = Evaluator::new(&module)
                .eval_module(ast, &globals())
                .unwrap();
            assert_eq!(copied.unpack_bool(), Some(true));
            assert!(module.get("copy").unwrap().ptr_eq(value));
        });
    }

    #[test]
    fn label_passthrough_typed_union_rejects_nonlabels_without_coercion() {
        use starlark::environment::LibraryExtension;

        Module::with_temp_heap(|module| {
            let globals = GlobalsBuilder::extended_by(&[LibraryExtension::StructType])
                .with(analysis_globals)
                .build();
            let mut evaluator = Evaluator::new(&module);
            let constructor = evaluator
                .eval_module(
                    AstModule::parse("ctor.star", "Label".into(), &Dialect::Standard).unwrap(),
                    &globals,
                )
                .unwrap();
            for (expression, actual) in [
                ("None", "NoneType"),
                ("True", "bool"),
                ("1", "int"),
                ("[]", "list"),
                ("{}", "dict"),
                (
                    "struct(type='Label', workspace_name='dep+', package='pkg', name='item')",
                    "struct",
                ),
            ] {
                let value = evaluator
                    .eval_module(
                        AstModule::parse("arg.star", expression.into(), &Dialect::Standard)
                            .unwrap(),
                        &globals,
                    )
                    .unwrap();
                assert!(LabelInput::unpack_value(value).unwrap().is_none());
                let error = evaluator
                    .eval_function(constructor, &[value], &[])
                    .unwrap_err()
                    .to_string();
                // BuiltinFunction.checkParamValue (BuiltinFunction.java:404-410)
                // with Label's allowedTypes String, Label.
                assert!(
                    error.contains(&format!(
                        "in call to Label(), parameter 'input' got value of type '{actual}', want 'string or Label'"
                    )),
                    "{expression}: {error}"
                );
                assert!(!error.contains("absolute canonical"), "{error}");
            }
        });
    }

    #[test]
    fn label_passthrough_signature_is_required_positional_only() {
        Module::with_temp_heap(|module| {
            let mut evaluator = Evaluator::new(&module);
            let constructor = evaluator
                .eval_module(
                    AstModule::parse("ctor.star", "Label".into(), &Dialect::Standard).unwrap(),
                    &globals(),
                )
                .unwrap();
            let string = module.heap().alloc("//pkg:item");
            // Bazel 9.3 BuiltinFunction binding texts (BuiltinFunction.java:320-490):
            // `input` is positional-only.
            for (positional, named, expected) in [
                (
                    vec![],
                    vec![],
                    "Label() missing 1 required positional argument: input",
                ),
                (
                    vec![string, string],
                    vec![],
                    "Label() accepts no more than 1 positional argument but got 2",
                ),
                (
                    vec![],
                    vec![("input", string)],
                    "Label() got named argument for positional-only parameter 'input'",
                ),
                (
                    vec![],
                    vec![("label_string", string)],
                    "Label() got unexpected keyword argument 'label_string'",
                ),
                (
                    vec![string],
                    vec![("input", string)],
                    "Label() got named argument for positional-only parameter 'input'",
                ),
                (
                    vec![string],
                    vec![("label_string", string)],
                    "Label() got unexpected keyword argument 'label_string'",
                ),
            ] {
                let error = evaluator
                    .eval_function(constructor, &positional, &named)
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(expected), "{expected}: {error}");
                assert!(!error.contains("absolute canonical"), "{error}");
            }
        });
    }

    #[test]
    fn label_passthrough_does_not_change_contextless_string_parsing() {
        pass("Label('@//pkg:item') == Label('@@//pkg:item')");
        pass("str(Label('//pkg')) == '@@//pkg:pkg'");
        pass("str(Label('@@dep++ext+made//pkg:item')) == '@@dep++ext+made//pkg:item'");
        for source in ["Label(':relative')", "Label('@unknown//pkg:item')"] {
            fail(source, "Label requires an absolute canonical label");
        }
        pass("str(Label('//pkg:café')) == '@@//pkg:café'");
        pass("str(Label('//a b/(c):d e')) == '@@//a b/(c):d e'");
        pass("str(Label('@@gawk')) == '@@gawk//:gawk'");
        fail(
            "Label('//pkg:../item')",
            "invalid target name '../item': target names may not contain up-level references '..'",
        );
    }

    #[test]
    fn label_repo_name_and_passthrough_compose_without_remapping() {
        pass(
            r#"
main = Label("//pkg:rule")
external = Label("@@dep++observe+made//pkg:rule")
copies = [Label(main), Label(external)]
(copies[0].repo_name == "" and copies[1].repo_name == "dep++observe+made" and
 copies[0].workspace_name == copies[0].repo_name and
 copies[1].workspace_name == copies[1].repo_name and
 {main: "main", external: "external"}[copies[1]] == "external" and
 str(copies[0]) == "@@//pkg:rule" and "repo_name" in dir(copies[1]))
"#,
        );
    }

    #[test]
    fn json_file_roots_preserve_main_source_and_generated_paths() {
        for (owner, name) in [
            ("//:input.txt", "input.txt"),
            ("//pkg:dir/input.txt", "dir/input.txt"),
            ("@@//pkg:input.txt", "input.txt"),
        ] {
            let label = crate::label::Label::parse_canonical(owner).unwrap();
            let source = File {
                is_directory: false,
                path: label.source_exec_path().to_string_lossy().into_owned(),
                short_path: label.short_path().to_string_lossy().into_owned(),
                owner: owner.into(),
                is_source: true,
            };
            let before = source.clone();
            assert_eq!(file_json_root(&source).unwrap(), "");
            assert_eq!(source, before);
            let output = label.with_name(&format!("generated/{name}")).unwrap();
            let generated = File {
                is_directory: false,
                path: output
                    .generated_exec_path(crate::label::GENERATED_EXEC_ROOT)
                    .to_string_lossy()
                    .into_owned(),
                short_path: output.short_path().to_string_lossy().into_owned(),
                // A generated artifact is owned by its declaring rule, not by
                // its output name. Only its package must match the owner.
                owner: owner.into(),
                is_source: false,
            };
            let before = generated.clone();
            assert_eq!(
                file_json_root(&generated).unwrap(),
                "bazel-out/k8-fastbuild/bin"
            );
            assert_eq!(generated, before);
            // Every configuration's root (crate::config_registry).
            let root = "bazel-out/k8-opt-exec-ST-0123456789ab/bin";
            let exec = File {
                is_directory: false,
                path: output
                    .generated_exec_path(root)
                    .to_string_lossy()
                    .into_owned(),
                ..generated.clone()
            };
            assert_eq!(file_json_root(&exec).unwrap(), root);
        }
    }

    #[test]
    fn json_file_roots_reject_unsupported_or_contradictory_profiles() {
        for (path, short_path, owner, is_source) in [
            (
                "external/dep+/pkg/input",
                "../dep+/pkg/input",
                "@@dep+//pkg:input",
                true,
            ),
            (
                "bazel-out/k8-fastbuild/bin/external/dep+/pkg/out",
                "../dep+/pkg/out",
                "@@dep+//pkg:rule",
                false,
            ),
            ("pkg/input", "pkg/input", "@@dep+//pkg:input", true),
            ("pkg/input", "pkg/input", ":input", true),
            ("pkg/input", "pkg/input", "@//pkg:input", true),
            ("pkg/input", "pkg/input", "//pkg:../input", true),
            ("pkg/input", "pkg/other", "//pkg:other", true),
            ("pkg/input", "pkg/input", "//pkg:other", true),
            ("pkg/input", "pkg/input", "//elsewhere:input", true),
            ("pkg/../input", "pkg/../input", "//:input", true),
            ("/pkg/input", "/pkg/input", "//pkg:input", true),
            ("pkg//input", "pkg//input", "//pkg:input", true),
            ("pkg/./input", "pkg/./input", "//pkg:input", true),
            ("pkg/input/", "pkg/input/", "//pkg:input", true),
            ("pkg/in\\put", "pkg/in\\put", "//pkg:input", true),
            ("pkg/café", "pkg/café", "//pkg:input", true),
            ("", "", "//:input", true),
            (
                "bazel-out/k8-fastbuild/bin/pkg/out",
                "pkg/out",
                "//elsewhere:rule",
                false,
            ),
            (
                "bazel-out/k8-fastbuild/bin/pkg2/out",
                "pkg2/out",
                "//pkg:rule",
                false,
            ),
            ("bazel-out/k8-fastbuild/bin/pkg", "pkg", "//pkg:rule", false),
            (
                "bazel-out/k8-fastbuild/bin/pkg/out",
                "pkg/other",
                "//pkg:rule",
                false,
            ),
            (
                "bazel-out/k8-fastbuild/bin/pkg/out",
                "pkg/other",
                "//pkg:rule",
                false,
            ),
            ("bazel-out/../bin/pkg/out", "pkg/out", "//pkg:rule", false),
            ("bazel-out//bin/pkg/out", "pkg/out", "//pkg:rule", false),
            (
                "bazel-out/k8-fastbuild/bin2/pkg/out",
                "pkg/out",
                "//pkg:rule",
                false,
            ),
            (
                "bazel-out/k8-fastbuild/bin//pkg/out",
                "pkg/out",
                "//pkg:rule",
                false,
            ),
            (
                "bazel-out/k8-fastbuild/bin/pkg/out",
                "pkg/out",
                "//pkg:out",
                true,
            ),
            ("pkg/out", "pkg/out", "//pkg:rule", false),
        ] {
            let file = File {
                is_directory: false,
                path: path.into(),
                short_path: short_path.into(),
                owner: owner.into(),
                is_source,
            };
            let error = file_json_root(&file).unwrap_err().to_string();
            assert!(
                error.contains("unsupported File JSON profile"),
                "{file:?}: {error}"
            );
        }
    }

    #[test]
    fn depset_postorder_duplicates_and_strings() {
        pass("depset([a, a2, b]).to_list() == [a, b]");
        pass("depset([b], transitive=[depset([a]), depset([a2])]).to_list() == [a, b]");
        pass(
            "depset(['root'], order='postorder', transitive=[depset(['shared', 'left']), depset(['shared', 'right'])]).to_list() == ['shared', 'left', 'right', 'root']",
        );
        pass(
            "depset(('x', 'x', 'y')).to_list() == ['x', 'y'] and not depset() and bool(depset(['x']))",
        );
        pass("d = depset([a]); x = d.to_list(); x.append(b); d.to_list() == [a]");
        fail("depset([a, 'x'])", "same type");
        fail("depset([a], transitive=[depset(['x'])])", "same type");
        pass("depset([1, 2, 1]).to_list() == [1, 2]");
        fail("depset([[]])", "unsupported");
        fail("depset(transitive=[[a]])", "must be depsets");
        // A range is a Sequence (RangeList) for both depset parameters.
        pass("depset(range(3)).to_list() == [0, 1, 2] and depset(direct=range(0)).to_list() == []");
        pass("depset(range(2), transitive=[depset([5])]).to_list() == [5, 0, 1]");
        fail(
            "depset(transitive=range(1))",
            "depset transitive entries must be depsets",
        );
        // Depset.depset's `Sequence or NoneType` (Depset.java:572-627).
        fail(
            "depset(direct=depset([a]))",
            "in call to depset(), parameter 'direct' got value of type 'depset', want 'sequence or NoneType'",
        );
        fail("depset(order='invalid')", "unsupported");
        fail("depset().order", "order");
        fail("depset().to_list(1)", "argument");
    }

    #[test]
    fn external_labels_files_and_targets_retain_repository_when_frozen() {
        let frozen = Module::with_temp_heap(|module| {
            let external = File {
                is_directory: false,
                path: "external/dep+/pkg/input".into(),
                short_path: "../dep+/pkg/input".into(),
                owner: "@@dep+//pkg:rule".into(),
                is_source: true,
            };
            module.set("file", alloc_file(module.heap(), external.clone()));
            module.set(
                "target",
                alloc_target(module.heap(), "@@dep+//pkg:rule", vec![external]),
            );
            let ast = AstModule::parse(
                "test.star",
                r#"
labels = depset([Label("//pkg:rule"), Label("@@dep+//pkg:rule"), Label("@@dep+//pkg:rule")])
owner = file.owner
ok = (len(labels.to_list()) == 2 and owner == target.label and
      owner.workspace_name == "dep+" and owner.workspace_root == "external/dep+" and
      str(owner) == "@@dep+//pkg:rule" and owner != Label("//pkg:rule"))
"#
                .into(),
                &Dialect::Standard,
            )
            .unwrap();
            Evaluator::new(&module)
                .eval_module(ast, &globals())
                .unwrap();
            module.freeze().unwrap()
        });
        assert_eq!(frozen.get("ok").unwrap().value().unpack_bool(), Some(true));
        assert_eq!(
            unpack_label(frozen.get("owner").unwrap().value()),
            Some("@@dep+//pkg:rule".into())
        );
    }

    #[test]
    fn depset_diamond_orders_match_bazel_9_3() {
        // Reference: bazel-9.3.0 --batch query --keep_going //... with the
        // corresponding DAG in /tmp/opencode/depset-order-ref/defs.bzl.
        for (order, expected) in [
            (
                "default",
                "['leaf', 'shared', 'left', 'dup', 'right', 'root']",
            ),
            (
                "postorder",
                "['leaf', 'shared', 'left', 'dup', 'right', 'root']",
            ),
            (
                "preorder",
                "['root', 'dup', 'left', 'leaf', 'shared', 'right']",
            ),
            (
                "topological",
                "['root', 'left', 'right', 'dup', 'leaf', 'shared']",
            ),
        ] {
            pass(&format!(
                "shared = depset(['leaf', 'shared'], order='{order}')\nleft = depset(['left', 'dup'], transitive=[shared], order='{order}')\nright = depset(['right', 'dup'], transitive=[shared], order='{order}')\nroot = depset(['root', 'dup'], transitive=[left, right], order='{order}')\nroot.to_list() == {expected}"
            ));
        }
        // A previously flattened child would move the shared leaf before the
        // second parent and violate this topological invariant.
        pass(
            "leaf = depset(['leaf'], order='topological')\nleft = depset(['left'], transitive=[leaf], order='topological')\nright = depset(['right'], transitive=[leaf], order='topological')\ndepset(['root'], transitive=[left, right], order='topological').to_list() == ['root', 'left', 'right', 'leaf']",
        );
    }

    #[test]
    fn depset_mixed_orders_and_empty_compatibility_match_reference() {
        for (order, parent_expected, child_expected) in [
            (
                "default",
                "['leaf', 'child', 'root']",
                "['leaf', 'child', 'root']",
            ),
            (
                "postorder",
                "['leaf', 'child', 'root']",
                "['leaf', 'child', 'root']",
            ),
            (
                "preorder",
                "['child', 'leaf', 'root']",
                "['root', 'leaf', 'child']",
            ),
            (
                "topological",
                "['leaf', 'child', 'root']",
                "['root', 'child', 'leaf']",
            ),
        ] {
            pass(&format!(
                "child = depset(['child'], transitive=[depset(['leaf'])], order='{order}')\ndepset(['root'], transitive=[child]).to_list() == {parent_expected}"
            ));
            pass(&format!(
                "child = depset(['child'], transitive=[depset(['leaf'])])\ndepset(['root'], transitive=[child], order='{order}').to_list() == {child_expected}"
            ));
        }
        pass(
            "depset(['root'], transitive=[depset(['a'], order='preorder'), depset(['b'], order='postorder')]).to_list() == ['a', 'b', 'root']",
        );
        for parent in ["postorder", "preorder", "topological"] {
            for child in ["postorder", "preorder", "topological"] {
                if parent != child {
                    fail(
                        &format!(
                            "depset(['x'], order='{parent}', transitive=[depset(['y'], order='{child}')])"
                        ),
                        "incompatible",
                    );
                    pass(&format!(
                        "depset(['x'], order='{parent}', transitive=[depset(order='{child}')]).to_list() == ['x']"
                    ));
                    pass(&format!(
                        "not depset(order='{parent}', transitive=[depset(order='{child}')])"
                    ));
                }
            }
        }
    }

    #[test]
    fn depset_scalar_tuple_types_and_repr() {
        pass("depset([1, -2, 1]).to_list() == [1, -2]");
        pass("depset([True, False, True]).to_list() == [True, False]");
        pass(
            "depset([Label('//p:a'), Label('//p:b'), Label('//p:a')]).to_list() == [Label('//p:a'), Label('//p:b')]",
        );
        pass("depset([(1, 'x'), (1, 'x'), (2, 'y')]).to_list() == [(1, 'x'), (2, 'y')]");
        pass("depset([(a, (True, Label('//p:a')))]).to_list() == [(a, (True, Label('//p:a')))]");
        pass("depset([2147483647, -2147483648]).to_list() == [2147483647, -2147483648]");
        fail("depset([2147483648])", "32-bit");
        fail("depset([-2147483649])", "32-bit");
        fail("depset([999999999999999999999999999999])", "32-bit");
        fail("depset([True, 1])", "same type");
        fail("depset([1], transitive=[depset(['s'])])", "same type");
        fail("depset([Label('//p:a'), '//p:a'])", "same type");
        fail("depset([{}])", "unsupported");
        fail("depset([([1],)])", "unsupported");
        pass("repr(depset(['x'], order='preorder')) == 'depset([\"x\"], order = \"preorder\")'");
        pass(
            "[f.path for f in depset([a, b], order='topological').to_list()] == ['pkg/a.txt', 'pkg/b']",
        );
    }

    /// Bazel 9.3 repr/str of the owned analysis values (docs/REPR.md).
    #[test]
    fn bazel_repr_and_str_of_analysis_values() {
        // Label.repr/str (cmdline/Label.java:718-754).
        pass("repr(Label('//p:a')) == 'Label(\"//p:a\")'");
        pass("repr(Label('@@r+//p:a')) == 'Label(\"@@r+//p:a\")'");
        pass("str(Label('//p:a')) == '@@//p:a' and str(Label('@@r+//p:a')) == '@@r+//p:a'");
        pass("'%s|%r' % (Label('//p:a'), Label('//p:a')) == '@@//p:a|Label(\"//p:a\")'");
        pass("repr([Label('//p:a')]) == '[Label(\"//p:a\")]'");
        // Label.compareTo (cmdline/Label.java:677-685): repository, package, name.
        pass(
            "sorted([Label('//p:b'), Label('@@r+//a:a'), Label('//p:a'), Label('//o:z')]) == [Label('//o:z'), Label('//p:a'), Label('//p:b'), Label('@@r+//a:a')]",
        );
        pass("Label('//p:a') < Label('//p:b') and not (Label('//q:a') < Label('//p:b'))");
        // Artifact.repr (actions/Artifact.java:1256-1262).
        pass("repr(a) == '<source file pkg/a.txt>' and str(a) == repr(a)");
        // Depset.repr (collect/nestedset/Depset.java:319-328).
        pass("repr(depset([a])) == 'depset([<source file pkg/a.txt>])'");
        pass(
            "repr(depset([Label('//p:a'), Label('@@r+//p:b')])) == 'depset([Label(\"//p:a\"), Label(\"@@r+//p:b\")])'",
        );
        pass(
            "repr(depset([(1, 'x')], order='postorder')) == 'depset([(1, \"x\")], order = \"postorder\")'",
        );
        pass("str(depset([])) == 'depset([])'");
        // RuleConfiguredTarget.repr; BuiltinProvider.repr; NativeInfo.repr.
        pass("repr(target) == '<target //pkg:rule>' and str(target) == repr(target)");
        pass("repr(DefaultInfo) == '<function DefaultInfo>'");
        pass(
            "repr(DefaultInfo(files=depset([a]))) == 'struct(data_runfiles = None, default_runfiles = None, files = depset([<source file pkg/a.txt>]), files_to_run = None)'",
        );
        pass(
            "repr(DefaultInfo()) == 'struct(data_runfiles = None, default_runfiles = None, files = None, files_to_run = None)'",
        );
        pass(
            "repr(target[DefaultInfo]) == 'struct(data_runfiles = <unknown object com.google.devtools.build.lib.analysis.Runfiles>, default_runfiles = <unknown object com.google.devtools.build.lib.analysis.Runfiles>, files = depset([<source file pkg/a.txt>]), files_to_run = <unknown object com.google.devtools.build.lib.analysis.FilesToRunProvider>)'",
        );
        // BuiltinFunction.repr of a top-level builtin and of a bound method.
        pass(
            "repr(depset) == '<built-in function depset>' and repr(len) == '<built-in function len>'",
        );
        pass("repr(depset([]).to_list) == '<built-in method to_list of depset value>'");
        pass("repr('x'.upper) == '<built-in method upper of string value>'");
        pass("type(len) == 'builtin_function_or_method' and type('x'.upper) == type(len)");
        pass("type(DefaultInfo) == 'Provider'");
        // Bazel 9.3: `unhashable type: 'builtin_function_or_method'` (N22 capture).
        fail("{len: 1}", "unhashable type: 'builtin_function_or_method'");
        // fail() formats with debugPrint (MethodLibrary.java:895-913; Label.java:725-737);
        // without a main repository mapping external labels stay canonical.
        fail(
            "fail('x', Label('//p:a'), Label('//p:p'), Label('@@r+//:r+'), Label('@@r+//q:z'), [Label('//p:a')], 1)",
            "x //p:a //p @@r+ @@r+//q:z [Label(\"//p:a\")] 1",
        );
    }

    #[test]
    fn file_and_target_reprs_follow_their_configured_class() {
        let generated = File {
            is_directory: false,
            path: "bazel-out/k8-fastbuild/bin/pkg/out.txt".into(),
            short_path: "pkg/out.txt".into(),
            owner: "//pkg:rule".into(),
            is_source: false,
        };
        assert_eq!(generated.to_string(), "<generated file pkg/out.txt>");
        let external_source = File {
            is_directory: false,
            path: "external/dep+/pkg/in.txt".into(),
            short_path: "../dep+/pkg/in.txt".into(),
            owner: "@@dep+//pkg:in.txt".into(),
            is_source: true,
        };
        assert_eq!(external_source.to_string(), "<source file pkg/in.txt>");
        let external_generated = File {
            is_directory: false,
            path: "bazel-out/k8-fastbuild/bin/external/dep+/pkg/out.txt".into(),
            short_path: "../dep+/pkg/out.txt".into(),
            owner: "@@dep+//pkg:rule".into(),
            is_source: false,
        };
        assert_eq!(
            external_generated.to_string(),
            "<generated file external/dep+/pkg/out.txt>"
        );
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let repr = |label: &str, kind: TargetKind| {
                alloc_target_of_kind(
                    heap,
                    label,
                    vec![],
                    BTreeMap::new(),
                    kind,
                    vec![],
                    Default::default(),
                )
                .to_repr()
            };
            assert_eq!(repr("//p:r", TargetKind::Rule), "<target //p:r>");
            assert_eq!(
                repr("@@dep+//p:r", TargetKind::Rule),
                "<target @@dep+//p:r>"
            );
            assert_eq!(
                repr("//p:f.txt", TargetKind::InputFile),
                "<input file target //p:f.txt>"
            );
            assert_eq!(
                repr("//p:out", TargetKind::OutputFile),
                "<output file target //p:out>"
            );
            assert_eq!(
                repr("//p:group", TargetKind::Unknown),
                "<unknown target //p:group>"
            );
            assert_eq!(
                repr(
                    "//p:actual",
                    TargetKind::Alias("//p:alias".into(), Box::new(TargetKind::Rule))
                ),
                "<alias target //p:alias of //p:actual>"
            );
            let target = alloc_target_of_kind(
                heap,
                "//p:r",
                vec![],
                BTreeMap::new(),
                TargetKind::Rule,
                vec![],
                Default::default(),
            );
            assert_eq!(target.to_str(), target.to_repr());
        });
    }

    #[test]
    fn depset_shared_nodes_typed_empty_and_native_helpers() {
        let shared = Depset::compose(
            vec![Item::String("leaf".to_owned())],
            vec![],
            Order::Topological,
            None,
        )
        .unwrap();
        let left = Depset::compose(
            vec![Item::String("left".to_owned())],
            vec![shared.node.clone()],
            Order::Topological,
            None,
        )
        .unwrap();
        let right = Depset::compose(
            vec![Item::String("right".to_owned())],
            vec![shared.node.clone()],
            Order::Topological,
            None,
        )
        .unwrap();
        assert!(Arc::ptr_eq(&left.node.children[0], &right.node.children[0]));
        assert_eq!(left.node.direct.len(), 1);
        Module::with_temp_heap(|module| {
            let empty = alloc_depset(module.heap(), vec![]);
            module.set("empty", empty);
            assert_eq!(unpack_depset(empty).unwrap(), Vec::<File>::new());
            let ast = AstModule::parse(
                "typed.star",
                "depset(['string'], transitive=[empty])".to_owned(),
                &Dialect::Standard,
            )
            .unwrap();
            assert!(
                Evaluator::new(&module)
                    .eval_module(ast, &globals())
                    .unwrap_err()
                    .to_string()
                    .contains("same type")
            );
            module.set("a", alloc_file(module.heap(), file("pkg/a.txt")));
            module.set("b", alloc_file(module.heap(), file("pkg/b")));
            for order in ["postorder", "preorder", "topological"] {
                let ast = AstModule::parse(
                    "files.star",
                    format!(
                        "depset([a], order='{order}', transitive=[depset([b], order='{order}')])"
                    ),
                    &Dialect::Standard,
                )
                .unwrap();
                let value = Evaluator::new(&module)
                    .eval_module(ast, &globals())
                    .unwrap();
                let expected = if order == "postorder" {
                    vec![file("pkg/b"), file("pkg/a.txt")]
                } else {
                    vec![file("pkg/a.txt"), file("pkg/b")]
                };
                assert_eq!(unpack_depset(value).unwrap(), expected);
                assert_eq!(file_list(value).unwrap(), expected);
                assert_eq!(contained_depset_files(value).unwrap(), expected);
            }
            let ast = AstModule::parse(
                "tuple.star",
                "depset([(a, (b, a))])".to_owned(),
                &Dialect::Standard,
            )
            .unwrap();
            let value = Evaluator::new(&module)
                .eval_module(ast, &globals())
                .unwrap();
            assert!(unpack_depset(value).is_err());
            assert_eq!(
                contained_depset_files(value).unwrap(),
                vec![file("pkg/a.txt"), file("pkg/b")]
            );
        });
    }

    #[test]
    fn depset_iterative_deep_dag_and_explicit_limits() {
        pass(
            "def graph():\n    d = depset([0], order='topological')\n    for i in range(1, 600):\n        d = depset([i], order='topological', transitive=[d, d])\n    return d.to_list() == list(reversed(range(600)))\ngraph()",
        );
        fail(
            "def graph():\n    d = depset([0])\n    for i in range(1100):\n        d = depset([i], transitive=[d])\n    return d\ngraph()",
            "DAG depth limit",
        );
    }

    #[test]
    fn target_and_default_info_provider() {
        pass("DefaultInfo in target");
        for key in ["'DefaultInfo'", "DefaultInfo()", "len", "None", "42"] {
            for operator in ["in", "not in"] {
                fail(
                    &format!("{key} {operator} target"),
                    "Target membership requires an exported provider constructor",
                );
            }
        }
        pass("target[DefaultInfo].files.to_list() == [a] and target.files.to_list() == [a]");
        pass("target.label == Label('//pkg:rule') and type(target) == 'Target'");
        // DefaultDefaultInfo fields are None unless given (DefaultInfo.java:84-145).
        pass(
            "DefaultInfo().files == None and DefaultInfo(files=depset([a])).files.to_list() == [a]",
        );
        pass(
            "[DefaultInfo().files_to_run, DefaultInfo().data_runfiles, DefaultInfo().default_runfiles] == [None, None, None]",
        );
        pass(
            "dir(DefaultInfo()) == ['data_runfiles', 'default_runfiles', 'files', 'files_to_run']",
        );
        pass(
            "repr(DefaultInfo()) == 'struct(data_runfiles = None, default_runfiles = None, files = None, files_to_run = None)'",
        );
        // The element type is only checked when the rule returns it.
        pass("type(DefaultInfo(files=depset(['x']))) == 'DefaultInfo'");
        fail("target['DefaultInfo']", "constructor");
        fail("target[DefaultInfo()]", "constructor");
        fail(
            "DefaultInfo(files=[a])",
            "in call to DefaultInfo(), parameter 'files' got value of type 'list', want 'depset or NoneType'",
        );
        fail(
            "DefaultInfo(runfiles=[]) ",
            "in call to DefaultInfo(), parameter 'runfiles' got value of type 'list', want 'runfiles or NoneType'",
        );
        fail(
            "DefaultInfo(executable='x')",
            "in call to DefaultInfo(), parameter 'executable' got value of type 'string', want 'File or NoneType'",
        );
        fail(
            "DefaultInfo(unknown=1)",
            "DefaultInfo() got unexpected keyword argument 'unknown'",
        );
        pass("DefaultInfo(executable=a).files == None");
        // The dependency view (DelegatingDefaultInfo).
        pass(
            "target[DefaultInfo].default_runfiles.files.to_list() == [] and target[DefaultInfo].data_runfiles == target[DefaultInfo].default_runfiles",
        );
        pass(
            "repr(target[DefaultInfo].files_to_run) == '<unknown object com.google.devtools.build.lib.analysis.FilesToRunProvider>' and target[DefaultInfo].files_to_run.executable == None",
        );
        fail("DefaultInfo(depset([a]))", "positional");
        fail("DefaultInfo().unknown", "unknown");
        fail("target.unknown", "unknown");
    }

    #[test]
    fn native_unpackers_and_default_output_distinction() {
        Module::with_temp_heap(|module| {
            module.set("a", alloc_file(module.heap(), file("pkg/a.txt")));
            for (source, expected) in [
                ("None", None),
                ("[]", None),
                ("()", None),
                ("DefaultInfo()", None),
                ("DefaultInfo(files=depset())", Some(vec![])),
                (
                    "DefaultInfo(files=depset([a]))",
                    Some(vec![file("pkg/a.txt")]),
                ),
                ("[DefaultInfo()]", None),
                ("[DefaultInfo(files=None)]", None),
                ("[DefaultInfo(files=depset())]", Some(vec![])),
                (
                    "(DefaultInfo(files=depset([a])),)",
                    Some(vec![file("pkg/a.txt")]),
                ),
            ] {
                let ast =
                    AstModule::parse("test.star", source.to_owned(), &Dialect::Standard).unwrap();
                let value = Evaluator::new(&module)
                    .eval_module(ast, &globals())
                    .unwrap();
                assert_eq!(default_files(value).unwrap(), expected, "{source}");
            }
            for source in ["[DefaultInfo(), DefaultInfo()]", "[42]", "[DefaultInfo]"] {
                let ast =
                    AstModule::parse("test.star", source.to_owned(), &Dialect::Standard).unwrap();
                let value = Evaluator::new(&module)
                    .eval_module(ast, &globals())
                    .unwrap();
                assert!(default_files(value).is_err(), "{source}");
            }
            for source in ["[a]", "(a,)", "depset([a])"] {
                let ast =
                    AstModule::parse("test.star", source.to_owned(), &Dialect::Standard).unwrap();
                let value = Evaluator::new(&module)
                    .eval_module(ast, &globals())
                    .unwrap();
                assert_eq!(file_list(value).unwrap(), vec![file("pkg/a.txt")]);
            }
            assert!(unpack_file(module.heap().alloc("x")).is_err());
            assert!(file_list(module.heap().alloc("x")).is_err());
        });
    }

    #[test]
    fn owned_values_survive_module_freeze() {
        let frozen = Module::with_temp_heap(|module| {
            module.set("a", alloc_file(module.heap(), file("pkg/a.txt")));
            module.set(
                "target",
                alloc_target(module.heap(), "//pkg:rule", vec![file("pkg/a.txt")]),
            );
            let ast = AstModule::parse(
                "freeze.star",
                "d = depset([a]); info = DefaultInfo(files=d); strings = depset(['x'])".to_owned(),
                &Dialect::Standard,
            )
            .unwrap();
            Evaluator::new(&module)
                .eval_module(ast, &globals())
                .unwrap();
            module.freeze().unwrap()
        });
        Module::with_temp_heap(|module| {
            module.import_public_symbols(&frozen);
            let ast = AstModule::parse("use.star", "d.to_list() == info.files.to_list() and target[DefaultInfo].files.to_list() == d.to_list() and strings.to_list() == ['x']".to_owned(), &Dialect::Standard).unwrap();
            let value = Evaluator::new(&module)
                .eval_module(ast, &globals())
                .unwrap();
            assert_eq!(value.to_repr(), "True");
        });
    }
}
