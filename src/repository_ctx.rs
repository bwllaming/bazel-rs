//! Native `repository_ctx` (Bazel 9.3 StarlarkRepositoryContext.java and
//! StarlarkBaseExternalContext.java). Paths follow Bazel's lexical
//! `Path.getRelative` model: strings resolve against the repository directory
//! (the caller's private stage), Labels resolve to source files, and `path`
//! values pass through. Writes must stay inside the repository directory
//! (`checkInOutputDirectory`); the native profile additionally refuses to
//! traverse symlinked parents while writing and requires output names the
//! content-addressed store can represent. Reads, `which` and `execute` act on
//! the host like Bazel's: repository rules are trusted code, and this is not
//! an OS sandbox. The caller owns the stage and publication/cleanup and must
//! exclude concurrent hostile mutation of that stage.
#![allow(clippy::too_many_arguments)]

use allocative::Allocative;
use anyhow::{Context as _, anyhow, bail, ensure};
use serde::Serialize;
use starlark::environment::{Methods, MethodsBuilder, Module};
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::{TupleRef, UnpackTuple};
use starlark::values::{Heap, OwnedFrozenValue, Value, ValueLike};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::custom_rule::AttrValue;
use crate::extension_download::{self as transport, Algorithm, Checksum, TypedIoError};
use crate::host_os::latin1_text;
use crate::label::{Label, PackageId};
use crate::repository::RepositorySet;
use crate::repository_environment::{Captured, EnvError, Watch, WatchedEnvironment};
use crate::repository_rule::RuleDefinition;
use crate::starlark_types::{alloc_label, unpack_label};

// Repository I/O has no artificial size or count caps (Bazel has none):
// downloads and archives stream to disk in constant memory; only APIs whose
// result is a Starlark string (read, template input, execute output) hold
// content in memory, as Bazel does.
static NEXT_EXEC_RESULT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Debug, Default, Serialize)]
pub struct ExecutionInputs {
    /// Ordered reads, including repeated reads of a subsequently changed file.
    pub templates: Vec<TemplateInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watched_env: Option<WatchedEnvironment>,
    /// Bazel RepoRecordedInput.File/Dirents observations outside the
    /// repository directory (StarlarkBaseExternalContext.java:1656-1755).
    /// Omitted when empty so pre-existing repository identities are unchanged.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileInput>,
    /// Non-hermetic facilities that Bazel never records as inputs (execute,
    /// which, downloads). Values are deliberately absent: a publication is
    /// reused without re-running them when the specification and every
    /// recorded input are unchanged (repository_store.rs, N61), like Bazel's
    /// fetch-once `external/<name>` directories.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub host_facilities: BTreeSet<&'static str>,
    /// The canonical workspace root, when a non-local repository rule's
    /// result depends on where the workspace is (crate::worktree,
    /// docs/WORKTREES.md): it read `workspace_root`, wrote a file naming the
    /// workspace's path or left a link into the workspace. Part of the
    /// identity and recorded as a probe (crate::repository_warm), so another
    /// worktree sharing the cache executes the rule itself. Omitted
    /// otherwise, so pre-existing repository identities are unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Module extensions only: the RepoRecordedInput.WithValue observations
    /// in recording order (StarlarkBaseExternalContext.recordInputWithValue,
    /// java:255-280), for MODULE.bazel.lock. Never part of a provenance.
    #[serde(skip)]
    pub(crate) bazel: Vec<crate::lockfile_extensions::RecordedInput>,
    /// Why an observation has no Bazel lockfile representation, if any.
    #[serde(skip)]
    pub bazel_unsupported: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TemplateInput {
    /// A canonical label for label templates, `stage:<relative>` inside the
    /// repository directory, otherwise the RepoCacheFriendlyPath (see
    /// [`FileInput::path`]).
    pub source: String,
    pub bytes: Vec<u8>,
    /// `source` is a canonical label (never part of an identity).
    #[serde(skip)]
    pub label: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct FileInput {
    /// A canonical label for label reads, otherwise the path's
    /// RepoCacheFriendlyPath (RepoRecordedInput.java:325-405) as
    /// toRepoCacheFriendlyPath forms it: `@@//relative` in the main
    /// workspace, `@@NAME//relative` in external repository NAME (natively:
    /// under its materialized root, RepositorySet::friendly_repository), the
    /// absolute host path elsewhere — never a cache-location-dependent path
    /// into the repository store (review i20).
    pub path: String,
    /// `file` or `dirents` (RepoRecordedInput.File / Dirents).
    pub kind: &'static str,
    /// `missing`, `directory`, `other`, `file:<sha256>` or `dirents:<sha256>`.
    pub state: String,
    /// `path` is a canonical label (never part of an identity).
    #[serde(skip)]
    pub label: bool,
}

#[derive(Default)]
pub(crate) struct State {
    inputs: ExecutionInputs,
    /// Whether Bazel would have created the repository directory yet: it does
    /// so lazily on the first write/makeDirectories/execute, not before the
    /// callback (captured read('.') and template('.') failures).
    created: bool,
    client: Option<reqwest::blocking::Client>,
    /// A module extension's lock on its working directory, held from its
    /// creation until it is deleted (Bazel: the output base lock).
    lock: Option<fs::File>,
    /// The repositories a label of this execution resolved into
    /// ([`Context::label_path`]): the self-referencing ones among them are
    /// held as a reader around each `execute` (crate::repository_slots).
    resolved: BTreeSet<String>,
    /// The repositories of each label-list attribute of a repository rule
    /// (label_list, label-keyed and label-valued dicts), fetched ahead
    /// together once the rule resolves a label of one of them
    /// ([`Context::label_path`]); emptied then.
    label_siblings: Vec<BTreeSet<String>>,
}

/// The repositories each multi-label attribute of `attrs` names (its
/// canonical `@@repo//...` labels), for [`State::label_siblings`].
fn label_siblings(attrs: &BTreeMap<String, AttrValue>) -> Vec<BTreeSet<String>> {
    let repository = |label: &str| -> Option<String> {
        let rest = label.strip_prefix("@@")?;
        let (name, _) = rest.split_once("//")?;
        (!name.is_empty()).then(|| name.to_owned())
    };
    attrs
        .values()
        .filter_map(|value| {
            let labels: Vec<&str> = match value {
                AttrValue::Labels(labels) => labels.iter().map(String::as_str).collect(),
                AttrValue::LabelKeyedStringDict(entries) => {
                    entries.iter().map(|(label, _)| label.as_str()).collect()
                }
                AttrValue::StringKeyedLabelDict(entries) => {
                    entries.iter().map(|(_, label)| label.as_str()).collect()
                }
                AttrValue::LabelListDict(entries) => entries
                    .iter()
                    .flat_map(|(_, labels)| labels.iter().map(String::as_str))
                    .collect(),
                _ => return None,
            };
            let repositories: BTreeSet<String> =
                labels.into_iter().filter_map(repository).collect();
            (repositories.len() > 1).then_some(repositories)
        })
        .collect()
}

/// Which StarlarkBaseExternalContext this is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// StarlarkRepositoryContext: `allowWatchingPathsOutsideWorkspace = true`.
    Repository,
    /// ModuleExtensionContext (ModuleExtensionContext.java:72-86):
    /// `allowWatchingPathsOutsideWorkspace = false`, a temporary working
    /// directory that is deleted afterwards, no repository output store.
    ModuleExtension,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShouldWatch {
    Yes,
    No,
    Auto,
}

impl ShouldWatch {
    /// StarlarkBaseExternalContext.java:1709-1718.
    fn parse(text: &str) -> anyhow::Result<Self> {
        Ok(match text {
            "yes" => Self::Yes,
            "no" => Self::No,
            "auto" => Self::Auto,
            _ => bail!("bad value for 'watch' parameter; want 'yes', 'no', or 'auto', got {text}"),
        })
    }
}

// Marker/trace/freeze derives generate unsafe implementations. Runtime host
// state owns all its data; the Starlark attrs field is always traced/frozen.
#[allow(unsafe_code)]
mod values {
    use super::*;
    use starlark::collections::StarlarkHasher;
    use starlark::starlark_complex_value;
    use starlark::values::{
        Coerce, Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
        Trace, starlark_value,
    };
    use std::hash::Hash;

    #[derive(ProvidesStaticType)]
    pub(crate) struct Context {
        pub repositories: Arc<RepositorySet>,
        /// repository_ctx or module_ctx (StarlarkBaseExternalContext subclass).
        pub kind: Kind,
        /// The working directory: the repository directory (a private stage)
        /// or the module extension's `<cache>/modextwd/<unique name>`.
        pub stage: PathBuf,
        pub workspace: PathBuf,
        pub label: Label,
        /// Canonical repository name, or the module extension id
        /// (identifyingStringForLogging, see `identifying`).
        pub canonical: String,
        pub state: RefCell<State>,
        pub environment: RefCell<Watch>,
        /// A repository rule's printed output in order — print(), the
        /// `execute(quiet=False)` echo and the reproducibility DEBUG lines —
        /// collected for its replay record while it is printed as it
        /// happens ([`execute`]; only while the record mode's replay
        /// capture records local executions), or only collected by a replay
        /// ([`execute_captured`]). Module extensions print and collect
        /// nothing here.
        pub output: RefCell<Vec<u8>>,
        /// Print what the execution prints as it happens (false: a replay,
        /// which never prints).
        pub live: bool,
        /// Set when the execution reads `repository_ctx.os.environ`.
        pub environ_read: Arc<std::sync::atomic::AtomicBool>,
        /// Set when the execution depends on the workspace's path
        /// (crate::worktree, docs/WORKTREES.md): it read `workspace_root`
        /// or wrote a file naming the workspace; with links into the
        /// workspace (`replant_symlinks`) a non-local rule's publication
        /// records the workspace ([`ExecutionInputs::workspace`]).
        pub workspace_observed: Arc<std::sync::atomic::AtomicBool>,
        /// A repository rule's mentions of the workspace's path — paths
        /// into the cache excepted, which every worktree sharing it sees
        /// alike (a main checkout's `bazel-rs-out`); None for a module
        /// extension, whose results the lockfile keeps instead.
        pub mentions: Option<crate::worktree::WorkspaceMentions>,
    }

    #[derive(
        Debug,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        Trace,
        Freeze,
        Coerce,
        StarlarkPagablePanic,
    )]
    #[repr(C)]
    pub(super) struct RepositoryCtxGen<V> {
        pub attrs: V,
        pub os: V,
        pub name: String,
        pub original_name: String,
        /// Absolute main workspace root as Latin-1 text (`workspace_root`).
        pub workspace_root: String,
        /// The execution's [`WorkspaceObservation`], set by a
        /// `workspace_root` read (None in unit tests).
        pub observed: V,
    }

    impl<V> fmt::Display for RepositoryCtxGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // StarlarkRepositoryContext has no repr override (bazel/repository/
            // starlark/StarlarkRepositoryContext.java:73; StarlarkValue.java:50-52):
            // Starlark's default for the Java class (captured repr/str of ctx).
            f.write_str(
                "<unknown object com.google.devtools.build.lib.bazel.repository.starlark.StarlarkRepositoryContext>",
            )
        }
    }

    starlark_complex_value!(pub(super) RepositoryCtx);

    #[starlark_value(type = "repository_ctx")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for RepositoryCtxGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn get_methods() -> Option<&'static Methods> {
            Some(REPOSITORY_METHODS.methods())
        }

        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            match name {
                "attr" => Some(self.attrs.to_value()),
                "os" => Some(self.os.to_value()),
                "name" => Some(heap.alloc(self.name.as_str())),
                "original_name" => Some(heap.alloc(self.original_name.as_str())),
                "workspace_root" => {
                    if let Some(observed) = self
                        .observed
                        .to_value()
                        .downcast_ref::<WorkspaceObservation>()
                    {
                        observed
                            .flag
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    Some(
                        heap.alloc(RepositoryPath {
                            bytes: crate::repository_rule::latin1_bytes(&self.workspace_root)
                                .unwrap_or_default(),
                        }),
                    )
                }
                _ => None,
            }
        }

        /// `hasattr` reads nothing (no workspace observation).
        fn has_attr(&self, attribute: &str, _heap: Heap<'v>) -> bool {
            REPOSITORY_ATTRIBUTES.contains(&attribute)
        }

        fn dir_attr(&self) -> Vec<String> {
            REPOSITORY_ATTRIBUTES.map(str::to_owned).to_vec()
        }
    }

    const REPOSITORY_ATTRIBUTES: [&str; 5] =
        ["attr", "name", "original_name", "os", "workspace_root"];

    /// One repository rule execution's workspace-path flag
    /// ([`Context::workspace_observed`]), reachable from its repository_ctx
    /// only; never part of an identity.
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct WorkspaceObservation {
        #[allocative(skip)]
        pub flag: Arc<std::sync::atomic::AtomicBool>,
    }
    starlark::starlark_simple_value!(WorkspaceObservation);

    impl fmt::Display for WorkspaceObservation {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<workspace observation>")
        }
    }

    #[starlark_value(type = "workspace_observation")]
    impl<'v> StarlarkValue<'v> for WorkspaceObservation {}

    /// Bazel `path` (StarlarkPath.java): an absolute, lexically normalized
    /// host path. Immutable, hashable, equal by path. str() is the path,
    /// repr() its quoted form. Bytes map to Latin-1 characters, like Bazel's
    /// internal strings.
    #[derive(
        Debug,
        Clone,
        PartialEq,
        Eq,
        Hash,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        StarlarkPagablePanic,
    )]
    pub(super) struct RepositoryPath {
        pub bytes: Vec<u8>,
    }
    starlark::starlark_simple_value!(RepositoryPath);

    impl fmt::Display for RepositoryPath {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&starlark::values::string::StarlarkStr::repr(&latin1_text(
                &self.bytes,
            )))
        }
    }

    #[starlark_value(type = "path")]
    impl<'v> StarlarkValue<'v> for RepositoryPath {
        fn get_methods() -> Option<&'static Methods> {
            Some(PATH_METHODS.methods())
        }

        fn collect_str(&self, collector: &mut String) {
            collector.push_str(&latin1_text(&self.bytes));
        }

        fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
            Ok(other
                .downcast_ref::<Self>()
                .is_some_and(|other| other.bytes == self.bytes))
        }

        fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
            self.bytes.hash(hasher);
            Ok(())
        }
    }

    /// StarlarkBaseExternalContext.PendingDownload (java:585-656): the token
    /// of `download(block = False)`. Its only member is `wait()`; repr is
    /// `<pending download to 'PATH'>`; it has no @StarlarkBuiltin, so its
    /// type() is the Java simple class name. Mutable (not hashable).
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct PendingDownload {
        pub output: Vec<u8>,
        #[allocative(skip)]
        pub outcome: super::PendingOutcome,
    }
    starlark::starlark_simple_value!(PendingDownload);

    impl fmt::Display for PendingDownload {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "<pending download to '{}'>", latin1_text(&self.output))
        }
    }

    #[starlark_value(type = "PendingDownload")]
    impl<'v> StarlarkValue<'v> for PendingDownload {
        fn get_methods() -> Option<&'static Methods> {
            Some(PENDING_DOWNLOAD_METHODS.methods())
        }
    }

    /// Bazel `exec_result` (StarlarkExecutionResult.java:49-102).
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct ExecResult {
        pub return_code: i64,
        pub stdout: String,
        pub stderr: String,
        /// Identity: StarlarkExecutionResult is immutable (hashable) and keeps
        /// Object.equals/hashCode (StarlarkExecutionResult.java:70-73).
        pub id: u64,
    }
    starlark::starlark_simple_value!(ExecResult);

    impl fmt::Display for ExecResult {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("<unknown object com.google.devtools.build.lib.bazel.repository.starlark.StarlarkExecutionResult>")
        }
    }

    #[starlark_value(type = "exec_result")]
    impl<'v> StarlarkValue<'v> for ExecResult {
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            match name {
                "return_code" => Some(heap.alloc(self.return_code)),
                "stdout" => Some(heap.alloc(self.stdout.as_str())),
                "stderr" => Some(heap.alloc(self.stderr.as_str())),
                _ => None,
            }
        }

        fn dir_attr(&self) -> Vec<String> {
            ["return_code", "stderr", "stdout"]
                .map(str::to_owned)
                .to_vec()
        }

        fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
            Ok(other
                .downcast_ref::<Self>()
                .is_some_and(|other| other.id == self.id))
        }

        fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
            self.id.hash(hasher);
            Ok(())
        }
    }

    /// Bazel `RepoDefinition` (`ctx.attr`, StarlarkRepositoryContext.java:
    /// 162-172): a Structure of the attribute values whose type() is
    /// "RepoDefinition".
    #[derive(
        Debug,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        Trace,
        Freeze,
        Coerce,
        StarlarkPagablePanic,
    )]
    #[repr(C)]
    pub(super) struct RepoDefinitionGen<V> {
        pub fields: V,
    }

    impl<'v, V: ValueLike<'v>> fmt::Display for RepoDefinitionGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt::Display::fmt(&self.fields.to_value(), f)
        }
    }

    starlark_complex_value!(pub(super) RepoDefinition);

    /// Bazel `repo_metadata` (bazel/repository/starlark/RepoMetadata.java:
    /// 23-44): the opaque record built by `repository_ctx.repo_metadata`
    /// (StarlarkRepositoryContext.java:611-656). No fields or methods; repr
    /// is StarlarkValue's default (StarlarkValue.java:50-52); Java record
    /// equality compares the reproducibility and the dict contents
    /// (Dict.java:208-211); not hashable (mutable, StarlarkValue.java:92-94).
    #[derive(
        Debug,
        ProvidesStaticType,
        NoSerialize,
        Allocative,
        Trace,
        Freeze,
        Coerce,
        StarlarkPagablePanic,
    )]
    #[repr(C)]
    pub(super) struct RepoMetadataGen<V> {
        pub reproducible: bool,
        pub attrs: V,
    }

    impl<V> fmt::Display for RepoMetadataGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(
                "<unknown object com.google.devtools.build.lib.bazel.repository.starlark.RepoMetadata>",
            )
        }
    }

    starlark_complex_value!(pub(super) RepoMetadata);

    #[starlark_value(type = "repo_metadata")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for RepoMetadataGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
            let Some(other) = RepoMetadata::from_value(other) else {
                return Ok(false);
            };
            if other.reproducible != self.reproducible {
                return Ok(false);
            }
            self.attrs.to_value().equals(other.attrs.to_value())
        }
    }

    #[starlark_value(type = "RepoDefinition")]
    impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for RepoDefinitionGen<V>
    where
        Self: ProvidesStaticType<'v>,
    {
        fn get_attr(&self, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
            self.fields.to_value().get_attr(name, heap).ok().flatten()
        }

        fn has_attr(&self, name: &str, heap: Heap<'v>) -> bool {
            self.fields.to_value().has_attr(name, heap)
        }

        fn dir_attr(&self) -> Vec<String> {
            self.fields.to_value().dir_attr()
        }
    }
}
pub(crate) use values::{Context, PendingDownload};
use values::{
    ExecResult, RepoDefinitionGen, RepoMetadata, RepoMetadataGen, RepositoryCtxGen, RepositoryPath,
    WorkspaceObservation,
};

/// A `download(block = False)`'s result: known at the call, or committed
/// by pending_downloads.
#[derive(Debug)]
pub(crate) enum PendingOutcome {
    Ready(Result<DownloadOutcome, String>),
    Queued(Arc<pending_downloads::Slot>),
}

mod pending_downloads;

/// Commit this thread's pending non-blocking downloads (pending_downloads):
/// before a callback observes anything they could change, and when it ends.
pub(crate) fn flush_pending_downloads() {
    pending_downloads::flush();
}

/// Non-blocking downloads transferred on the pending-download pool so far.
pub(crate) fn overlapped_downloads() -> u64 {
    pending_downloads::submitted()
}

/// The repository-rule callback's own context (not a module extension's).
fn own_context<'a>(eval: &'a Evaluator<'_, '_, '_>) -> Option<&'a Context> {
    eval.extra.and_then(|extra| extra.downcast_ref::<Context>())
}

/// The StarlarkBaseExternalContext of the running callback: a repository
/// rule's, or the one embedded in a module extension's context.
fn context<'a>(eval: &'a Evaluator<'_, '_, '_>) -> anyhow::Result<&'a Context> {
    // Every context method observes what the sequential engine shows once
    // the earlier non-blocking downloads finished.
    pending_downloads::flush();
    context_unflushed(eval)
}

/// [`context`] without committing pending downloads (a non-blocking
/// download decides itself which pending ones it could observe).
fn context_unflushed<'a>(eval: &'a Evaluator<'_, '_, '_>) -> anyhow::Result<&'a Context> {
    own_context(eval)
        .or_else(|| crate::extension_ctx::base_context(eval))
        .ok_or_else(|| anyhow!("repository_ctx is only available during repository execution"))
}

/// True while evaluating a repository-rule callback (byte-string profile).
pub(crate) fn evaluating_callback(eval: &Evaluator<'_, '_, '_>) -> bool {
    own_context(eval).is_some()
}

pub(crate) fn repository_context(eval: &Evaluator<'_, '_, '_>) -> Option<Arc<RepositorySet>> {
    own_context(eval).map(|context| context.repositories.clone())
}

pub(crate) fn definition_package(eval: &Evaluator<'_, '_, '_>) -> Option<PackageId> {
    let context = own_context(eval)?;
    Some(
        eval.call_stack_top_location()
            .and_then(|location| Label::parse_canonical(location.filename()).ok())
            .unwrap_or_else(|| context.label.clone())
            .package_id(),
    )
}

/// Bazel PathFragment normalization of `base.getRelative(relative)`: an
/// absolute `relative` replaces `base`; `.`/empty segments vanish; `..`
/// removes one segment and never climbs above the root.
fn normalize(base: &[u8], relative: &[u8]) -> Vec<u8> {
    let mut segments: Vec<&[u8]> = Vec::new();
    let start: &[&[u8]] = if relative.first() == Some(&b'/') {
        &[]
    } else {
        &[base]
    };
    for part in start.iter().chain(std::iter::once(&relative)) {
        for segment in part.split(|byte| *byte == b'/') {
            match segment {
                b"" | b"." => {}
                b".." => {
                    segments.pop();
                }
                segment => segments.push(segment),
            }
        }
    }
    let mut out = Vec::new();
    for segment in segments {
        out.push(b'/');
        out.extend_from_slice(segment);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

fn path_buf(bytes: &[u8]) -> PathBuf {
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

fn display(bytes: &[u8]) -> String {
    latin1_text(bytes)
}

/// Bazel's internal strings are Latin-1; characters above U+00FF have no
/// byte representation and are rejected explicitly.
fn latin1(text: &str, what: &str) -> anyhow::Result<Vec<u8>> {
    crate::repository_rule::latin1_bytes(text).ok_or_else(|| {
        anyhow!(
            "{what} contains characters above U+00FF, which are unsupported (Latin-1 byte strings)"
        )
    })
}

fn real_dir(path: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect directory {}", path.display()))?;
    ensure!(
        metadata.file_type().is_dir(),
        "directory {} must be a real directory, not a symlink",
        path.display()
    );
    Ok(())
}

fn checked_root(root: &Path) -> anyhow::Result<PathBuf> {
    // Check ancestors as well as the final root before canonicalization.
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()?.join(root)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component);
        real_dir(&current)?;
    }
    Ok(fs::canonicalize(root)?)
}

/// Strict source-label component profile (unchanged from the file/template
/// profile): label packages and names never traverse symlinks.
fn label_components(path: &str) -> anyhow::Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= 4096,
        "repository path must contain 1..4096 bytes"
    );
    for component in path.split('/') {
        ensure!(
            !matches!(component, "" | "." | ".."),
            "unsupported repository path {path:?}: expected clean relative components"
        );
    }
    Ok(())
}

fn parents(root: &Path, relative: &str, create: bool) -> anyhow::Result<PathBuf> {
    label_components(relative)?;
    real_dir(root)?;
    let mut path = root.to_path_buf();
    let mut parts = relative.split('/').peekable();
    while let Some(part) = parts.next() {
        path.push(part);
        if parts.peek().is_some() {
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                    fs::create_dir(&path)?
                }
                Err(error) => return Err(error.into()),
                Ok(metadata) => ensure!(
                    metadata.file_type().is_dir(),
                    "parent {} must be a real directory, not a symlink",
                    path.display()
                ),
            }
        }
    }
    Ok(path)
}

/// `strerror` text of an I/O error, as Java/JNI report it.
fn io_reason(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.rsplit_once(" (os error ") {
        Some((text, _)) => text.to_owned(),
        None => text,
    }
}

/// Java's `FileInputStream` failure: `java.io.FileNotFoundException: PATH (REASON)`
/// (captured for read()/template() of missing files and directories).
fn file_not_found(path: &Path, error: &std::io::Error) -> anyhow::Error {
    anyhow!(
        "java.io.FileNotFoundException: {} ({})",
        display(path.as_os_str().as_bytes()),
        io_reason(error)
    )
}

/// Read a whole file whose content Bazel returns as a string (read(),
/// template input, patch text). No size cap: the memory is inherent.
fn read_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut file = fs::File::open(path).map_err(|error| file_not_found(path, &error))?;
    if file.metadata().is_ok_and(|metadata| metadata.is_dir()) {
        return Err(file_not_found(
            path,
            &std::io::Error::from_raw_os_error(21), // EISDIR, as FileInputStream reports it
        ));
    }
    file.read_to_end(&mut bytes)
        .map_err(|error| file_not_found(path, &error))?;
    Ok(bytes)
}

/// SHA-256 of a file, hashed incrementally (constant memory).
/// Digest of a directory tree (RepoRecordedInput.DirTree): every entry's
/// relative name and type, file contents and link targets, in name order.
fn tree_digest(root: &Path) -> anyhow::Result<String> {
    fn visit(directory: &Path, prefix: &[u8], out: &mut Vec<u8>) -> anyhow::Result<()> {
        let mut entries = fs::read_dir(directory)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()?;
        entries.sort();
        for name in entries {
            let path = directory.join(&name);
            let mut relative = prefix.to_vec();
            if !relative.is_empty() {
                relative.push(b'/');
            }
            relative.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&relative);
            out.push(0);
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                out.extend_from_slice(b"link\0");
                out.extend_from_slice(fs::read_link(&path)?.as_os_str().as_bytes());
            } else if metadata.is_dir() {
                out.extend_from_slice(b"dir");
                out.push(0);
                visit(&path, &relative, out)?;
            } else if metadata.is_file() {
                out.extend_from_slice(format!("file\0{}", file_sha256(&path)?).as_bytes());
            } else {
                out.extend_from_slice(b"other");
            }
            out.push(0);
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(root, b"", &mut out)?;
    Ok(sha256(&out))
}

/// RepoRecordedInput.File value: `missing`, `directory`, `other` or
/// `file:<sha256>`. Regular-file digests come from [`FILE_DIGESTS`].
fn file_state(host: &Path) -> anyhow::Result<String> {
    Ok(match fs::metadata(host) {
        Err(_) => "missing".to_owned(),
        Ok(metadata) if metadata.is_dir() => "directory".to_owned(),
        Ok(metadata) if metadata.is_file() => format!(
            "file:{}",
            FILE_DIGESTS.sha256(host, std::time::SystemTime::now())?
        ),
        Ok(_) => "other".to_owned(),
    })
}

/// `file_state` for warm-publication revalidation (RepoRecordedInput
/// states), observed through crate::observed_fs for the analysis snapshot
/// (the same state string). A regular file's digest comes from
/// [`FILE_DIGESTS`] — the in-process memo, the persistent digest table or a
/// read — and every answer is recorded as the READ of `host` it stands for
/// ([`FileDigestMemo::observed_sha256`]).
pub(crate) fn recorded_file_state(host: &Path) -> Option<String> {
    crate::observed_fs::note_repository_input(host, false);
    Some(match crate::observed_fs::metadata(host) {
        Err(_) => "missing".to_owned(),
        Ok(metadata) if metadata.is_dir() => "directory".to_owned(),
        Ok(metadata) if metadata.is_file() => format!(
            "file:{}",
            FILE_DIGESTS
                .observed_sha256(host, std::time::SystemTime::now())
                .ok()?
        ),
        Ok(_) => "other".to_owned(),
    })
}

/// A file whose mtime and ctime are this much older than the moment hashing
/// starts may be memoized ("racy git", like `digest_memo::RACY_MARGIN` and
/// `repository_store::RACY_MARGIN`): a later in-place change then
/// necessarily carries a different timestamp.
const RACY_MARGIN: Duration = Duration::from_secs(2);
/// Entries [`FILE_DIGESTS`] holds before it starts over (bounded memory).
const FILE_DIGEST_MEMO_LIMIT: usize = 1 << 16;

/// The process-wide recorded-input digest memo (review i20). Every
/// go_repository execution and every warm validation of one observes
/// `bin/gazelle` and `bin/fetch_repo` of go_repository_tools (~16 MB
/// together): a warm ~/code build of 221 go_repositories hashed 1,061
/// files / 3.6 GB without it, 198 files / 35 MB with it
/// (docs/REPOSITORY_WARM_PERF.md).
static FILE_DIGESTS: std::sync::LazyLock<FileDigestMemo> =
    std::sync::LazyLock::new(FileDigestMemo::default);

/// What a memoized digest is valid for: the file's canonical path and stat
/// key (device, inode, size, mtime, ctime — vfs/DigestUtils.CacheKey, like
/// the action digest memo). Any rewrite, rename over or chmod changes it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FileDigestKey {
    path: PathBuf,
    device: u64,
    inode: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl FileDigestKey {
    fn new(path: PathBuf, metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }

    /// Whether `metadata` still has this key's stat.
    fn same_stat(&self, metadata: &fs::Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        (self.device, self.inode, self.size, self.mtime, self.ctime)
            == (
                metadata.dev(),
                metadata.ino(),
                metadata.size(),
                (metadata.mtime(), metadata.mtime_nsec()),
                (metadata.ctime(), metadata.ctime_nsec()),
            )
    }

    /// mtime and ctime at least [`RACY_MARGIN`] before `start`.
    fn settled(&self, start: std::time::SystemTime) -> bool {
        [self.mtime, self.ctime]
            .into_iter()
            .all(|(seconds, nanoseconds)| {
                u64::try_from(seconds)
                    .ok()
                    .and_then(|seconds| {
                        std::time::UNIX_EPOCH
                            .checked_add(Duration::new(seconds, nanoseconds as u32))
                    })
                    .and_then(|time| time.checked_add(RACY_MARGIN))
                    .is_some_and(|time| time <= start)
            })
    }
}

/// One memoized digest: set once by the observation that hashed the file —
/// `None` when that digest may not be remembered (the file changed while
/// it was read, or could not be read).
type DigestCell = Arc<std::sync::OnceLock<Option<String>>>;

/// An in-process memo of regular-file SHA-256 digests for recorded-input
/// file states, shared by worker threads. The key is the opened file's own
/// stat (fstat), so a digest always belongs to the inode it is keyed by.
/// Only settled files are remembered ([`FileDigestKey::settled`] at the
/// moment hashing starts) and only when their stat is unchanged after
/// hashing, so a hit never answers for different bytes; a racy file is
/// hashed on every observation. Concurrent observations of one key hash it
/// once: the others wait for that digest (warm validations of ~400
/// go_repositories run in parallel). A digest the memo does not hold comes
/// from crate::digest_memo::opened_sha256 ([`FileDigestMemo::digest`]): the
/// persistent digest table's when that records the file in exactly its
/// opened state, so the memo sits in front of the table as `file_sha256`
/// does for `watch_tree`.
#[derive(Default)]
struct FileDigestMemo {
    cells: std::sync::Mutex<std::collections::HashMap<FileDigestKey, DigestCell>>,
    /// Digests this memo computed (tests: per instance, unlike the
    /// process-wide PERF-REPO counters).
    #[cfg(test)]
    hashed: std::sync::atomic::AtomicUsize,
}

impl FileDigestMemo {
    /// The SHA-256 of regular file `host`, hashed at most once per settled
    /// stat key; `start` is the moment this observation began.
    fn sha256(&self, host: &Path, start: std::time::SystemTime) -> anyhow::Result<String> {
        self.sha256_in(crate::digest_memo::repository_table(), host, start)
    }

    /// [`FileDigestMemo::sha256`] for the warm revalidation of recorded
    /// inputs, inside the analysis snapshot's recording window
    /// (integration-20, INTEGRATION-20.md F4): every answer — an in-process
    /// hit, the persistent table's digest or a read — is recorded as the
    /// READ of `host` it stands for, keyed by the opened file's fstat: the
    /// exact stat the memo and the table were looked up by
    /// (crate::observed_fs::note_read_digest; a symlink's LSTAT too). The
    /// canonical path of the in-process key is observed as well.
    fn observed_sha256(&self, host: &Path, start: std::time::SystemTime) -> anyhow::Result<String> {
        let (digest, opened) =
            self.answer(crate::digest_memo::repository_table(), host, start, true)?;
        if crate::observed_fs::recording()
            && let Some(sha256) = crate::digest_memo::parse_hex(&digest)
        {
            crate::observed_fs::note_read_digest(host, &opened, sha256);
        }
        Ok(digest)
    }

    /// [`FileDigestMemo::sha256`] with an explicit persistent table
    /// (`None`: none installed, or `--verify_repo_tampering`).
    fn sha256_in(
        &self,
        table: Option<&crate::digest_memo::Table>,
        host: &Path,
        start: std::time::SystemTime,
    ) -> anyhow::Result<String> {
        self.answer(table, host, start, false)
            .map(|(digest, _)| digest)
    }

    /// The digest of regular file `host` and the fstat of the file opened
    /// for it (the stat every lookup of the answer was keyed by). With
    /// `observed` the in-process key's canonical path is taken through
    /// crate::observed_fs.
    fn answer(
        &self,
        table: Option<&crate::digest_memo::Table>,
        host: &Path,
        start: std::time::SystemTime,
        observed: bool,
    ) -> anyhow::Result<(String, fs::Metadata)> {
        let file = fs::File::open(host).map_err(|error| file_not_found(host, &error))?;
        let metadata = file
            .metadata()
            .map_err(|error| file_not_found(host, &error))?;
        let size = metadata.len();
        let canonical = if observed {
            crate::observed_fs::canonicalize(host)
        } else {
            fs::canonicalize(host)
        };
        let key = canonical
            .ok()
            .map(|canonical| FileDigestKey::new(canonical, &metadata))
            .filter(|key| key.settled(start));
        let Some(key) = key else {
            let digest = self.digest(table, host, &file, &metadata, start)?;
            return Ok((digest, metadata));
        };
        let cell = {
            let mut cells = self
                .cells
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if cells.len() >= FILE_DIGEST_MEMO_LIMIT && !cells.contains_key(&key) {
                cells.clear();
            }
            cells.entry(key.clone()).or_default().clone()
        };
        let mut hashed_here = None;
        let remembered = cell.get_or_init(|| {
            let digest = self.digest(table, host, &file, &metadata, start);
            let remembered = match &digest {
                // Only if the file kept the stat it is keyed by.
                Ok(digest) if file.metadata().is_ok_and(|after| key.same_stat(&after)) => {
                    Some(digest.clone())
                }
                _ => None,
            };
            hashed_here = Some(digest);
            remembered
        });
        if remembered.is_none() {
            // Not memoizable: forget the cell so a later observation tries
            // again (the key of a file that changed is dead anyway).
            self.forget(&key, &cell);
        }
        let digest = match (hashed_here, remembered) {
            // This observation hashed the file: its own answer.
            (Some(digest), _) => digest,
            (None, Some(digest)) => {
                crate::repository_store::timing::count_recorded_digest(true, size);
                Ok(digest.clone())
            }
            // Another observation's digest could not be remembered.
            (None, None) => self.digest(table, host, &file, &metadata, start),
        }?;
        Ok((digest, metadata))
    }

    /// The digest of the opened `file` (nothing has been read from it),
    /// whose fstat is `metadata`, through crate::digest_memo::opened_sha256
    /// (`Use::Recorded`; the one implementation of the table's contract):
    /// the persistent table's when it records `host` in exactly that state
    /// (the external, ctime key of every repository use of the table);
    /// otherwise the bytes read from `file` itself, recorded in the table
    /// for later processes when the file was settled at `start` (this
    /// observation's start, before the read) and kept its stat while it was
    /// read. A table answer is counted with the in-process memo's hits.
    fn digest(
        &self,
        table: Option<&crate::digest_memo::Table>,
        host: &Path,
        file: &fs::File,
        metadata: &fs::Metadata,
        start: std::time::SystemTime,
    ) -> anyhow::Result<String> {
        use crate::digest_memo::{self, Answer, Use};
        let (sha256, answer) =
            digest_memo::opened_sha256_in(table, file, metadata, host, Use::Recorded, || start)
                .map_err(|error| file_not_found(host, &error))?;
        #[cfg(test)]
        if answer == Answer::Read {
            self.hashed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        crate::repository_store::timing::count_recorded_digest(
            answer == Answer::Table,
            metadata.len(),
        );
        Ok(digest_memo::hex(&sha256))
    }

    fn forget(&self, key: &FileDigestKey, cell: &DigestCell) {
        let mut cells = self
            .cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cells
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, cell))
        {
            cells.remove(key);
        }
    }
}

/// RepoRecordedInput.Dirents state (maybeWatchDirents, java:1740-1755): the
/// digest of the sorted entry names, each NUL-terminated. `None` when the
/// directory cannot be observed.
pub(crate) fn recorded_dirents_state(host: &Path) -> Option<String> {
    crate::observed_fs::note_repository_input(host, true);
    let mut names: Vec<Vec<u8>> =
        crate::observed_fs::read_dir_projected(host, &crate::observed_fs::Projection::All)
            .ok()?
            .into_iter()
            .map(|entry| {
                entry
                    .ok()
                    .map(|entry| entry.file_name().as_encoded_bytes().to_vec())
            })
            .collect::<Option<_>>()?;
    names.sort_unstable();
    let mut hash = Vec::new();
    for name in names {
        hash.extend_from_slice(&name);
        hash.push(0);
    }
    Some(format!("dirents:{}", sha256(&hash)))
}

/// StarlarkRepositoryContext.watch_tree's recorded state (SRC:568-609): the
/// recursive tree digest. `None` when the tree cannot be observed.
pub(crate) fn recorded_dirtree_state(host: &Path) -> Option<String> {
    crate::observed_fs::note_repository_input(host, true);
    observed_tree_digest(host)
        .ok()
        .map(|digest| format!("dirtree:{digest}"))
}

/// [`tree_digest`] observed through crate::observed_fs (the same digest),
/// for warm-publication revalidation inside the snapshot window. A file's
/// digest comes from the persistent digest table as for [`tree_digest`]
/// ([`observed_file_sha256`]).
fn observed_tree_digest(root: &Path) -> std::io::Result<String> {
    use crate::observed_fs;
    fn visit(directory: &Path, prefix: &[u8], out: &mut Vec<u8>) -> std::io::Result<()> {
        let mut entries =
            observed_fs::read_dir_projected(directory, &observed_fs::Projection::All)?
                .into_iter()
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<Result<Vec<_>, _>>()?;
        entries.sort();
        for name in entries {
            let path = directory.join(&name);
            let mut relative = prefix.to_vec();
            if !relative.is_empty() {
                relative.push(b'/');
            }
            relative.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&relative);
            out.push(0);
            let metadata = observed_fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                out.extend_from_slice(b"link\0");
                out.extend_from_slice(observed_fs::read_link(&path)?.as_os_str().as_bytes());
            } else if metadata.is_dir() {
                out.extend_from_slice(b"dir");
                out.push(0);
                visit(&path, &relative, out)?;
            } else if metadata.is_file() {
                let digest = observed_fs::hex(&observed_file_sha256(&path)?);
                out.extend_from_slice(format!("file\0{digest}").as_bytes());
            } else {
                out.extend_from_slice(b"other");
            }
            out.push(0);
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(root, b"", &mut out)?;
    Ok(sha256(&out))
}

/// [`file_sha256`] for [`observed_tree_digest`] (integration-20,
/// INTEGRATION-20.md F4): the file at `path` is opened and hashed through
/// crate::digest_memo::opened_sha256 — the persistent table's digest when it
/// records `path` in exactly the opened file's state, otherwise the bytes
/// read from it — and the answer, hit or read, is recorded as the READ of
/// `path` it stands for, keyed by the opened file's fstat
/// (crate::observed_fs::note_read_digest).
fn observed_file_sha256(path: &Path) -> std::io::Result<[u8; 32]> {
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    let sha256 = crate::digest_memo::opened_sha256(
        &file,
        &metadata,
        path,
        crate::digest_memo::Use::Recorded,
    )?;
    crate::observed_fs::note_read_digest(path, &metadata, sha256);
    Ok(sha256)
}

/// The SHA-256 of a file of a watched tree ([`tree_digest`]; the files a
/// repository rule watches or reads go through [`FILE_DIGESTS`], which
/// consults the same table on a miss): from the persistent digest memo when
/// it records the path in exactly its current state (settled files only;
/// never under `--verify_repo_tampering`), so a warm run does not read
/// unchanged files again (crate::digest_memo::path_sha256: it opens the file
/// and hashes it through crate::digest_memo::opened_sha256, the entry point
/// an in-process digest memo in front of the table must use as well).
fn file_sha256(path: &Path) -> anyhow::Result<String> {
    let digest = crate::digest_memo::path_sha256(path, crate::digest_memo::Use::Recorded, || {
        Algorithm::Sha256
            .hash_file(path)
            .map(|(digest, _)| digest)
            .map_err(|error| file_not_found(path, &error))
    })?;
    Ok(transport::hex(&digest))
}

/// Path.resolveSymbolicLinks (StarlarkPath.java:186-196): resolve component
/// by component; a missing component (e.g. a dangling link's target) fails
/// with unix_jni's "[unix_jni.cc:382] PATH (REASON)" naming that component.
fn resolve_symbolic_links(path: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut pending: std::collections::VecDeque<Vec<u8>> = path
        .split(|b| *b == b'/')
        .filter(|c| !c.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    let mut resolved = b"/".to_vec();
    let mut hops = 0;
    while let Some(component) = pending.pop_front() {
        let candidate = normalize(&resolved, &component);
        match fs::symlink_metadata(path_buf(&candidate)) {
            Err(error) => bail!(
                "[unix_jni.cc:382] {} ({})",
                display(&candidate),
                io_reason(&error)
            ),
            Ok(metadata) if metadata.file_type().is_symlink() => {
                hops += 1;
                ensure!(
                    hops <= 32,
                    "{} (Too many levels of symbolic links)",
                    display(path)
                );
                let target = fs::read_link(path_buf(&candidate))?;
                let target = target.as_os_str().as_bytes();
                if target.first() == Some(&b'/') {
                    resolved = b"/".to_vec();
                }
                for part in target.split(|b| *b == b'/').filter(|c| !c.is_empty()).rev() {
                    pending.push_front(part.to_vec());
                }
            }
            Ok(_) => resolved = candidate,
        }
    }
    Ok(resolved)
}

/// PackageLookupFunction.explainNoBuildFileValue (java:130-155) for a label
/// whose package directory or BUILD file is absent.
fn no_build_file(root: &Path, label: &Label) -> anyhow::Result<()> {
    let directory = if label.package.is_empty() {
        root.to_path_buf()
    } else {
        root.join(&label.package)
    };
    let has_marker = fs::metadata(&directory).is_ok_and(|metadata| metadata.is_dir())
        && ["BUILD.bazel", "BUILD"]
            .iter()
            .any(|name| fs::symlink_metadata(directory.join(name)).is_ok());
    if has_marker {
        return Ok(());
    }
    let education = "Add a BUILD file to a directory to mark it as a package.";
    if label.repository.is_empty() {
        bail!(
            "Unable to load package for {label}: BUILD file not found in any of the following directories. {education}\n - {}",
            label.package
        )
    }
    bail!(
        "Unable to load package for {label}: BUILD file not found in directory '{}' of external repository @@{}. {education}",
        label.package,
        label.repository
    )
}

/// PathFragment normalization of an archive `strip_prefix`
/// (StripPrefixedPath.java:53,71-77): empty and `.` segments vanish, `..`
/// pops, a leading `/` is dropped; so `pre/` strips `pre`.
fn normalized_prefix(prefix: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in prefix.split('/') {
        match segment {
            "" | "." => {}
            ".." if segments.last().is_some_and(|last| *last != "..") => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    segments.join("/")
}

/// A decompressor IOException wrapped like DecompressorValue.decompress:
/// `Error extracting ARCHIVE to ARCHIVE_PARENT: MESSAGE` (java:260-270).
fn extraction_io_error(archive: &[u8], message: &str) -> anyhow::Error {
    anyhow!(
        "java.io.IOException: Error extracting {} to {}: {message}",
        display(archive),
        display(&normalize(archive, b".."))
    )
}

/// DecompressorValue.CouldNotFindPrefixException (java:134-164) wrapped by
/// `Error extracting ARCHIVE to ARCHIVE_PARENT: ...` (java:260-270).
fn prefix_not_found(archive: &[u8], prefix: &str, available: &BTreeSet<String>) -> anyhow::Error {
    let suggestion = if available.is_empty() {
        "We could not find any directory in this archive (maybe there is no need for `strip_prefix`?)".to_owned()
    } else {
        format!(
            "Here are possible prefixes for this archive: \"{}\".",
            available.iter().cloned().collect::<Vec<_>>().join("\", \"")
        )
    };
    extraction_io_error(
        archive,
        &format!("Prefix \"{prefix}\" was given, but not found in the archive. {suggestion}"),
    )
}

/// `Checksum.fromString`/`fromSubresourceIntegrity` (Checksum.java:58-120)
/// messages for malformed checksums; such errors are deferred by Bazel until
/// after the download attempt (StarlarkBaseExternalContext.java:1094-1101).
fn checksum_text_error(sha256: &str, integrity: &str) -> Option<String> {
    use base64::Engine;
    if !sha256.is_empty() {
        let valid = sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit());
        return (!valid).then(|| format!("Invalid SHA-256 checksum '{sha256}'"));
    }
    if integrity.is_empty() {
        return None;
    }
    let Some((key, data, length)) = [
        ("sha1-", "SHA-1", 20),
        ("sha256-", "SHA-256", 32),
        ("sha384-", "SHA-384", 48),
        ("sha512-", "SHA-512", 64),
        ("blake3-", "BLAKE3", 32),
    ]
    .into_iter()
    .find_map(|(prefix, key, length)| {
        integrity
            .strip_prefix(prefix)
            .map(|data| (key, data, length))
    }) else {
        return Some(format!(
            "Unsupported checksum algorithm: '{integrity}' (expected SHA-1, SHA-256, SHA-384, or SHA-512)"
        ));
    };
    // java.util.Base64.getDecoder(): optional padding, trailing bits allowed.
    let engine = base64::engine::general_purpose::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::general_purpose::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    match engine.decode(data) {
        Err(_) => Some(format!("Invalid base64 '{data}'")),
        Ok(bytes) if bytes.len() != length => {
            Some(format!("Invalid {key} SRI checksum '{integrity}'"))
        }
        Ok(_) => None,
    }
}

/// Bazel's per-download temporary directory name (`createTempDirectory("temp")`,
/// StarlarkBaseExternalContext.java:1126): "temp" and a random long.
fn temp_name() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    format!("temp{}", hasher.finish() >> 1)
}

fn sha256(bytes: &[u8]) -> String {
    transport::sha256(bytes)
}

/// The archive kind accepted by the native extractor for a Bazel type name
/// (DecompressorValue.java:40-93). Bazel-only formats fail explicitly.
fn decompressor(kind: &str) -> anyhow::Result<&'static str> {
    const NATIVE: &[(&str, &str)] = &[
        ("zip", "zip"),
        ("jar", "zip"),
        ("war", "zip"),
        ("aar", "zip"),
        ("nupkg", "zip"),
        ("whl", "zip"),
        ("tar", "tar"),
        ("tar.gz", "tar.gz"),
        ("tgz", "tar.gz"),
        ("tar.xz", "tar.xz"),
        ("txz", "tar.xz"),
        ("tar.zst", "tar.zst"),
        ("tzst", "tar.zst"),
        ("tar.bz2", "tar.bz2"),
        ("tbz", "tar.bz2"),
        // ArFunction (DecompressorValue.java:151-152, 172).
        ("ar", "ar"),
        ("deb", "ar"),
    ];
    if let Some((_, native)) = NATIVE.iter().find(|(name, _)| *name == kind) {
        return Ok(native);
    }
    if BAZEL_FORMATS.contains(&kind) {
        bail!(
            "archive type {kind} is not supported by the native extractor (supported: zip, jar, war, aar, nupkg, whl, tar, tar.gz, tgz, tar.xz, txz, tar.zst, tzst, tar.bz2, tbz, ar, deb)"
        );
    }
    bail!(
        "No decompressor found for type {kind}. Available types are: {}",
        readable_formats("", "or")
    )
}

const BAZEL_FORMATS: &[&str] = &[
    "zip", "jar", "war", "aar", "nupkg", "whl", "tar", "tar.gz", "tgz", "gz", "tar.xz", "txz",
    "xz", "tar.zst", "tzst", "zst", "tar.bz2", "tbz", "bz2", "ar", "deb", "7z",
];

fn readable_formats(prefix: &str, conjunction: &str) -> String {
    let all: Vec<String> = BAZEL_FORMATS
        .iter()
        .map(|kind| format!("{prefix}{kind}"))
        .collect();
    format!(
        "{} {conjunction} {}",
        all[..all.len() - 1].join(", "),
        all[all.len() - 1]
    )
}

/// DecompressorValue.getDecompressor(Path): first format whose `.ext` ends
/// the basename, in Bazel's search order.
fn decompressor_for_name(basename: &[u8], shown: &str) -> anyhow::Result<&'static str> {
    for kind in BAZEL_FORMATS {
        if basename.ends_with(format!(".{kind}").as_bytes()) {
            return decompressor(kind);
        }
    }
    bail!(
        "Expected a file with a {} suffix (got {shown})",
        readable_formats(".", "or")
    )
}

/// The file name download_and_extract downloads to and extracts from:
/// DownloadManager.getDownloadDestination (java:522-536) with the `type`
/// it always passes (StarlarkBaseExternalContext.java:1137): the first
/// URL's decoded basename (`temp` when empty; Bazel's `cacheprobe[.TYPE]`
/// placeholder without URLs, java:230-238), `.TYPE` appended unless the name
/// already ends with it, and every UTF-16 unit outside `[A-Za-z0-9._-]`
/// replaced by `_` (FS_UNSAFE_CHARS). The decompressor is then chosen by
/// this name alone (java:1186-1190).
fn download_basename(url: Option<&reqwest::Url>, kind: &str) -> String {
    let base = match url {
        Some(url) => url_basename(url),
        None if kind.is_empty() => "cacheprobe".to_owned(),
        None => format!("cacheprobe.{kind}"),
    };
    let mut base = if base.is_empty() {
        "temp".to_owned()
    } else {
        base
    };
    if !kind.is_empty() && !base.ends_with(&format!(".{kind}")) {
        base.push('.');
        base.push_str(kind);
    }
    base.chars()
        .flat_map(|c| {
            let safe = c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
            let (c, count) = if safe { (c, 1) } else { ('_', c.len_utf16()) };
            std::iter::repeat_n(c, count)
        })
        .collect()
}

/// `PathFragment.create(uri.getPath()).getBaseName()` (DownloadManager
/// getUrlBaseName, java:553-568): the percent-decoded path's last segment.
fn url_basename(url: &reqwest::Url) -> String {
    let raw = url.path().as_bytes();
    let mut decoded = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        let hex = |byte: u8| char::from(byte).to_digit(16);
        if raw[index] == b'%'
            && let (Some(high), Some(low)) = (
                raw.get(index + 1).copied().and_then(hex),
                raw.get(index + 2).copied().and_then(hex),
            )
        {
            decoded.push(u8::try_from(high * 16 + low).expect("two hex digits"));
            index += 3;
            continue;
        }
        decoded.push(raw[index]);
        index += 1;
    }
    let text = String::from_utf8_lossy(&decoded);
    let mut segments: Vec<&str> = Vec::new();
    for segment in text.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments
        .last()
        .map_or_else(String::new, |last| (*last).to_owned())
}

impl Context {
    /// The StarlarkBaseExternalContext of a module extension
    /// (ModuleExtensionContext.java:72-86): `working_directory` is its
    /// temporary working directory, `extension_id` `LABEL%NAME`.
    pub(crate) fn module_extension(
        repositories: Arc<RepositorySet>,
        working_directory: &Path,
        label: Label,
        extension_id: String,
    ) -> anyhow::Result<Self> {
        let workspace = fs::canonicalize(repositories.main_root()).with_context(|| {
            format!(
                "cannot resolve workspace root {}",
                repositories.main_root().display()
            )
        })?;
        Ok(Self {
            repositories,
            kind: Kind::ModuleExtension,
            stage: working_directory.to_path_buf(),
            workspace,
            label,
            canonical: extension_id,
            state: RefCell::new(State::default()),
            environment: RefCell::new(Watch::default()),
            output: RefCell::default(),
            live: true,
            environ_read: Arc::default(),
            workspace_observed: Arc::default(),
            mentions: None,
        })
    }

    /// One line a repository rule execution prints (`print()`, the
    /// reproducibility DEBUG lines): to stderr as it happens — exactly the
    /// evaluator's default handler's `eprintln!` — unless this is a
    /// captured replay, and into the execution's output record when one is
    /// kept ([`Self::collects_output`]).
    fn print_line(&self, text: &str) {
        // DEBUG events: marked notes wherever they are kept (crate::ui).
        if self.live && !crate::repository_speculation::defer_stderr(&crate::ui::marked(text)) {
            crate::ui::note(text);
        }
        if self.collects_output() {
            self.output
                .borrow_mut()
                .extend_from_slice(&crate::ui::marked(text));
        }
    }

    /// `execute(quiet=False)`'s echo of the captured output (Bazel prints
    /// it to its stderr, StarlarkExecutionResult.java:205-207), recorded
    /// like [`Self::print_line`].
    fn echo(&self, stdout: &[u8], stderr: &[u8]) {
        if self.live
            && !(crate::repository_speculation::active()
                && crate::repository_speculation::defer_stderr(stdout)
                && crate::repository_speculation::defer_stderr(stderr))
        {
            crate::repository_process::echo(stdout, stderr);
        }
        if self.collects_output() {
            let mut output = self.output.borrow_mut();
            output.extend_from_slice(stdout);
            output.extend_from_slice(stderr);
        }
    }

    /// Whether [`Self::output`] collects what a repository rule prints: for
    /// a replay (which never prints live), while the record mode's replay
    /// capture records local executions, and while the analysis snapshot's
    /// window records (a non-local rule that printed taints the run,
    /// crate::repository) — otherwise nobody reads it, and a chatty rule's
    /// output would only cost memory.
    fn collects_output(&self) -> bool {
        self.kind == Kind::Repository
            && (!self.live
                || self.repositories.records_for_replay()
                || crate::observed_fs::recording())
    }

    /// Use `client` for downloads (tests: a private trust root).
    #[cfg(test)]
    pub(crate) fn with_client(self, client: reqwest::blocking::Client) -> Self {
        self.state.borrow_mut().client = Some(client);
        self
    }

    /// The recorded inputs and environment observations after the callback.
    pub(crate) fn finish(self) -> anyhow::Result<ExecutionInputs> {
        Ok(self.finish_with_names()?.0)
    }

    /// `finish` plus the observed environment names, for warm revalidation.
    pub(crate) fn finish_with_names(self) -> anyhow::Result<(ExecutionInputs, Vec<String>)> {
        let names = self.environment.borrow().names();
        let watched_env = self.environment.borrow().inputs()?;
        let mut inputs = std::mem::take(&mut self.state.borrow_mut().inputs);
        inputs.watched_env = watched_env;
        Ok((inputs, names))
    }

    /// identifyingStringForLogging: `repository @@NAME` or
    /// `module extension LABEL%NAME` (ModuleExtensionEvaluationProgress.java:26-28).
    fn identifying(&self) -> String {
        match self.kind {
            Kind::Repository => format!("repository @@{}", self.canonical),
            Kind::ModuleExtension => format!("module extension {}", self.canonical),
        }
    }

    fn stage_bytes(&self) -> &[u8] {
        self.stage.as_os_str().as_bytes()
    }

    /// `path` relative to the repository directory, if inside it.
    fn under_stage<'a>(&self, path: &'a [u8]) -> Option<&'a [u8]> {
        let stage = self.stage_bytes();
        if path == stage {
            Some(b"")
        } else {
            path.strip_prefix(stage)
                .and_then(|rest| rest.strip_prefix(b"/"))
        }
    }

    /// StarlarkBaseExternalContext.getPath (java:1593-1601) for string,
    /// Label and path arguments. Label paths record no implicit watch
    /// (--incompatible_no_implicit_watch_label defaults to true).
    fn path_of(&self, value: Value<'_>, method: &str, parameter: &str) -> anyhow::Result<Vec<u8>> {
        if let Some(text) = value.unpack_str() {
            return Ok(normalize(self.stage_bytes(), &latin1(text, "path")?));
        }
        if let Some(path) = value.downcast_ref::<RepositoryPath>() {
            return Ok(path.bytes.clone());
        }
        if let Some((repository, package, name)) = crate::starlark_types::label_parts(value) {
            return self
                .label_path(repository, package, name)
                .map(|(path, _)| path.as_os_str().as_bytes().to_vec());
        }
        bail!(
            "in call to {method}(), parameter '{parameter}' got value of type '{}', want 'string, Label, or path'",
            value.get_type()
        )
    }

    /// RepositoryUtils.getRootedPathFromLabel (java:54-72): the package must
    /// exist; the file need not.
    fn label_path(
        &self,
        repository: &str,
        package: &str,
        name: &str,
    ) -> anyhow::Result<(PathBuf, Label)> {
        let label = Label {
            repository: repository.to_owned(),
            package: package.to_owned(),
            name: name.to_owned(),
        };
        if !repository.is_empty() {
            // A rule resolving one label of a label list resolves them all
            // (rules_python's whl_library walks `_python_path_entries`):
            // their repositories are fetched in parallel instead of one
            // after another at each resolution. Scheduling only
            // (crate::repository_speculation); the demand below is the one
            // that counts.
            let siblings = {
                let mut state = self.state.borrow_mut();
                let groups = &mut state.label_siblings;
                let (wanted, rest): (Vec<_>, Vec<_>) = std::mem::take(groups)
                    .into_iter()
                    .partition(|group| group.contains(repository));
                *groups = rest;
                wanted
            };
            for group in siblings {
                self.repositories.fetch_ahead(
                    group
                        .iter()
                        .map(String::as_str)
                        .filter(|name| *name != repository),
                );
            }
        }
        let root = self
            .repositories
            .root(repository)
            .map_err(anyhow::Error::msg)?
            .to_path_buf();
        if !repository.is_empty() {
            self.state
                .borrow_mut()
                .resolved
                .insert(repository.to_owned());
        }
        if self
            .repositories
            .ignored_subdirectories(repository)
            .map_err(anyhow::Error::msg)?
            .matching_entry(package)
            .is_some()
            || crate::ignored::is_deleted_package(repository, package)
        {
            bail!(
                "Unable to load package for {label}: {}",
                crate::ignored::DELETED_PACKAGE_MESSAGE
            );
        }
        no_build_file(&root, &label)?;
        crate::workspace::package_build_file(&root, package)
            .map_err(|message| anyhow!("Unable to load package for {label}: {message}"))?;
        let root = fs::canonicalize(&root)
            .map_err(|error| anyhow!("cannot resolve repository root for {label}: {error}"))?;
        let relative = if package.is_empty() {
            name.to_owned()
        } else {
            format!("{package}/{name}")
        };
        Ok((root.join(relative), label))
    }

    /// The readers' hold (crate::repository_slots::SlotReaders) of the
    /// mutable repository directories an `execute` (`argv`, `environment`,
    /// `working_directory`) may write into: every self-referencing
    /// repository a label of this execution resolved into (its published
    /// files name its directory, as gazelle 0.51's `go_repository_cache`
    /// names GOPATH in `go.env`), every resolved ordinary repository the
    /// command writes into (below), and every real directory among the
    /// slots its arguments, environment or working directory name. Never
    /// this execution's own (its writer holds it) nor one this thread holds
    /// as a writer. A resolved one whose slot is not complete (missing, a
    /// link, an interrupted writer's) is made again from its tree first.
    /// Nothing is held between two `execute`s: a consumer holding a
    /// directory for its whole execution would deadlock with another
    /// process re-executing that repository while it demands a repository
    /// this one is executing (go_repository reads `go.env`, then demands
    /// go_repository_tools, which reads `go.env` too).
    ///
    /// Written into: a resolved ordinary repository whose published tree
    /// the arguments or environment name a missing path of (gazelle's
    /// relocatable `go.env`: GOCACHE is `<dirname of the cache repository's
    /// go.env>/gocache`, internal/env.bzl resolve_go_env, passed as an
    /// `env -i` argument, internal/common.bzl env_execute), or whose slot
    /// is already the mutable copy of that tree (its marker names it).
    /// Genuine leaves that repository a plain writable directory under
    /// `external/` and checks it by its marker alone, so the tool's cache
    /// survives across commands; natively the tree stays immutable (the analysis snapshot
    /// and every reuse verify it) and the command is redirected to the
    /// repository's slot, made a mutable copy of the tree (marker, reader
    /// locks, re-made when the repository is re-executed): every path below
    /// the tree in the command names the slot instead, genuine's
    /// `external/<name>`.
    fn mutable_slot_readers(
        &self,
        argv: &mut [Vec<u8>],
        environment: &mut [(Vec<u8>, Vec<u8>)],
        working_directory: &mut PathBuf,
    ) -> anyhow::Result<crate::repository_slots::SlotReaders> {
        use crate::repository_slots::{SlotClass, SlotReaders, slot_path};
        use std::os::unix::ffi::OsStringExt;
        let Ok(store) = self.repositories.source_cache_path() else {
            return Ok(SlotReaders::default());
        };
        let store = store.to_path_buf();
        // canonical name -> (identity, tree, exact) of a resolved mutable
        // repository (exact: an ordinary one written into, whose slot must
        // be the copy of this very tree); None for a directory only named.
        let mut wanted: BTreeMap<String, Option<(String, PathBuf, bool)>> = BTreeMap::new();
        // (tree root, slot) of the repositories written into.
        let mut redirects: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let resolved: Vec<String> = self.state.borrow().resolved.iter().cloned().collect();
        for name in resolved {
            if (self.kind == Kind::Repository && name == self.canonical)
                || crate::repository_slots::writing(&slot_path(&store, &name))
            {
                continue;
            }
            let Ok(root) = self.repositories.root(&name) else {
                continue;
            };
            match crate::repository_store::slot_class(&store, &name, root) {
                Some((identity, SlotClass::SelfReferencing)) => {
                    wanted.insert(name, Some((identity, root.to_path_buf(), false)));
                }
                Some((identity, SlotClass::Ordinary)) => {
                    // Paths below the tree are spelled through the
                    // canonical root (label paths are canonicalized).
                    let tree = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
                    let mut spellings = vec![tree.as_os_str().as_bytes().to_vec()];
                    if root != tree {
                        spellings.push(root.as_os_str().as_bytes().to_vec());
                    }
                    let texts = || {
                        argv.iter()
                            .map(Vec::as_slice)
                            .chain(environment.iter().map(|(_, value)| value.as_slice()))
                            .chain(std::iter::once(working_directory.as_os_str().as_bytes()))
                    };
                    if !spellings
                        .iter()
                        .any(|spelling| crate::repository_slots::names_path(spelling, texts()))
                    {
                        continue;
                    }
                    let written = [&tree, &root.to_path_buf()].iter().any(|tree| {
                        crate::repository_slots::written_into(
                            tree,
                            argv.iter()
                                .map(Vec::as_slice)
                                .chain(environment.iter().map(|(_, value)| value.as_slice())),
                        )
                    });
                    if !written
                        && !crate::repository_slots::complete_for(&store, &name, Some(&identity))
                    {
                        continue;
                    }
                    let slot = slot_path(&store, &name).as_os_str().as_bytes().to_vec();
                    redirects.extend(spellings.into_iter().map(|from| (from, slot.clone())));
                    wanted.insert(name, Some((identity, tree, true)));
                }
                None => {}
            }
        }
        for (from, to) in &redirects {
            for text in argv
                .iter_mut()
                .chain(environment.iter_mut().map(|(_, value)| value))
            {
                if let Some(redirected) = crate::repository_slots::redirected(text, from, to) {
                    *text = redirected;
                }
            }
            if let Some(redirected) = crate::repository_slots::redirected(
                working_directory.as_os_str().as_bytes(),
                from,
                to,
            ) {
                *working_directory = PathBuf::from(std::ffi::OsString::from_vec(redirected));
            }
        }
        let texts = argv
            .iter()
            .map(Vec::as_slice)
            .chain(environment.iter().map(|(_, value)| value.as_slice()))
            .chain(std::iter::once(working_directory.as_os_str().as_bytes()));
        for name in crate::repository_slots::named_slots(&store, texts) {
            if !wanted.contains_key(&name)
                && fs::symlink_metadata(slot_path(&store, &name)).is_ok_and(|m| m.is_dir())
            {
                wanted.insert(name, None);
            }
        }
        wanted.retain(|name, _| {
            !(self.kind == Kind::Repository && *name == self.canonical)
                && !crate::repository_slots::writing(&slot_path(&store, name))
        });
        if wanted.is_empty() {
            return Ok(SlotReaders::default());
        }
        let names: Vec<String> = wanted.keys().cloned().collect();
        for _ in 0..16 {
            let readers = SlotReaders::acquire(&store, &names).map_err(anyhow::Error::msg)?;
            let incomplete = wanted.iter().find_map(|(name, published)| {
                published
                    .as_ref()
                    .filter(|(identity, _, exact)| {
                        !crate::repository_slots::complete_for(
                            &store,
                            name,
                            exact.then_some(identity.as_str()),
                        )
                    })
                    .map(|published| (name, published))
            });
            let Some((name, (identity, tree, exact))) = incomplete else {
                return Ok(readers);
            };
            drop(readers);
            crate::repository_slots::restore_mutable(&store, name, identity, tree, *exact)
                .map_err(anyhow::Error::msg)?;
        }
        bail!(
            "the repository directories of {} keep changing while {} waits to execute",
            names.join(", "),
            self.identifying()
        )
    }

    /// Label file read (read/template/patch with a Label argument):
    /// getPathFromLabel (java:2386-2412; the package must exist), then a
    /// read of that path, following symlinks like Bazel's.
    fn read_label(&self, canonical: &str) -> anyhow::Result<Vec<u8>> {
        let label = Label::parse_canonical(canonical).map_err(anyhow::Error::msg)?;
        let (path, _) = self.label_path(&label.repository, &label.package, &label.name)?;
        if fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
            bail!("attempting to read() a directory: {}", path.display());
        }
        read_file(&path)
    }

    /// Bazel checkInOutputDirectory (java:331-339) plus the native output
    /// profile. Returns the stage-relative name ("" for the root).
    fn output(&self, path: &[u8]) -> anyhow::Result<String> {
        let relative = self.under_stage(path).ok_or_else(|| {
            anyhow!(
                "Cannot write outside of the repository directory for path {}",
                display(path)
            )
        })?;
        let relative = std::str::from_utf8(relative).map_err(|_| {
            anyhow!(
                "repository output path {} must be UTF-8 in the native store profile",
                display(path)
            )
        })?;
        if relative.is_empty() {
            return Ok(String::new());
        }
        if self.kind == Kind::ModuleExtension {
            // The working directory is scratch, never published to the store.
            return Ok(relative.to_owned());
        }
        // Bazel checkInOutputDirectory checks containment only; the store
        // adds the Linux kernel limits (PATH_MAX 4096 total, NAME_MAX 255
        // per component, no component-count cap). `# % ? : \\` are
        // ordinary Linux filename bytes and are accepted.
        ensure!(
            relative.len() <= 4096,
            "repository path must contain 1..4096 bytes"
        );
        ensure!(
            relative.split('/').all(|part| part.len() <= 255),
            "repository output path {relative:?} exceeds 255 bytes in a component"
        );
        Ok(relative.to_owned())
    }

    /// makeDirectories for an output: parents become real directories; a
    /// symlinked parent is never traversed while writing.
    /// The directory of the stage holding `relative` made writable while
    /// it is changed (crate::nofollow_modes::WritableDirectory: copies of
    /// sealed trees keep their read-only modes).
    fn writable_holding(&self, relative: &str) -> crate::nofollow_modes::WritableDirectory {
        crate::nofollow_modes::WritableDirectory::holding(&self.stage, &self.stage.join(relative))
    }

    fn output_parents(&self, relative: &str) -> anyhow::Result<PathBuf> {
        self.ensure_stage()?;
        if relative.is_empty() {
            return Ok(self.stage.clone());
        }
        parents(&self.stage, relative, true)
    }

    fn write(&self, path: &[u8], bytes: &[u8], executable: bool) -> anyhow::Result<()> {
        let relative = self.output(path)?;
        ensure!(
            !relative.is_empty(),
            "Could not create {}: is the repository directory",
            display(path)
        );
        if let Some(mentions) = &self.mentions
            && mentions.named_in(bytes)
        {
            self.workspace_observed
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let _writable = self.writable_holding(&relative);
        let path = self.output_parents(&relative)?;
        self.state.borrow_mut().created = true;
        // `p.getPath().delete()` removes a file, a symlink (never its target)
        // or an EMPTY directory; a non-empty one fails in unix_jni.
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                if fs::remove_dir(&path).is_err() {
                    bail!(
                        "java.io.IOException: [unix_jni.cc:660] {} (Directory not empty)",
                        display(path.as_os_str().as_bytes())
                    );
                }
            }
            Ok(_) => fs::remove_file(&path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // An executable is written under the exclusive spawn lock: no child
        // started meanwhile can inherit its write descriptor (ETXTBSY).
        crate::spawn_lock::exclusive(executable, || -> anyhow::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            file.write_all(bytes)?;
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(if executable {
                0o755
            } else {
                0o644
            }))?;
            Ok(())
        })
    }

    /// Create the working directory if needed and note that it exists.
    fn mark_created(&self) -> anyhow::Result<()> {
        self.ensure_stage()?;
        self.state.borrow_mut().created = true;
        Ok(())
    }

    /// A module extension's working directory is created on first use
    /// (`makeDirectories`/`createDirectory`), under an exclusive lock that
    /// serializes native processes sharing the cache; a stale directory left
    /// by an interrupted process is deleted first. Repository stages exist.
    fn ensure_stage(&self) -> anyhow::Result<()> {
        if self.kind != Kind::ModuleExtension || self.state.borrow().lock.is_some() {
            return Ok(());
        }
        use fs2::FileExt;
        let parent = self
            .stage
            .parent()
            .ok_or_else(|| anyhow!("module extension working directory has no parent"))?;
        fs::create_dir_all(parent)?;
        let mut name = self.stage.file_name().unwrap_or_default().to_owned();
        name.push(".lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(parent.join(name))?;
        lock.lock_exclusive()
            .map_err(|error| anyhow!("cannot lock module extension working directory: {error}"))?;
        remove_tree(&self.stage)?;
        fs::create_dir(&self.stage)?;
        self.state.borrow_mut().lock = Some(lock);
        Ok(())
    }

    /// toRepoCacheFriendlyPath (java:1656-1704): the key `path` is recorded
    /// under, or None when it is not watched. A module extension may not
    /// watch paths outside the workspace and the external repositories
    /// (`allowWatchingPathsOutsideWorkspace = false`,
    /// ModuleExtensionContext.java:85); native external repositories live in
    /// the repository cache (Bazel: `<output_base>/external`).
    fn watch_key(&self, path: &[u8], watch: ShouldWatch) -> anyhow::Result<Option<String>> {
        if watch == ShouldWatch::No {
            return Ok(None);
        }
        if self.under_stage(path).is_some() {
            if watch == ShouldWatch::Auto {
                return Ok(None);
            }
            bail!("attempted to watch path under working directory");
        }
        if self.kind == Kind::ModuleExtension && !self.in_workspace(path) && !self.in_external(path)
        {
            if watch == ShouldWatch::Auto {
                return Ok(None);
            }
            bail!(
                "attempted to watch path outside workspace, but it's prohibited in the current context"
            );
        }
        Ok(Some(self.friendly(path)))
    }

    fn in_workspace(&self, path: &[u8]) -> bool {
        let workspace = self.workspace.as_os_str().as_bytes();
        path.strip_prefix(workspace)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(b"/"))
    }

    fn in_external(&self, path: &[u8]) -> bool {
        let Ok(cache) = self.repositories.source_cache_path() else {
            return false;
        };
        let canonical = fs::canonicalize(cache).unwrap_or_else(|_| cache.to_path_buf());
        [cache, canonical.as_path()].iter().any(|root| {
            path.strip_prefix(root.as_os_str().as_bytes())
                .is_some_and(|rest| rest.starts_with(b"/"))
        })
    }

    /// RepoRecordedInput.File of `path` (java:1721-1738).
    fn watch(&self, path: &[u8], watch: ShouldWatch) -> anyhow::Result<()> {
        let Some(key) = self.watch_key(path, watch)? else {
            return Ok(());
        };
        let state = file_state(&path_buf(path))?;
        self.record_bazel_file(&key, path, &state);
        self.record(key, "file", state)
    }

    /// The recorded spelling of a watched or read host path: its
    /// RepoCacheFriendlyPath (toRepoCacheFriendlyPath, java:1656-1704) —
    /// `@@//rel` in the workspace, `@@NAME//rel` in external repository
    /// NAME — or the absolute path outside every repository. Paths into the
    /// repository store used to stay absolute (or `@@//<cache>/...` with the
    /// cache inside the workspace), so recorded inputs, and with them the
    /// identities of repositories reading other repositories' files,
    /// depended on the cache location (review i20). A store path no
    /// repository root contains is still absolute — never `@@//<cache>/...`,
    /// with the cache inside the workspace as outside it
    /// (`RepositorySet::friendly_repository`).
    fn friendly(&self, path: &[u8]) -> String {
        match self.repositories.friendly_repository(&path_buf(path)) {
            Some((name, relative)) => {
                format!("@@{name}//{}", display(relative.as_os_str().as_bytes()))
            }
            None => display(path),
        }
    }

    /// Append a module extension's Bazel recorded input (first value wins).
    pub(crate) fn record_bazel(&self, input: crate::lockfile_extensions::RecordedInput) {
        if self.kind == Kind::ModuleExtension {
            crate::lockfile_extensions::push_recorded(
                &mut self.state.borrow_mut().inputs.bazel,
                input,
            );
        }
    }

    fn bazel_unsupported(&self, reason: String) {
        if self.kind == Kind::ModuleExtension {
            let mut state = self.state.borrow_mut();
            if state.inputs.bazel_unsupported.is_none() {
                state.inputs.bazel_unsupported = Some(reason);
            }
        }
    }

    /// The lockfile key of a watched host path (`watch_key` admitted it and
    /// formed `friendly`): its RepoCacheFriendlyPath, None for a path in no
    /// repository.
    fn bazel_friendly(friendly: &str) -> Option<String> {
        friendly.starts_with("@@").then(|| friendly.to_owned())
    }

    /// A file observation of a watched host path, for the lockfile.
    fn record_bazel_file(&self, friendly: &str, path: &[u8], state: &str) {
        if self.kind != Kind::ModuleExtension {
            return;
        }
        match (
            Self::bazel_friendly(friendly),
            crate::lockfile_extensions::file_marker_value(state),
        ) {
            (Some(bazel), Ok(value)) => {
                self.record_bazel(crate::lockfile_extensions::RecordedInput::file(
                    &bazel, value,
                ));
            }
            (None, _) => self.bazel_unsupported(format!(
                "the module extension watched {}, which is in no known repository",
                display(path)
            )),
            (_, Err(reason)) => self.bazel_unsupported(reason),
        }
    }

    /// RepoRecordedInput.File of a file named by a canonical label.
    fn record_label(&self, canonical: String, state: String) -> anyhow::Result<()> {
        if self.kind == Kind::ModuleExtension {
            match (
                Label::parse_canonical(&canonical),
                crate::lockfile_extensions::file_marker_value(&state),
            ) {
                (Ok(label), Ok(value)) => {
                    let path = crate::lockfile_extensions::FriendlyPath::of_label(&label);
                    self.record_bazel(crate::lockfile_extensions::RecordedInput::file(
                        &path.to_string(),
                        value,
                    ));
                }
                (Err(error), _) | (_, Err(error)) => self.bazel_unsupported(error),
            }
        }
        self.record_input(FileInput {
            path: canonical,
            kind: "file",
            state,
            label: true,
        })
    }

    fn record(&self, path: String, kind: &'static str, state: String) -> anyhow::Result<()> {
        self.record_input(FileInput {
            path,
            kind,
            state,
            label: false,
        })
    }

    fn record_input(&self, input: FileInput) -> anyhow::Result<()> {
        let mut current = self.state.borrow_mut();
        let files = &mut current.inputs.files;
        if !files.contains(&input) {
            files.push(input);
        }
        Ok(())
    }

    fn facility(&self, name: &'static str) {
        self.state.borrow_mut().inputs.host_facilities.insert(name);
    }

    /// Read a template/read() argument. Labels use the strict source profile;
    /// strings and paths are host paths read like Bazel (symlinks followed).
    fn read_input(
        &self,
        value: Value<'_>,
        method: &str,
        parameter: &str,
        watch: ShouldWatch,
        directory_error: &str,
    ) -> anyhow::Result<(String, Vec<u8>)> {
        if let Some(canonical) = unpack_label(value) {
            let bytes = self.read_label(&canonical)?;
            // Label templates are already keyed by their `templates` entry
            // (canonical label and full bytes, recorded regardless of watch
            // mode), so content changes rekey without a `files` entry; adding
            // one would change the pre-N21 identity of existing repositories.
            // Label read() calls had no previous identity and are recorded.
            if method != "template" && watch != ShouldWatch::No {
                self.record_label(canonical.clone(), format!("file:{}", sha256(&bytes)))?;
            }
            return Ok((canonical, bytes));
        }
        let path = self.path_of(value, method, parameter)?;
        let host = path_buf(&path);
        if self.under_stage(&path) == Some(b"".as_slice()) && !self.state.borrow().created {
            // Bazel's repository directory does not exist yet, so its isDir()
            // check passes; template() then creates it (makeDirectories of the
            // output) before reading, read() does not.
            let errno = if method == "template" { 21 } else { 2 };
            return Err(file_not_found(
                &host,
                &std::io::Error::from_raw_os_error(errno),
            ));
        }
        if fs::metadata(&host).is_ok_and(|metadata| metadata.is_dir()) {
            bail!("{directory_error}: {}", display(&path));
        }
        self.watch(&path, watch)?;
        let bytes = read_file(&host)?;
        let source = match self.under_stage(&path) {
            Some(relative) => format!("stage:{}", display(relative)),
            None => self.friendly(&path),
        };
        Ok((source, bytes))
    }

    fn client_environment(
        &self,
    ) -> anyhow::Result<Arc<crate::repository_environment::ClientEnvironment>> {
        self.repositories
            .environment()
            .client_environment()
            .map_err(|error| anyhow!("{error}"))
    }

    /// which() search (java:2336-2383): absolute PATH entries only, an
    /// existing regular file (symlinks followed) with the owner execute bit.
    fn which(&self, program: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        let environment = self.client_environment()?;
        let Some(search) = environment.get(b"PATH") else {
            return Ok(None);
        };
        // Java String.trim(): strip every char <= U+0020 at both ends.
        let program = latin1(
            latin1_text(program).trim_matches(|c: char| c <= ' '),
            "program",
        )?;
        for directory in search.split(|byte| *byte == b':') {
            if directory.first() != Some(&b'/') {
                continue;
            }
            let mut candidate = normalize(directory, b"");
            candidate = normalize(&candidate, &program);
            if let Ok(metadata) = fs::metadata(path_buf(&candidate)) {
                use std::os::unix::fs::PermissionsExt;
                if metadata.is_file() && metadata.permissions().mode() & 0o100 != 0 {
                    return Ok(Some(candidate));
                }
            }
        }
        Ok(None)
    }

    fn client(&self) -> Result<reqwest::blocking::Client, TypedIoError> {
        let mut state = self.state.borrow_mut();
        if state.client.is_none() {
            state.client = Some(transport::client()?);
        }
        Ok(state.client.clone().expect("initialized"))
    }

    /// Stream the first completing mirror into `target` (a file, or a file
    /// named by the URL basename in a directory: Bazel's DownloadManager names
    /// the downloaded file after the URL), hashing while writing
    /// (extension_download.rs: redirects, retry table, TLS policy, timeouts).
    /// Each failed mirror is reported like HttpDownloader.recordFailure
    /// (HttpDownloader.java:217-240): `WARNING: Download from URL failed:
    /// class CLASS MESSAGE`, also when `allow_fail` later tolerates it.
    fn fetch_to(
        &self,
        urls: &[reqwest::Url],
        expected: Option<&Checksum>,
        sri: bool,
        headers: &transport::RequestHeaders,
        target: DownloadTarget<'_>,
    ) -> Result<(transport::Fetched, PathBuf), DownloadFailure> {
        if !self.repositories.repository_downloads_enabled() {
            // DownloadManager.java:331-333, after the cache lookups.
            return Err(DownloadFailure {
                error: TypedIoError::Completion(format!(
                    "{DISABLED_PREFIX}Failed to download {}: download is disabled.",
                    self.identifying()
                )),
                last: None,
            });
        }
        let client = self
            .client()
            .map_err(|error| DownloadFailure { error, last: None })?;
        let mut path = PathBuf::new();
        let mut last = None;
        let (keep_failed, directory, file) = match target {
            DownloadTarget::Directory(directory) => (false, Some(directory), None),
            DownloadTarget::File(file) => (true, None, Some(file)),
        };
        let fetched = transport::fetch_to_file(
            &client,
            urls,
            expected,
            sri,
            headers,
            &mut |url| {
                path = match (directory, file) {
                    (Some(directory), _) => directory.join(storage_name(url)),
                    (None, Some(file)) => file.to_path_buf(),
                    (None, None) => unreachable!("one download target"),
                };
                path.clone()
            },
            keep_failed,
            &mut |url, failure| {
                let warning = format!(
                    "WARNING: Download from {url} failed: class {} {}",
                    failure.class, failure.message
                );
                if !crate::repository_speculation::defer_stderr(&crate::ui::marked(&warning)) {
                    crate::ui::note(&warning);
                }
                last = Some(failure.clone());
            },
        );
        match fetched {
            Ok(fetched) => Ok((fetched, path)),
            Err(error) => Err(DownloadFailure { error, last }),
        }
    }

    /// The transfer phase of download() (java:841-895, completeDownload
    /// java:658-697): errors here surface from `wait()` for `block = False`.
    fn download(
        &self,
        request: &DownloadRequest,
        output: &[u8],
        relative: &str,
        executable: bool,
        allow_fail: bool,
        canonical_id: &str,
        block: bool,
    ) -> anyhow::Result<Started> {
        self.facility("download");
        let urls = &request.urls;
        // makeDirectories(outputPath) (java:862): parents are real directories.
        let target = if relative.is_empty() {
            // A module extension's working directory does not exist before
            // its first write, so download(output='') creates a FILE there.
            if self.kind == Kind::ModuleExtension && fs::symlink_metadata(&self.stage).is_err() {
                self.ensure_stage()?;
                fs::remove_dir(&self.stage)?;
                self.state.borrow_mut().created = true;
            } else {
                self.mark_created()?;
            }
            self.stage.clone()
        } else {
            self.mark_created()?;
            // A pending download writes after this returns: the directory
            // stays writable.
            self.writable_holding(relative).keep();
            // makeDirectories failing (e.g. the working directory became a
            // file) is an IOException of the download itself (java:858-874):
            // `success = False` under allow_fail.
            match self.output_parents(relative) {
                Ok(target) => target,
                Err(_) if allow_fail => return Ok(Started::Done(DownloadOutcome::Failed)),
                Err(error) => return Err(error),
            }
        };
        // An output that is a directory is still requested from every
        // mirror; each fails opening the output (HttpDownloader:
        // `FileNotFoundException: PATH (Is a directory)`), see fetch_to_file.
        // DownloadManager.java:247-261: with a checksum, an existing output
        // whose digest already matches is used without any download (also
        // with downloads disabled: the disable check comes later, java:331).
        let reused = request.expected.as_ref().and_then(|expected| {
            fs::metadata(&target)
                .is_ok_and(|metadata| metadata.is_file())
                .then(|| expected.algorithm.hash_file(&target).ok())
                .flatten()
                .filter(|(digest, _)| *digest == expected.bytes)
                .map(|(_, size)| (expected.clone(), size))
        });
        let cached = match (&reused, &request.expected) {
            (None, Some(expected)) => self
                .cache_get(expected, canonical_id, &target)?
                .map(|size| (expected.clone(), size)),
            _ => None,
        };
        if reused.is_none()
            && cached.is_none()
            && !block
            && let Some(slot) = self.submit_download(
                request,
                output,
                &target,
                executable,
                allow_fail,
                canonical_id,
            )
        {
            return Ok(Started::Pending(slot));
        }
        let (checksum, size) = match reused.or(cached) {
            Some(found) => found,
            None => match self.fetch_to(
                urls,
                request.expected.as_ref(),
                request.sri,
                &request.headers,
                DownloadTarget::File(&target),
            ) {
                Ok((fetched, _)) => {
                    match self.cache_put(&fetched.checksum, &target, canonical_id) {
                        Ok(()) => {}
                        Err(_) if allow_fail => return Ok(Started::Done(DownloadOutcome::Failed)),
                        Err(error) => return Err(error),
                    }
                    (fetched.checksum, fetched.size)
                }
                Err(failure)
                    if allow_fail && matches!(failure.error, TypedIoError::Completion(_)) =>
                {
                    return Ok(Started::Done(DownloadOutcome::Failed));
                }
                Err(failure) => return Err(failure.into_error(urls, output)),
            },
        };
        if executable {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&target)?.permissions().mode();
            fs::set_permissions(&target, fs::Permissions::from_mode(mode | 0o111))?;
            // The download streamed without the spawn lock; re-create the file
            // as an inode no child can hold open for writing.
            crate::spawn_lock::seal(&target)?;
        }
        if let Some(error) = &request.deferred {
            bail!("Checksum error in {}: {error}", self.identifying());
        }
        Ok(Started::Done(DownloadOutcome::Done(checksum, size)))
    }

    /// The transfer of a `download(block = False)` on the pending-download
    /// pool (pending_downloads), when it may overlap the callback; `None`:
    /// transfer at the call, as a blocking download does.
    fn submit_download(
        &self,
        request: &DownloadRequest,
        output: &[u8],
        target: &Path,
        executable: bool,
        allow_fail: bool,
        canonical_id: &str,
    ) -> Option<Arc<pending_downloads::Slot>> {
        if !pending_downloads::enabled()
            || !self.repositories.repository_downloads_enabled()
            // A credential helper's failure is a warning printed while the
            // request is formed (crate::credentials::download_credentials).
            || request
                .urls
                .iter()
                .any(|url| crate::credentials::helper_applies(url.as_str()))
            || fs::symlink_metadata(target).is_ok_and(|metadata| metadata.is_dir())
        {
            return None;
        }
        let client = self.client().ok()?;
        let deferred = request
            .deferred
            .as_ref()
            .map(|error| format!("Checksum error in {}: {error}", self.identifying()));
        Some(pending_downloads::submit(pending_downloads::Request {
            client,
            urls: &request.urls,
            expected: request.expected.as_ref(),
            sri: request.sri,
            headers: &request.headers,
            deferred: deferred.as_deref(),
            target,
            output,
            allow_fail,
            executable,
            cache: self
                .repositories
                .source_cache_path()
                .ok()
                .map(Path::to_path_buf),
            canonical_id,
        }))
    }

    /// The repository download cache (DownloadCache.java, `--repository_cache`):
    /// blobs keyed by checksum under `<cache>/downloads/<algorithm>/<hex>`
    /// (shared with registry archives), canonical ids as marker files.
    fn cache_paths(&self, checksum: &Checksum, canonical_id: &str) -> Option<(PathBuf, PathBuf)> {
        let root = self.repositories.source_cache_path().ok()?;
        Some(cache_paths_at(root, checksum, canonical_id))
    }

    /// DownloadManager.java:264-277 + DownloadCache.get (java:146-221): a
    /// verified blob with the expected checksum (and canonical id, when one
    /// is given) is copied to `target` without any network access, even when
    /// downloads are disabled.
    fn cache_get(
        &self,
        expected: &Checksum,
        canonical_id: &str,
        target: &Path,
    ) -> anyhow::Result<Option<u64>> {
        let Some((blob, marker)) = self.cache_paths(expected, canonical_id) else {
            return Ok(None);
        };
        let native = |blob: &Path| -> Option<u64> {
            if !fs::symlink_metadata(blob).is_ok_and(|metadata| metadata.is_file()) {
                return None;
            }
            let (digest, size) = expected.algorithm.hash_file(blob).ok()?;
            (digest == expected.bytes && (canonical_id.is_empty() || marker.exists()))
                .then_some(size)
        };
        let size = match native(&blob) {
            Some(size) => {
                // Shared with genuine Bazel (`--repository_cache`,
                // crate::repository_cache).
                crate::repository_cache::offer(
                    expected.algorithm,
                    &expected.bytes,
                    canonical_id,
                    &blob,
                );
                size
            }
            // The shared `--repository_cache` (genuine's layout) serves a
            // native miss; the hit enters the native store with its marker.
            None if crate::repository_cache::fetch_into(
                expected.algorithm,
                &expected.bytes,
                canonical_id,
                &blob,
            ) =>
            {
                if !canonical_id.is_empty()
                    && let Some(parent) = marker.parent()
                    && fs::create_dir_all(parent).is_ok()
                {
                    let _ = OpenOptions::new()
                        .create(true)
                        .truncate(false)
                        .write(true)
                        .open(&marker);
                }
                match native(&blob) {
                    Some(size) => size,
                    None => return Ok(None),
                }
            }
            None => return Ok(None),
        };
        // DownloadCache.get touches the entry it serves (java:214): its
        // last use (docs/CACHE_GC.md).
        crate::cache_gc::note_used_path(&blob);
        // DownloadManager.java:266-276: a failing copy is ignored ("We'll
        // just download again"), e.g. when the output is a directory.
        let copied = (|| -> std::io::Result<()> {
            if fs::symlink_metadata(target).is_ok_and(|metadata| !metadata.is_dir()) {
                fs::remove_file(target)?;
            }
            // DownloadCache.get: a copy, or a hard link under
            // --experimental_repository_cache_hardlinks.
            crate::repository_cache::materialize_hit(&blob, target)
        })();
        Ok(copied.ok().map(|()| size))
    }

    /// DownloadManager.java:381-386: every completed download enters the
    /// cache, keyed by the given checksum or by its SHA-256. A failing store
    /// is not caught there: it is an IOException of the download itself
    /// (`success = False` under allow_fail, else the download's error). Only
    /// the distdir path (not modelled) downgrades it to a warning (java:310-315).
    fn cache_put(
        &self,
        checksum: &Checksum,
        source: &Path,
        canonical_id: &str,
    ) -> anyhow::Result<()> {
        cache_put_at(
            self.repositories.source_cache_path().ok(),
            checksum,
            source,
            canonical_id,
        )
    }

    /// Stream-extract the archive file into `output` (stage-relative),
    /// merging like Bazel's decompressors writing into an existing directory.
    /// `output_created`: download_and_extract created the output first
    /// (StarlarkBaseExternalContext.java:1120).
    fn extract_archive(
        &self,
        archive_file: &Path,
        kind: &str,
        strip_prefix: &str,
        output: &str,
        archive: &[u8],
        rename_files: &[(Vec<u8>, Vec<u8>)],
        output_created: bool,
    ) -> anyhow::Result<()> {
        if kind == "ar" {
            return self.extract_ar_archive(
                archive_file,
                output,
                archive,
                rename_files,
                output_created,
            );
        }
        let normalized = normalized_prefix(strip_prefix);
        self.mark_created()?;
        let destination = if output.is_empty() {
            self.stage.clone()
        } else {
            let path = parents(&self.stage, output, true)?;
            match fs::symlink_metadata(&path) {
                Ok(metadata) => ensure!(
                    metadata.file_type().is_dir(),
                    "extraction output {} must be a real directory",
                    path.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&path)?
                }
                Err(error) => return Err(error.into()),
            }
            path
        };
        let temporary = tempfile::Builder::new()
            .prefix(".extract-")
            .tempdir_in(&self.stage)?;
        // ZipReader.getFilename (java.io.File.getName): the archive's basename
        // names it in the zip link-escape error (ZipDecompressor.java:154-163).
        let archive_name = display(archive.rsplit(|byte| *byte == b'/').next().unwrap_or(b""));
        match crate::archive::extract_file(
            archive_file,
            kind,
            &normalized,
            temporary.path(),
            &archive_name,
        ) {
            Ok(()) => {}
            Err(crate::archive::Failure::PrefixNotFound(available)) => {
                // Bazel lists the first segment of every multi-segment entry
                // seen before the prefix was found, collected in the same pass.
                return Err(prefix_not_found(archive, strip_prefix, &available));
            }
            Err(crate::archive::Failure::Io(error)) => {
                return Err(extraction_io_error(archive, &error));
            }
            Err(crate::archive::Failure::Other(error)) => {
                if crate::archive::is_bazel_extraction_io_error(&error) {
                    return Err(extraction_io_error(archive, &error));
                }
                return Err(anyhow::Error::msg(error));
            }
        }
        merge(temporary.path(), &destination)?;
        temporary.close()?;
        Ok(())
    }

    /// ArFunction (docs/AR_ARCHIVES.md): members are extracted into a
    /// private staging directory with Bazel's view of the output (its
    /// errors name the output itself) and merged into the output, which,
    /// like Bazel's, `extract` creates only when a member is written.
    fn extract_ar_archive(
        &self,
        archive_file: &Path,
        output: &str,
        archive: &[u8],
        rename_files: &[(Vec<u8>, Vec<u8>)],
        output_created: bool,
    ) -> anyhow::Result<()> {
        use crate::archive::ArDestination;
        self.ensure_stage()?;
        let destination = if output.is_empty() {
            self.stage.clone()
        } else {
            self.stage.join(output)
        };
        let state = if output_created {
            ArDestination::Directory
        } else if output.is_empty() {
            if self.state.borrow().created {
                ArDestination::Directory
            } else {
                ArDestination::Missing
            }
        } else {
            match fs::symlink_metadata(&destination) {
                Ok(metadata) if metadata.is_dir() => ArDestination::Directory,
                Ok(_) => ArDestination::File,
                Err(_) => ArDestination::Missing,
            }
        };
        let temporary = tempfile::Builder::new()
            .prefix(".extract-")
            .tempdir_in(&self.stage)?;
        let options = crate::archive::ArOptions {
            shown: &destination,
            state,
            rename_files,
        };
        match crate::archive::extract_ar_file(archive_file, temporary.path(), &options) {
            Ok(()) => {}
            Err(crate::archive::Failure::Io(error)) => {
                return Err(extraction_io_error(archive, &error));
            }
            Err(error) => return Err(anyhow::Error::msg(error.to_string())),
        }
        let extracted = fs::read_dir(temporary.path())?.next().is_some();
        if extracted || output_created {
            self.mark_created()?;
            if !output.is_empty() {
                let path = parents(&self.stage, output, true)?;
                match fs::symlink_metadata(&path) {
                    Ok(metadata) => ensure!(
                        metadata.file_type().is_dir(),
                        "extraction output {} must be a real directory",
                        path.display()
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        fs::create_dir(&path)?
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            merge(temporary.path(), &destination)?;
        }
        temporary.close()?;
        Ok(())
    }
}

impl Drop for Context {
    /// A module extension's working directory is deleted whatever the
    /// outcome (ModuleExtensionContext.shouldDeleteWorkingDirectoryOnClose,
    /// java:96-101), while its lock is still held.
    fn drop(&mut self) {
        if self.kind == Kind::ModuleExtension && self.state.borrow().lock.is_some() {
            let _ = remove_tree(&self.stage);
        }
    }
}

/// Delete a tree, making directories writable first (a callback may have
/// extracted read-only directories), never following symlinks.
pub(crate) fn remove_tree(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(metadata) if !metadata.is_dir() => return fs::remove_file(path),
        Ok(metadata) => {
            let mode = metadata.permissions().mode();
            if mode & 0o700 != 0o700 {
                fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))?;
            }
        }
    }
    for entry in fs::read_dir(path)? {
        remove_tree(&entry?.path())?;
    }
    fs::remove_dir(path)
}

/// Where a download streams to.
#[derive(Clone, Copy)]
enum DownloadTarget<'a> {
    /// A file named by the URL basename inside this directory.
    Directory(&'a Path),
    /// Exactly this file (repository_ctx.download's output path).
    File(&'a Path),
}

/// A failed download: the typed error and, when mirrors were tried, Bazel's
/// description of the last mirror's failure.
/// The repository download cache's blob and canonical-id marker paths under
/// `root` (DownloadCache.java, `--repository_cache`): blobs keyed by checksum
/// under `<root>/downloads/<algorithm>/<hex>` (shared with registry
/// archives), canonical ids as marker files.
fn cache_paths_at(root: &Path, checksum: &Checksum, canonical_id: &str) -> (PathBuf, PathBuf) {
    let algorithm = checksum.algorithm.name();
    let key = transport::hex(&checksum.bytes);
    let marker = root
        .join("download_ids")
        .join(algorithm)
        .join(&key)
        .join(format!("id-{}", transport::sha256(canonical_id.as_bytes())));
    (
        crate::materializer::download_cache_blob(root, algorithm, &key),
        marker,
    )
}

/// DownloadManager.java:381-386: every completed download enters the cache
/// under `root` (none: no cache), keyed by the given checksum or by its
/// SHA-256. A failing store is not caught there: it is an IOException of the
/// download itself (`success = False` under allow_fail, else the download's
/// error). Only the distdir path (not modelled) downgrades it to a warning
/// (java:310-315).
fn cache_put_at(
    root: Option<&Path>,
    checksum: &Checksum,
    source: &Path,
    canonical_id: &str,
) -> anyhow::Result<()> {
    let Some((blob, marker)) = root.map(|root| cache_paths_at(root, checksum, canonical_id)) else {
        return Ok(());
    };
    let store = || -> std::io::Result<()> {
        let directory = blob.parent().expect("cache blob parent");
        fs::create_dir_all(directory)?;
        let present = fs::symlink_metadata(&blob).is_ok_and(|metadata| metadata.is_file())
            && checksum
                .algorithm
                .hash_file(&blob)
                .is_ok_and(|(digest, _)| digest == checksum.bytes);
        if !present {
            let mut temp = tempfile::NamedTempFile::new_in(directory)?;
            std::io::copy(&mut fs::File::open(source)?, temp.as_file_mut())?;
            temp.persist(&blob).map_err(|error| error.error)?;
        }
        if !canonical_id.is_empty() {
            fs::create_dir_all(marker.parent().expect("marker parent"))?;
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&marker)?;
        }
        Ok(())
    };
    store().map_err(|error| {
        anyhow!(
            "java.io.IOException: {} ({})",
            blob.display(),
            io_reason(&error)
        )
    })?;
    // The shared `--repository_cache` in genuine's layout
    // (crate::repository_cache): the same IOException class on failure.
    crate::repository_cache::put(checksum.algorithm, &checksum.bytes, canonical_id, &blob).map_err(
        |error| {
            anyhow!(
                "java.io.IOException: {} ({})",
                crate::repository_cache::path()
                    .map(Path::display)
                    .map(|path| path.to_string())
                    .unwrap_or_default(),
                io_reason(&error)
            )
        },
    )
}

struct DownloadFailure {
    error: TypedIoError,
    last: Option<transport::BazelFailure>,
}

impl DownloadFailure {
    /// The repository-level error: HttpDownloader's aggregated
    /// `Error downloading [URLS] to PATH: LAST_MESSAGE` (java:243-262) once
    /// mirrors were tried; native transport text otherwise.
    fn into_error(self, urls: &[reqwest::Url], path: &[u8]) -> anyhow::Error {
        match (&self.error, self.last) {
            (TypedIoError::Completion(_), Some(last)) => download_error(urls, path, &last.message),
            (TypedIoError::Completion(text), None) if text.starts_with(DISABLED_PREFIX) => {
                anyhow!("{text}")
            }
            _ => anyhow!("{}", self.error),
        }
    }
}

/// The IOException of a disabled download (DownloadManager.java:331-333).
const DISABLED_PREFIX: &str = "java.io.IOException: ";

/// On-disk name of a downloaded file: the URL's last path segment, or a
/// fixed name when that segment cannot name a file.
fn storage_name(url: &reqwest::Url) -> String {
    let name = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or("");
    if name.is_empty() || name == "." || name == ".." {
        "download".to_owned()
    } else {
        name.to_owned()
    }
}

/// Move every extracted entry into `destination`, replacing files and
/// symlinks and merging directories; never follows destination symlinks.
fn merge(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let _writable = crate::nofollow_modes::WritableDirectory::of(destination);
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        match fs::symlink_metadata(&to) {
            Ok(existing) if kind.is_dir() && existing.file_type().is_dir() => {
                merge(&from, &to)?;
                continue;
            }
            Ok(existing) if existing.file_type().is_dir() => {
                bail!("cannot extract {} over a directory", to.display())
            }
            Ok(_) => fs::remove_file(&to)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        fs::rename(&from, &to)?;
    }
    Ok(())
}

fn replace_bytes(input: &[u8], key: &[u8], replacement: &[u8]) -> anyhow::Result<Vec<u8>> {
    if key.is_empty() {
        return Ok(input.to_vec());
    }
    let mut output = Vec::new();
    let mut offset = 0;
    while offset < input.len() {
        let bytes = if input[offset..].starts_with(key) {
            offset += key.len();
            replacement
        } else {
            offset += 1;
            &input[offset - 1..offset]
        };
        output.extend_from_slice(bytes);
    }
    Ok(output)
}

/// Bazel `Dict.cast(x, String.class, String.class, what)`.
fn string_dict(value: Option<Value<'_>>, what: &str) -> anyhow::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let dict = DictRef::from_value(value).ok_or_else(|| {
        anyhow!(
            "got value of type '{}' for '{what}', want 'dict'",
            value.get_type()
        )
    })?;
    let mut entries = Vec::new();
    for (key, item) in dict.iter() {
        let (Some(key), Some(item)) = (key.unpack_str(), item.unpack_str()) else {
            bail!(
                "got dict<{}, {}> for '{what}', want dict<string, string>",
                key.get_type(),
                item.get_type()
            );
        };
        entries.push((latin1(key, what)?, latin1(item, what)?));
    }
    Ok(entries)
}

fn renamed_strip_prefix<'a>(
    method: &str,
    strip_prefix: &'a str,
    old: &'a str,
) -> anyhow::Result<&'a str> {
    // StarlarkBaseExternalContext.java:1436-1448.
    if old.is_empty() {
        return Ok(strip_prefix);
    }
    if strip_prefix.is_empty() {
        return Ok(old);
    }
    bail!(
        "{method}() got multiple values for parameter 'strip_prefix' (via compatibility alias 'stripPrefix')"
    )
}

fn validate_stripping(
    method: &str,
    strip_prefix: &str,
    strip_components: i32,
) -> anyhow::Result<()> {
    // StarlarkBaseExternalContext.java:1450-1464.
    ensure!(
        strip_components >= 0,
        "{method}() has an invalid argument for 'strip_components': {strip_components}. Must be non-negative."
    );
    ensure!(
        strip_prefix.is_empty() || strip_components == 0,
        "{method}() got multiple strip values. Only one of 'strip_prefix' or 'strip_components' can be set"
    );
    Ok(())
}

/// What the native extractor does not implement once the decompressor is
/// known: `strip_components` and `rename_files` for zip and tar. ArFunction
/// ignores `strip_components` and applies `rename_files` (ArFunction.java:
/// 45-53).
fn native_extraction_limits(
    method: &str,
    kind: &str,
    strip_components: i32,
    rename_files: &[(Vec<u8>, Vec<u8>)],
) -> anyhow::Result<()> {
    if kind == "ar" {
        return Ok(());
    }
    ensure!(
        strip_components == 0,
        "{method}() strip_components is not supported by the native extractor"
    );
    ensure!(
        rename_files.is_empty(),
        "{method}() rename_files is not supported by the native extractor"
    );
    Ok(())
}

/// A repository_ctx or module_ctx receiver.
fn external_receiver(this: Value<'_>) -> bool {
    values::RepositoryCtx::from_value(this).is_some() || crate::extension_ctx::is_module_ctx(this)
}

fn exec_receiver(this: Value<'_>) -> anyhow::Result<()> {
    ensure!(external_receiver(this), "expected repository_ctx");
    Ok(())
}

starlark::methods_static!(REPOSITORY_METHODS = all_repository_methods);
starlark::methods_static!(PATH_METHODS = path_methods);
starlark::methods_static!(PENDING_DOWNLOAD_METHODS = pending_download_methods);

#[starlark_module]
fn pending_download_methods(builder: &mut MethodsBuilder) {
    /// PendingDownload.await (java:640-651): the blocking call's result.
    fn wait<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let pending = this
            .downcast_ref::<PendingDownload>()
            .ok_or_else(|| anyhow!("expected PendingDownload"))?;
        let outcome = match &pending.outcome {
            PendingOutcome::Ready(outcome) => outcome.clone(),
            PendingOutcome::Queued(slot) => pending_downloads::wait(slot),
        };
        match outcome {
            Ok(outcome) => Ok(outcome.alloc(heap)),
            Err(message) => Err(anyhow::Error::msg(message)),
        }
    }
}

/// A private, value-free marker carried intact by ErrorKind::Native. Only this
/// method's failures suppress source diagnostics; user messages cannot opt in.
#[derive(Debug)]
pub(crate) enum GetenvError {
    Receiver,
    Phase,
    Arguments,
    Name,
    Default,
    Environment(EnvError),
}

impl fmt::Display for GetenvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Receiver => f.write_str("getenv requires a repository_ctx receiver"),
            Self::Phase => f.write_str("repository_ctx is only available during repository execution"),
            Self::Arguments => f.write_str("repository getenv requires one or two positional arguments and no keyword arguments"),
            Self::Name => f.write_str("repository getenv name must be a string"),
            Self::Default => f.write_str("repository getenv default must be a string or None"),
            Self::Environment(error) => fmt::Display::fmt(error, f),
        }
    }
}
impl std::error::Error for GetenvError {}
impl From<EnvError> for GetenvError {
    fn from(error: EnvError) -> Self {
        Self::Environment(error)
    }
}

fn getenv_impl<'v>(
    this: Value<'v>,
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Result<Value<'v>, GetenvError> {
    // Validate the genuine receiver and phase before inspecting user arguments.
    if !external_receiver(this) {
        return Err(GetenvError::Receiver);
    }
    // The environment: nothing a pending download changes.
    let context = context_unflushed(eval).map_err(|_| GetenvError::Phase)?;
    let mut observed = None;
    let value = getenv_observed(
        &context.environment,
        context.repositories.environment(),
        args,
        eval.heap(),
        &mut observed,
    )?;
    if let Some((name, captured)) = observed {
        context.record_bazel(crate::lockfile_extensions::RecordedInput::env(
            &name, captured,
        ));
    }
    Ok(value)
}

/// Shared `getenv(name, default=None)` (StarlarkBaseExternalContext.java:
/// 1535-1565) for repository_ctx and module_ctx: positional-only binding,
/// a recorded RepoRecordedInput.EnvVar observation, missing differs from empty.
pub(crate) fn getenv_observed<'v>(
    watch: &RefCell<Watch>,
    snapshot: &crate::repository_environment::Snapshot,
    args: &Arguments<'v, '_>,
    heap: Heap<'v>,
    observed: &mut Option<(String, Option<String>)>,
) -> Result<Value<'v>, GetenvError> {
    // Discard binder errors without formatting or retaining their payloads.
    args.no_named_args().map_err(|_| GetenvError::Arguments)?;
    let mut positions = args.positions(heap).map_err(|_| GetenvError::Arguments)?;
    let name = positions.next().ok_or(GetenvError::Arguments)?;
    let default = positions.next().unwrap_or_else(Value::new_none);
    if positions.next().is_some() {
        return Err(GetenvError::Arguments);
    }
    let name = name.unpack_str().ok_or(GetenvError::Name)?;
    if !default.is_none() && default.unpack_str().is_none() {
        return Err(GetenvError::Default);
    }
    let entry = watch.borrow_mut().observe(snapshot, name)?;
    *observed = Some((
        name.to_owned(),
        match entry.captured()? {
            Captured::Absent => None,
            Captured::Value(value) => Some(value.to_string()),
        },
    ));
    match entry.captured()? {
        Captured::Absent => Ok(default),
        Captured::Value(value) => {
            watch.borrow_mut().charge_return(value)?;
            Ok(heap.alloc(value.as_ref()))
        }
    }
}

pub(crate) fn callback_error(error: starlark::Error) -> anyhow::Error {
    if let starlark::ErrorKind::Native(native) = error.kind()
        && let Some(private) = native.downcast_ref::<GetenvError>()
    {
        // Render the bounded marker itself, not arbitrary attached context or
        // the diagnostic's source/stack, which may contain rejected arguments.
        return anyhow!(private.to_string());
    }
    anyhow!(error.to_string())
}

/// Dict.cast(x, String.class, Object.class, what) (Dict.java:659-684): the
/// first entry whose key is not a string fails; values are unrestricted.
fn dict_cast_error(value: Value<'_>, what: &str) -> Option<String> {
    let Some(dict) = DictRef::from_value(value) else {
        return Some(format!("got {} for '{what}', want dict", value.get_type()));
    };
    dict.iter()
        .find(|(key, _)| key.unpack_str().is_none())
        .map(|(key, value)| {
            format!(
                "got dict<{}, {}> for '{what}', want dict<string, unknown>",
                key.get_type(),
                value.get_type()
            )
        })
}

/// getAuthContents + getAuthHeaders (java:350-415): `basic` and `pattern`
/// entries become an Authorization header for exactly that URL; entries of
/// other types are ignored.
fn auth_headers(
    method: &str,
    auth: Value<'_>,
) -> anyhow::Result<Vec<(reqwest::Url, transport::Headers)>> {
    use base64::Engine as _;
    let dict = DictRef::from_value(auth).ok_or_else(|| {
        anyhow!(
            "in call to {method}(), parameter 'auth' got value of type '{}', want 'dict'",
            auth.get_type()
        )
    })?;
    // Dict.cast(x, String.class, Dict.class, "auth") (Dict.java:659-684).
    if let Some((key, value)) = dict
        .iter()
        .find(|(key, value)| key.unpack_str().is_none() || DictRef::from_value(*value).is_none())
    {
        bail!(
            "got dict<{}, {}> for 'auth', want dict<string, dict>",
            key.get_type(),
            value.get_type()
        );
    }
    let mut result = Vec::new();
    for (key, value) in dict.iter() {
        let key = key.unpack_str().expect("checked");
        let entry = DictRef::from_value(value).expect("checked");
        let get = |name: &str| {
            entry
                .iter()
                .find(|(key, _)| key.unpack_str() == Some(name))
                .map(|(_, value)| value)
        };
        // Starlark/Java string conversion of an arbitrary value.
        let text = |value: Value<'_>| match value.unpack_str() {
            Some(text) => text.to_owned(),
            None => value.to_str(),
        };
        let header = match get("type").and_then(|value| value.unpack_str()) {
            Some("basic") => {
                let (Some(login), Some(password)) = (get("login"), get("password")) else {
                    bail!(
                        "Found request to do basic auth for {key} without 'login' and 'password' being provided."
                    );
                };
                let credentials = format!("{}:{}", text(login), text(password));
                // credentials.getBytes(UTF_8) of the Java string.
                let encoded =
                    base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes());
                format!("Basic {encoded}").into_bytes()
            }
            Some("pattern") => {
                let Some(pattern) = get("pattern") else {
                    bail!(
                        "Found request to do pattern auth for {key} without a pattern being provided"
                    );
                };
                let mut result = text(pattern);
                for component in ["password", "login"] {
                    let demarcated = format!("<{component}>");
                    if !result.contains(&demarcated) {
                        continue;
                    }
                    let Some(value) = get(component) else {
                        bail!(
                            "Auth pattern contains {demarcated} but it was not provided in auth dict."
                        );
                    };
                    result = result.replace(&demarcated, &text(value));
                }
                latin1(&result, "auth pattern")?
            }
            _ => continue,
        };
        // new URI(key): a key no request can match is never used.
        if let Ok(url) = reqwest::Url::parse(key) {
            result.push((url, vec![("Authorization".to_owned(), header)]));
        }
    }
    Ok(result)
}

/// getHeaderContents (java:417-441): string or sequence-of-string values.
fn header_contents(method: &str, headers: Value<'_>) -> anyhow::Result<transport::Headers> {
    if DictRef::from_value(headers).is_none() {
        bail!(
            "in call to {method}(), parameter 'headers' got value of type '{}', want 'dict'",
            headers.get_type()
        );
    }
    if let Some(error) = dict_cast_error(headers, "headers") {
        bail!(error);
    }
    let dict = DictRef::from_value(headers).expect("checked");
    let mut result = Vec::new();
    for (key, value) in dict.iter() {
        let name = key.unpack_str().expect("checked").to_owned();
        let values = if let Some(text) = value.unpack_str() {
            vec![text]
        } else if let Some(items) = ListRef::from_value(value)
            .map(|list| list.content().to_vec())
            .or_else(|| TupleRef::from_value(value).map(|tuple| tuple.content().to_vec()))
        {
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    item.unpack_str().ok_or_else(|| {
                        anyhow!(
                            "at index {index} of header values, got element of type {}, want string",
                            item.get_type()
                        )
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?
        } else {
            bail!(
                "headers argument must be a dict whose keys are string and whose values are either string or sequence of string"
            );
        };
        for value in values {
            result.push((name.clone(), latin1(value, "header value")?));
        }
    }
    Ok(result)
}

/// Validated `url`/checksum/auth/headers arguments of download() and
/// download_and_extract() (getUrls/checkAllUrls java:443-469,
/// validateChecksum java:512-541).
struct DownloadRequest {
    urls: Vec<reqwest::Url>,
    headers: transport::RequestHeaders,
    expected: Option<Checksum>,
    /// A malformed checksum, raised only after the download attempt
    /// (java:1094-1101, 1168-1170).
    deferred: Option<String>,
    sri: bool,
}

impl DownloadRequest {
    #[allow(clippy::too_many_arguments)]
    fn new<'v>(
        method: &str,
        url: Value<'v>,
        sha256: &str,
        integrity: &str,
        allow_fail: bool,
        auth: Option<Value<'v>>,
        headers: Option<Value<'v>>,
    ) -> anyhow::Result<Self> {
        // getAuthHeaders(getAuthContents(auth)), then getHeaderContents
        // (java:812-816), before the URLs.
        let mut request_headers = transport::RequestHeaders::default();
        if let Some(auth) = auth {
            request_headers.auth = auth_headers(method, auth)?;
        }
        if let Some(headers) = headers {
            request_headers.common = header_contents(method, headers)?;
        }
        let strings = url_strings(method, url)?;
        ensure!(
            allow_fail || !strings.is_empty(),
            "java.io.IOException: urls not set"
        );
        let checksum_given = !sha256.is_empty() || !integrity.is_empty();
        let mut urls = Vec::new();
        for text in &strings {
            let parsed = transport::validate_url(text).map_err(|error| anyhow!("{error}"))?;
            if checksum_given || parsed.scheme() != "http" {
                urls.push(parsed);
            }
        }
        ensure!(
            allow_fail || !urls.is_empty(),
            "java.io.IOException: No URLs left after removing plain http URLs due to missing checksum. Please provide either a checksum or an https download location."
        );
        if !sha256.is_empty() && !integrity.is_empty() {
            bail!("Expected either 'sha256' or 'integrity', but not both");
        }
        let deferred = checksum_text_error(sha256, integrity);
        let expected = if deferred.is_some() {
            None
        } else {
            transport::checksums(sha256, integrity)
                .map_err(|error| anyhow!("{error}"))?
                .expected
        };
        Ok(Self {
            urls,
            headers: request_headers,
            expected,
            deferred,
            sri: !integrity.is_empty(),
        })
    }
}

/// `Error downloading [URLS] to PATH: MESSAGE` (HttpDownloader).
fn download_error(urls: &[reqwest::Url], path: &[u8], message: &str) -> anyhow::Error {
    anyhow!(
        "java.io.IOException: Error downloading [{}] to {}: {message}",
        urls.iter()
            .map(reqwest::Url::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        display(path)
    )
}

/// A download() call: finished, or (`block = False`) transferring on the
/// pending-download pool.
pub(crate) enum Started {
    Done(DownloadOutcome),
    Pending(Arc<pending_downloads::Slot>),
}

/// A finished download(): `success = False` under allow_fail, or the digest.
#[derive(Debug, Clone)]
pub(crate) enum DownloadOutcome {
    Failed,
    Done(Checksum, u64),
}

impl DownloadOutcome {
    fn alloc<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        match self {
            Self::Failed => heap.alloc(AllocStruct([("success", false)])),
            Self::Done(checksum, size) => download_result(heap, checksum, *size),
        }
    }
}

/// calculateDownloadResult (java:559-583).
fn download_result<'v>(heap: Heap<'v>, checksum: &Checksum, size: u64) -> Value<'v> {
    let mut fields = vec![
        ("success", heap.alloc(true)),
        ("integrity", heap.alloc(checksum.integrity())),
    ];
    if checksum.algorithm == Algorithm::Sha256 {
        fields.push(("sha256", heap.alloc(transport::hex(&checksum.bytes))));
    }
    fields.push(("size_bytes", heap.alloc(size as i64)));
    heap.alloc(AllocStruct(fields))
}

fn url_strings<'v>(method: &str, url: Value<'v>) -> anyhow::Result<Vec<&'v str>> {
    // getUrls/checkAllUrls (java:443-469).
    if let Some(text) = url.unpack_str() {
        return Ok(vec![text]);
    }
    let items = if let Some(list) = ListRef::from_value(url) {
        list.content().to_vec()
    } else if let Some(tuple) = TupleRef::from_value(url) {
        tuple.content().to_vec()
    } else {
        bail!(
            "in call to {method}(), parameter 'url' got value of type '{}', want 'string or Iterable'",
            url.get_type()
        );
    };
    items
        .into_iter()
        .map(|item| {
            item.unpack_str().ok_or_else(|| {
                anyhow!(
                    "Expected a string or sequence of strings for 'url' argument, but got '{}' item in the sequence",
                    item.get_type()
                )
            })
        })
        .collect()
}

/// StarlarkBaseExternalContext members shared by repository_ctx and module_ctx.
#[starlark_module]
pub(crate) fn base_methods(builder: &mut MethodsBuilder) {
    fn getenv<'v>(
        this: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        // Raw Arguments bypass generated binding: keyword/arity failures must
        // enter the same typed privacy boundary as name/default/read failures.
        getenv_impl(this, args, eval).map_err(anyhow::Error::new)
    }

    /// StarlarkBaseExternalContext.createFile (java:1466-1527).
    fn file<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(default = "")] content: &str,
        #[starlark(default = true)] executable: bool,
        #[starlark(default = false)] legacy_utf8: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (this, legacy_utf8);
        // Writing a file observes only the pending downloads to it or into
        // it (bazel-gazelle's go_deps writes a `.info` file after each
        // non-blocking `.mod` download): the others stay in flight. A label
        // may fetch its repository first: commit them all.
        let (context, path) = if unpack_label(path).is_some()
            || (path.downcast_ref::<RepositoryPath>().is_none() && path.unpack_str().is_none())
        {
            let context = context(eval)?;
            (context, context.path_of(path, "file", "path")?)
        } else {
            let context = context_unflushed(eval)?;
            let bytes = context.path_of(path, "file", "path")?;
            pending_downloads::flush_conflicting(&path_buf(&bytes), None);
            (context, bytes)
        };
        // Bazel writes its internal Latin-1 string bytes; characters above
        // U+00FF are not representable there and are rejected explicitly.
        let bytes = crate::repository_rule::latin1_bytes(content).ok_or_else(|| {
            anyhow!("repository file content above U+00FF is unsupported (Latin-1 byte strings)")
        })?;
        context.write(&path, &bytes, executable)?;
        Ok(NoneType)
    }

    /// StarlarkBaseExternalContext.readFile (java:1603-1647).
    fn read<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(require = named, default = "auto")] watch: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<String> {
        let _ = this;
        // Reading a file observes only the pending downloads to it or into
        // it (a label may fetch its repository first: commit them all).
        let context = if unpack_label(path).is_some() {
            context(eval)?
        } else {
            let context = context_unflushed(eval)?;
            if path.downcast_ref::<RepositoryPath>().is_none() && path.unpack_str().is_none() {
                pending_downloads::flush();
            } else {
                let bytes = context.path_of(path, "read", "path")?;
                pending_downloads::flush_conflicting(&path_buf(&bytes), None);
            }
            context
        };
        let watch = ShouldWatch::parse(watch)?;
        let (_, bytes) = context.read_input(
            path,
            "read",
            "path",
            watch,
            "attempting to read() a directory",
        )?;
        Ok(latin1_text(&bytes))
    }

    /// StarlarkBaseExternalContext.getPath (java:1567-1601).
    fn path<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = this;
        if path.downcast_ref::<RepositoryPath>().is_some() {
            return Ok(path);
        }
        // A string names a path without touching it; a label may fetch its
        // repository (which observes what the earlier downloads changed).
        let context = if unpack_label(path).is_some() {
            context(eval)?
        } else {
            context_unflushed(eval)?
        };
        let bytes = context.path_of(path, "path", "path")?;
        Ok(eval.heap().alloc(RepositoryPath { bytes }))
    }

    /// StarlarkBaseExternalContext.watchForStarlark (java:1757-1787).
    fn watch<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = this;
        let context = context(eval)?;
        let resolved = context.path_of(path, "watch", "path")?;
        if context.kind == Kind::ModuleExtension
            && let Some(canonical) = unpack_label(path)
        {
            // A Label names a file of a repository (Bazel: under the workspace
            // or <output_base>/external), which a module extension may watch
            // even when the native root is elsewhere (local_path_override).
            return context
                .record_label(canonical, file_state(&path_buf(&resolved))?)
                .map(|()| NoneType);
        }
        context.watch(&resolved, ShouldWatch::Yes)?;
        Ok(NoneType)
    }

    /// StarlarkBaseExternalContext.which (java:2323-2383).
    fn which<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] program: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = this;
        let context = context(eval)?;
        if program.contains('/') || program.contains('\\') {
            bail!("Program argument of which() may not contain a / or a \\ ('{program}' given)");
        }
        ensure!(
            !program.is_empty(),
            "Program argument of which() may not be empty"
        );
        context.facility("which");
        Ok(match context.which(&latin1(program, "program")?)? {
            Some(bytes) => eval.heap().alloc(RepositoryPath { bytes }),
            None => Value::new_none(),
        })
    }

    /// StarlarkBaseExternalContext.execute (java:1970-2108).
    fn execute<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] arguments: Value<'v>,
        #[starlark(default = 600)] timeout: i32,
        environment: Option<Value<'v>>,
        #[starlark(default = true)] quiet: bool,
        #[starlark(default = "")] working_directory: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<ExecResult> {
        exec_receiver(this)?;
        let _span = crate::timeline::span("execute", || {
            format!("{arguments}").chars().take(200).collect()
        });
        let context = context(eval)?;
        let items = if let Some(list) = ListRef::from_value(arguments) {
            list.content().to_vec()
        } else if let Some(tuple) = TupleRef::from_value(arguments) {
            tuple.content().to_vec()
        } else {
            bail!(
                "in call to execute(), parameter 'arguments' got value of type '{}', want 'sequence'",
                arguments.get_type()
            );
        };
        // validateExecuteArguments (java:1930-1944), before any other effect.
        for (index, item) in items.iter().enumerate() {
            ensure!(
                item.unpack_str().is_some()
                    || unpack_label(*item).is_some()
                    || item.downcast_ref::<RepositoryPath>().is_some(),
                "Argument {index} of execute is neither a path, label, nor string."
            );
        }
        let mut overrides: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        if let Some(environment) = environment {
            let dict = DictRef::from_value(environment).ok_or_else(|| {
                anyhow!(
                    "in call to execute(), parameter 'environment' got value of type '{}', want 'dict'",
                    environment.get_type()
                )
            })?;
            for (key, value) in dict.iter() {
                let key = key.unpack_str().ok_or_else(|| {
                    anyhow!(
                        "got dict<{}, ...> for 'environment', want dict<string, Object>",
                        key.get_type()
                    )
                })?;
                let key = latin1(key, "environment")?;
                if value.is_none() {
                    overrides.push((key, None));
                } else if let Some(text) = value.unpack_str() {
                    overrides.push((key, Some(latin1(text, "environment")?)));
                } else {
                    bail!("environment values must be strings or None, got {value}");
                }
            }
        }
        ensure!(
            timeout > 0,
            "execute() timeout must be positive (Bazel rejects a non-positive timeout internally)"
        );
        // An empty list reaches process-wrapper, which prints its usage and
        // exits 1 (process-wrapper-options.cc:135-138); see repository_process.
        let mut argv = Vec::with_capacity(items.len());
        for item in &items {
            let bytes = if let Some(text) = item.unpack_str() {
                latin1(text, "execute() argument")?
            } else {
                context.path_of(*item, "execute", "arguments")?
            };
            ensure!(
                !bytes.contains(&0),
                "execute() arguments must not contain NUL"
            );
            argv.push(bytes);
        }
        let client = context.client_environment()?;
        let mut environment: BTreeMap<Vec<u8>, Vec<u8>> = client
            .iter()
            .map(|(name, value)| (name.to_vec(), value.to_vec()))
            .collect();
        for (key, value) in &overrides {
            if let Some(value) = value {
                ensure!(
                    !key.is_empty()
                        && !key.contains(&b'=')
                        && !key.contains(&0)
                        && !value.contains(&0),
                    "execute() environment names must be nonempty without '=' or NUL"
                );
                environment.insert(key.clone(), value.clone());
            }
        }
        for (key, value) in &overrides {
            if value.is_none() {
                environment.remove(key);
            }
        }
        // createDirectory(workingDirectory) (java:2076).
        context.mark_created()?;
        let mut directory = if working_directory.is_empty() {
            context.stage.clone()
        } else {
            let path = normalize(
                context.stage_bytes(),
                &latin1(working_directory, "working_directory")?,
            );
            match context.under_stage(&path) {
                Some(_) => {
                    let relative = context.output(&path)?;
                    if relative.is_empty() {
                        context.stage.clone()
                    } else {
                        let directory = parents(&context.stage, &relative, true)?;
                        match fs::symlink_metadata(&directory) {
                            Ok(metadata) => ensure!(
                                metadata.file_type().is_dir(),
                                "execute() working directory {} must be a real directory",
                                directory.display()
                            ),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                fs::create_dir(&directory)?
                            }
                            Err(error) => return Err(error.into()),
                        }
                        directory
                    }
                }
                None => {
                    let directory = path_buf(&path);
                    ensure!(
                        fs::metadata(&directory).is_ok_and(|metadata| metadata.is_dir()),
                        "execute() working directory {} outside the repository directory must already exist (the native profile creates directories only inside it)",
                        display(&path)
                    );
                    directory
                }
            }
        };
        context.facility("execute");
        // Only a subprocess can write into another repository's directory
        // (Starlark writes only below its own, [`Context::output`]): the
        // mutable ones it may write into are held as a reader while it runs
        // (and paths into a published tree it writes into name that
        // repository's mutable slot instead).
        let mut environment: Vec<(Vec<u8>, Vec<u8>)> = environment.into_iter().collect();
        let readers = context.mutable_slot_readers(&mut argv, &mut environment, &mut directory)?;
        let request = crate::repository_process::Request {
            argv,
            environment,
            working_directory: &directory,
            timeout: Duration::from_secs(timeout as u64),
        };
        let outcome = crate::repository_process::run(&request).map_err(anyhow::Error::msg);
        drop(readers);
        match outcome? {
            crate::repository_process::Outcome::Interrupted => bail!(
                "repository execute() subprocess was terminated by SIGTERM; Bazel treats this as an interrupt"
            ),
            crate::repository_process::Outcome::Completed(return_code, stdout, stderr) => {
                if !quiet {
                    context.echo(&stdout, &stderr);
                }
                Ok(ExecResult {
                    return_code,
                    stdout: latin1_text(&stdout),
                    stderr: latin1_text(&stderr),
                    id: NEXT_EXEC_RESULT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                })
            }
        }
    }

    /// StarlarkBaseExternalContext.reportProgress (java:1797-1826): a UI
    /// progress event only; the native CLI has no progress display.
    fn report_progress<'v>(
        this: Value<'v>,
        #[starlark(require = pos, default = "")] status: &str,
    ) -> anyhow::Result<NoneType> {
        let _ = (this, status);
        Ok(NoneType)
    }

    /// StarlarkBaseExternalContext.extract (java:1246-1400).
    #[allow(non_snake_case)] // Bazel's legacy `stripPrefix` keyword.
    fn extract<'v>(
        this: Value<'v>,
        archive: Value<'v>,
        output: Option<Value<'v>>,
        #[starlark(default = "")] strip_prefix: &str,
        #[starlark(require = named)] rename_files: Option<Value<'v>>,
        #[starlark(require = named, default = "auto")] watch_archive: &str,
        #[starlark(require = named, default = "")] stripPrefix: &str,
        #[starlark(require = named, default = 0)] strip_components: i32,
        #[starlark(require = named, default = "")] r#type: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = this;
        let _span = crate::timeline::span("extract", || format!("{archive}"));
        let context = context(eval)?;
        let strip_prefix = renamed_strip_prefix("extract", strip_prefix, stripPrefix)?;
        validate_stripping("extract", strip_prefix, strip_components)?;
        let archive_path = context.path_of(archive, "extract", "archive")?;
        let host = path_buf(&archive_path);
        let metadata = fs::metadata(&host)
            .map_err(|_| anyhow!("Archive path '{}' does not exist.", display(&archive_path)))?;
        ensure!(
            !metadata.is_dir(),
            "attempting to extract a directory: {}",
            display(&archive_path)
        );
        context.watch(&archive_path, ShouldWatch::parse(watch_archive)?)?;
        let output = match output {
            Some(output) => context.path_of(output, "extract", "output")?,
            None => context.stage_bytes().to_vec(),
        };
        let relative = context.output(&output)?;
        // Dict.cast (java:1372-1373) before the decompressor is chosen.
        let rename_files = string_dict(rename_files, "rename_files")?;
        let kind = if r#type.trim().is_empty() {
            let basename = archive_path.rsplit(|b| *b == b'/').next().unwrap_or(b"");
            decompressor_for_name(basename, &display(&archive_path))?
        } else {
            decompressor(r#type)?
        };
        native_extraction_limits("extract", kind, strip_components, &rename_files)?;
        context.extract_archive(
            &host,
            kind,
            strip_prefix,
            &relative,
            &archive_path,
            &rename_files,
            false,
        )?;
        Ok(NoneType)
    }

    /// StarlarkBaseExternalContext.downloadAndExtract (java:907-1197): the
    /// archive streams into a private temporary directory of the repository
    /// (Bazel: `<output>/temp<N>/<basename>`, java:1126) while it is hashed,
    /// then streams through the decompressor into the output; the temporary
    /// directory is deleted afterwards like Bazel's.
    #[allow(non_snake_case)] // Bazel's legacy `stripPrefix` keyword.
    fn download_and_extract<'v>(
        this: Value<'v>,
        url: Value<'v>,
        output: Option<Value<'v>>,
        #[starlark(default = "")] sha256: &str,
        #[starlark(default = "")] r#type: &str,
        #[starlark(default = "")] strip_prefix: &str,
        #[starlark(default = false)] allow_fail: bool,
        #[starlark(default = "")] canonical_id: &str,
        auth: Option<Value<'v>>,
        headers: Option<Value<'v>>,
        #[starlark(require = named, default = "")] integrity: &str,
        #[starlark(require = named)] rename_files: Option<Value<'v>>,
        #[starlark(require = named, default = "")] stripPrefix: &str,
        #[starlark(require = named, default = 0)] strip_components: i32,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = this;
        let _span = crate::timeline::span("download_and_extract", || {
            format!("{url}").chars().take(200).collect()
        });
        let context = context(eval)?;
        let strip_prefix = renamed_strip_prefix("download_and_extract", strip_prefix, stripPrefix)?;
        validate_stripping("download_and_extract", strip_prefix, strip_components)?;
        let request = DownloadRequest::new(
            "download_and_extract",
            url,
            sha256,
            integrity,
            allow_fail,
            auth,
            headers,
        )?;
        // Dict.cast (java:1103-1104), after the URL and checksum checks.
        let rename_files = string_dict(rename_files, "rename_files")?;
        let output = match output {
            Some(output) => context.path_of(output, "download_and_extract", "output")?,
            None => context.stage_bytes().to_vec(),
        };
        let relative = context.output(&output)?;
        context.facility("download");
        // The download lands in `<output>/temp<N>/<name>` (java:1126,
        // DownloadManager.getDownloadDestination); errors, the suffix check
        // and the decompressor use that name.
        let temp = temp_name();
        let name = download_basename(request.urls.first(), r#type);
        let mut archive = output.clone();
        archive.extend_from_slice(format!("/{temp}/{name}").as_bytes());
        context.mark_created()?;
        let scratch = tempfile::Builder::new()
            .prefix(".download-")
            .tempdir_in(&context.stage)?;
        let urls = &request.urls;
        // DownloadManager.java:264-277: a cache hit by checksum is named after
        // the first URL (or Bazel's `cacheprobe` placeholder without URLs).
        let cached = match &request.expected {
            Some(expected) => {
                let name = urls.first().map_or("cacheprobe".to_owned(), storage_name);
                let file = scratch.path().join(&name);
                context
                    .cache_get(expected, canonical_id, &file)?
                    .map(|size| (expected.clone(), size, file))
            }
            None => None,
        };
        let (checksum, size, file) = match cached {
            Some(cached) => cached,
            None => match context.fetch_to(
                urls,
                request.expected.as_ref(),
                request.sri,
                &request.headers,
                DownloadTarget::Directory(scratch.path()),
            ) {
                Ok((fetched, file)) => {
                    match context.cache_put(&fetched.checksum, &file, canonical_id) {
                        Ok(()) => {}
                        Err(_) if allow_fail => {
                            return Ok(eval.heap().alloc(AllocStruct([("success", false)])));
                        }
                        Err(error) => return Err(error),
                    }
                    (fetched.checksum, fetched.size, file)
                }
                Err(failure)
                    if allow_fail && matches!(failure.error, TypedIoError::Completion(_)) =>
                {
                    return Ok(eval.heap().alloc(AllocStruct([("success", false)])));
                }
                Err(failure) => return Err(failure.into_error(urls, &archive)),
            },
        };
        if let Some(error) = &request.deferred {
            bail!("Checksum error in {}: {error}", context.identifying());
        }
        // DecompressorValue.getDecompressor(downloadedPath): by name only
        // (java:1186-1190), the `type` being part of the name.
        let kind = decompressor_for_name(name.as_bytes(), &display(&archive))?;
        native_extraction_limits(
            "download_and_extract",
            kind,
            strip_components,
            &rename_files,
        )?;
        context.extract_archive(
            &file,
            kind,
            strip_prefix,
            &relative,
            &archive,
            &rename_files,
            true,
        )?;
        scratch.close()?;
        Ok(download_result(eval.heap(), &checksum, size))
    }

    /// StarlarkBaseExternalContext.download (java:799-895): like Bazel's
    /// HttpDownloader (java:90-109) the body streams straight into the output
    /// path (hashing while writing), so a checksum mismatch, which
    /// HashInputStream reports only at end of stream (java:61-91), leaves the
    /// downloaded bytes there, also under `allow_fail`. An existing output is
    /// replaced (unlinked, never written through, like ctx.file) only once a
    /// mirror responds. With a checksum, an output that already matches is
    /// reused without downloading (DownloadManager.java:247-261). `executable` adds
    /// the execute bits after success (setExecutable, java:676).
    /// `block = False` returns a PendingDownload whose `wait()` returns or
    /// raises what the blocking call would (java:585-697). The native
    /// download completes before the call returns, so it is never "pending
    /// after the callback finished" (java:295-315).
    fn download<'v>(
        this: Value<'v>,
        url: Value<'v>,
        output: Option<Value<'v>>,
        #[starlark(default = "")] sha256: &str,
        #[starlark(default = false)] executable: bool,
        #[starlark(default = false)] allow_fail: bool,
        #[starlark(default = "")] canonical_id: &str,
        auth: Option<Value<'v>>,
        headers: Option<Value<'v>>,
        #[starlark(require = named, default = "")] integrity: &str,
        #[starlark(require = named, default = true)] block: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = this;
        let _span = crate::timeline::span(
            if block {
                "download"
            } else {
                "download-nonblocking"
            },
            || format!("{url}").chars().take(200).collect(),
        );
        // A blocking download commits every pending one first (`context`);
        // a non-blocking one only those it could observe (below).
        let context = if block {
            context(eval)?
        } else {
            context_unflushed(eval)?
        };
        let request = DownloadRequest::new(
            "download", url, sha256, integrity, allow_fail, auth, headers,
        )?;
        let output = match output {
            Some(output) => context.path_of(output, "download", "output")?,
            None => context.stage_bytes().to_vec(),
        };
        if !block {
            pending_downloads::flush_conflicting(&path_buf(&output), request.expected.as_ref());
        }
        // checkInOutputDirectory("write", outputPath) (java:861).
        let relative = context.output(&output)?;
        let started = context.download(
            &request,
            &output,
            &relative,
            executable,
            allow_fail,
            canonical_id,
            block,
        );
        let outcome = match started {
            Ok(Started::Pending(slot)) => PendingOutcome::Queued(slot),
            Ok(Started::Done(outcome)) => PendingOutcome::Ready(Ok(outcome)),
            Err(error) if !block => PendingOutcome::Ready(Err(format!("{error:#}"))),
            Err(error) => return Err(error),
        };
        if block {
            let PendingOutcome::Ready(Ok(outcome)) = outcome else {
                unreachable!("a blocking download finishes at the call")
            };
            return Ok(outcome.alloc(eval.heap()));
        }
        Ok(eval.heap().alloc(PendingDownload {
            output: output.clone(),
            outcome,
        }))
    }
}

/// StarlarkRepositoryContext-only members.
#[starlark_module]
fn repository_methods(builder: &mut MethodsBuilder) {
    /// StarlarkRepositoryContext.createFileFromTemplate (java:258-359).
    fn template<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(require = pos)] template: Value<'v>,
        substitutions: Option<Value<'v>>,
        #[starlark(default = true)] executable: bool,
        #[starlark(require = named, default = "auto")] watch_template: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = this;
        let context = context(eval)?;
        let output = context.path_of(path, "template", "path")?;
        let replacements = string_dict(substitutions, "substitutions")?;
        let watch = ShouldWatch::parse(watch_template)?;
        let label = unpack_label(template).is_some();
        let (source, mut bytes) = context.read_input(
            template,
            "template",
            "template",
            watch,
            "attempting to use a directory as template",
        )?;
        context.output(&output)?;
        {
            let mut state = context.state.borrow_mut();
            state.inputs.templates.push(TemplateInput {
                source,
                bytes: bytes.clone(),
                label,
            });
        }
        for (key, value) in replacements {
            bytes = replace_bytes(&bytes, &key, &value)?;
        }
        context.write(&output, &bytes, executable)?;
        Ok(NoneType)
    }

    /// StarlarkRepositoryContext.delete (java:373-405).
    fn delete<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<bool> {
        let _ = this;
        let context = context(eval)?;
        if path.unpack_str().is_none() && path.downcast_ref::<RepositoryPath>().is_none() {
            bail!(
                "in call to delete(), parameter 'path' got value of type '{}', want 'string or path'",
                path.get_type()
            );
        }
        let path = context.path_of(path, "delete", "path")?;
        let workspace = context.workspace.as_os_str().as_bytes();
        let in_workspace = path == workspace
            || path
                .strip_prefix(workspace)
                .is_some_and(|rest| rest.starts_with(b"/"));
        let Some(relative) = context.under_stage(&path) else {
            if in_workspace {
                let relative = display(path[workspace.len()..].strip_prefix(b"/").unwrap_or(b""));
                let ignored = context
                    .repositories
                    .ignored_subdirectories("")
                    .map_err(anyhow::Error::msg)?
                    .matching_entry(&relative)
                    .is_some();
                ensure!(
                    ignored,
                    "delete() can only be applied to external paths (that is, outside the workspace or ignored in .bazelignore)"
                );
            }
            bail!(
                "repository_ctx.delete() of {} outside the repository directory is not supported by the native profile",
                display(&path)
            );
        };
        ensure!(
            !relative.is_empty(),
            "repository_ctx.delete() of the repository directory itself is not supported by the native profile"
        );
        let relative = context.output(&path)?;
        let target = parents(&context.stage, &relative, false)?;
        let _writable = context.writable_holding(&relative);
        match fs::symlink_metadata(&target) {
            // deleteTreesBelow makes directories writable first
            // (unix_jni.cc:760-773), like remove_tree.
            Ok(metadata) if metadata.file_type().is_dir() => remove_tree(&target)?,
            Ok(_) => fs::remove_file(&target)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        Ok(true)
    }

    /// StarlarkRepositoryContext.rename (java:407-473).
    fn rename<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] src: Value<'v>,
        #[starlark(require = pos)] dst: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = this;
        let context = context(eval)?;
        let source = context.path_of(src, "rename", "src")?;
        let destination = context.path_of(dst, "rename", "dst")?;
        let (from, to) = (context.output(&source)?, context.output(&destination)?);
        // RepositoryFunctionException(IOException) text, java:456-466.
        let failure = |reason: &dyn fmt::Display| {
            anyhow!(
                "java.io.IOException: Could not rename {} to {}: {reason}",
                display(&source),
                display(&destination)
            )
        };
        ensure!(
            !from.is_empty() && !to.is_empty(),
            failure(&"the repository directory itself cannot be renamed")
        );
        if fs::symlink_metadata(context.stage.join(&to)).is_ok() {
            return Err(failure(&"already exists"));
        }
        let from_path = parents(&context.stage, &from, false).map_err(|e| failure(&e))?;
        let _writable_from = context.writable_holding(&from);
        let _writable_to = context.writable_holding(&to);
        let to_path = context.output_parents(&to).map_err(|e| failure(&e))?;
        context.mark_created()?;
        // A directory moving to another parent needs its own write
        // permission (its `..` changes); restored at its new path.
        let moved = crate::nofollow_modes::WritableDirectory::of(&from_path);
        let renamed = fs::rename(&from_path, &to_path);
        let _moved = if renamed.is_ok() {
            moved.moved(&to_path)
        } else {
            moved
        };
        renamed.map_err(|error| {
            // unix_jni.cc renameTo: "[unix_jni.cc:638] SRC -> DST (REASON)".
            failure(&format!(
                "[unix_jni.cc:638] {} -> {} ({})",
                display(&source),
                display(&destination),
                io_reason(&error)
            ))
        })?;
        Ok(NoneType)
    }

    /// StarlarkRepositoryContext.symlink (java:196-256). The link holds the
    /// absolute target during the callback, exactly like Bazel. Links into the
    /// repository directory are rewritten relative only after the callback
    /// (`replant_symlinks`): the private stage path does not survive
    /// publication, while Bazel leaves them absolute in <output_base>/external.
    fn symlink<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] target: Value<'v>,
        #[starlark(require = pos)] link_name: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = this;
        let context = context(eval)?;
        let target = context.path_of(target, "symlink", "target")?;
        let link = context.path_of(link_name, "symlink", "link_name")?;
        let relative = context.output(&link)?;
        // RepositoryFunctionException(IOException) text, java:240-250.
        let failure = |reason: &dyn fmt::Display| {
            anyhow!(
                "java.io.IOException: Could not create symlink from {} to {}: {reason}",
                display(&target),
                display(&link)
            )
        };
        ensure!(
            !relative.is_empty(),
            failure(&"the repository directory itself cannot be a symlink")
        );
        let _writable = context.writable_holding(&relative);
        let host_link = context.output_parents(&relative).map_err(|e| failure(&e))?;
        context.mark_created()?;
        if fs::symlink_metadata(&host_link).is_ok() {
            // unix_jni.cc symlink(): "[unix_jni.cc:297] LINK (File exists)".
            return Err(failure(&format!(
                "[unix_jni.cc:297] {} (File exists)",
                display(&link)
            )));
        }
        // Bazel creates the link to the absolute target path (java:232); so do
        // we, so readlink/realpath inside the callback observe Bazel's values.
        std::os::unix::fs::symlink(path_buf(&target), &host_link).map_err(|e| failure(&e))?;
        Ok(NoneType)
    }

    /// StarlarkRepositoryContext.patch (java:475-567): resolve the patch file
    /// and directory, require the directory inside the repository
    /// (checkInOutputDirectory), reject a directory patch file, watch it,
    /// then apply it with PatchUtil (patching.rs, the port shared with
    /// registry, override and Git patches).
    fn patch<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] patch_file: Value<'v>,
        #[starlark(default = 0)] strip: i32,
        #[starlark(require = named)] directory: Option<Value<'v>>,
        #[starlark(require = named, default = "auto")] watch_patch: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = this;
        let context = context(eval)?;
        let patch_path = context.path_of(patch_file, "patch", "patch_file")?;
        let directory_path = match directory {
            Some(directory) => context.path_of(directory, "patch", "directory")?,
            None => context.stage_bytes().to_vec(),
        };
        let relative = context.output(&directory_path)?;
        let host = path_buf(&patch_path);
        if fs::metadata(&host).is_ok_and(|metadata| metadata.is_dir()) {
            bail!(
                "attempting to use a directory as patch file: {}",
                display(&patch_path)
            );
        }
        let watch = ShouldWatch::parse(watch_patch)?;
        let failed = |message: &dyn fmt::Display| {
            anyhow!("Error applying patch {}: {message}", display(&patch_path))
        };
        // PatchUtil.applyInternal (PatchUtil.java:451-453).
        let missing = || failed(&format!("Cannot find patch file: {}", display(&patch_path)));
        let bytes = if let Some(canonical) = unpack_label(patch_file) {
            // Labels keep the strict native source profile of read()/template().
            if fs::symlink_metadata(&host).is_err() {
                if watch != ShouldWatch::No {
                    context.record_label(canonical, "missing".to_owned())?;
                }
                return Err(missing());
            }
            let bytes = context.read_label(&canonical)?;
            if watch != ShouldWatch::No {
                context.record_label(canonical, format!("file:{}", sha256(&bytes)))?;
            }
            bytes
        } else {
            context.watch(&patch_path, watch)?;
            if fs::metadata(&host).is_err() {
                return Err(missing());
            }
            read_file(&host)?
        };
        // A negative strip strips nothing (stripPath, PatchUtil.java:270-279)
        // but is shown in messages.
        let report = |error: crate::patching::PatchError| match error {
            crate::patching::PatchError::Failed(message) => failed(&message),
            other => anyhow!("{other}"),
        };
        let parsed = crate::patching::parse_signed(&bytes, i64::from(strip)).map_err(report)?;
        // PatchUtil writes create missing parents (writeLinesAs); existing
        // ancestors must be real directories (no symlinked parents).
        let root = context.stage.join(&relative);
        context.mark_created()?;
        parsed.apply_tree_detailed(&root).map_err(report)?;
        Ok(NoneType)
    }

    /// StarlarkRepositoryContext.watchTree (java:568-609): a directory's
    /// type and its transitive tree (RepoRecordedInput.DirTree).
    fn watch_tree<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] path: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = this;
        let context = context(eval)?;
        let path = context.path_of(path, "watch_tree", "path")?;
        let host = path_buf(&path);
        ensure!(
            fs::metadata(&host).is_ok_and(|metadata| metadata.is_dir()),
            "can't call watch_tree() on non-directory {}",
            display(&path)
        );
        let Some(key) = context.watch_key(&path, ShouldWatch::Yes)? else {
            return Ok(NoneType);
        };
        context.record(key.clone(), "file", file_state(&host)?)?;
        context.record(key, "dirtree", format!("dirtree:{}", tree_digest(&host)?))?;
        Ok(NoneType)
    }

    /// StarlarkRepositoryContext.repoMetadata (java:611-656). Both parameters
    /// are named-only; the result is only meaningful as the implementation's
    /// return value (RepositoryFetchFunction.java:681-695).
    fn repo_metadata<'v>(
        this: Value<'v>,
        #[starlark(require = named)] reproducible: Option<Value<'v>>,
        #[starlark(require = named)] attrs_for_reproducibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = this;
        let reproducible = match reproducible {
            None => false,
            Some(value) => value.unpack_bool().ok_or_else(|| {
                anyhow!(
                    "in call to repo_metadata(), parameter 'reproducible' got value of type '{}', want 'bool'",
                    value.get_type()
                )
            })?,
        };
        let attrs = match attrs_for_reproducibility {
            None => eval.heap().alloc(starlark::values::dict::AllocDict::EMPTY),
            Some(value) => {
                ensure!(
                    DictRef::from_value(value).is_some(),
                    "in call to repo_metadata(), parameter 'attrs_for_reproducibility' got value of type '{}', want 'dict'",
                    value.get_type()
                );
                value
            }
        };
        let empty = DictRef::from_value(attrs).is_some_and(|dict| dict.is_empty());
        ensure!(
            !reproducible || empty,
            "attrs_for_reproducibility can only be specified if reproducible is False"
        );
        if let Some(error) = dict_cast_error(attrs, "attrs_for_reproducibility") {
            bail!(error);
        }
        Ok(eval.heap().alloc(RepoMetadataGen {
            reproducible,
            attrs,
        }))
    }
}

fn all_repository_methods(builder: &mut MethodsBuilder) {
    base_methods(builder);
    repository_methods(builder);
}

fn path_receiver<'v>(this: Value<'v>) -> anyhow::Result<&'v RepositoryPath> {
    this.downcast_ref::<RepositoryPath>()
        .ok_or_else(|| anyhow!("expected path"))
}

#[starlark_module]
fn path_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn basename<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let path = path_receiver(this)?;
        let name = path.bytes.rsplit(|b| *b == b'/').next().unwrap_or(b"");
        Ok(heap.alloc(latin1_text(name)))
    }

    #[starlark(attribute)]
    fn dirname<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let path = path_receiver(this)?;
        if path.bytes == b"/" {
            return Ok(Value::new_none());
        }
        Ok(heap.alloc(RepositoryPath {
            bytes: normalize(&path.bytes, b".."),
        }))
    }

    #[starlark(attribute)]
    fn exists<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let path = path_buf(&path_receiver(this)?.bytes);
        pending_downloads::flush_conflicting(&path, None);
        Ok(fs::metadata(path).is_ok())
    }

    #[starlark(attribute)]
    fn is_dir<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let path = path_buf(&path_receiver(this)?.bytes);
        pending_downloads::flush_conflicting(&path, None);
        Ok(fs::metadata(path).is_ok_and(|m| m.is_dir()))
    }

    #[starlark(attribute)]
    fn realpath<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        pending_downloads::flush();
        let path = path_receiver(this)?;
        Ok(heap.alloc(RepositoryPath {
            bytes: resolve_symbolic_links(&path.bytes)?,
        }))
    }

    /// StarlarkPath.getChild (java:138-156).
    fn get_child<'v>(
        this: Value<'v>,
        #[starlark(args)] relative_paths: UnpackTuple<Value<'v>>,
    ) -> anyhow::Result<RepositoryPath> {
        let path = path_receiver(this)?;
        let mut joined = Vec::new();
        for (index, item) in relative_paths.items.iter().enumerate() {
            let text = item.unpack_str().ok_or_else(|| {
                anyhow!(
                    "at index {index} of relative_paths, got element of type {}, want string",
                    item.get_type()
                )
            })?;
            if index > 0 {
                joined.push(b'/');
            }
            joined.extend_from_slice(&latin1(text, "relative_paths")?);
        }
        Ok(RepositoryPath {
            bytes: normalize(&path.bytes, &joined),
        })
    }

    /// StarlarkPath.readdir (java:85-125); entries in directory order.
    fn readdir<'v>(
        this: Value<'v>,
        #[starlark(require = named, default = "auto")] watch: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let path = path_receiver(this)?;
        let host = path_buf(&path.bytes);
        ensure!(
            fs::metadata(&host).is_ok_and(|m| m.is_dir()),
            "can't readdir(), not a directory: {}",
            display(&path.bytes)
        );
        let watch = ShouldWatch::parse(watch)?;
        let context = context(eval)?;
        let mut entries = Vec::new();
        let mut names = Vec::new();
        for entry in fs::read_dir(&host)? {
            let name = entry?.file_name();
            let mut bytes = path.bytes.clone();
            if bytes != b"/" {
                bytes.push(b'/');
            }
            bytes.extend_from_slice(name.as_bytes());
            entries.push(bytes);
            names.push(name.as_bytes().to_vec());
        }
        // maybeWatchDirents (java:1740-1755): the type, then the entries.
        if let Some(key) = context.watch_key(&path.bytes, watch)? {
            context.watch(&path.bytes, watch)?;
            let bazel_names = names.clone();
            names.sort_unstable();
            let mut hash = Vec::new();
            for name in names {
                hash.extend_from_slice(&name);
                hash.push(0);
            }
            if let Some(bazel) = Context::bazel_friendly(&key) {
                context.record_bazel(crate::lockfile_extensions::RecordedInput::dirents(
                    &bazel,
                    crate::lockfile_extensions::dirents_marker_value(&bazel_names),
                ));
            }
            context.record(key, "dirents", format!("dirents:{}", sha256(&hash)))?;
        }
        let heap = eval.heap();
        Ok(heap.alloc(
            entries
                .into_iter()
                .map(|bytes| heap.alloc(RepositoryPath { bytes }))
                .collect::<Vec<_>>(),
        ))
    }
}

/// Native publication divergence: genuine Bazel 9.3 leaves `ctx.symlink`
/// and tool-created links into the repository directory ABSOLUTE in
/// <output_base>/external/<repo> (captured). Its replanting
/// (RepositoryUtils.java:88-...) applies only when a repository is moved into
/// the repository contents cache. The native repository directory (its slot,
/// crate::repository_slots) is published as a content-addressed directory
/// whose name depends on these very links, so an absolute link cannot name
/// its final location; links into the slot are therefore rewritten relative
/// after the callback (same resolved contents). A link resolving OUTSIDE the
/// slot keeps what it resolved to from the slot — where genuine's stays —
/// so a relative one becomes absolute, and one into a sibling repository's
/// directory that links its published tree (`<store>/external/<name>/rest`,
/// genuine's `../<name>/rest`) is pinned to that tree: published trees never
/// depend on what a slot links at the moment (crate::source_links refuses
/// links through a slot).
/// Returns whether a link leads into the workspace `mentions` names
/// (crate::worktree::WorkspaceMentions: such a tree depends on where the
/// workspace is; a link into the shared cache does not).
fn replant_symlinks(
    stage: &Path,
    mentions: Option<&crate::worktree::WorkspaceMentions>,
) -> anyhow::Result<bool> {
    /// `path` without `.`/`..` components, lexically (`..` above the root
    /// stays at the root).
    fn lexical(path: &Path) -> PathBuf {
        let mut out = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                other => out.push(other),
            }
        }
        out
    }
    /// `absolute` pinned: below a sibling slot that links a published
    /// tree (or the embedded `@bazel_tools` tree), the same path below that
    /// tree.
    fn pinned(slots: Option<&Path>, absolute: PathBuf) -> PathBuf {
        let Some(slots) = slots else {
            return absolute;
        };
        let Ok(rest) = absolute.strip_prefix(slots) else {
            return absolute;
        };
        let mut components = rest.components();
        let Some(std::path::Component::Normal(name)) = components.next() else {
            return absolute;
        };
        match fs::read_link(slots.join(name)) {
            // `bazel_tools` links the embedded tools tree
            // (crate::repository::RepositorySet::bind_builtin_tools_slot).
            Ok(target)
                if (target.starts_with("../direct/trees")
                    || target.starts_with("../builtins/trees"))
                    && target.components().count() == 4 =>
            {
                lexical(&slots.join(target)).join(components.as_path())
            }
            _ => absolute,
        }
    }
    fn visit(
        stage: &Path,
        slots: Option<&Path>,
        mentions: Option<&crate::worktree::WorkspaceMentions>,
        directory: &Path,
        count: &mut usize,
        into_workspace: &mut bool,
    ) -> anyhow::Result<()> {
        // No depth cap: total path length is bounded by the store profile.
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            *count += 1;
            ensure!(
                *count <= 1_000_000,
                "repository tree exceeds 1000000 entries"
            );
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                visit(stage, slots, mentions, &path, count, into_workspace)?;
            } else if kind.is_symlink() {
                let target = fs::read_link(&path)?;
                let parent = path.parent().unwrap_or(stage);
                if !*into_workspace
                    && let Some(mentions) = mentions
                    && mentions.contains(&parent.join(&target))
                {
                    *into_workspace = true;
                }
                let resolved = lexical(&parent.join(&target));
                let replanted = if let Ok(inside) = resolved.strip_prefix(stage) {
                    // Inside the slot: relative (an absolute link only).
                    target.is_absolute().then(|| {
                        let parent_depth = parent
                            .strip_prefix(stage)
                            .map_or(0, |relative| relative.components().count());
                        let mut relative = PathBuf::new();
                        for _ in 0..parent_depth {
                            relative.push("..");
                        }
                        relative.push(inside);
                        if relative.as_os_str().is_empty() {
                            relative.push(".");
                        }
                        relative
                    })
                } else if target.is_absolute() {
                    // Outside, absolute: kept, unless pinned below a
                    // sibling's tree.
                    let pinned = pinned(slots, resolved.clone());
                    (pinned != resolved).then_some(pinned)
                } else {
                    // Outside, relative: what it resolves to from the slot,
                    // absolute (pinned below a sibling's tree).
                    Some(pinned(slots, resolved))
                };
                if let Some(replanted) = replanted {
                    fs::remove_file(&path)?;
                    std::os::unix::fs::symlink(replanted, &path)?;
                }
            }
        }
        Ok(())
    }
    let slots = stage
        .parent()
        .filter(|parent| parent.file_name() == Some(std::ffi::OsStr::new("external")));
    let mut into_workspace = false;
    visit(stage, slots, mentions, stage, &mut 0, &mut into_workspace)?;
    Ok(into_workspace)
}

fn is_latin1(text: &str) -> bool {
    crate::repository_rule::is_latin1(text)
}

fn alloc_attr_value<'v>(heap: Heap<'v>, name: &str, attr: &AttrValue) -> anyhow::Result<Value<'v>> {
    Ok(match attr {
        AttrValue::String(value) => {
            ensure!(
                is_latin1(value),
                "repository string attributes must be Latin-1 (U+0000..U+00FF)"
            );
            heap.alloc(value.as_str())
        }
        AttrValue::Strings(value) => {
            ensure!(
                value.iter().all(|value| is_latin1(value)),
                "repository string attributes must be Latin-1 (U+0000..U+00FF)"
            );
            heap.alloc(value.clone())
        }
        AttrValue::Int(value) => heap.alloc(*value),
        AttrValue::LabelKeyedStringDict(entries) => {
            crate::attribute_values::alloc_label_keyed_string_dict(heap, entries)?
        }
        AttrValue::Ints(_) => {
            anyhow::bail!(
                "attribute {name:?} kind is not supported for repository rules by the native profile"
            )
        }
        AttrValue::StringKeyedLabelDict(entries) => {
            crate::attribute_values::alloc_string_keyed_label_dict(heap, entries)?
        }
        AttrValue::LabelListDict(entries) => {
            crate::attribute_values::alloc_label_dict(heap, entries, true)?
        }
        AttrValue::StringDict(entries) => {
            ensure!(
                entries
                    .iter()
                    .all(|(key, value)| is_latin1(key) && is_latin1(value)),
                "repository string attributes must be Latin-1 (U+0000..U+00FF)"
            );
            crate::attribute_values::alloc_string_dict(heap, entries)?
        }
        AttrValue::StringListDict(entries) => {
            ensure!(
                entries
                    .iter()
                    .all(|(key, values)| is_latin1(key) && values.iter().all(|v| is_latin1(v))),
                "repository string attributes must be Latin-1 (U+0000..U+00FF)"
            );
            crate::attribute_values::alloc_string_list_dict(heap, entries)?
        }
        AttrValue::Bool(value) => heap.alloc(*value),
        AttrValue::Label(None) => Value::new_none(),
        AttrValue::Label(Some(label)) => alloc_label(heap, label)?,
        AttrValue::Labels(labels) => heap.alloc(
            labels
                .iter()
                .map(|label| alloc_label(heap, label))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
        AttrValue::Output(_) | AttrValue::Outputs(_) => {
            bail!("output attributes are unsupported for repository rules")
        }
    })
}

fn alloc_attrs<'v>(
    heap: Heap<'v>,
    attrs: &BTreeMap<String, AttrValue>,
    canonical_name: &str,
) -> anyhow::Result<Value<'v>> {
    let mut values = BTreeMap::new();
    for (name, attr) in attrs {
        values.insert(name.clone(), alloc_attr_value(heap, name, attr)?);
    }
    values.insert("name".to_owned(), heap.alloc(canonical_name));
    let fields = heap.alloc(AllocStruct(values));
    Ok(heap.alloc(RepoDefinitionGen { fields }))
}

/// RepositoryResolvedEvent (bazel/repository/RepositoryResolvedEvent.java:
/// 33-110) for the attributes a repository returned for reproducibility.
/// Returns the DEBUG message when it carries new information. Attributes are
/// visited in declaration order (RepoDefinition.getFieldNames).
fn resolved_event<'v>(
    definition: &RuleDefinition,
    attr: Value<'v>,
    result: Value<'v>,
    canonical_name: &str,
    heap: Heap<'v>,
) -> anyhow::Result<Option<String>> {
    let Some(result) = DictRef::from_value(result) else {
        return Ok(None);
    };
    if result.is_empty() {
        return Ok(None);
    }
    let mut modified = Vec::new();
    let mut dropped = Vec::new();
    for name in &definition.attribute_order {
        let Some(spec) = definition.schema.get(name) else {
            continue;
        };
        if name == "name" || name.starts_with('_') {
            continue;
        }
        let default = alloc_attr_value(heap, name, &spec.default)?;
        let current = attr
            .get_attr(name, heap)
            .map_err(|e| anyhow!("{e}"))?
            .unwrap_or_else(Value::new_none);
        let new = result.get_str(name).unwrap_or(default);
        if new.equals(current).map_err(|e| anyhow!("{e}"))? {
            continue;
        }
        if new.equals(default).map_err(|e| anyhow!("{e}"))? {
            dropped.push(name.clone());
        } else {
            modified.push(format!("{name} = {}", new.to_repr()));
        }
    }
    if modified.is_empty() && dropped.is_empty() {
        return Ok(None);
    }
    let prefix = format!(
        "Repo '{canonical_name}' indicated that a canonical reproducible form can be obtained by"
    );
    // `Starlark.repr` of the dropped key set prints Java's Set.toString().
    let dropped = format!("[{}]", dropped.join(", "));
    Ok(Some(if modified.is_empty() {
        format!("{prefix} dropping arguments {dropped}")
    } else if dropped == "[]" {
        format!("{prefix} modifying arguments {}", modified.join(", "))
    } else {
        format!(
            "{prefix} modifying arguments {} and dropping {dropped}",
            modified.join(", ")
        )
    }))
}

/// RepositoryResolvedEvent.getRuleDefinitionInformation (java:124-136).
fn rule_definition_information(definition: &RuleDefinition, canonical_name: &str) -> String {
    let (bzl, rule) = definition
        .identity
        .split_once('%')
        .unwrap_or((definition.identity.as_str(), ""));
    let bzl = Label::parse_canonical(bzl).map_or_else(
        |_| bzl.to_owned(),
        |label| format!("@@{}//{}:{}", label.repository, label.package, label.name),
    );
    format!("Repo {canonical_name} defined by rule {rule} in {bzl}")
}

/// What executing a repository rule produced for the store: the recorded
/// runtime inputs (serialized by the caller into the identity digest) and the
/// observed environment names for warm-publication revalidation.
#[derive(Debug)]
pub struct ExecutedRepository {
    pub inputs: ExecutionInputs,
    pub watch_names: Vec<String>,
    /// Everything the execution printed, in order: print(), the
    /// `execute(quiet=False)` echo and the reproducibility DEBUG lines.
    /// [`execute`] wrote it to stderr as it happened (and collects it only
    /// while the record mode's replay capture records local executions or
    /// the analysis snapshot's window records: empty otherwise);
    /// [`execute_captured`] did not.
    pub output: Vec<u8>,
    /// The execution read `repository_ctx.os.environ`. With
    /// `inputs.host_facilities` (`execute`, `which`, downloads) these are a
    /// replayed local rule's impurity flags (crate::snapshot::replay); never
    /// part of an identity.
    pub environ_read: bool,
    /// The other repositories a label of this execution resolved into
    /// ([`Context::label_path`]): the store records, for those whose tree
    /// path the published bytes embed, which tree that was
    /// (crate::repository_store::Executed::resolved).
    pub resolved: BTreeSet<String>,
}

/// A repository rule execution's print handler ([`Context::print_line`]).
struct CollectedPrints<'a>(&'a Context);

impl starlark::PrintHandler for CollectedPrints<'_> {
    fn println(&self, text: &str) -> starlark::Result<()> {
        let line = crate::debug_print::line(text);
        // A print follows the warnings of the downloads issued before it.
        pending_downloads::flush();
        self.0.print_line(&line);
        Ok(())
    }
}

/// Execute only into the caller's precreated private stage. Failures may leave
/// partial files there; callers must not publish them. Frozen owners remain live
/// throughout evaluation, including transitive/private helper functions.
///
/// What the rule prints reaches stderr as it happens, exactly as before
/// (print(), the `execute(quiet=False)` echo, the reproducibility DEBUG
/// lines), and while the record mode's replay capture records local
/// executions it is also collected per execution (PLAN-L2 update 1 §(b)):
/// the bytes are returned ([`ExecutedRepository::output`], successful
/// executions) for the replay record. Printing as it happens keeps every
/// position in the stream, also around nested executions (a repository a
/// rule demands while it runs prints in the middle of its output).
pub fn execute(
    repositories: Arc<RepositorySet>,
    definition: &RuleDefinition,
    functions: &[OwnedFrozenValue],
    attrs: &BTreeMap<String, AttrValue>,
    canonical_name: &str,
    original_name: &str,
    definition_label: &Label,
    stage: &Path,
) -> Result<ExecutedRepository, String> {
    execute_with(
        repositories,
        definition,
        functions,
        attrs,
        canonical_name,
        original_name,
        definition_label,
        stage,
        true,
    )
}

/// [`execute`] that prints nothing: the output stays in
/// [`ExecutedRepository::output`] (a replay of a local repository rule,
/// crate::snapshot::replay; never printed during the replay).
pub fn execute_captured(
    repositories: Arc<RepositorySet>,
    definition: &RuleDefinition,
    functions: &[OwnedFrozenValue],
    attrs: &BTreeMap<String, AttrValue>,
    canonical_name: &str,
    original_name: &str,
    definition_label: &Label,
    stage: &Path,
) -> Result<ExecutedRepository, String> {
    execute_with(
        repositories,
        definition,
        functions,
        attrs,
        canonical_name,
        original_name,
        definition_label,
        stage,
        false,
    )
}

fn execute_with(
    repositories: Arc<RepositorySet>,
    definition: &RuleDefinition,
    functions: &[OwnedFrozenValue],
    attrs: &BTreeMap<String, AttrValue>,
    canonical_name: &str,
    original_name: &str,
    definition_label: &Label,
    stage: &Path,
    live: bool,
) -> Result<ExecutedRepository, String> {
    // Genuine's `external/bazel_tools` beside the rule's directory.
    repositories.bind_builtin_tools_slot()?;
    // What the rule watches are this repository's recorded inputs
    // (crate::snapshot::incremental).
    let _owner = crate::observed_fs::repository_owner(Some(canonical_name));
    let run = || -> anyhow::Result<ExecutedRepository> {
        ensure!(
            canonical_name.is_ascii() && original_name.is_ascii(),
            "repository names must be ASCII"
        );
        let workspace = fs::canonicalize(repositories.main_root()).with_context(|| {
            format!(
                "cannot resolve workspace root {}",
                repositories.main_root().display()
            )
        })?;
        // The repository store's paths name no workspace dependence, also
        // when the cache lies inside the workspace (crate::worktree).
        let mentions = Some(match repositories.source_cache_path() {
            Ok(store) => crate::worktree::WorkspaceMentions::except_cache(&workspace, store),
            Err(_) => crate::worktree::WorkspaceMentions::new(&workspace, []),
        });
        let context = Context {
            repositories: repositories.clone(),
            kind: Kind::Repository,
            stage: checked_root(stage)?,
            workspace,
            label: definition_label.clone(),
            canonical: canonical_name.to_owned(),
            state: RefCell::new(State::default()),
            environment: RefCell::new(Watch::default()),
            output: RefCell::default(),
            live,
            environ_read: Arc::default(),
            workspace_observed: Arc::default(),
            mentions,
        };
        context.state.borrow_mut().label_siblings = label_siblings(attrs);
        let evaluated = Module::with_temp_heap(|module| -> anyhow::Result<()> {
            let heap = module.heap();
            let host = context.repositories.host().map_err(anyhow::Error::msg)?;
            // repository_ctx.os.environ is the command's repoEnv; a snapshot
            // failure surfaces only when environ is actually read.
            let os = crate::host_os::alloc_observed(
                heap,
                host,
                context
                    .repositories
                    .environment()
                    .client_environment()
                    .map_err(|error| error.to_string()),
                context.environ_read.clone(),
            )?;
            let attr_value = alloc_attrs(heap, attrs, canonical_name)?;
            let observed = heap.alloc(WorkspaceObservation {
                flag: context.workspace_observed.clone(),
            });
            let ctx = heap.alloc(RepositoryCtxGen {
                attrs: attr_value,
                os,
                name: canonical_name.to_owned(),
                original_name: original_name.to_owned(),
                workspace_root: latin1_text(context.workspace.as_os_str().as_bytes()),
                observed,
            });
            for function in functions {
                heap.access_owned_frozen_value(function);
            }
            let implementation = heap.access_owned_frozen_value(&definition.implementation);
            let prints = CollectedPrints(&context);
            let mut eval = Evaluator::new(&module);
            // Bazel has no heap or step limit for repository rules; only the
            // call-stack depth is bounded (Starlark forbids recursion).
            eval.set_max_callstack_size(128)?;
            eval.extra = Some(&context);
            eval.set_print_handler(&prints);
            for name in definition.environ.names() {
                context
                    .environment
                    .borrow_mut()
                    .observe(context.repositories.environment(), name)?;
            }
            let _model = starlark::values::string::enter_latin1_char_model(true);
            let returned = eval.eval_function(implementation, &[ctx], &[]);
            // The callback's non-blocking downloads finish within it, also
            // when it fails (their warnings precede its error).
            pending_downloads::flush();
            let returned = returned.map_err(callback_error)?;
            // RepositoryFetchFunction.java:681-695: a dict must have string
            // keys ("return value"), repo_metadata is taken as is, and every
            // other value (None, ints, ...) means NONREPRODUCIBLE under the
            // default --incompatible_require_repo_extension_metadata=false
            // (RepositoryOptions.java:346-347). Reproducibility only selects
            // Bazel's repo contents cache; the native store keys publications
            // by specification, so it has no effect here.
            let reproducibility = if let Some(metadata) = RepoMetadata::from_value(returned) {
                Some(metadata.attrs.to_value())
            } else if DictRef::from_value(returned).is_some() {
                if let Some(error) = dict_cast_error(returned, "return value") {
                    bail!(error);
                }
                Some(returned)
            } else {
                None
            };
            // RepositoryFetchFunction.java:697-703: Event.debug lines, printed
            // per execution (a warm publication reuse never runs this code,
            // matching Bazel's once-per-fetch behavior).
            if let Some(result) = reproducibility
                && let Some(message) =
                    resolved_event(definition, attr_value, result, canonical_name, heap)?
            {
                context.print_line(&format!("DEBUG: {message}"));
                context.print_line(&format!(
                    "DEBUG: {}",
                    rule_definition_information(definition, canonical_name)
                ));
            }
            Ok(())
        });
        evaluated?;
        let links_into_workspace = replant_symlinks(&context.stage, context.mentions.as_ref())?;
        let environ_read = context
            .environ_read
            .load(std::sync::atomic::Ordering::Relaxed);
        // A non-local publication that depends on the workspace's path
        // records the workspace (docs/WORKTREES.md): another worktree
        // sharing the cache never reuses it. A local rule never reuses a
        // publication anyway; its identity stays as it was.
        let workspace = (!definition.local
            && (links_into_workspace
                || context
                    .workspace_observed
                    .load(std::sync::atomic::Ordering::Relaxed)))
        .then(|| latin1_text(context.workspace.as_os_str().as_bytes()));
        let output = context.output.take();
        let mut resolved = context.state.borrow().resolved.clone();
        resolved.remove(canonical_name);
        let (mut inputs, watch_names) = context.finish_with_names()?;
        inputs.workspace = workspace;
        Ok(ExecutedRepository {
            inputs,
            watch_names,
            output,
            environ_read,
            resolved,
        })
    };
    run().map_err(|error| {
        format!(
            "repository {canonical_name} ({}): {error:#}",
            definition.identity
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loading::{LoadedDefinition, load_definition_in};
    use crate::repository_rule::{bind_attributes, unpack};

    /// Review i20: recorded-input file digests are memoized per canonical
    /// path and stat key, and only for settled files: a racy file is hashed
    /// on every observation, a file rewritten in place or replaced by a
    /// rename (same size, mtime restored) is hashed anew rather than
    /// answered stale, another spelling of a remembered file is a hit, and
    /// concurrent observations of one file hash it once. Each memo instance
    /// counts its own hashes (the process-wide one is shared with
    /// concurrently running tests).
    /// The repositories of each multi-label attribute, grouped by
    /// attribute; single-repository and non-label attributes form none.
    #[test]
    fn label_siblings_group_the_repositories_of_each_label_list() {
        let attrs = BTreeMap::from([
            (
                "entries".to_owned(),
                AttrValue::Labels(vec![
                    "@@a//:BUILD.bazel".to_owned(),
                    "@@b//pkg:x".to_owned(),
                    "@@a//other:y".to_owned(),
                    "//:main".to_owned(),
                ]),
            ),
            (
                "one".to_owned(),
                AttrValue::Labels(vec!["@@c//:x".to_owned(), "@@c//:y".to_owned()]),
            ),
            (
                "keyed".to_owned(),
                AttrValue::LabelKeyedStringDict(vec![
                    ("@@d//:x".to_owned(), "v".to_owned()),
                    ("@@e//:x".to_owned(), "w".to_owned()),
                ]),
            ),
            (
                "label".to_owned(),
                AttrValue::Label(Some("@@f//:x".to_owned())),
            ),
            (
                "text".to_owned(),
                AttrValue::Strings(vec!["@@g//:x".to_owned(), "@@h//:x".to_owned()]),
            ),
        ]);
        let set = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
        assert_eq!(
            label_siblings(&attrs),
            vec![set(&["a", "b"]), set(&["d", "e"])]
        );
    }

    #[test]
    fn recorded_file_digests_are_memoized_only_when_settled() {
        use std::sync::atomic::Ordering::SeqCst;
        use std::time::SystemTime;
        let dir = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let file = dir.path().join("tool");
        fs::write(&file, b"aaaa").unwrap();
        let memo = FileDigestMemo::default();
        let digest = |path: &Path, start: SystemTime| memo.sha256(path, start).unwrap();
        // A settled reference instant: injected instead of waiting out the
        // racy window.
        let settled = SystemTime::now() + Duration::from_secs(3600);

        // Racy (just written): hashed every time, never remembered.
        assert_eq!(digest(&file, SystemTime::now()), sha256(b"aaaa"));
        assert_eq!(digest(&file, SystemTime::now()), sha256(b"aaaa"));
        assert_eq!(memo.hashed.load(SeqCst), 2);
        assert!(memo.cells.lock().unwrap().is_empty());

        // Settled: hashed once, then a hit, through any spelling.
        std::os::unix::fs::symlink(&file, dir.path().join("link")).unwrap();
        assert_eq!(digest(&file, settled), sha256(b"aaaa"));
        assert_eq!(digest(&file, settled), sha256(b"aaaa"));
        assert_eq!(digest(&dir.path().join("link"), settled), sha256(b"aaaa"));
        assert_eq!(memo.hashed.load(SeqCst), 3);

        // Rewritten in place, same size, mtime restored: the ctime moved
        // (past the filesystem's timestamp tick), so the new bytes are
        // hashed.
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        fs::write(&file, b"bbbb").unwrap();
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), mtime);
        assert_eq!(digest(&file, settled), sha256(b"bbbb"));
        assert_eq!(digest(&file, settled), sha256(b"bbbb"));
        assert_eq!(memo.hashed.load(SeqCst), 4);

        // Replaced by a rename (another inode, same size, mtime restored):
        // the new bytes are hashed.
        let replacement = dir.path().join("tool.new");
        fs::write(&replacement, b"dddd").unwrap();
        fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        fs::rename(&replacement, &file).unwrap();
        assert_eq!(digest(&file, settled), sha256(b"dddd"));
        assert_eq!(digest(&dir.path().join("link"), settled), sha256(b"dddd"));
        assert_eq!(memo.hashed.load(SeqCst), 5);

        // Concurrent observations of one settled file: one hash.
        let other = dir.path().join("other");
        fs::write(&other, b"cccc").unwrap();
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| assert_eq!(digest(&other, settled), sha256(b"cccc")));
            }
        });
        assert_eq!(memo.hashed.load(SeqCst), 6);

        // Missing: the error of an unmemoized read, nothing remembered.
        let remembered = memo.cells.lock().unwrap().len();
        let error = memo
            .sha256(&dir.path().join("absent"), settled)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("java.io.FileNotFoundException: "),
            "{error}"
        );
        assert_eq!(memo.cells.lock().unwrap().len(), remembered);
    }

    /// Review i20: a file whose stat changed while it was hashed is not
    /// remembered — the hashing observation (the leader) answers with its
    /// own digest and forgets the cell — and an observation that took the
    /// same cell meanwhile (the waiter) hashes its own opened file. A FIFO
    /// stands in for a file rewritten during the read: hashing blocks until
    /// the test writes, so the test changes the stat key (an explicit
    /// mtime) exactly while the leader hashes, after every observation took
    /// its key.
    #[test]
    fn a_digest_of_a_file_changed_while_hashed_is_never_remembered() {
        use std::sync::atomic::Ordering::SeqCst;
        use std::time::{Instant, SystemTime};
        let dir = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let fifo = dir.path().join("fifo");
        let made =
            crate::spawn_lock::output(std::process::Command::new("mkfifo").arg(&fifo)).unwrap();
        assert!(made.status.success(), "{made:?}");
        let memo = FileDigestMemo::default();
        let settled = SystemTime::now() + Duration::from_secs(3600);
        // The holders of the one cell: the memo and each observation that
        // took it (after taking its stat key).
        let holders = || -> usize {
            let cells = memo.cells.lock().unwrap();
            cells.values().map(Arc::strong_count).sum()
        };

        for (observers, bytes, mtime) in [(1, &b"first"[..], 1_000), (2, &b"second"[..], 2_000)] {
            let mut digests = std::thread::scope(|scope| {
                // The test is the FIFO's writer from the start (read-write
                // opens never block), so the observations' opens do not
                // block and their reads end once it closes.
                let mut writer = fs::File::options()
                    .read(true)
                    .write(true)
                    .open(&fifo)
                    .unwrap();
                let observations: Vec<_> = (0..observers)
                    .map(|_| scope.spawn(|| memo.sha256(&fifo, settled).unwrap()))
                    .collect();
                let deadline = Instant::now() + Duration::from_secs(60);
                while holders() != 1 + observers {
                    assert!(
                        Instant::now() < deadline,
                        "observations never took the cell"
                    );
                    std::thread::sleep(Duration::from_millis(1));
                }
                writer
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(mtime))
                    .unwrap();
                writer.write_all(bytes).unwrap();
                drop(writer);
                observations
                    .into_iter()
                    .map(|observation| observation.join().unwrap())
                    .collect::<Vec<_>>()
            });
            digests.sort();
            if observers == 1 {
                // The leader alone: its own digest of what it read.
                assert_eq!(digests, [sha256(bytes)]);
                assert_eq!(memo.hashed.load(SeqCst), 1);
            } else {
                // The leader read every byte; the waiter, finding no
                // remembered digest, hashed its own descriptor of the
                // drained FIFO.
                let mut expected = [sha256(bytes), sha256(b"")];
                expected.sort();
                assert_eq!(digests, expected);
                assert_eq!(memo.hashed.load(SeqCst), 3);
            }
            assert!(memo.cells.lock().unwrap().is_empty());
        }
    }

    /// Review i20: the memo is bounded. A new key arriving when it holds
    /// [`FILE_DIGEST_MEMO_LIMIT`] entries starts it over; a remembered key
    /// arriving then is a hit and keeps every entry.
    #[test]
    fn the_file_digest_memo_starts_over_at_its_limit() {
        use std::sync::atomic::Ordering::SeqCst;
        use std::time::SystemTime;
        let dir = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::write(&a, b"aaaa").unwrap();
        fs::write(&b, b"bbbb").unwrap();
        let memo = FileDigestMemo::default();
        let settled = SystemTime::now() + Duration::from_secs(3600);
        let digest = |path: &Path| memo.sha256(path, settled).unwrap();
        let remembered = || memo.cells.lock().unwrap().len();
        // Entries of files never observed.
        let fill = |count: usize| {
            let mut cells = memo.cells.lock().unwrap();
            for index in 0..count {
                cells.insert(
                    FileDigestKey {
                        path: PathBuf::from(format!("/never/observed/{index}")),
                        device: 0,
                        inode: index as u64,
                        size: 0,
                        mtime: (0, 0),
                        ctime: (0, 0),
                    },
                    DigestCell::default(),
                );
            }
        };

        // Full: a new key starts over.
        fill(FILE_DIGEST_MEMO_LIMIT);
        assert_eq!(digest(&a), sha256(b"aaaa"));
        assert_eq!(remembered(), 1);
        assert_eq!(memo.hashed.load(SeqCst), 1);

        // Full again, `a` among the entries: its hit keeps them all.
        fill(FILE_DIGEST_MEMO_LIMIT - 1);
        assert_eq!(remembered(), FILE_DIGEST_MEMO_LIMIT);
        assert_eq!(digest(&a), sha256(b"aaaa"));
        assert_eq!(remembered(), FILE_DIGEST_MEMO_LIMIT);
        assert_eq!(memo.hashed.load(SeqCst), 1);

        // Another file starts over: `a` was forgotten and is hashed anew.
        assert_eq!(digest(&b), sha256(b"bbbb"));
        assert_eq!(remembered(), 1);
        assert_eq!(digest(&a), sha256(b"aaaa"));
        assert_eq!(remembered(), 2);
        assert_eq!(memo.hashed.load(SeqCst), 3);
    }

    /// Integration-20 (i20-hardening f7c1faa + perf-exec 46c2d6b): the
    /// in-process memo sits in front of the persistent digest table. A
    /// digest it does not hold comes from the table when that records the
    /// path in exactly the opened file's state (a later process reads
    /// nothing); a settled file it hashed is recorded there, a racy one is
    /// not; an edit keeping the size and restoring the mtime (the ctime
    /// moved) is read again; without a table the file is simply hashed.
    #[test]
    fn recorded_file_digest_misses_consult_the_persistent_table() {
        use crate::digest_memo::{Key, Saved, Table};
        use std::sync::atomic::Ordering::SeqCst;
        use std::time::SystemTime;
        let cache = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let outside = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let file = outside.path().join("tool");
        fs::write(&file, b"aaaa").unwrap();
        let settled = SystemTime::now() + Duration::from_secs(3600);
        let key = |path: &Path| Key::of(&fs::metadata(path).unwrap(), false).unwrap();

        // Racy (just written): hashed, never recorded.
        let table = Table::new(cache.path());
        let memo = FileDigestMemo::default();
        let digest = memo.sha256_in(Some(&table), &file, SystemTime::now());
        assert_eq!(digest.unwrap(), sha256(b"aaaa"));
        assert_eq!(memo.hashed.load(SeqCst), 1);
        assert_eq!(table.lookup(&file, &key(&file)), None);
        assert_eq!(table.save().unwrap(), Saved::Nothing);

        // Settled: hashed once (then an in-process hit) and recorded.
        let memo = FileDigestMemo::default();
        for _ in 0..2 {
            let digest = memo.sha256_in(Some(&table), &file, settled);
            assert_eq!(digest.unwrap(), sha256(b"aaaa"));
        }
        assert_eq!(memo.hashed.load(SeqCst), 1);
        assert!(matches!(
            table.save().unwrap(),
            Saved::Written { written: 1, .. }
        ));

        // A later process: the table's digest, the file is not read.
        let reader = Table::new(cache.path());
        let later = FileDigestMemo::default();
        let digest = later.sha256_in(Some(&reader), &file, settled);
        assert_eq!(digest.unwrap(), sha256(b"aaaa"));
        assert_eq!(later.hashed.load(SeqCst), 0);

        // Rewritten in place, same size, mtime restored: read again.
        let mtime = fs::metadata(&file).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        fs::write(&file, b"bbbb").unwrap();
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let edited = FileDigestMemo::default();
        let digest = edited.sha256_in(Some(&reader), &file, settled);
        assert_eq!(digest.unwrap(), sha256(b"bbbb"));
        assert_eq!(edited.hashed.load(SeqCst), 1);

        // Without a table: hashed.
        let alone = FileDigestMemo::default();
        assert_eq!(
            alone.sha256_in(None, &file, settled).unwrap(),
            sha256(b"bbbb")
        );
        assert_eq!(alone.hashed.load(SeqCst), 1);
    }

    struct Fixture {
        root: tempfile::TempDir,
        stage: tempfile::TempDir,
        repositories: Arc<RepositorySet>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
            let stage = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
            fs::write(root.path().join("BUILD"), "fail('never evaluate BUILD')\n").unwrap();
            fs::write(root.path().join("MODULE.bazel"), "module(name='root')\n").unwrap();
            let repositories = Arc::new(
                RepositorySet::new(root.path().to_owned())
                    .with_source_cache(root.path().join("cache")),
            );
            Self {
                root,
                stage,
                repositories,
            }
        }

        fn file(&self, name: &str, bytes: impl AsRef<[u8]>) {
            let path = self.root.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        fn load(&self, body: &str, schema: &str) -> LoadedDefinition {
            let body = body
                .lines()
                .map(|line| format!("    {line}\n"))
                .collect::<String>();
            self.file("defs.bzl", format!("def _impl(ctx):\n{body}\nR = repository_rule(implementation=_impl, attrs={schema})\n"));
            load_definition_in(
                self.repositories.clone(),
                &Label::parse_canonical("//:defs.bzl").unwrap(),
                "R",
                &crate::package::bzl_globals(),
            )
            .unwrap()
        }

        fn run_loaded(
            &self,
            loaded: LoadedDefinition,
            raw: BTreeMap<String, serde_json::Value>,
        ) -> Result<ExecutionInputs, String> {
            let definition = unpack(&loaded.value).unwrap();
            let attrs = bind_attributes(&definition, &raw, "canonical+repo", |text| {
                self.repositories.parse(text, "", "")
            })
            .unwrap();
            // Drop the loader export and source provenance before invoking. The
            // extracted implementation and functions own every frozen value.
            let functions = loaded.functions;
            drop(loaded.value);
            drop(loaded.sources);
            execute(
                self.repositories.clone(),
                &definition,
                &functions,
                &attrs,
                "canonical+repo",
                "original",
                &Label::parse_canonical("//:defs.bzl").unwrap(),
                self.stage.path(),
            )
            .map(|executed| executed.inputs)
        }

        fn run(&self, body: &str) -> Result<ExecutionInputs, String> {
            self.run_loaded(self.load(body, "{}"), BTreeMap::new())
        }

        fn error(&self, body: &str, expected: &str) {
            let error = self.run(body).unwrap_err();
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }

        fn bytes(&self, name: &str) -> Vec<u8> {
            fs::read(self.stage.path().join(name)).unwrap()
        }
    }

    #[test]
    fn empty_execution_inputs_keep_exact_legacy_bytes() {
        assert_eq!(
            serde_json::to_string(&ExecutionInputs::default()).unwrap(),
            r#"{"templates":[]}"#
        );
    }

    /// A live execution collects what it prints only while the record
    /// mode's replay capture records local executions (nobody else reads
    /// it); a captured replay always does.
    #[test]
    fn printed_output_is_collected_only_for_replay_records() {
        let fixture = Fixture::new();
        let loaded = fixture.load(
            "print('printed')\nctx.execute(['/bin/sh', '-c', 'echo echoed'], quiet = False)\nctx.file('BUILD', '')",
            "{}",
        );
        let definition = unpack(&loaded.value).unwrap();
        let functions = loaded.functions;
        let capturing = Arc::new(
            RepositorySet::new(fixture.root.path().to_owned())
                .with_source_cache(fixture.root.path().join("cache"))
                .with_replay_capture(Arc::default()),
        );
        let run = |repositories: &Arc<RepositorySet>, live: bool| {
            let stage = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
            let run = if live { execute } else { execute_captured };
            run(
                repositories.clone(),
                &definition,
                &functions,
                &BTreeMap::new(),
                "canonical+repo",
                "original",
                &Label::parse_canonical("//:defs.bzl").unwrap(),
                stage.path(),
            )
            .unwrap()
            .output
        };
        // The print is a marked note (crate::ui), the echo is not.
        let mut printed = crate::ui::marked("DEBUG: //:defs.bzl:2:10: printed");
        printed.extend_from_slice(b"echoed\n");
        assert_eq!(run(&fixture.repositories, true), b"");
        assert_eq!(run(&capturing, true), printed);
        assert_eq!(run(&fixture.repositories, false), printed);
    }

    #[test]
    fn string_dict_repository_projection_drives_ordered_template_and_is_frozen() {
        let fixture = Fixture::new();
        let schema = "{'d':attr.string_dict(default={'AB':'Z', 'Z':'Q'})}";
        let loaded = fixture.load("(ctx.attr.d.keys() == ['AB','Z'] and type(ctx.attr.d['AB']) == 'string') or fail('projection')\nctx.file('in', 'AB')\nctx.template('out', 'in', ctx.attr.d)", schema);
        fixture.run_loaded(loaded, BTreeMap::new()).unwrap();
        assert_eq!(fixture.bytes("out"), b"Q");
        let loaded = fixture.load("ctx.attr.d['AB']='changed'", schema);
        let error = fixture
            .run_loaded(loaded, BTreeMap::new())
            .unwrap_err()
            .to_lowercase();
        assert!(
            error.contains("frozen") || error.contains("immutable"),
            "{error}"
        );
        Module::with_temp_heap(|module| {
            for (key, value) in [("é", "ok"), ("k", "é")] {
                let attrs = BTreeMap::from([(
                    "d".into(),
                    AttrValue::StringDict(vec![(key.into(), value.into())]),
                )]);
                alloc_attrs(module.heap(), &attrs, "repo").unwrap();
            }
            for (key, value) in [("\u{100}", "ok"), ("k", "\u{100}")] {
                let attrs = BTreeMap::from([(
                    "d".into(),
                    AttrValue::StringDict(vec![(key.into(), value.into())]),
                )]);
                assert!(
                    alloc_attrs(module.heap(), &attrs, "repo")
                        .unwrap_err()
                        .to_string()
                        .contains("Latin-1")
                );
            }
        });
    }

    #[test]
    fn frozen_repository_context_retains_its_single_host_value_after_owner_drop() {
        let owner = Module::with_temp_heap(|module| {
            let heap = module.heap();
            module.set(
                "ctx",
                heap.alloc(RepositoryCtxGen {
                    attrs: Value::new_none(),
                    os: crate::host_os::alloc(heap, RepositorySet::new(".".into()).host().unwrap())
                        .unwrap(),
                    name: "repo".into(),
                    original_name: "original".into(),
                    workspace_root: "/".into(),
                    observed: Value::new_none(),
                }),
            );
            module.freeze().unwrap()
        });
        let ctx = owner.get("ctx").unwrap();
        drop(owner);
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let value = heap.access_owned_frozen_value(&ctx);
            let first = value.get_attr("os", heap).unwrap().unwrap();
            let second = value.get_attr("os", heap).unwrap().unwrap();
            assert!(first.ptr_eq(second));
            assert_eq!(
                first.get_attr("name", heap).unwrap().unwrap().unpack_str(),
                Some("linux")
            );
            assert_eq!(
                first.get_attr("arch", heap).unwrap().unwrap().unpack_str(),
                Some("amd64")
            );
        });
    }

    #[test]
    fn getenv_present_empty_absent_defaults_and_template_flow() {
        use crate::repository_environment::tests::Reader;
        use std::sync::atomic::Ordering;
        for value in [Some("SyntheticSecret"), Some(""), None] {
            let mut fixture = Fixture::new();
            let reader = Reader::new(Ok(value.map(Into::into)));
            fixture.repositories = Arc::new(
                (*fixture.repositories)
                    .clone()
                    .with_environment_reader(reader.clone()),
            );
            let inputs = fixture.run("v = ctx.getenv('SYNTHETIC', 'first')\nctx.file('value', v)\nctx.file('template', '@VALUE@')\nctx.template('expanded', 'template', {'@VALUE@': ctx.getenv('SYNTHETIC', 'second')})").unwrap();
            assert_eq!(fixture.bytes("value"), value.unwrap_or("first").as_bytes());
            assert_eq!(
                fixture.bytes("expanded"),
                value.unwrap_or("second").as_bytes()
            );
            assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
            let metadata = serde_json::to_string(&inputs).unwrap();
            assert!(metadata.contains("watched_env"));
            assert!(!metadata.contains("SyntheticSecret"));
            assert!(!format!("{inputs:?}").contains("SyntheticSecret"));
        }
    }

    #[test]
    fn getenv_validates_bindings_before_reader_and_never_echoes_rejected_values() {
        use crate::repository_environment::tests::Reader;
        use std::sync::atomic::Ordering;
        let mut fixture = Fixture::new();
        let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
        fixture.repositories = Arc::new(
            (*fixture.repositories)
                .clone()
                .with_environment_reader(reader.clone()),
        );
        for body in [
            "ctx.getenv(['RejectedSecret'])",
            "ctx.getenv('SYNTHETIC', ['RejectedSecret'])",
            "ctx.getenv('SYNTHETIC', default='RejectedSecret')",
            "ctx.getenv(name='RejectedSecret')",
            "ctx.getenv('SYNTHETIC', None, 'RejectedSecret')",
            "ctx.getenv('SYNTHETIC', RejectedSecret='RejectedSecret')",
            "ctx.getenv('SYNTHETIC', **{'RejectedSecret': 'RejectedSecret'})",
            "ctx.getenv('SYNTHETIC', **['RejectedSecret'])",
            "ctx.getenv(*['SYNTHETIC', None, 'RejectedSecret'])",
            "ctx.getenv(*struct(RejectedSecret='RejectedSecret'))",
            "ctx.getenv('SYNTHETIC', **{1: 'RejectedSecret'})",
            "ctx.getenv(name='RejectedSecret', **{'name': 'RejectedSecret'})",
            "ctx.getenv()",
        ] {
            let error = fixture.run(body).unwrap_err();
            assert!(!error.contains("RejectedSecret"), "{error}");
            assert!(!error.contains("SyntheticSecret"), "{error}");
        }
        assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn non_environment_callback_errors_preserve_source_even_after_getenv_or_spoofed_messages() {
        use crate::repository_environment::tests::Reader;
        let mut fixture = Fixture::new();
        let reader = Reader::new(Ok(None));
        fixture.repositories = Arc::new(
            (*fixture.repositories)
                .clone()
                .with_environment_reader(reader),
        );
        for body in [
            "native.package_group(name='bad')",
            "ctx.getenv('SYNTHETIC')\nnative.package_group(name='bad')",
        ] {
            let error = fixture.run(body).unwrap_err();
            assert!(
                error.contains("native.package_group(name='bad')"),
                "{error}"
            );
            assert!(
                error.contains("rule declaration outside package evaluation"),
                "{error}"
            );
            assert!(error.contains("Traceback"), "{error}");
        }
        let error = fixture
            .run("def getenv():\n    fail('repository getenv name must be a string')\ngetenv()")
            .unwrap_err();
        assert!(
            error.contains("fail('repository getenv name must be a string')"),
            "{error}"
        );
        assert!(error.contains("Traceback"), "{error}");
    }

    #[test]
    fn getenv_private_marker_is_typed_and_drops_arbitrary_attached_context() {
        let marked = starlark::Error::new_native(
            anyhow::Error::new(GetenvError::Name).context("RejectedSecret"),
        );
        let error = callback_error(marked).to_string();
        assert_eq!(error, "repository getenv name must be a string");
        let unmarked = starlark::Error::new_native(anyhow!(
            "repository getenv name must be a string: ordinary error"
        ));
        assert_eq!(
            callback_error(unmarked).to_string(),
            "repository getenv name must be a string: ordinary error"
        );
        Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            assert!(matches!(
                getenv_impl(
                    module.heap().alloc("RejectedSecret"),
                    &Arguments::default(),
                    &mut eval
                ),
                Err(GetenvError::Receiver)
            ));
        });
    }

    #[test]
    fn getenv_wrong_phase_rejects_before_lookup() {
        use crate::repository_environment::tests::Reader;
        use std::sync::atomic::Ordering;
        let fixture = Fixture::new();
        let reader = Reader::new(Ok(Some("SyntheticSecret".into())));
        let repositories = Arc::new(
            (*fixture.repositories)
                .clone()
                .with_environment_reader(reader.clone()),
        );
        Module::with_temp_heap(|module| {
            let ctx = module.heap().alloc(RepositoryCtxGen {
                attrs: Value::new_none(),
                os: crate::host_os::alloc(module.heap(), repositories.host().unwrap()).unwrap(),
                name: "repo".into(),
                original_name: "repo".into(),
                workspace_root: "/".into(),
                observed: Value::new_none(),
            });
            let method = ctx.get_attr("getenv", module.heap()).unwrap().unwrap();
            let loading = crate::loading::ExtensionContext {
                byte_strings: false,
                repositories,
                label: Label::parse_canonical("//:defs.bzl").unwrap(),
            };
            let mut eval = Evaluator::new(&module);
            let error = eval
                .eval_function(method, &[module.heap().alloc("SYNTHETIC")], &[])
                .unwrap_err();
            assert!(
                error
                    .without_diagnostic()
                    .to_string()
                    .contains("only available during repository execution")
            );
            assert!(
                matches!(error.kind(), starlark::ErrorKind::Native(native) if matches!(native.downcast_ref::<GetenvError>(), Some(GetenvError::Phase)))
            );
            // A loading context carries repositories too, but is not an execution capability.
            eval.extra = Some(&loading);
            assert!(
                eval.eval_function(method, &[module.heap().alloc("SYNTHETIC")], &[])
                    .is_err()
            );
        });
        assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn getenv_error_never_defaults_and_is_cached_between_callbacks() {
        use crate::repository_environment::{ReadError, tests::Reader};
        use std::sync::atomic::Ordering;
        let mut fixture = Fixture::new();
        let reader = Reader::new(Err(ReadError));
        fixture.repositories = Arc::new(
            (*fixture.repositories)
                .clone()
                .with_environment_reader(reader.clone()),
        );
        for _ in 0..2 {
            let error = fixture
                .run("ctx.file('value', ctx.getenv('RejectedSecret', 'RejectedSecret'))")
                .unwrap_err();
            assert!(error.contains("repository environment read failed"));
            assert!(!error.contains("RejectedSecret"), "{error}");
            assert!(!fixture.stage.path().join("value").exists());
        }
        assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn attributes_are_real_labels_and_names_are_canonical() {
        let fixture = Fixture::new();
        let loaded = fixture.load(
            "ctx.file('attrs', '|'.join([ctx.name, ctx.attr.name, ctx.original_name, type(ctx.attr.src), ctx.attr.src.name, type(ctx.attr.srcs[0]), str(ctx.attr.count), str(ctx.attr.flag), ctx.attr.words[0], str(ctx.attr.optional)]))",
            "{'src': attr.label(default='//:private.txt'), 'srcs': attr.label_list(default=['//:private.txt']), 'count': attr.int(default=7), 'flag': attr.bool(default=True), 'words': attr.string_list(default=['word']), 'optional': attr.label()}",
        );
        fixture.run_loaded(loaded, BTreeMap::new()).unwrap();
        assert_eq!(
            fixture.bytes("attrs"),
            b"canonical+repo|canonical+repo|original|Label|private.txt|Label|7|True|word|None"
        );
    }

    #[test]
    fn file_defaults_overwrite_nested_exact_bytes_and_modes() {
        let fixture = Fixture::new();
        fixture.run("ctx.file('nested/empty')\nctx.file('nested/data', 'old')\nctx.file('nested/data', 'new\\000\\r\\n', False, True)\nctx.file('exec', 'x', legacy_utf8=True)").unwrap();
        assert!(fixture.bytes("nested/empty").is_empty());
        assert_eq!(fixture.bytes("nested/data"), b"new\0\r\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for (name, mode) in [
                ("nested/empty", 0o755),
                ("nested/data", 0o644),
                ("exec", 0o755),
            ] {
                assert_eq!(
                    fs::metadata(fixture.stage.path().join(name))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    mode
                );
            }
        }
    }

    #[test]
    fn output_paths_normalize_like_bazel_and_stay_inside_the_repository_directory() {
        let fixture = Fixture::new();
        fixture.ok("ctx.file('a/../escape', '1')\nctx.file('x/./y', '2')\nctx.file('m//n/', '3')");
        assert_eq!(fixture.bytes("escape"), b"1");
        assert_eq!(fixture.bytes("x/y"), b"2");
        assert_eq!(fixture.bytes("m/n"), b"3");
        // Bazel-legal Linux filename bytes publish like any other name.
        fixture.ok(
            "ctx.file('back\\\\slash', '4')\nctx.file('colon:name', '5')\nctx.file('x#y%z?w', '6')",
        );
        assert_eq!(fixture.bytes("back\\slash"), b"4");
        assert_eq!(fixture.bytes("colon:name"), b"5");
        assert_eq!(fixture.bytes("x#y%z?w"), b"6");
        for (path, expected) in [
            (
                "../escape",
                "Cannot write outside of the repository directory for path",
            ),
            (
                "/absolute-n21",
                "Cannot write outside of the repository directory for path /absolute-n21",
            ),
            ("nul\\000path", "unexpected NUL"),
            ("", "is the repository directory"),
            (".", "is the repository directory"),
        ] {
            fixture.error(&format!("ctx.file('{path}', 'x')"), expected);
        }
        fixture.error(
            "ctx.file(Label('//:x'), 'x')",
            "Cannot write outside of the repository directory",
        );
        fixture.error("ctx.file(path='x')", "path");
        fixture.error("ctx.file(1, 'x')", "want 'string, Label, or path'");
        assert!(!fixture.root.path().join("escape").exists());
        let mut names: Vec<_> = fs::read_dir(fixture.stage.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["back\\slash", "colon:name", "escape", "m", "x", "x#y%z?w"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn writes_never_traverse_symlinked_parents_but_replace_final_links() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        fixture.file("outside", b"untouched");
        symlink(fixture.root.path(), fixture.stage.path().join("parent")).unwrap();
        symlink(
            fixture.root.path().join("outside"),
            fixture.stage.path().join("final"),
        )
        .unwrap();
        fixture.error("ctx.file('parent/outside', 'changed')", "symlink");
        // `delete()` then write (StarlarkBaseExternalContext.java:1513-1517):
        // the link itself is replaced, never its target.
        fixture.ok("ctx.file('final', 'changed')");
        assert!(
            fs::symlink_metadata(fixture.stage.path().join("final"))
                .unwrap()
                .is_file()
        );
        assert_eq!(fixture.bytes("final"), b"changed");
        assert_eq!(
            fs::read(fixture.root.path().join("outside")).unwrap(),
            b"untouched"
        );
        fs::remove_file(fixture.stage.path().join("final")).unwrap();
        let root_link = fixture.root.path().join("stage-link");
        symlink(fixture.stage.path(), &root_link).unwrap();
        let loaded = fixture.load("ctx.file('x')", "{}");
        let definition = unpack(&loaded.value).unwrap();
        let error = execute(
            fixture.repositories.clone(),
            &definition,
            &loaded.functions,
            &BTreeMap::new(),
            "repo",
            "repo",
            &Label::parse_canonical("//:defs.bzl").unwrap(),
            &root_link,
        )
        .unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert!(!fixture.stage.path().join("x").exists());
    }

    #[test]
    fn json_unicode_escapes_use_bazel_byte_strings_in_definition_and_callback() {
        let fixture = Fixture::new();
        fixture.file(
            "defs.bzl",
            r#"TOP = json.decode('"\\u00e9\\ud83d\\ude00"')
def _impl(ctx):
    ctx.file("top", TOP, executable = False)
    ctx.file("inner", json.decode('"\\u00e9A"'), executable = False)
R = repository_rule(implementation = _impl)
"#,
        );
        let loaded = load_definition_in(
            fixture.repositories.clone(),
            &Label::parse_canonical("//:defs.bzl").unwrap(),
            "R",
            &crate::package::bzl_globals(),
        )
        .unwrap();
        fixture.run_loaded(loaded, BTreeMap::new()).unwrap();
        // Bazel writes UTF-8 bytes of escape runs, never U+00E9 as byte E9.
        assert_eq!(fixture.bytes("top"), b"\xc3\xa9\xf0\x9f\x98\x80");
        assert_eq!(fixture.bytes("inner"), b"\xc3\xa9A");
    }

    #[test]
    fn latin1_attributes_write_bytes_and_wider_unicode_is_rejected_at_both_boundaries() {
        let fixture = Fixture::new();
        let loaded = fixture.load("ctx.file('x', ctx.attr.text)", "{'text': attr.string()}");
        let definition = unpack(&loaded.value).unwrap();
        // Runtime byte data maps to U+0000..U+00FF; Bazel writes those bytes.
        let all: String = (0u8..=255).map(char::from).collect();
        let bound = bind_attributes(
            &definition,
            &BTreeMap::from([("text".to_owned(), serde_json::json!(all))]),
            "canonical+repo",
            |text| fixture.repositories.parse(text, "", ""),
        )
        .unwrap();
        execute(
            fixture.repositories.clone(),
            &definition,
            &loaded.functions,
            &bound,
            "canonical+repo",
            "original",
            &Label::parse_canonical("//:defs.bzl").unwrap(),
            fixture.stage.path(),
        )
        .unwrap();
        assert_eq!(
            fs::read(fixture.stage.path().join("x")).unwrap(),
            (0u8..=255).collect::<Vec<_>>()
        );
        fs::remove_file(fixture.stage.path().join("x")).unwrap();

        let error = bind_attributes(
            &definition,
            &BTreeMap::from([("text".to_owned(), serde_json::json!("\u{100}"))]),
            "canonical+repo",
            |text| fixture.repositories.parse(text, "", ""),
        )
        .unwrap_err();
        assert!(error.contains("Latin-1"), "{error}");

        // Deliberately bypass the binder with a typed host value to exercise
        // the executor's defense. The loaded Starlark source remains ASCII.
        let attrs = BTreeMap::from([("text".to_owned(), AttrValue::String("\u{100}".to_owned()))]);
        let error = execute(
            fixture.repositories.clone(),
            &definition,
            &loaded.functions,
            &attrs,
            "canonical+repo",
            "original",
            &Label::parse_canonical("//:defs.bzl").unwrap(),
            fixture.stage.path(),
        )
        .unwrap_err();
        assert!(error.contains("Latin-1"), "{error}");
        assert!(!fixture.stage.path().join("x").exists());
    }

    #[test]
    fn template_preserves_binary_and_applies_insertion_order() {
        let fixture = Fixture::new();
        fixture.file("private.bin", b"AB AB\xff\r\n");
        let loaded = fixture.load(
            "ctx.template('out', ctx.attr.src, {'AB': 'Z', 'Z': 'Q', '': 'ignored'})",
            "{'src': attr.label(default='//:private.bin')}",
        );
        let inputs = fixture.run_loaded(loaded, BTreeMap::new()).unwrap();
        assert_eq!(fixture.bytes("out"), b"Q Q\xff\r\n");
        assert_eq!(inputs.templates.len(), 1);
        assert_eq!(inputs.templates[0].source, "//:private.bin");
        assert_eq!(inputs.templates[0].bytes, b"AB AB\xff\r\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(fixture.stage.path().join("out"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn label_templates_keep_pre_n21_execution_input_identity() {
        // Golden: the exact bytes main (040d055) serialized for a repository
        // using only ctx.file and Label templates — no `files`, `watched_env`
        // or `host_facilities` keys. The `templates` entry (canonical label and
        // full bytes) alone keys template content, whatever the watch mode.
        let golden = r#"{"templates":[{"source":"//:data.tpl","bytes":[65,32,66,32,65,66,13,10,255]},{"source":"//:tool.tpl","bytes":[84,32,37,123,110,125,10]}]}"#;
        for watch in ["", ", watch_template = 'yes'", ", watch_template = 'auto'"] {
            let fixture = Fixture::new();
            fixture.file("data.tpl", b"A B AB\r\n\xff");
            fixture.file("tool.tpl", b"T %{n}\n");
            let loaded = fixture.load(
                &format!("ctx.file('BUILD.bazel', '')\nctx.file('plain.txt', 'plain', executable = False)\nctx.template('data.txt', ctx.attr.source, {{'A': 'B'}}, executable = False{watch})\nctx.template('tool.sh', Label('//:tool.tpl'), {{'%{{n}}': 'x'}}{watch})"),
                "{'source': attr.label(default = '//:data.tpl')}",
            );
            let inputs = fixture.run_loaded(loaded, BTreeMap::new()).unwrap();
            assert_eq!(fixture.bytes("data.txt"), b"B B BB\r\n\xff");
            assert_eq!(fixture.bytes("tool.sh"), b"T x\n");
            assert_eq!(serde_json::to_string(&inputs).unwrap(), golden);
            let host = crate::host_os::HostMetadata::fixture("linux", "amd64", "linux");
            assert_eq!(
                crate::host_os::repository_inputs(&host, &inputs).unwrap(),
                format!(r#"{{"host_input_profile":"repository-host-v1","host":{{"name":"linux","arch":"amd64"}},"execution":{golden}}}"#).into_bytes()
            );
        }
        // Changed template bytes still change the identity (no files entry
        // needed), including with watch_template = 'no'.
        let fixture = Fixture::new();
        fixture.file("tool.tpl", b"U\n");
        let inputs = fixture
            .run("ctx.template('tool.sh', Label('//:tool.tpl'), watch_template = 'no')")
            .unwrap();
        assert_eq!(
            serde_json::to_string(&inputs).unwrap(),
            r#"{"templates":[{"source":"//:tool.tpl","bytes":[85,10]}]}"#
        );
        // Label read() had no previous identity and stays a recorded file
        // input unless watch = 'no'; a Label template plus a read of the same
        // Label records only the read.
        fixture.file("tool.tpl", b"T\n");
        let inputs = fixture
            .run("ctx.template('tool.sh', Label('//:tool.tpl'))\nctx.read(Label('//:tool.tpl'))\nctx.read(Label('//:tool.tpl'), watch = 'no')")
            .unwrap();
        assert_eq!(
            serde_json::to_string(&inputs).unwrap(),
            format!(
                r#"{{"templates":[{{"source":"//:tool.tpl","bytes":[84,10]}}],"files":[{{"path":"//:tool.tpl","kind":"file","state":"file:{}"}}]}}"#,
                sha256(b"T\n")
            )
        );
        let inputs = fixture
            .run("ctx.read(Label('//:tool.tpl'), watch = 'no')")
            .unwrap();
        assert_eq!(
            serde_json::to_string(&inputs).unwrap(),
            r#"{"templates":[]}"#
        );
    }

    #[test]
    fn template_same_file_ordered_repeat_reads_and_empty_key_noop() {
        let fixture = Fixture::new();
        let inputs = fixture.run("ctx.file('in', 'aaa')\nctx.template('in', 'in', {'aa': 'b', '': 'x'}, False)\nctx.template('copy', 'in')").unwrap();
        assert_eq!(fixture.bytes("in"), b"ba");
        assert_eq!(fixture.bytes("copy"), b"ba");
        assert_eq!(
            inputs
                .templates
                .iter()
                .map(|input| (input.source.as_str(), input.bytes.as_slice()))
                .collect::<Vec<_>>(),
            vec![
                ("stage:in", b"aaa".as_slice()),
                ("stage:in", b"ba".as_slice())
            ]
        );
    }

    #[test]
    fn label_templates_require_build_not_exports_or_analysis() {
        let fixture = Fixture::new();
        fixture.file("pkg/private", b"private");
        let schema = "{'src': attr.label(default='//pkg:private')}";
        let loaded = fixture.load("ctx.template('x', ctx.attr.src)", schema);
        assert!(
            fixture
                .run_loaded(loaded, BTreeMap::new())
                .unwrap_err()
                .contains("BUILD")
        );
        fixture.file(
            "pkg/BUILD.bazel",
            b"fail('do not evaluate or require exports_files')",
        );
        fixture
            .run_loaded(
                fixture.load("ctx.template('x', ctx.attr.src)", schema),
                BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(fixture.bytes("x"), b"private");
    }

    #[cfg(unix)]
    #[test]
    fn label_templates_follow_symlinked_files_packages_and_markers_like_bazel() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        fixture.file("actual", b"private");
        fixture.file("pkg/BUILD", b"");
        symlink(
            fixture.root.path().join("actual"),
            fixture.root.path().join("pkg/link"),
        )
        .unwrap();
        symlink(
            fixture.root.path().join("pkg"),
            fixture.root.path().join("alias"),
        )
        .unwrap();
        // getPathFromLabel then a read that follows links (java:1631-1647).
        // A symlinked package directory stays a separate native package
        // restriction (workspace package lookup).
        let error = fixture
            .run_loaded(
                fixture.load(
                    "ctx.template('x', ctx.attr.src)",
                    "{'src': attr.label(default='//alias:link')}",
                ),
                BTreeMap::new(),
            )
            .unwrap_err();
        assert!(
            error.contains("symlink package directories are unsupported"),
            "{error}"
        );
        {
            let schema = "{'src': attr.label(default='//pkg:link')}";
            fixture
                .run_loaded(
                    fixture.load("ctx.template('x', ctx.attr.src)", schema),
                    BTreeMap::new(),
                )
                .unwrap();
            assert_eq!(fixture.bytes("x"), b"private");
        }
        fs::remove_file(fixture.root.path().join("pkg/BUILD")).unwrap();
        symlink(
            fixture.root.path().join("BUILD"),
            fixture.root.path().join("pkg/BUILD"),
        )
        .unwrap();
        fixture
            .run_loaded(
                fixture.load(
                    "ctx.template('x', ctx.attr.src)",
                    "{'src': attr.label(default='//pkg:link')}",
                ),
                BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(fixture.bytes("x"), b"private");
    }

    #[test]
    fn file_template_and_write_sizes_have_no_native_caps() {
        // Above the former 16 MiB per-file / 64 MiB cumulative / 10000-write
        // native caps; Bazel has none.
        let fixture = Fixture::new();
        let input = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(fixture.stage.path().join("huge"))
            .unwrap();
        input.set_len(17 * 1024 * 1024).unwrap();
        fixture.run("ctx.template('out', 'huge')").unwrap();
        assert_eq!(fixture.bytes("out").len(), 17 * 1024 * 1024);
        fixture
            .run("for i in range(5):\n    ctx.file('repeat%d' % i, 'x' * (16 * 1024 * 1024 + 1))")
            .unwrap();
        assert_eq!(fixture.bytes("repeat4").len(), 16 * 1024 * 1024 + 1);
        fixture.run("ctx.file('in', 'aa')\nctx.template('expanded', 'in', {'a': 'b' * (8 * 1024 * 1024 + 1)})").unwrap();
        assert_eq!(fixture.bytes("expanded").len(), 2 * (8 * 1024 * 1024 + 1));
    }

    #[test]
    fn rejects_long_paths_but_not_deep_or_many_operations() {
        let fixture = Fixture::new();
        // No component-count cap: depth is bounded only by 4096 total bytes.
        fixture.run("ctx.file('/'.join(['a'] * 65))").unwrap();
        fixture.error("ctx.file('a' * 4097)", "4096");
        fixture
            .run("for i in range(10001):\n    ctx.file('empty')")
            .unwrap();
    }

    #[test]
    fn transitive_private_functions_survive_loader_drop() {
        let fixture = Fixture::new();
        fixture.file("helper.bzl", "def _private(ctx):\n    ctx.file('alive', 'yes')\ndef public(ctx):\n    _private(ctx)\n");
        fixture.file(
            "defs.bzl",
            "load(':helper.bzl', 'public')\nR = repository_rule(implementation=public)\n",
        );
        let loaded = load_definition_in(
            fixture.repositories.clone(),
            &Label::parse_canonical("//:defs.bzl").unwrap(),
            "R",
            &crate::package::bzl_globals(),
        )
        .unwrap();
        assert!(loaded.functions.len() >= 2);
        fixture.run_loaded(loaded, BTreeMap::new()).unwrap();
        assert_eq!(fixture.bytes("alive"), b"yes");
    }

    #[test]
    fn callback_label_uses_loaded_helpers_lexical_package() {
        let fixture = Fixture::new();
        fixture.file("helper/BUILD", b"");
        fixture.file("helper/source", b"lexical");
        fixture.file("helper/impl.bzl", "def _private(ctx):\n    ctx.template('lexical', Label(':source'))\ndef implementation(ctx):\n    _private(ctx)\n");
        fixture.file("defs.bzl", "load('//helper:impl.bzl', 'implementation')\nR = repository_rule(implementation=implementation)\n");
        let loaded = load_definition_in(
            fixture.repositories.clone(),
            &Label::parse_canonical("//:defs.bzl").unwrap(),
            "R",
            &crate::package::bzl_globals(),
        )
        .unwrap();
        let inputs = fixture.run_loaded(loaded, BTreeMap::new()).unwrap();
        assert_eq!(fixture.bytes("lexical"), b"lexical");
        assert_eq!(inputs.templates[0].source, "//helper:source");
    }

    #[cfg(unix)]
    #[test]
    fn stage_templates_follow_links_like_bazel_but_directories_fail() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        fixture.file("outside", b"private");
        symlink(
            fixture.root.path().join("outside"),
            fixture.stage.path().join("link"),
        )
        .unwrap();
        fixture.ok("ctx.template('out', 'link', {'private': 'public'})");
        assert_eq!(fixture.bytes("out"), b"public");
        fs::create_dir(fixture.stage.path().join("directory")).unwrap();
        fs::write(fixture.stage.path().join("directory/inner"), b"").unwrap();
        fixture.error(
            "ctx.file('directory', 'x')",
            &format!(
                "java.io.IOException: [unix_jni.cc:660] {}/directory (Directory not empty)",
                fixture.stage_text()
            ),
        );
        // An EMPTY directory is deleted and replaced, like Path.delete().
        fs::create_dir(fixture.stage.path().join("empty")).unwrap();
        fixture.ok("ctx.file('empty', 'now a file')");
        assert_eq!(fixture.bytes("empty"), b"now a file");
        fixture.error(
            "ctx.template('out2', 'directory')",
            "attempting to use a directory as template",
        );
        assert!(!fixture.stage.path().join("out2").exists());
    }

    struct EnvMap {
        values: BTreeMap<Vec<u8>, Vec<u8>>,
        reads: std::sync::atomic::AtomicUsize,
        fail_all: bool,
    }

    impl crate::repository_environment::EnvironmentReader for EnvMap {
        fn read(
            &self,
            name: &str,
        ) -> Result<Option<std::ffi::OsString>, crate::repository_environment::ReadError> {
            use std::os::unix::ffi::OsStringExt;
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self
                .values
                .get(name.as_bytes())
                .map(|v| std::ffi::OsString::from_vec(v.clone())))
        }

        fn all(
            &self,
        ) -> Result<
            Vec<(std::ffi::OsString, std::ffi::OsString)>,
            crate::repository_environment::ReadError,
        > {
            use std::os::unix::ffi::OsStringExt;
            if self.fail_all {
                return Err(crate::repository_environment::ReadError);
            }
            Ok(self
                .values
                .iter()
                .map(|(k, v)| {
                    (
                        std::ffi::OsString::from_vec(k.clone()),
                        std::ffi::OsString::from_vec(v.clone()),
                    )
                })
                .collect())
        }
    }

    impl Fixture {
        fn with_env(mut self, pairs: &[(&str, &[u8])]) -> (Self, Arc<EnvMap>) {
            let reader = Arc::new(EnvMap {
                values: pairs
                    .iter()
                    .map(|(k, v)| (k.as_bytes().to_vec(), v.to_vec()))
                    .collect(),
                reads: Default::default(),
                fail_all: false,
            });
            self.repositories = Arc::new(
                (*self.repositories)
                    .clone()
                    .with_environment_reader(reader.clone()),
            );
            (self, reader)
        }

        fn stage_text(&self) -> String {
            fs::canonicalize(self.stage.path())
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        }

        fn root_text(&self) -> String {
            fs::canonicalize(self.root.path())
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        }

        fn ok(&self, body: &str) -> ExecutionInputs {
            self.run(body).unwrap_or_else(|error| panic!("{error}"))
        }
    }

    include!("../tests/support/executable_script.rs");

    /// A fake tool that `ctx.which` finds and `ctx.execute` runs; see
    /// `write_executable_script` for why `fs::write` here made exec flaky.
    fn executable(path: &Path, text: &str) {
        write_executable_script(path, text, 0o755);
    }

    #[test]
    fn os_environ_is_a_fresh_sorted_client_environment_without_recorded_dependency() {
        let (fixture, reader) = Fixture::new().with_env(&[
            ("ZED", b"last"),
            ("ALPHA", b"first"),
            ("PATH", b"/usr/bin:/bin"),
            ("RAW", b"\xff\x00"),
        ]);
        let inputs = fixture.ok("env = ctx.os.environ\n(type(env) == 'dict' and env.keys() == ['ALPHA', 'PATH', 'ZED']) or fail('keys ' + str(env.keys()))\n(env['ALPHA'] == 'first' and env.get('MISSING', 'd') == 'd' and 'ZED' in env) or fail('values')\n(ctx.os.environ == env and getattr(ctx.os, 'environ', {}) == env) or fail('stable')\n(dir(ctx.os) == ['arch', 'environ', 'name'] and hasattr(ctx.os, 'environ')) or fail('surface')\nctx.file('name', ctx.os.name + '/' + ctx.os.arch, executable = False)");
        assert_eq!(fixture.bytes("name"), b"linux/amd64");
        // Bazel: reading environ establishes no dependency (StarlarkOS.java:49-54).
        assert!(inputs.watched_env.is_none());
        assert!(inputs.files.is_empty() && inputs.host_facilities.is_empty());
        assert_eq!(reader.reads.load(std::sync::atomic::Ordering::SeqCst), 0);
        // The JVM decodes --client_env as UTF-8 with replacement
        // (BlazeRuntime.java:915-916): \xff becomes U+FFFD, i.e. the three
        // Latin-1 characters of EF BF BD; valid UTF-8 is unchanged.
        let (fixture, _) = Fixture::new().with_env(&[("RAW", b"\xffA"), ("UTF8", b"caf\xc3\xa9")]);
        fixture.ok("(len(ctx.os.environ['RAW']) == 4 and len(ctx.os.environ['UTF8']) == 5) or fail('decoding')\nctx.file('raw', ctx.os.environ['RAW'] + '|' + ctx.os.environ['UTF8'], executable = False)\nr = ctx.execute(['/bin/sh', '-c', 'printf %s \"$RAW\"'])\nctx.file('child', r.stdout, executable = False)");
        assert_eq!(fixture.bytes("raw"), b"\xef\xbf\xbdA|caf\xc3\xa9");
        assert_eq!(fixture.bytes("child"), b"\xef\xbf\xbdA");
        // Every access is a NEW mutable dict (captured: assignment and pop()
        // succeed and are invisible to the next access).
        fixture.ok("e = ctx.os.environ\ne['X'] = 'y'\n(e.pop('RAW') != None and 'RAW' in ctx.os.environ and 'X' not in ctx.os.environ and ctx.os.environ != e) or fail('fresh')");
        fixture.error(
            "json.encode(ctx.os)",
            "cannot encode unsupported value as JSON",
        );
        fixture.error("ctx.os.name = 'changed'", "repository_os");
    }

    /// A relative HOME (or TMPDIR) of the client environment reaches the rule
    /// verbatim, as in Bazel 9.3 (the client forwards its environment as
    /// --client_env, option_processor.cc:723-726; CommandEnvironment.java:
    /// 333-346 builds the repository environment from it), and an
    /// execute() child resolves it against its working directory, the
    /// repository directory. Genuine Bazel 9.3.0 does the same. This is how
    /// a harness passing `HOME=runs/<name>-home` made every gazelle
    /// go_repository fetch fail with a different `fetch_repo: resulting
    /// module with sum ...` on each attempt (genuine too): `go mod download`
    /// wrote its telemetry counters ($HOME/.config/go/telemetry,
    /// time-dependent bytes) into the module directory fetch_repo hashes
    /// (docs/REPOSITORY_ENVIRONMENT.md). Pinned so the value is never
    /// silently absolutized, which Bazel does not do.
    #[test]
    fn relative_home_reaches_execute_verbatim_and_resolves_in_the_repository() {
        let (fixture, _) =
            Fixture::new().with_env(&[("HOME", b"rel/home"), ("PATH", b"/usr/bin:/bin")]);
        fixture.ok("(ctx.os.environ['HOME'] == 'rel/home') or fail('verbatim')\nr = ctx.execute(['/bin/sh', '-c', 'mkdir -p \"$HOME/.config/go/telemetry\" && printf counter > \"$HOME/.config/go/telemetry/count\"'])\n(r.return_code == 0) or fail(r.stderr)");
        assert_eq!(
            fixture.bytes("rel/home/.config/go/telemetry/count"),
            b"counter"
        );
    }

    #[test]
    fn os_environ_snapshot_failure_is_cached_and_reported_only_on_access() {
        let mut fixture = Fixture::new();
        let reader = Arc::new(EnvMap {
            values: BTreeMap::new(),
            reads: Default::default(),
            fail_all: true,
        });
        fixture.repositories = Arc::new(
            (*fixture.repositories)
                .clone()
                .with_environment_reader(reader),
        );
        fixture.ok("ctx.file('ok', ctx.os.name)");
        fixture.error(
            "ctx.os.environ",
            "repository_os.environ is unavailable: repository environment read failed",
        );
        fixture.error("ctx.which('sh')", "repository environment read failed");
        fixture.error(
            "ctx.execute(['true'])",
            "repository environment read failed",
        );
    }

    #[test]
    fn path_api_normalizes_like_bazel_and_exposes_structure() {
        let fixture = Fixture::new();
        fixture.file("pkg/BUILD", "");
        let stage = fixture.stage_text();
        let root = fixture.root_text();
        fixture.ok(&format!(
            r#"p = ctx.path("a/./b/../c")
(type(p) == "path" and str(p) == "{stage}/a/c" and repr(p) == '"{stage}/a/c"') or fail("str " + str(p))
(p.basename == "c" and p.dirname == ctx.path("a") and ctx.path(p) == p) or fail("structure")
(ctx.path("/").dirname == None and ctx.path("/").basename == "") or fail("root")
(str(ctx.path("../../../../../../../../../..")) == "/") or fail("climb")
(str(ctx.path("/x//y/")) == "/x/y") or fail("absolute")
(not p.exists and not p.is_dir) or fail("missing")
ctx.file("a/c", "x")
(p.exists and ctx.path("a").is_dir and not p.is_dir and p.realpath == p) or fail("exists")
(p.get_child("d", "e/f") == ctx.path("a/c/d/e/f") and p.get_child() == p and p.get_child("/abs") == ctx.path("/abs")) or fail("child")
([str(x) for x in ctx.path("a").readdir()] == [str(p)]) or fail("readdir")
(ctx.path(Label("//pkg:missing-file")) == ctx.path("{root}/pkg/missing-file")) or fail("label")
(ctx.workspace_root == ctx.path("{root}") and type(ctx.workspace_root) == "path") or fail("workspace")
({{p: 1}}[ctx.path("a/c")] == 1 and p != str(p)) or fail("hash")
("workspace_root" in dir(ctx) and "path" in dir(ctx)) or fail("dir")"#
        ));
        fixture.error(
            "ctx.path(Label('//nopkg:x'))",
            "Unable to load package for //nopkg:x",
        );
        fixture.error(
            "ctx.path(1)",
            "parameter 'path' got value of type 'int', want 'string, Label, or path'",
        );
        fixture.error(
            "ctx.path('a/c').readdir()",
            "can't readdir(), not a directory",
        );
        fixture.error(
            "ctx.path('/no/such/path/n21').realpath",
            "/no/such/path/n21",
        );
        fixture.error("ctx.path('a').get_child(1)", "want string");
    }

    #[test]
    fn read_and_watch_record_host_inputs_but_never_the_repository_directory() {
        let fixture = Fixture::new();
        fixture.file("pkg/BUILD", "");
        fixture.file("pkg/data.json", "{\"k\": 1}");
        let outside = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let host = fs::canonicalize(outside.path()).unwrap();
        fs::write(host.join("host.txt"), b"host \xff").unwrap();
        fs::create_dir(host.join("dir")).unwrap();
        fs::write(host.join("dir/one"), b"").unwrap();
        let root = fixture.root_text();
        let host = host.to_str().unwrap();
        let inputs = fixture.ok(&format!(
            r#"ctx.file("in", "abc")
(ctx.read("in") == "abc" and ctx.read(ctx.path("in"), watch = "no") == "abc") or fail("stage")
(json.decode(ctx.read(Label("//pkg:data.json"))) == {{"k": 1}}) or fail("label")
(len(ctx.read("{host}/host.txt")) == 6) or fail("host")
ctx.file("copy", ctx.read("{host}/host.txt"))
ctx.read("{host}/host.txt", watch = "no")
ctx.read("{root}/pkg/data.json")
ctx.watch("{host}/absent")
([x.basename for x in ctx.path("{host}/dir").readdir()] == ["one"]) or fail("readdir")"#
        ));
        let files: Vec<_> = inputs
            .files
            .iter()
            .map(|input| (input.path.as_str(), input.kind, input.state.as_str()))
            .collect();
        let json = sha256(b"{\"k\": 1}");
        let text = sha256(b"host \xff");
        assert_eq!(
            files,
            vec![
                ("//pkg:data.json", "file", format!("file:{json}").as_str()),
                (
                    format!("{host}/host.txt").as_str(),
                    "file",
                    format!("file:{text}").as_str()
                ),
                ("@@//pkg/data.json", "file", format!("file:{json}").as_str()),
                (format!("{host}/absent").as_str(), "file", "missing"),
                (format!("{host}/dir").as_str(), "file", "directory"),
                (
                    format!("{host}/dir").as_str(),
                    "dirents",
                    format!("dirents:{}", sha256(b"one\0")).as_str()
                ),
            ]
        );
        assert!(
            !serde_json::to_string(&inputs)
                .unwrap()
                .contains(&fixture.stage_text())
        );
        assert_eq!(fixture.bytes("copy"), b"host \xff");
        fixture.error(
            "ctx.read('in', watch = 'yes')",
            "attempted to watch path under working directory",
        );
        fixture.error(
            "ctx.read('in', watch = 'maybe')",
            "bad value for 'watch' parameter; want 'yes', 'no', or 'auto', got maybe",
        );
        fixture.error(
            &format!("ctx.read('{host}/dir')"),
            "attempting to read() a directory",
        );
        fixture.error("ctx.read('in', 'yes')", "read");
        fixture.error(
            "ctx.watch('in')",
            "attempted to watch path under working directory",
        );
        fixture.error(
            &format!("ctx.read('{host}/absent')"),
            "No such file or directory",
        );
    }

    #[test]
    fn which_searches_absolute_client_path_entries_for_owner_executables() {
        let bin = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let bin_text = fs::canonicalize(bin.path()).unwrap();
        let bin_text = bin_text.to_str().unwrap();
        executable(&bin.path().join("fake-cc"), "#!/bin/sh\n");
        fs::write(bin.path().join("plain"), "").unwrap();
        fs::create_dir(bin.path().join("dir")).unwrap();
        let path = format!("relative:{bin_text}:/nonexistent-n21");
        let (fixture, _) = Fixture::new().with_env(&[("PATH", path.as_bytes())]);
        let inputs = fixture.ok(&format!(
            r#"(ctx.which("fake-cc") == ctx.path("{bin_text}/fake-cc")) or fail("found")
(ctx.which(" fake-cc ") == ctx.path("{bin_text}/fake-cc")) or fail("trim")
(ctx.which("plain") == None and ctx.which("dir") == None and ctx.which("missing") == None) or fail("absent")"#
        ));
        assert_eq!(inputs.host_facilities, BTreeSet::from(["which"]));
        fixture.error(
            "ctx.which('a/b')",
            "Program argument of which() may not contain a / or a \\ ('a/b' given)",
        );
        fixture.error(
            "ctx.which('')",
            "Program argument of which() may not be empty",
        );
        let (fixture, _) = Fixture::new().with_env(&[]);
        fixture.ok("(ctx.which('sh') == None) or fail('no PATH')");
    }

    #[test]
    fn execute_uses_client_environment_overrides_paths_labels_and_bazel_results() {
        let (fixture, _) = Fixture::new().with_env(&[
            ("PATH", b"/usr/bin:/bin"),
            ("KEEP", b"kept"),
            ("DROP", b"dropped"),
        ]);
        fixture.file("pkg/BUILD", "");
        fixture.file("pkg/data.txt", "label-bytes");
        let stage = fixture.stage_text();
        let inputs = fixture.ok(&format!(
            r#"r = ctx.execute(["sh", "-c", 'printf "%s|%s|%s" "$KEEP" "${{DROP-unset}}" "$NEW"; printf "e\\377" >&2; exit 7'], environment = {{"NEW": "new", "DROP": None}})
(type(r) == "exec_result" and dir(r) == ["return_code", "stderr", "stdout"]) or fail("type")
(r.return_code == 7 and r.stdout == "kept|unset|new" and len(r.stderr) == 2) or fail("result " + repr([r.return_code, r.stdout, r.stderr]))
ctx.file("stderr", r.stderr)
(ctx.execute(["pwd"], working_directory = "sub/dir").stdout == "{stage}/sub/dir\n") or fail("cwd")
(ctx.execute(["cat", Label("//pkg:data.txt")]).stdout == "label-bytes") or fail("label")
ctx.file("in", "path-bytes")
(ctx.execute(["cat", ctx.path("in")], 30, {{}}, True, "").stdout == "path-bytes") or fail("path")
t = ctx.execute(["sleep", "5"], timeout = 1)
(t.return_code == 256 and t.stdout == "" and t.stderr == "Timed out") or fail("timeout")
m = ctx.execute(["no-such-program-n21"])
(m.return_code == 1 and m.stdout == "" and m.stderr == 'src/main/tools/process-wrapper-legacy.cc:80: "execvp(no-such-program-n21, ...)": No such file or directory\n') or fail("missing " + m.stderr)
e = ctx.execute([])
(e.return_code == 1 and e.stderr.startswith("No command specified.\nUsage: process-wrapper -- command arg1 @args\n")) or fail("empty " + e.stderr)
ctx.execute(["ln", "-s", str(ctx.path("in")), "absolute-link"])"#
        ));
        assert_eq!(inputs.host_facilities, BTreeSet::from(["execute"]));
        assert_eq!(fixture.bytes("stderr"), b"e\xff");
        // Absolute links into the repository directory are replanted.
        assert_eq!(
            fs::read_link(fixture.stage.path().join("absolute-link")).unwrap(),
            Path::new("in")
        );
        fixture.error(
            "ctx.execute(['sh', 1])",
            "Argument 1 of execute is neither a path, label, nor string.",
        );
        fixture.error(
            "ctx.execute(['sh'], environment = {'X': 1})",
            "environment values must be strings or None, got 1",
        );
        fixture.error(
            "ctx.execute(['true'], timeout = 0)",
            "timeout must be positive",
        );
        fixture.error("ctx.execute('true')", "parameter 'arguments'");
        fixture.error("ctx.execute(['sh', '-c', 'kill -15 $$'])", "SIGTERM");
        fixture.error(
            "ctx.execute(['true'], working_directory = '/no/such/dir/n21')",
            "must already exist",
        );
    }

    #[test]
    fn cc_configure_shaped_rule_uses_environ_which_execute_template_label_and_symlink() {
        let bin = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let bin_text = fs::canonicalize(bin.path())
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        executable(
            &bin.path().join("gcc"),
            "#!/bin/sh\necho 'gcc (Fake) 13.2.0'\n",
        );
        executable(
            &bin.path().join("clang-n21"),
            "#!/bin/sh\necho 'clang version 18' >&2\n",
        );
        let body = r##"env = ctx.os.environ
if "BAZEL_DO_NOT_DETECT_CPP_TOOLCHAIN" in env and env["BAZEL_DO_NOT_DETECT_CPP_TOOLCHAIN"] == "1":
    ctx.file("BUILD", "# disabled")
    return
cc = ctx.which(env.get("CC", "gcc"))
if cc == None:
    fail("Cannot find gcc")
version = ctx.execute([cc, "--version"])
verbose = ctx.execute([cc, "-v"])
paths = dict([(label, ctx.path(Label(label))) for label in ["//tools:BUILD.tpl", "//tools:cc_toolchain_config.bzl"]])
ctx.symlink(paths["//tools:cc_toolchain_config.bzl"], "cc_toolchain_config.bzl")
ctx.template("BUILD", paths["//tools:BUILD.tpl"], {"%{cc}": str(cc), "%{version}": (version.stdout + verbose.stderr).strip(), "%{cpu}": "k8" if ctx.os.arch == "amd64" else ctx.os.arch})
ctx.file("tools/cpp/empty.cc", "int main() {}")"##;
        for (cc, expected) in [
            (None, "gcc (Fake) 13.2.0"),
            (Some("clang-n21"), "clang version 18"),
        ] {
            let path = format!("{bin_text}:/usr/bin:/bin");
            let mut env: Vec<(&str, &[u8])> = vec![("PATH", path.as_bytes())];
            if let Some(cc) = cc {
                env.push(("CC", cc.as_bytes()));
            }
            let (fixture, _) = Fixture::new().with_env(&env);
            fixture.file("tools/BUILD", "");
            fixture.file(
                "tools/BUILD.tpl",
                "cc = '%{cc}'\nversion = '%{version}'\ncpu = '%{cpu}'\n",
            );
            fixture.file("tools/cc_toolchain_config.bzl", "CONFIG = 1\n");
            let inputs = fixture.ok(body);
            let program = format!("{bin_text}/{}", cc.unwrap_or("gcc"));
            assert_eq!(
                String::from_utf8(fixture.bytes("BUILD")).unwrap(),
                format!("cc = '{program}'\nversion = '{expected}'\ncpu = 'k8'\n")
            );
            assert_eq!(
                fs::read_link(fixture.stage.path().join("cc_toolchain_config.bzl")).unwrap(),
                Path::new(&fixture.root_text()).join("tools/cc_toolchain_config.bzl")
            );
            assert_eq!(fixture.bytes("cc_toolchain_config.bzl"), b"CONFIG = 1\n");
            assert_eq!(fixture.bytes("tools/cpp/empty.cc"), b"int main() {}");
            assert_eq!(inputs.host_facilities, BTreeSet::from(["execute", "which"]));
            assert_eq!(inputs.files.len(), 1);
            assert_eq!(inputs.files[0].path, "@@//tools/BUILD.tpl");
            assert_eq!(inputs.templates[0].source, "@@//tools/BUILD.tpl");
        }
        let (fixture, _) = Fixture::new().with_env(&[("BAZEL_DO_NOT_DETECT_CPP_TOOLCHAIN", b"1")]);
        fixture.ok(body);
        assert_eq!(fixture.bytes("BUILD"), b"# disabled");
    }

    #[test]
    fn sh_config_shaped_rule_prefers_bazel_sh_then_which() {
        let body = r#"sh_path = ctx.os.environ.get("BAZEL_SH")
if not sh_path:
    sh_path = ctx.which("sh")
    if sh_path:
        sh_path = str(sh_path)
ctx.file("BUILD", "sh_toolchain(path = %r)\n" % sh_path, executable = False)"#;
        let (fixture, _) =
            Fixture::new().with_env(&[("BAZEL_SH", b"/opt/custom/bash"), ("PATH", b"/bin")]);
        fixture.ok(body);
        assert_eq!(
            fixture.bytes("BUILD"),
            b"sh_toolchain(path = \"/opt/custom/bash\")\n"
        );
        let (fixture, _) = Fixture::new().with_env(&[("PATH", b"/n21-missing:/bin")]);
        fixture.ok(body);
        assert_eq!(
            fixture.bytes("BUILD"),
            b"sh_toolchain(path = \"/bin/sh\")\n"
        );
        let (fixture, _) = Fixture::new().with_env(&[]);
        fixture.ok(body);
        assert_eq!(fixture.bytes("BUILD"), b"sh_toolchain(path = None)\n");
    }

    #[test]
    fn multitool_shaped_rule_decodes_a_label_lockfile() {
        let fixture = Fixture::new();
        fixture.file(
            "multitool.lock.json",
            r#"{"$schema": "x", "tool": {"binaries": [{"kind": "file", "os": "linux", "cpu": "x86_64", "url": "https://example.invalid/tool", "sha256": "00"}]}}"#,
        );
        let loaded = fixture.load(
            r##"lock = json.decode(ctx.read(ctx.attr.lockfile))
for name, tool in lock.items():
    if name.startswith("$"):
        continue
    for binary in tool["binaries"]:
        if binary["os"] == ctx.os.name and binary["cpu"] == "x86_64":
            ctx.file("tools/" + name + "/BUILD.bazel", "# " + binary["url"] + "\n", executable = False)"##,
            "{'lockfile': attr.label(default = '//:multitool.lock.json', allow_single_file = True)}",
        );
        let inputs = fixture.run_loaded(loaded, BTreeMap::new()).unwrap();
        assert_eq!(
            fixture.bytes("tools/tool/BUILD.bazel"),
            b"# https://example.invalid/tool\n"
        );
        assert_eq!(inputs.files[0].path, "//:multitool.lock.json");
    }

    #[test]
    fn delete_rename_and_symlink_follow_bazel_output_rules() {
        let fixture = Fixture::new();
        let root = fixture.root_text();
        let outside = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let outside_text = fs::canonicalize(outside.path())
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        fs::write(outside.path().join("target"), b"outside").unwrap();
        fixture.ok(&format!(
            r#"ctx.file("d/x", "1")
(ctx.delete("d") and not ctx.delete("d") and not ctx.path("d").exists) or fail("delete")
ctx.file("a", "1")
ctx.rename("a", "n/b")
(ctx.read("n/b") == "1" and not ctx.path("a").exists) or fail("rename")
ctx.file("target-file", "inside")
ctx.symlink("target-file", "links/l")
ctx.symlink(ctx.path("{outside_text}/target"), "abs")
ctx.symlink(".", "self")
(ctx.read("links/l") == "inside" and ctx.read("abs") == "outside") or fail("follow")
ctx.symlink("target-file", "replaced")
ctx.file("replaced", "file now")"#
        ));
        assert_eq!(
            fs::read_link(fixture.stage.path().join("links/l")).unwrap(),
            Path::new("../target-file")
        );
        assert_eq!(
            fs::read_link(fixture.stage.path().join("self")).unwrap(),
            Path::new(".")
        );
        assert_eq!(
            fs::read_link(fixture.stage.path().join("abs")).unwrap(),
            Path::new(&outside_text).join("target")
        );
        assert!(
            fs::symlink_metadata(fixture.stage.path().join("replaced"))
                .unwrap()
                .is_file()
        );
        assert_eq!(fs::read(outside.path().join("target")).unwrap(), b"outside");
        fixture.error("ctx.rename('target-file', 'n/b')", "already exists");
        fixture.error(
            "ctx.rename('target-file', '../escape')",
            "Cannot write outside of the repository directory",
        );
        fixture.error("ctx.symlink('target-file', 'links/l')", "(File exists)");
        fixture.error(
            "ctx.symlink('target-file', '../escape')",
            "Cannot write outside of the repository directory",
        );
        fixture.error(
            "ctx.delete(Label('//:BUILD'))",
            "parameter 'path' got value of type 'Label', want 'string or path'",
        );
        fixture.error(
            &format!("ctx.delete('{root}/BUILD')"),
            "delete() can only be applied to external paths (that is, outside the workspace or ignored in .bazelignore)",
        );
        fixture.error(
            &format!("ctx.delete('{outside_text}/target')"),
            "outside the repository directory is not supported by the native profile",
        );
        assert!(outside.path().join("target").exists());
        assert!(fixture.root.path().join("BUILD").exists());
    }

    fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, bytes) in entries {
            let mut header = tar::Header::new_ustar();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder.append_data(&mut header, name, *bytes).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn extract_uses_native_archives_with_bazel_validation_and_merging() {
        let fixture = Fixture::new();
        fs::write(
            fixture.stage.path().join("archive.tar.gz"),
            tar_gz(&[("top/a.txt", b"A"), ("top/sub/b.txt", b"B")]),
        )
        .unwrap();
        fs::copy(
            fixture.stage.path().join("archive.tar.gz"),
            fixture.stage.path().join("noext"),
        )
        .unwrap();
        fixture.ok(r#"ctx.file("out/existing", "kept")
ctx.extract("archive.tar.gz", output = "out", stripPrefix = "top")
ctx.extract(ctx.path("noext"), "typed", "top", type = "tgz")
ctx.extract("archive.tar.gz", "out", strip_prefix = "top")"#);
        assert_eq!(fixture.bytes("out/a.txt"), b"A");
        assert_eq!(fixture.bytes("out/sub/b.txt"), b"B");
        assert_eq!(fixture.bytes("out/existing"), b"kept");
        assert_eq!(fixture.bytes("typed/a.txt"), b"A");
        assert!(fs::read_dir(fixture.stage.path()).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".extract-")
        }));
        fixture.error("ctx.extract('archive.tar.gz', strip_prefix = 'a', stripPrefix = 'b')", "extract() got multiple values for parameter 'strip_prefix' (via compatibility alias 'stripPrefix')");
        fixture.error(
            "ctx.extract('archive.tar.gz', strip_components = -1)",
            "extract() has an invalid argument for 'strip_components': -1. Must be non-negative.",
        );
        fixture.error(
            "ctx.extract('archive.tar.gz', strip_prefix = 'top', strip_components = 1)",
            "extract() got multiple strip values.",
        );
        fixture.error(
            "ctx.extract('archive.tar.gz', strip_components = 1)",
            "strip_components is not supported by the native extractor",
        );
        fixture.error(
            "ctx.extract('archive.tar.gz', rename_files = {'a': 'b'})",
            "rename_files is not supported by the native extractor",
        );
        fixture.error("ctx.extract('noext')", "Expected a file with a .zip, .jar, .war, .aar, .nupkg, .whl, .tar, .tar.gz, .tgz, .gz, .tar.xz, .txz, .xz, .tar.zst, .tzst, .zst, .tar.bz2, .tbz, .bz2, .ar, .deb or .7z suffix (got ");
        fixture.error("ctx.extract('noext', type = 'foo')", "No decompressor found for type foo. Available types are: zip, jar, war, aar, nupkg, whl, tar, tar.gz, tgz, gz, tar.xz, txz, xz, tar.zst, tzst, zst, tar.bz2, tbz, bz2, ar, deb or 7z");
        fixture.error(
            "ctx.extract('noext', type = '7z')",
            "archive type 7z is not supported by the native extractor",
        );
        fixture.error("ctx.extract('missing.zip')", "Archive path '");
        fixture.error("ctx.extract('out')", "attempting to extract a directory");
        fixture.error(
            "ctx.extract('archive.tar.gz', output = '../escape')",
            "Cannot write outside of the repository directory",
        );
        fixture.error(
            "ctx.extract('archive.tar.gz', 'other', 'nomatch')",
            &format!(
                "java.io.IOException: Error extracting {stage}/archive.tar.gz to {stage}: Prefix \"nomatch\" was given, but not found in the archive. Here are possible prefixes for this archive: \"top\".",
                stage = fixture.stage_text()
            ),
        );
        // A trailing slash is PathFragment-normalized (StripPrefixedPath.java:53).
        fixture.ok("ctx.extract('archive.tar.gz', 'slash', 'top/')\nctx.extract('archive.tar.gz', 'dotted', './top/./')");
        assert_eq!(fixture.bytes("slash/a.txt"), b"A");
        assert_eq!(fixture.bytes("dotted/sub/b.txt"), b"B");
    }

    #[test]
    fn extract_ar_archives_apply_rename_files_and_ignore_stripping() {
        // ArFunction (docs/AR_ARCHIVES.md); tests/ar_archives.rs replays the
        // genuine capture.
        let fixture = Fixture::new();
        let mut deb = b"!<arch>\n".to_vec();
        for (name, data) in [("debian-binary", &b"2.0\n"[..]), ("data.tar.xz", b"xz")] {
            deb.extend_from_slice(
                format!(
                    "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                    1,
                    0,
                    0,
                    100644,
                    data.len()
                )
                .as_bytes(),
            );
            deb.extend_from_slice(data);
        }
        fs::write(fixture.stage.path().join("pkg.deb"), &deb).unwrap();
        fs::write(fixture.stage.path().join("blob"), &deb).unwrap();
        fs::write(fixture.stage.path().join("bad.deb"), b"<!DOCTYPE html>\n").unwrap();
        fixture.ok(r#"ctx.extract("pkg.deb", output = "out", strip_prefix = "none", rename_files = {"debian-binary": "meta/version"})
ctx.extract("pkg.deb", output = "comp", strip_components = 2)
ctx.extract("blob", output = "typed", type = "ar")"#);
        assert_eq!(fixture.bytes("out/meta/version"), b"2.0\n");
        assert_eq!(fixture.bytes("out/data.tar.xz"), b"xz");
        assert_eq!(fixture.bytes("comp/debian-binary"), b"2.0\n");
        assert_eq!(fixture.bytes("typed/data.tar.xz"), b"xz");
        fixture.error(
            "ctx.extract('bad.deb', 'bad')",
            &format!(
                "java.io.IOException: Error extracting {stage}/bad.deb to {stage}: Invalid header <!DOCTYP",
                stage = fixture.stage_text()
            ),
        );
    }

    #[test]
    fn extract_reports_bazel_archive_link_errors_with_the_archive_basename() {
        // N19 archive links through repository_ctx.extract: the zip
        // link-escape error names the archive by its basename
        // (ZipReader.getFilename) and, like every decompressor IOException,
        // is wrapped by DecompressorValue.decompress (java:260-270).
        let fixture = Fixture::new();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default().unix_permissions(0o644);
        writer
            .add_symlink("pkg/a/esc", "../../outside", options)
            .unwrap();
        fs::create_dir(fixture.stage.path().join("dl")).unwrap();
        fs::write(
            fixture.stage.path().join("dl/esc.zip"),
            writer.finish().unwrap().into_inner(),
        )
        .unwrap();
        fixture.error(
            "ctx.extract('dl/esc.zip', 'out', 'pkg')",
            &format!(
                "java.io.IOException: Error extracting {stage}/dl/esc.zip to {stage}/dl: Zip entries cannot refer to files outside of their directory: esc.zip has a symlink a/esc pointing to ../../outside",
                stage = fixture.stage_text()
            ),
        );
        // Without the prefix the link stays inside the archive root.
        fixture.ok("ctx.extract('dl/esc.zip', 'whole')");
        assert_eq!(
            fs::read_link(fixture.stage.path().join("whole/pkg/a/esc")).unwrap(),
            Path::new("../../outside")
        );
        // Tar hard links: real links inside the callback; a missing target
        // keeps Bazel's FileSystem.createHardLink text, wrapped the same way.
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_ustar();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder
            .append_data(&mut header, "top/file", &b"F"[..])
            .unwrap();
        for (name, target) in [("top/hard", "top/file"), ("top/broken", "top/missing")] {
            let mut header = tar::Header::new_ustar();
            header.set_size(0);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Link);
            header.set_cksum();
            builder.append_link(&mut header, name, target).unwrap();
        }
        fs::write(
            fixture.stage.path().join("dl/links.tar"),
            builder.into_inner().unwrap(),
        )
        .unwrap();
        fixture.error(
            "ctx.extract('dl/links.tar', 'hl', 'top')",
            &format!(
                "java.io.IOException: Error extracting {stage}/dl/links.tar to {stage}/dl: File \"missing\" linked from \"broken\" does not exist",
                stage = fixture.stage_text()
            ),
        );
    }

    #[test]
    fn download_and_extract_preflight_policy_and_allow_fail_never_reach_the_network() {
        let fixture = Fixture::new();
        fixture.ok(r#"r = ctx.download_and_extract([], allow_fail = True)
(r.success == False and type(r) == "struct") or fail("allow_fail")"#);
        fixture.error("ctx.download_and_extract([])", "urls not set");
        fixture.error("ctx.download_and_extract('http://example.invalid/a.tar.gz')", "No URLs left after removing plain http URLs due to missing checksum. Please provide either a checksum or an https download location.");
        fixture.error("ctx.download_and_extract([1])", "Expected a string or sequence of strings for 'url' argument, but got 'int' item in the sequence");
        fixture.error("ctx.download_and_extract('https://example.invalid/a.tar.gz', sha256 = 'a' * 64, integrity = 'sha256-x')", "Expected either 'sha256' or 'integrity', but not both");
        fixture.error("ctx.download_and_extract('https://example.invalid/a.tar.gz', auth = {'https://example.invalid': {'type': 'basic'}})", "Found request to do basic auth for https://example.invalid without 'login' and 'password' being provided.");
        fixture.error(
            "ctx.download_and_extract('https://example.invalid/a.tar.gz', output = '../x')",
            "Cannot write outside of the repository directory",
        );
        fixture.error("ctx.download_and_extract('https://example.invalid/a.tar.gz', strip_prefix = 'a', stripPrefix = 'b')", "download_and_extract() got multiple values for parameter 'strip_prefix'");
        let mut disabled = Fixture::new();
        disabled.repositories = Arc::new(
            (*disabled.repositories)
                .clone()
                .with_repository_downloads_enabled(false),
        );
        disabled.error(
            "ctx.download_and_extract('https://example.invalid/a.tar.gz', sha256 = 'a' * 64)",
            "download is disabled.",
        );
        disabled.ok("(not ctx.download_and_extract('https://example.invalid/a.tar.gz', allow_fail = True).success) or fail('allow_fail')");
        // A malformed checksum is deferred past a failed download (java:1159-1170).
        disabled.ok("(not ctx.download_and_extract('https://example.invalid/a.tar.gz', sha256 = 'bad', allow_fail = True).success) or fail('deferred')");
        disabled.error(
            "ctx.download_and_extract('https://example.invalid/a.tar.gz', sha256 = 'bad')",
            "download is disabled.",
        );
    }

    #[test]
    fn download_into_a_directory_requests_every_mirror_then_fails_opening_the_output() {
        use crate::extension_io::tests::{Server, response};
        let fixture = Fixture::new();
        let server = Server::new(vec![
            response(200, b"body", ""),
            response(200, b"body", ""),
            response(200, b"body", ""),
        ]);
        let url = server.url.clone();
        let digest = sha256(b"body");
        fixture.ok(&format!(
            "ctx.file('d/x', '')\n(not ctx.download(['{url}/a', '{url}/b'], 'd', sha256 = '{digest}', allow_fail = True).success) or fail('dir')"
        ));
        // HttpDownloader connects before opening the output (captured N43b
        // child-then-root-directory: two requests, then `(Is a directory)`).
        assert_eq!(server.count(), 2);
        fixture.error(
            &format!("ctx.file('d/x', '')\nctx.download('{url}/c', 'd', sha256 = '{digest}')"),
            &format!(
                "java.io.IOException: Error downloading [{url}/c] to {stage}/d: {stage}/d (Is a directory)",
                stage = fixture.stage_text()
            ),
        );
        assert_eq!(server.count(), 3);
    }

    #[test]
    fn a_failing_download_cache_store_fails_the_download_like_bazel() {
        use crate::extension_io::tests::{Server, response};
        let fixture = Fixture::new();
        let downloads = fixture.root.path().join("cache/downloads");
        fs::create_dir_all(&downloads).unwrap();
        fs::write(downloads.join("sha256"), b"not a directory").unwrap();
        let server = Server::new(vec![response(200, b"body", ""), response(200, b"body", "")]);
        let url = server.url.clone();
        let digest = sha256(b"body");
        // DownloadManager.java:381-386 does not catch the store's IOException.
        fixture.ok(&format!(
            "(not ctx.download('{url}/a', 'a', sha256 = '{digest}', allow_fail = True).success) or fail('allow_fail')"
        ));
        fixture.error(
            &format!("ctx.download('{url}/b', 'b', sha256 = '{digest}')"),
            &format!("java.io.IOException: {}/sha256/", downloads.display()),
        );
        assert_eq!(server.count(), 2);
    }

    #[test]
    fn malformed_zip_reports_zip_readers_missing_end_of_central_directory() {
        let fixture = Fixture::new();
        fs::write(fixture.stage.path().join("bad.zip"), b"not a zip archive").unwrap();
        fixture.error(
            "ctx.extract('bad.zip', 'out')",
            &format!(
                "java.io.IOException: Error extracting {stage}/bad.zip to {stage}: Zip file 'bad.zip' is malformed. It does not contain an end of central directory record.",
                stage = fixture.stage_text()
            ),
        );
    }

    #[test]
    fn download_cache_serves_checksummed_downloads_offline_and_keys_canonical_ids() {
        use crate::extension_io::tests::{Server, response};
        let digest = sha256(b"cached body");
        let mut fixture = Fixture::new();
        let server = Server::new(vec![response(200, b"cached body", "")]);
        fixture.ok(&format!(
            "ctx.download('{}/first', 'a', sha256 = '{digest}', canonical_id = 'id-1')",
            server.url
        ));
        assert_eq!(server.count(), 1);
        // DownloadManager.java:264-277 precede the disable check (java:331).
        fixture.repositories = Arc::new(
            (*fixture.repositories)
                .clone()
                .with_repository_downloads_enabled(false),
        );
        fixture.ok(&format!(
            r#"r = ctx.download("https://example.invalid/other", "b", sha256 = "{digest}")
(r.success and r.size_bytes == 11) or fail(r)
ctx.read("b") == "cached body" or fail("bytes")
ctx.download("https://example.invalid/other", "c", sha256 = "{digest}", canonical_id = "id-1").success or fail("id")"#
        ));
        fixture.error(
            &format!(
                "ctx.download('https://example.invalid/other', 'd', sha256 = '{digest}', canonical_id = 'id-2')"
            ),
            "download is disabled.",
        );
        assert_eq!(server.count(), 1);
    }

    #[test]
    fn download_and_extract_fetches_verifies_and_extracts_from_a_loopback_fixture() {
        use crate::extension_io::tests::{Server, response};
        let archive = tar_gz(&[("top/a.txt", b"A"), ("top/sub/b.txt", b"B")]);
        let digest = sha256(&archive);
        let fixture = Fixture::new();
        let server = Server::new(vec![
            response(200, &archive, ""),
            response(200, &archive, ""),
            response(200, &archive, ""),
            response(200, &archive, ""),
        ]);
        let url = server.url.clone();
        let size = archive.len();
        fixture.ok(&format!(
            r#"r = ctx.download_and_extract("{url}/pkg.tar.gz", output = "dl", sha256 = "{digest}", stripPrefix = "top", canonical_id = "ignored")
(r.success and r.sha256 == "{digest}" and r.size_bytes == {size} and r.integrity.startswith("sha256-")) or fail("result " + repr(r))
t = ctx.download_and_extract(["{url}/no-extension"], "typed", "{digest}", "tgz")
(t.success) or fail("typed")
c = ctx.download_and_extract(["{url}/pkg.tar.gz"], "canon", "{digest}", canonical_id = "other")
(c.success) or fail("canonical")
m = ctx.download_and_extract("{url}/pkg.tar.gz", output = "mismatch", sha256 = "{zero}", allow_fail = True)
(not m.success) or fail("mismatch")"#,
            zero = "0".repeat(64)
        ));
        assert_eq!(fixture.bytes("dl/a.txt"), b"A");
        assert_eq!(fixture.bytes("dl/sub/b.txt"), b"B");
        assert_eq!(fixture.bytes("typed/top/a.txt"), b"A");
        assert_eq!(fixture.bytes("canon/top/a.txt"), b"A");
        assert!(!fixture.stage.path().join("mismatch/a.txt").exists());
        // The repository download cache (DownloadManager.java:264-277): the
        // second request with the same checksum is a cache hit without any
        // request; a canonical id that was never recorded is a miss.
        assert_eq!(server.count(), 3);
        fixture.error(
            &format!(
                "ctx.download_and_extract('{url}/pkg.tar.gz', output = 'deferred', sha256 = 'bad')"
            ),
            "Checksum error in repository @@canonical+repo",
        );
        assert_eq!(server.count(), 4);
    }

    #[test]
    fn download_streams_large_files_to_the_output_and_extract_streams_xz() {
        use crate::extension_io::tests::{Server, response};
        use std::os::unix::fs::PermissionsExt;
        // 20 MiB: above the former 8 MiB module I/O / 16 MiB repository caps.
        let big: Vec<u8> = (0..20 * 1024 * 1024).map(|i| (i % 239) as u8).collect();
        let digest = sha256(&big);
        let fixture = Fixture::new();
        let server = Server::new(vec![
            response(200, &big, ""),
            response(200, b"tool", ""),
            response(200, b"other", ""),
        ]);
        let url = server.url.clone();
        let size = big.len();
        fixture.ok(&format!(
            r#"r = ctx.download("{url}/big.bin", "out/big.bin", "{digest}")
(r.success and r.sha256 == "{digest}" and r.size_bytes == {size}) or fail("result " + repr(r))
t = ctx.download(["{url}/tool"], output = "bin/tool", executable = True, sha256 = "{tool}")
t.success or fail("tool")
m = ctx.download("{url}/other", "mismatch", sha256 = "{never}", allow_fail = True)
(not m.success) or fail("mismatch")"#,
            tool = sha256(b"tool"),
            never = sha256(b"never served"),
        ));
        assert!(fixture.bytes("out/big.bin") == big);
        assert_eq!(fixture.bytes("bin/tool"), b"tool");
        let mode = |name: &str| {
            fs::metadata(fixture.stage.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!((mode("out/big.bin"), mode("bin/tool")), (0o644, 0o755));
        // Like Bazel, the mismatching bytes stay at the output (allow_fail).
        assert_eq!(fixture.bytes("mismatch"), b"other");
        // No leftover scratch directories in the repository.
        for entry in fs::read_dir(fixture.stage.path()).unwrap() {
            let name = entry.unwrap().file_name();
            assert!(
                !name.to_string_lossy().starts_with(".download-"),
                "{name:?}"
            );
        }
        // block = False: the failure surfaces from wait() (java:640-697).
        // (A checksum in the download cache would be served without a request.)
        let never = sha256(b"never served");
        let gone = Server::new(vec![response(404, b"", "")]);
        fixture.error(
            &format!(
                "p = ctx.download('{}/x', 'x', sha256 = '{never}', block = False)\nprint('pending')\np.wait()",
                gone.url
            ),
            "Error downloading",
        );
        // HTTP failures use HttpDownloader's aggregated message (java:243-262).
        let missing = Server::new(vec![response(404, b"", ""), response(404, b"", "")]);
        let base = missing.url.clone();
        fixture.error(
            &format!("ctx.download(['{base}/a', '{base}/b'], 'gone', sha256 = '{never}')"),
            &format!(
                "java.io.IOException: Error downloading [{base}/a, {base}/b] to {}/gone: GET returned 404 Fixture",
                fixture.stage_text()
            ),
        );
        // extract() streams a .tar.xz from the repository directory.
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_ustar();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "zig-1/lib/a.zig", &b"z"[..])
            .unwrap();
        let tar = tar.into_inner().unwrap();
        let mut xz =
            lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::default()).unwrap();
        xz.write_all(&tar).unwrap();
        fs::write(
            fixture.stage.path().join("zig.tar.xz"),
            xz.finish().unwrap(),
        )
        .unwrap();
        fixture.ok("ctx.extract('zig.tar.xz', output = 'zig', stripPrefix = 'zig-1/')");
        assert_eq!(fixture.bytes("zig/lib/a.zig"), b"z");
    }

    #[test]
    fn download_reuses_a_matching_output_offline_and_never_writes_through_links() {
        use crate::extension_io::tests::{Server, response};
        let tool = sha256(b"tool");
        // DownloadManager.java:247-261: a matching output skips the download,
        // also with downloads disabled; a mismatching one is downloaded.
        let mut disabled = Fixture::new();
        disabled.repositories = Arc::new(
            (*disabled.repositories)
                .clone()
                .with_repository_downloads_enabled(false),
        );
        disabled.ok(&format!(
            r#"ctx.file("bin/tool", "tool", executable = False)
r = ctx.download("https://example.invalid/tool", "bin/tool", "{tool}", executable = True)
(r.success and r.sha256 == "{tool}" and r.size_bytes == 4) or fail("reuse " + repr(r))"#
        ));
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(disabled.stage.path().join("bin/tool"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        disabled.error(
            &format!(
                "ctx.file('stale', 'old')\nctx.download('https://example.invalid/tool', 'stale', '{tool}')"
            ),
            "download is disabled.",
        );
        // An existing hard-linked output is replaced, not written through.
        let fixture = Fixture::new();
        let server = Server::new(vec![response(200, b"tool", "")]);
        fs::write(fixture.stage.path().join("orig"), b"keep").unwrap();
        fs::hard_link(
            fixture.stage.path().join("orig"),
            fixture.stage.path().join("linked"),
        )
        .unwrap();
        fixture.ok(&format!(
            "ctx.download('{}/tool', 'linked', '{tool}')",
            server.url
        ));
        assert_eq!(fixture.bytes("linked"), b"tool");
        assert_eq!(fixture.bytes("orig"), b"keep");
    }

    #[test]
    fn captured_bazel_messages_and_value_shapes_are_reproduced() {
        let fixture = Fixture::new();
        let stage = fixture.stage_text();
        let root = fixture.root_text();
        fs::create_dir(fixture.root.path().join("nobuild")).unwrap();
        fs::write(fixture.root.path().join("nobuild/x.txt"), b"").unwrap();
        fixture.file("pkg/BUILD", "");
        // PackageLookupFunction.explainNoBuildFileValue (java:130-155).
        for (body, label, package) in [
            ("ctx.path(Label('//nopkg:x'))", "//nopkg:x", "nopkg"),
            (
                "ctx.path(Label('//nobuild:x.txt'))",
                "//nobuild:x.txt",
                "nobuild",
            ),
            (
                "ctx.read(Label('//nobuild:x.txt'))",
                "//nobuild:x.txt",
                "nobuild",
            ),
            (
                "ctx.template('t', Label('//nopkg:t'))",
                "//nopkg:t",
                "nopkg",
            ),
        ] {
            fixture.error(body, &format!("Unable to load package for {label}: BUILD file not found in any of the following directories. Add a BUILD file to a directory to mark it as a package.\n - {package}"));
        }
        fixture.error(
            "ctx.read(Label('//pkg:missing.txt'))",
            &format!(
                "java.io.FileNotFoundException: {root}/pkg/missing.txt (No such file or directory)"
            ),
        );
        // The repository directory does not exist until the first write.
        fixture.error(
            "ctx.read('.')",
            &format!("java.io.FileNotFoundException: {stage} (No such file or directory)"),
        );
        fixture.error(
            "ctx.template('t', '.')",
            &format!("java.io.FileNotFoundException: {stage} (Is a directory)"),
        );
        fixture.error(
            "ctx.file('x', '')\nctx.read('.')",
            &format!("attempting to read() a directory: {stage}"),
        );
        fixture.error(
            "ctx.read('nope')",
            &format!("java.io.FileNotFoundException: {stage}/nope (No such file or directory)"),
        );
        fixture.error(
            "ctx.symlink('nothere', 'dangling')\nctx.path('dangling').realpath",
            &format!("[unix_jni.cc:382] {stage}/nothere (No such file or directory)"),
        );
        fixture.error(
            "ctx.file('e', '')\nctx.symlink('a', 'e')",
            &format!("java.io.IOException: Could not create symlink from {stage}/a to {stage}/e: [unix_jni.cc:297] {stage}/e (File exists)"),
        );
        fixture.error(
            "ctx.rename('nope', 'b')",
            &format!("java.io.IOException: Could not rename {stage}/nope to {stage}/b: [unix_jni.cc:638] {stage}/nope -> {stage}/b (No such file or directory)"),
        );
        fixture.ok("ctx.file('a1', '')\nctx.file('b1', '')");
        fixture.error(
            "ctx.rename('a1', 'b1')",
            &format!(
                "java.io.IOException: Could not rename {stage}/a1 to {stage}/b1: already exists"
            ),
        );
        // Value shapes: repr(ctx), type(ctx.attr), hashable exec_result.
        fixture.ok(r#"(repr(ctx) == "<unknown object com.google.devtools.build.lib.bazel.repository.starlark.StarlarkRepositoryContext>" and str(ctx) == repr(ctx)) or fail("repr")
(type(ctx.attr) == "RepoDefinition" and ctx.attr.name == "canonical+repo" and dir(ctx.attr) == ["name"] and not hasattr(ctx.attr, "nope")) or fail("attr")
r = ctx.execute(["true"])
s = ctx.execute(["true"])
d = {r: 1, s: 2}
(d[r] == 1 and d[s] == 2 and r == r and r != s and len(d) == 2) or fail("hash")"#);
        // Checksum.java messages, deferred past a failed download.
        let mut disabled = Fixture::new();
        disabled.repositories = Arc::new(
            (*disabled.repositories)
                .clone()
                .with_repository_downloads_enabled(false),
        );
        disabled.ok("(not ctx.download_and_extract('https://example.invalid/a.tgz', integrity = 'nonsense', allow_fail = True).success) or fail('deferred')");
        for (option, message) in [
            ("sha256 = 'abc'", "Invalid SHA-256 checksum 'abc'"),
            (
                "integrity = 'nonsense'",
                "Unsupported checksum algorithm: 'nonsense' (expected SHA-1, SHA-256, SHA-384, or SHA-512)",
            ),
            ("integrity = 'sha256-!!'", "Invalid base64 '!!'"),
            (
                "integrity = 'sha256-AAAA'",
                "Invalid SHA-256 SRI checksum 'sha256-AAAA'",
            ),
        ] {
            assert_eq!(
                checksum_text_error(
                    option
                        .strip_prefix("sha256 = '")
                        .map_or("", |rest| rest.trim_end_matches('\'')),
                    option
                        .strip_prefix("integrity = '")
                        .map_or("", |rest| rest.trim_end_matches('\'')),
                )
                .as_deref(),
                Some(message)
            );
        }
        assert_eq!(checksum_text_error(&"a".repeat(64), ""), None);
        assert_eq!(
            checksum_text_error("", "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="),
            None
        );
    }

    #[test]
    fn download_checksum_mismatch_and_missing_suffix_name_the_downloaded_file() {
        use crate::extension_io::tests::{Server, response};
        let archive = tar_gz(&[("pre/a.txt", b"A")]);
        let fixture = Fixture::new();
        let server = Server::new(vec![
            response(200, &archive, ""),
            response(200, &archive, ""),
            response(200, &archive, ""),
        ]);
        let url = server.url.clone();
        let stage = fixture.stage_text();
        let actual = sha256(&archive);
        let error = fixture
            .run(&format!(
                "ctx.download_and_extract('{url}/arc.tar.gz', sha256 = '{}')",
                "0".repeat(64)
            ))
            .unwrap_err();
        let expected_tail = format!(
            "/arc.tar.gz: Checksum was {actual} but wanted {}",
            "0".repeat(64)
        );
        assert!(
            error.contains(&format!(
                "java.io.IOException: Error downloading [{url}/arc.tar.gz] to {stage}/temp"
            )) && error.contains(&expected_tail),
            "{error}"
        );
        let error = fixture
            .run(&format!(
                "ctx.download_and_extract('{url}/arc.tar.gz', integrity = 'sha256-{}=')",
                "A".repeat(43)
            ))
            .unwrap_err();
        assert!(
            error.contains("Checksum was sha256-") && error.contains(" but wanted sha256-AAAA"),
            "{error}"
        );
        let error = fixture
            .run(&format!(
                "ctx.download_and_extract('{url}/arcnoext', sha256 = '{actual}')"
            ))
            .unwrap_err();
        assert!(
            error.contains("suffix (got ")
                && error.contains(&format!("{stage}/temp"))
                && error.contains("/arcnoext)"),
            "{error}"
        );
    }

    #[test]
    fn unimplemented_bazel_methods_fail_explicitly_and_attr_is_exact() {
        let fixture = Fixture::new();
        fixture.ok("(hasattr(ctx, 'download') and hasattr(ctx, 'execute') and not hasattr(ctx.attr, 'undeclared')) or fail('surface')\n(ctx.report_progress('status') == None and ctx.report_progress() == None) or fail('progress')");
        fixture.error(
            "ctx.file('partial', 'private')\nctx.watch_tree('x')",
            "can't call watch_tree() on non-directory",
        );
        assert_eq!(fixture.bytes("partial"), b"private");
        // watch_tree (StarlarkRepositoryContext.java:568-609): the directory
        // type and its transitive tree; never inside the repository.
        fixture.error(
            "ctx.file('d/f', 'x')\nctx.watch_tree('d')",
            "attempted to watch path under working directory",
        );
        fixture.file("tree/sub/leaf", b"leaf");
        let inputs = fixture
            .run(&format!(
                "ctx.watch_tree({:?})",
                fixture.root.path().join("tree").display()
            ))
            .unwrap();
        let kinds: Vec<_> = inputs.files.iter().map(|f| f.kind).collect();
        assert_eq!(kinds, ["file", "dirtree"]);
        assert_eq!(inputs.files[0].state, "directory");
        assert!(inputs.files[1].state.starts_with("dirtree:"));
        fixture.error("ctx.report_progress(1)", "report_progress");
        fixture.error("ctx.template('out', 'partial', None)", "dict");
        fixture.error(
            "ctx.template('out', 'partial', watch_template = 'maybe')",
            "bad value for 'watch' parameter",
        );
        fixture.error("ctx.template(path='out', template='partial')", "path");
        fixture.error(
            "ctx.template('out', 'partial', watch_template = 'yes')",
            "attempted to watch path under working directory",
        );
    }

    #[test]
    fn repo_metadata_is_an_opaque_record_with_bazel_argument_checks() {
        let fixture = Fixture::new();
        fixture.ok(
            r#"m = ctx.repo_metadata(reproducible = True)
type(m) == "repo_metadata" or fail(type(m))
repr(m) == "<unknown object com.google.devtools.build.lib.bazel.repository.starlark.RepoMetadata>" or fail(repr(m))
str(m) == repr(m) or fail(str(m))
dir(m) == [] or fail(dir(m))
(not hasattr(m, "reproducible")) or fail("fields")
m == ctx.repo_metadata(reproducible = True) or fail("record equality")
m != ctx.repo_metadata() or fail("reproducibility participates")
d = ctx.repo_metadata(attrs_for_reproducibility = {"integrity": "sha256-x", "n": 1})
d == ctx.repo_metadata(attrs_for_reproducibility = {"n": 1, "integrity": "sha256-x"}) or fail("dict contents")
d != ctx.repo_metadata(attrs_for_reproducibility = {"n": 2}) or fail("dict values")
ctx.repo_metadata() == ctx.repo_metadata(reproducible = False, attrs_for_reproducibility = {}) or fail("defaults")
bool(m) or fail("truth")
return m"#,
        );
        for (call, message) in [
            (
                "ctx.repo_metadata(reproducible = True, attrs_for_reproducibility = {'a': 1})",
                "attrs_for_reproducibility can only be specified if reproducible is False",
            ),
            (
                "ctx.repo_metadata(attrs_for_reproducibility = {1: 'a'})",
                "got dict<int, string> for 'attrs_for_reproducibility', want dict<string, unknown>",
            ),
            (
                "ctx.repo_metadata(reproducible = 1)",
                "in call to repo_metadata(), parameter 'reproducible' got value of type 'int', want 'bool'",
            ),
            (
                "ctx.repo_metadata(attrs_for_reproducibility = [])",
                "in call to repo_metadata(), parameter 'attrs_for_reproducibility' got value of type 'list', want 'dict'",
            ),
            ("ctx.repo_metadata(True)", "repo_metadata"),
            ("{ctx.repo_metadata(): 1}", "repo_metadata"),
        ] {
            fixture.error(call, message);
        }
    }

    #[test]
    fn implementation_return_values_follow_repository_fetch_function() {
        let fixture = Fixture::new();
        // RepositoryFetchFunction.java:681-695: dicts with string keys, any
        // repo_metadata and every other value are accepted.
        for body in [
            "return None",
            "return ctx.repo_metadata(reproducible = True)",
            "return ctx.repo_metadata(attrs_for_reproducibility = {'name': 'x', 'n': [1]})",
            "return {}",
            "return {'name': 'changed', 'text': 1, 'list': [None]}",
            "return 42",
            "return 'text'",
            "return [1]",
            "return True",
        ] {
            fixture.ok(&format!("ctx.file('data', 'x')\n{body}"));
        }
        fixture.error(
            "return {1: 'a'}",
            "got dict<int, string> for 'return value', want dict<string, unknown>",
        );
        fixture.error(
            "return {'ok': 1, None: [1]}",
            "got dict<NoneType, list> for 'return value', want dict<string, unknown>",
        );
    }

    #[test]
    fn patch_applies_unified_diffs_with_bazel_paths_strip_directory_and_watch() {
        let fixture = Fixture::new();
        fixture.file(
            "fixes/BUILD.bazel",
            "exports_files(['proto_gazelle.patch'])\n",
        );
        // build_stack_rules_proto-shaped: a/b prefixes with timestamps, -p1.
        fixture.file(
            "fixes/proto_gazelle.patch",
            "--- a/rules/proto_gazelle.bzl\t2024-05-14 13:52:59.000000000 +0000\n+++ b/rules/proto_gazelle.bzl\t2026-06-04 19:22:29.815049510 +0000\n@@ -1,3 +1,4 @@\n load(\"@bazel_skylib//lib:shell.bzl\", \"shell\")\n+load(\"@rules_shell//shell:sh_binary.bzl\", \"sh_binary\")\n \n-native.sh_binary(name = \"x\")\n+sh_binary(name = \"x\")\n",
        );
        fixture.file(
            "host.patch",
            "--- old.txt\n+++ old.txt\n@@ -1 +1 @@\n-a\n+b\n",
        );
        let root = fixture.root_text();
        let inputs = fixture.ok(&format!(
            r#"ctx.file("rules/proto_gazelle.bzl", 'load("@bazel_skylib//lib:shell.bzl", "shell")\n\nnative.sh_binary(name = "x")\n')
ctx.patch(Label("//fixes:proto_gazelle.patch"), 1)
ctx.file("sub/old.txt", "a\n")
ctx.patch("{root}/host.patch", directory = "sub", watch_patch = "no")
ctx.file("old.txt", "a\n")
ctx.patch(ctx.path("{root}/host.patch"), strip = -3)
ctx.file("local.patch", "--- /dev/null\n+++ x/new.txt\n@@ -0,0 +1 @@\n+created\n")
ctx.patch("local.patch", strip = 1)
ctx.read("old.txt") == "b\n" or fail("strip <= 0")
ctx.read("sub/old.txt") == "b\n" or fail("directory")"#
        ));
        assert_eq!(
            fixture.bytes("rules/proto_gazelle.bzl"),
            b"load(\"@bazel_skylib//lib:shell.bzl\", \"shell\")\nload(\"@rules_shell//shell:sh_binary.bzl\", \"sh_binary\")\n\nsh_binary(name = \"x\")\n"
        );
        assert_eq!(fixture.bytes("new.txt"), b"created\n");
        // watch_patch='auto' records host/label patch files (maybeWatch), never
        // files inside the repository; 'no' records nothing.
        let recorded: Vec<_> = inputs.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            recorded.contains(&"//fixes:proto_gazelle.patch"),
            "{recorded:?}"
        );
        assert_eq!(
            recorded
                .iter()
                .filter(|path| path.ends_with("host.patch"))
                .count(),
            1,
            "{recorded:?}"
        );
        assert!(!recorded.iter().any(|path| path.contains("local.patch")));
    }

    #[test]
    fn patch_failures_use_bazel_messages() {
        let fixture = Fixture::new();
        let stage = fixture.stage_text();
        let root = fixture.root_text();
        fs::create_dir(fixture.root.path().join("dir")).unwrap();
        fixture.file("bad.patch", "--- a.txt\n+++ a.txt\n@@ -1 +1 @@\n-x\n+y\n");
        fixture.file(
            "ctx.patch",
            "--- a.txt\n+++ a.txt\n@@ -1,5 +1,5 @@\n u\n v\n-x\n+y\n w\n t\n",
        );
        for (body, message) in [
            (
                "ctx.patch('missing.patch')".to_owned(),
                format!(
                    "Error applying patch {stage}/missing.patch: Cannot find patch file: {stage}/missing.patch"
                ),
            ),
            (
                format!("ctx.patch('{root}/dir')"),
                format!("attempting to use a directory as patch file: {root}/dir"),
            ),
            (
                format!("ctx.patch('{root}/bad.patch', directory = '{root}')"),
                format!("Cannot write outside of the repository directory for path {root}"),
            ),
            (
                format!("ctx.file('a.txt', 'z\\n')\nctx.patch('{root}/bad.patch')"),
                // A one-line hunk only "matching" with fuzz 1 crashes Bazel 9.3
                // (captured B4/mismatch_context, exit 37).
                "genuine Bazel 9.3 crashes applying this patch".to_owned(),
            ),
            (
                format!("ctx.file('a.txt', 'u\\nz\\nw\\n')\nctx.patch('{root}/ctx.patch')"),
                format!(
                    "Error applying patch {root}/ctx.patch: in patch applied to {stage}/a.txt: could not apply patch due to CONTENT_DOES_NOT_MATCH_TARGET, error applying change near line 1"
                ),
            ),

            (
                format!("ctx.patch('{root}/bad.patch', watch_patch = 'maybe')"),
                "bad value for 'watch' parameter; want 'yes', 'no', or 'auto', got maybe"
                    .to_owned(),
            ),
            (
                "ctx.file('in.patch', '')\nctx.patch('in.patch', watch_patch = 'yes')".to_owned(),
                "attempted to watch path under working directory".to_owned(),
            ),
            (
                "ctx.patch(1)".to_owned(),
                "in call to patch(), parameter 'patch_file' got value of type 'int', want 'string, Label, or path'".to_owned(),
            ),
            (
                format!("ctx.patch(patch_file = '{root}/bad.patch')"),
                "patch_file".to_owned(),
            ),
            (
                format!("ctx.patch('{root}/bad.patch', 0, '')"),
                "patch".to_owned(),
            ),
        ] {
            fixture.error(&body, &message);
        }
        // An empty patch is a no-op, also for a fresh repository directory.
        fixture.file("empty.patch", "");
        fixture.ok(&format!("ctx.patch('{root}/empty.patch')"));
    }

    #[test]
    fn base64_and_strings_shaped_helpers_write_latin1_bytes() {
        // bazel_lib lib/private/strings.bzl:262-555 and base64.bzl:9-35: a
        // user-defined chr/ord over octal-escape tables; decode() of a
        // non-ASCII payload yields one char per byte, written as that byte.
        let fixture = Fixture::new();
        let table = (0..=255)
            .map(|i| format!("\"\\{i:03o}\""))
            .collect::<Vec<_>>()
            .join(", ");
        fixture.file(
            "strings.bzl",
            format!(
                "INT_TO_CHAR = [{table}]\nCHAR_TO_INT = {{c: i for i, c in enumerate(INT_TO_CHAR)}}\ndef chr(i):\n    if i < 0 or i > 255:\n        fail(\"expected a int between 0 and 255 (inclusive)\")\n    return INT_TO_CHAR[i]\ndef ord(c):\n    if len(c) != 1:\n        fail(\"expected a string with a single character\")\n    return CHAR_TO_INT.get(c)\n"
            ),
        );
        fixture.file(
            "base64.bzl",
            "load(\":strings.bzl\", \"chr\", \"ord\")\nB = \"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/\"\ndef encode(data):\n    bits = \"\".join([\"\".join([str((ord(c) >> (7 - k)) & 1) for k in range(8)]) for c in data.elems()])\n    bits += \"0\" * ((6 - len(bits) % 6) % 6)\n    out = \"\".join([B[int(bits[i:i + 6], 2)] for i in range(0, len(bits), 6)])\n    return out + \"=\" * ((4 - len(out) % 4) % 4)\ndef decode(data):\n    padding = data.count(\"=\")\n    data = data.replace(\"=\", \"A\")\n    bits = \"\".join([\"\".join([str((B.find(c) >> (5 - k)) & 1) for k in range(6)]) for c in data.elems()])\n    out = \"\".join([chr(int(bits[i:i + 8], 2)) for i in range(0, len(bits) - len(bits) % 8, 8)])\n    return out if padding == 0 else out[:-padding]\n",
        );
        fixture.file(
            "defs.bzl",
            "load(\":base64.bzl\", \"decode\", \"encode\")\nload(\":strings.bzl\", \"chr\", \"ord\")\ndef _impl(ctx):\n    raw = decode(\"/wDpw6k=\")\n    len(raw) == 5 or fail(len(raw))\n    [ord(c) for c in raw.elems()] == [255, 0, 233, 195, 169] or fail(\"bytes\")\n    encode(raw) == \"/wDpw6k=\" or fail(encode(raw))\n    ctx.file(\"decoded\", raw)\n    ctx.file(\"literal\", \"\\377\\200\" + chr(65) + chr(233))\nR = repository_rule(implementation=_impl)\n",
        );
        let loaded = load_definition_in(
            fixture.repositories.clone(),
            &Label::parse_canonical("//:defs.bzl").unwrap(),
            "R",
            &crate::package::bzl_globals(),
        )
        .unwrap();
        fixture.run_loaded(loaded, BTreeMap::new()).unwrap();
        assert_eq!(fixture.bytes("decoded"), [0xff, 0x00, 0xe9, 0xc3, 0xa9]);
        assert_eq!(fixture.bytes("literal"), [0xff, 0x80, b'A', 0xe9]);
    }

    /// The snapshot-observed recorded-input states equal the execution-side
    /// ones (warm reuse compares them with what execution recorded).
    #[test]
    fn observed_recorded_states_match_execution_states() {
        let temp = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("tree/sub")).unwrap();
        fs::write(root.join("tree/a.txt"), b"alpha").unwrap();
        fs::write(root.join("tree/sub/b.bin"), [0u8, 0xff, 1]).unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("tree/link")).unwrap();
        std::os::unix::fs::symlink("/nonexistent", root.join("tree/dangling")).unwrap();
        assert_eq!(
            observed_tree_digest(&root.join("tree")).unwrap(),
            tree_digest(&root.join("tree")).unwrap()
        );
        for name in [
            "tree",
            "tree/a.txt",
            "tree/link",
            "tree/dangling",
            "missing",
        ] {
            assert_eq!(
                recorded_file_state(&root.join(name)),
                file_state(&root.join(name)).ok(),
                "{name}"
            );
        }
        assert!(
            recorded_dirents_state(&root.join("tree"))
                .is_some_and(|state| state.starts_with("dirents:"))
        );
        assert_eq!(recorded_dirents_state(&root.join("missing")), None);
    }
}
