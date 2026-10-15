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

const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_OPERATIONS: usize = 10_000;
const MAX_EXECUTIONS: usize = 1_000;
const MAX_TIMEOUT_SECONDS: i64 = 24 * 60 * 60;
const MAX_ARGV_BYTES: usize = 2 * 1024 * 1024;
const MAX_DIRENTS: usize = 100_000;
const CHECKSUM_MISMATCH: &str = "Checksum was ";
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
    /// which, downloads). Values are deliberately absent: the native store
    /// re-executes every repository in every invocation (repository_store.rs),
    /// so no cached publication is ever reused without re-running them.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub host_facilities: BTreeSet<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct TemplateInput {
    pub source: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct FileInput {
    /// `@@//relative` inside the main workspace, a canonical label for label
    /// reads, otherwise the absolute host path (RepoCacheFriendlyPath).
    pub path: String,
    /// `file` or `dirents` (RepoRecordedInput.File / Dirents).
    pub kind: &'static str,
    /// `missing`, `directory`, `other`, `file:<sha256>` or `dirents:<sha256>`.
    pub state: String,
}

#[derive(Default)]
struct State {
    inputs: ExecutionInputs,
    written: usize,
    read: usize,
    writes: usize,
    executions: usize,
    /// Whether Bazel would have created the repository directory yet: it does
    /// so lazily on the first write/makeDirectories/execute, not before the
    /// callback (captured read('.') and template('.') failures).
    created: bool,
    budget: Option<transport::SharedBudget>,
    client: Option<reqwest::blocking::Client>,
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
    pub(super) struct Context {
        pub repositories: Arc<RepositorySet>,
        pub stage: PathBuf,
        pub workspace: PathBuf,
        pub label: Label,
        /// Canonical repository name (identifyingStringForLogging).
        pub canonical: String,
        pub state: RefCell<State>,
        pub environment: RefCell<Watch>,
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
    }

    impl<V> fmt::Display for RepositoryCtxGen<V> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // No repr override: Starlark's default for the Java class
            // (captured repr/str of ctx).
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
                "workspace_root" => Some(
                    heap.alloc(RepositoryPath {
                        bytes: crate::repository_rule::latin1_bytes(&self.workspace_root)
                            .unwrap_or_default(),
                    }),
                ),
                _ => None,
            }
        }

        fn dir_attr(&self) -> Vec<String> {
            ["attr", "name", "original_name", "os", "workspace_root"]
                .map(str::to_owned)
                .to_vec()
        }
    }

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
use values::{Context, ExecResult, RepoDefinitionGen, RepositoryCtxGen, RepositoryPath};

fn context<'a>(eval: &'a Evaluator<'_, '_, '_>) -> anyhow::Result<&'a Context> {
    eval.extra
        .and_then(|extra| extra.downcast_ref::<Context>())
        .ok_or_else(|| anyhow!("repository_ctx is only available during repository execution"))
}

/// True while evaluating a repository-rule callback (byte-string profile).
pub(crate) fn evaluating_callback(eval: &Evaluator<'_, '_, '_>) -> bool {
    context(eval).is_ok()
}

pub(crate) fn repository_context(eval: &Evaluator<'_, '_, '_>) -> Option<Arc<RepositorySet>> {
    context(eval)
        .ok()
        .map(|context| context.repositories.clone())
}

pub(crate) fn definition_package(eval: &Evaluator<'_, '_, '_>) -> Option<PackageId> {
    let context = context(eval).ok()?;
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
    ensure!(
        path.split('/').count() <= 64,
        "repository path exceeds depth 64"
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

fn regular_file(path: &Path) -> anyhow::Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "{} must be a regular file, not a symlink",
        path.display()
    );
    Ok(())
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

/// Read at most `limit` bytes, failing explicitly beyond it.
fn read_limited(path: &Path, limit: usize, what: &str) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut file = fs::File::open(path).map_err(|error| file_not_found(path, &error))?;
    if file.metadata().is_ok_and(|metadata| metadata.is_dir()) {
        return Err(file_not_found(
            path,
            &std::io::Error::from_raw_os_error(21), // EISDIR, as FileInputStream reports it
        ));
    }
    Read::by_ref(&mut file)
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| file_not_found(path, &error))?;
    ensure!(bytes.len() <= limit, "{what} exceeds 16 MiB limit");
    Ok(bytes)
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
    ];
    if let Some((_, native)) = NATIVE.iter().find(|(name, _)| *name == kind) {
        return Ok(native);
    }
    if BAZEL_FORMATS.contains(&kind) {
        bail!(
            "archive type {kind} is not supported by the native extractor (supported: zip, jar, war, aar, nupkg, whl, tar, tar.gz, tgz)"
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

impl Context {
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
        let root = self
            .repositories
            .root(repository)
            .map_err(anyhow::Error::msg)?
            .to_path_buf();
        if self
            .repositories
            .ignored_subdirectories(repository)
            .map_err(anyhow::Error::msg)?
            .matching_entry(package)
            .is_some()
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

    /// Strict Label file read (template/read with a Label argument): the
    /// package marker and every component must be real, non-symlink entries.
    fn read_label(&self, canonical: &str) -> anyhow::Result<Vec<u8>> {
        let label = Label::parse_canonical(canonical).map_err(anyhow::Error::msg)?;
        let raw_root = self
            .repositories
            .root(&label.repository)
            .map_err(anyhow::Error::msg)?;
        no_build_file(raw_root, &label)?;
        let root = checked_root(raw_root)?;
        let package = if label.package.is_empty() {
            root.clone()
        } else {
            let path = parents(&root, &label.package, false)?;
            real_dir(&path)?;
            path
        };
        let mut marker = false;
        for name in ["BUILD.bazel", "BUILD"] {
            match fs::symlink_metadata(package.join(name)) {
                Ok(metadata) => {
                    ensure!(
                        metadata.file_type().is_file(),
                        "package BUILD marker must be a regular file, not a symlink"
                    );
                    marker = true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        ensure!(
            marker,
            "Unable to load package for {label}: template label {label} requires a package BUILD file"
        );
        let path = parents(&package, &label.name, false)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                bail!("attempting to read() a directory: {}", path.display())
            }
            Err(error) => return Err(file_not_found(&path, &error)),
            Ok(_) => {}
        }
        regular_file(&path)?;
        read_limited(&path, MAX_FILE_BYTES, "template input")
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
        ensure!(
            relative.len() <= 4096,
            "repository path must contain 1..4096 bytes"
        );
        ensure!(
            relative.split('/').count() <= 64,
            "repository path exceeds depth 64"
        );
        ensure!(
            !relative.contains(['\\', ':', '?', '#', '%'])
                && !relative.chars().any(char::is_control),
            "unsupported repository output path {relative:?}: the native store profile forbids control characters, '\\\\', ':', '?', '#', and '%'"
        );
        Ok(relative.to_owned())
    }

    /// makeDirectories for an output: parents become real directories; a
    /// symlinked parent is never traversed while writing.
    fn output_parents(&self, relative: &str) -> anyhow::Result<PathBuf> {
        if relative.is_empty() {
            return Ok(self.stage.clone());
        }
        parents(&self.stage, relative, true)
    }

    fn write(&self, path: &[u8], bytes: &[u8], executable: bool) -> anyhow::Result<()> {
        ensure!(
            bytes.len() <= MAX_FILE_BYTES,
            "repository file exceeds 16 MiB limit"
        );
        let relative = self.output(path)?;
        let mut state = self.state.borrow_mut();
        ensure!(
            state.written + bytes.len() <= MAX_TOTAL_BYTES,
            "repository cumulative writes exceed 64 MiB limit"
        );
        ensure!(
            state.writes < MAX_OPERATIONS,
            "repository exceeds 10000 writes"
        );
        ensure!(
            !relative.is_empty(),
            "Could not create {}: is the repository directory",
            display(path)
        );
        let path = self.output_parents(&relative)?;
        state.created = true;
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
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(bytes)?;
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(if executable {
                0o755
            } else {
                0o644
            }))?;
        }
        state.written += bytes.len();
        state.writes += 1;
        Ok(())
    }

    fn mark_created(&self) {
        self.state.borrow_mut().created = true;
    }

    fn charge_read(&self, bytes: usize) -> anyhow::Result<()> {
        let mut state = self.state.borrow_mut();
        ensure!(
            state.read + bytes <= MAX_TOTAL_BYTES,
            "template cumulative reads exceed 64 MiB limit"
        );
        state.read += bytes;
        Ok(())
    }

    /// toRepoCacheFriendlyPath + RepoRecordedInput.File (java:1656-1733).
    fn watch(&self, path: &[u8], watch: ShouldWatch) -> anyhow::Result<()> {
        if watch == ShouldWatch::No {
            return Ok(());
        }
        if self.under_stage(path).is_some() {
            if watch == ShouldWatch::Auto {
                return Ok(());
            }
            bail!("attempted to watch path under working directory");
        }
        let host = path_buf(path);
        let state = match fs::metadata(&host) {
            Err(_) => "missing".to_owned(),
            Ok(metadata) if metadata.is_dir() => "directory".to_owned(),
            Ok(metadata) if metadata.is_file() => {
                let bytes = read_limited(&host, MAX_FILE_BYTES, "watched file")?;
                self.charge_read(bytes.len())?;
                format!("file:{}", sha256(&bytes))
            }
            Ok(_) => "other".to_owned(),
        };
        self.record(self.friendly(path), "file", state)
    }

    fn friendly(&self, path: &[u8]) -> String {
        let workspace = self.workspace.as_os_str().as_bytes();
        match path
            .strip_prefix(workspace)
            .filter(|rest| rest.is_empty() || rest.starts_with(b"/"))
        {
            Some(rest) => format!("@@//{}", display(rest.strip_prefix(b"/").unwrap_or(rest))),
            None => display(path),
        }
    }

    fn record(&self, path: String, kind: &'static str, state: String) -> anyhow::Result<()> {
        let mut current = self.state.borrow_mut();
        let files = &mut current.inputs.files;
        ensure!(
            files.len() < MAX_OPERATIONS,
            "repository exceeds 10000 recorded file inputs"
        );
        let input = FileInput { path, kind, state };
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
            self.charge_read(bytes.len())?;
            // Label templates are already keyed by their `templates` entry
            // (canonical label and full bytes, recorded regardless of watch
            // mode), so content changes rekey without a `files` entry; adding
            // one would change the pre-N21 identity of existing repositories.
            // Label read() calls had no previous identity and are recorded.
            if method != "template" && watch != ShouldWatch::No {
                self.record(
                    canonical.clone(),
                    "file",
                    format!("file:{}", sha256(&bytes)),
                )?;
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
        let bytes = read_limited(&host, MAX_FILE_BYTES, "template input")?;
        self.charge_read(bytes.len())?;
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

    fn budget(&self) -> transport::SharedBudget {
        self.state
            .borrow_mut()
            .budget
            .get_or_insert_with(Default::default)
            .clone()
    }

    /// Fetch one archive into memory with the module download transport
    /// (extension_download.rs: limits, redirects, retry table, TLS policy).
    fn fetch(
        &self,
        urls: &[reqwest::Url],
        expected: Option<&Checksum>,
        sri: bool,
    ) -> Result<(reqwest::Url, Vec<u8>), TypedIoError> {
        if !self.repositories.repository_downloads_enabled() {
            return Err(TypedIoError::Completion(
                "repository downloads are disabled".into(),
            ));
        }
        if urls.is_empty() {
            return Err(TypedIoError::Completion("no download URLs".into()));
        }
        let budget = self.budget();
        budget.borrow_mut().call()?;
        let client = {
            let mut state = self.state.borrow_mut();
            if state.client.is_none() {
                state.client = Some(transport::client()?);
            }
            state.client.clone().expect("initialized")
        };
        let mut last = TypedIoError::Completion("no mirror succeeded".into());
        for url in urls {
            let attempt = transport::open(&client, url, budget.clone()).and_then(|mut reader| {
                let mut bytes = Vec::new();
                let mut buffer = [0u8; transport::PRECHECK];
                loop {
                    let n =
                        transport::read_decoded(&mut *reader, &mut buffer, bytes.len(), &budget)?;
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                }
                if let Some(expected) = expected
                    && !expected.matches(&bytes)
                {
                    // HashOutputStream.java:84-90 ("Checksum was %s but
                    // wanted %s"; hex unless the checksum came from integrity).
                    let actual = Checksum {
                        algorithm: expected.algorithm,
                        bytes: expected.algorithm.hash(&bytes),
                    };
                    let shown = |checksum: &Checksum| {
                        if sri {
                            checksum.integrity()
                        } else {
                            transport::hex(&checksum.bytes)
                        }
                    };
                    return Err(TypedIoError::Completion(format!(
                        "{CHECKSUM_MISMATCH}{} but wanted {}",
                        shown(&actual),
                        shown(expected)
                    )));
                }
                Ok(bytes)
            });
            match attempt {
                Ok(bytes) => return Ok((url.clone(), bytes)),
                Err(error @ TypedIoError::Completion(_)) => last = error,
                Err(error) => return Err(error),
            }
        }
        Err(last)
    }

    /// Extract archive bytes into `output` (stage-relative), merging like
    /// Bazel's decompressors writing into an existing directory.
    fn extract_bytes(
        &self,
        bytes: &[u8],
        kind: &str,
        strip_prefix: &str,
        output: &str,
        archive: &[u8],
    ) -> anyhow::Result<()> {
        let normalized = normalized_prefix(strip_prefix);
        self.mark_created();
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
        if let Err(error) =
            crate::archive::extract_named(bytes, kind, &normalized, temporary.path(), &archive_name)
        {
            if !normalized.is_empty() && error.ends_with("did not match") {
                // Bazel lists the first segment of every multi-segment entry.
                let listing = tempfile::Builder::new()
                    .prefix(".extract-")
                    .tempdir_in(&self.stage)?;
                let mut available = BTreeSet::new();
                if crate::archive::extract(bytes, kind, "", listing.path()).is_ok() {
                    for entry in fs::read_dir(listing.path())? {
                        let entry = entry?;
                        if entry.file_type()?.is_dir()
                            && fs::read_dir(entry.path())?.next().is_some()
                        {
                            available.insert(entry.file_name().to_string_lossy().into_owned());
                        }
                    }
                }
                return Err(prefix_not_found(archive, strip_prefix, &available));
            }
            if crate::archive::is_bazel_extraction_io_error(&error) {
                return Err(extraction_io_error(archive, &error));
            }
            return Err(anyhow::Error::msg(error));
        }
        merge(temporary.path(), &destination)?;
        temporary.close()?;
        Ok(())
    }
}

/// Move every extracted entry into `destination`, replacing files and
/// symlinks and merging directories; never follows destination symlinks.
fn merge(source: &Path, destination: &Path) -> anyhow::Result<()> {
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
        ensure!(
            output.len() + bytes.len() <= MAX_FILE_BYTES,
            "template output exceeds 16 MiB limit"
        );
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
    rename_files: Option<Value<'_>>,
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
    ensure!(
        strip_components == 0,
        "{method}() strip_components is not supported by the native extractor"
    );
    if !string_dict(rename_files, "rename_files")?.is_empty() {
        bail!("{method}() rename_files is not supported by the native extractor");
    }
    Ok(())
}

fn exec_receiver(this: Value<'_>) -> anyhow::Result<()> {
    ensure!(
        values::RepositoryCtx::from_value(this).is_some(),
        "expected repository_ctx"
    );
    Ok(())
}

starlark::methods_static!(REPOSITORY_METHODS = repository_methods);
starlark::methods_static!(PATH_METHODS = path_methods);

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
    if values::RepositoryCtx::from_value(this).is_none() {
        return Err(GetenvError::Receiver);
    }
    let context = context(eval).map_err(|_| GetenvError::Phase)?;
    getenv_observed(
        &context.environment,
        context.repositories.environment(),
        args,
        eval.heap(),
    )
}

/// Shared `getenv(name, default=None)` (StarlarkBaseExternalContext.java:
/// 1535-1565) for repository_ctx and module_ctx: positional-only binding,
/// a recorded RepoRecordedInput.EnvVar observation, missing differs from empty.
pub(crate) fn getenv_observed<'v>(
    watch: &RefCell<Watch>,
    snapshot: &crate::repository_environment::Snapshot,
    args: &Arguments<'v, '_>,
    heap: Heap<'v>,
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

fn not_implemented(method: &str) -> anyhow::Error {
    anyhow!("repository_ctx.{method}() is not implemented by the native profile")
}

fn url_strings(url: Value<'_>) -> anyhow::Result<Vec<&str>> {
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
            "in call to download_and_extract(), parameter 'url' got value of type '{}', want 'string or Iterable'",
            url.get_type()
        );
    };
    ensure!(
        items.len() <= 8,
        "download_and_extract() supports at most 8 mirrors"
    );
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

#[starlark_module]
fn repository_methods(builder: &mut MethodsBuilder) {
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
        let context = context(eval)?;
        let path = context.path_of(path, "file", "path")?;
        // Bazel writes its internal Latin-1 string bytes; characters above
        // U+00FF are not representable there and are rejected explicitly.
        let bytes = crate::repository_rule::latin1_bytes(content).ok_or_else(|| {
            anyhow!("repository file content above U+00FF is unsupported (Latin-1 byte strings)")
        })?;
        context.write(&path, &bytes, executable)?;
        Ok(NoneType)
    }

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
            ensure!(
                state.inputs.templates.len() < MAX_OPERATIONS,
                "repository exceeds 10000 template reads"
            );
            state.inputs.templates.push(TemplateInput {
                source,
                bytes: bytes.clone(),
            });
        }
        for (key, value) in replacements {
            bytes = replace_bytes(&bytes, &key, &value)?;
        }
        context.write(&output, &bytes, executable)?;
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
        let context = context(eval)?;
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
        let bytes = context(eval)?.path_of(path, "path", "path")?;
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
        let path = context.path_of(path, "watch", "path")?;
        context.watch(&path, ShouldWatch::Yes)?;
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
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(&target)?,
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
        let to_path = context.output_parents(&to).map_err(|e| failure(&e))?;
        context.mark_created();
        fs::rename(&from_path, &to_path).map_err(|error| {
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
        let host_link = context.output_parents(&relative).map_err(|e| failure(&e))?;
        context.mark_created();
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
        let mut state = context.state.borrow_mut();
        ensure!(
            state.writes < MAX_OPERATIONS,
            "repository exceeds 10000 writes"
        );
        state.writes += 1;
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
        ensure!(
            i64::from(timeout) <= MAX_TIMEOUT_SECONDS,
            "execute() timeout exceeds the native limit of {MAX_TIMEOUT_SECONDS} seconds"
        );
        // An empty list reaches process-wrapper, which prints its usage and
        // exits 1 (process-wrapper-options.cc:135-138); see repository_process.
        let mut argv = Vec::with_capacity(items.len());
        let mut size = 0usize;
        for item in &items {
            let bytes = if let Some(text) = item.unpack_str() {
                latin1(text, "execute() argument")?
            } else {
                context.path_of(*item, "execute", "arguments")?
            };
            size += bytes.len() + 1;
            ensure!(size <= MAX_ARGV_BYTES, "execute() arguments exceed 2 MiB");
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
        let directory = if working_directory.is_empty() {
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
        {
            let mut state = context.state.borrow_mut();
            // createDirectory(workingDirectory) (java:2076).
            state.created = true;
            ensure!(
                state.executions < MAX_EXECUTIONS,
                "repository exceeds {MAX_EXECUTIONS} execute() calls"
            );
            state.executions += 1;
            state.inputs.host_facilities.insert("execute");
        }
        let request = crate::repository_process::Request {
            argv,
            environment: environment.into_iter().collect(),
            working_directory: &directory,
            timeout: Duration::from_secs(timeout as u64),
        };
        match crate::repository_process::run(&request).map_err(anyhow::Error::msg)? {
            crate::repository_process::Outcome::Interrupted => bail!(
                "repository execute() subprocess was terminated by SIGTERM; Bazel treats this as an interrupt"
            ),
            crate::repository_process::Outcome::Completed(return_code, stdout, stderr) => {
                if !quiet {
                    crate::repository_process::echo(&stdout, &stderr);
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
        let context = context(eval)?;
        let strip_prefix = renamed_strip_prefix("extract", strip_prefix, stripPrefix)?;
        validate_stripping("extract", strip_prefix, strip_components, rename_files)?;
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
        let kind = if r#type.trim().is_empty() {
            let basename = archive_path.rsplit(|b| *b == b'/').next().unwrap_or(b"");
            decompressor_for_name(basename, &display(&archive_path))?
        } else {
            decompressor(r#type)?
        };
        let bytes = read_limited(&host, MAX_FILE_BYTES, "archive")?;
        context.charge_read(bytes.len())?;
        context.extract_bytes(&bytes, kind, strip_prefix, &relative, &archive_path)?;
        Ok(NoneType)
    }

    /// StarlarkBaseExternalContext.downloadAndExtract (java:907-1197).
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
        // canonical_id only keys Bazel's repository cache, which the native
        // profile does not have; it never changes which bytes are accepted.
        let _ = canonical_id;
        let context = context(eval)?;
        let strip_prefix = renamed_strip_prefix("download_and_extract", strip_prefix, stripPrefix)?;
        validate_stripping(
            "download_and_extract",
            strip_prefix,
            strip_components,
            rename_files,
        )?;
        for (option, name) in [(auth, "auth"), (headers, "headers")] {
            if let Some(option) = option {
                let dict =
                    DictRef::from_value(option).ok_or_else(|| anyhow!("{name} must be a dict"))?;
                ensure!(
                    dict.is_empty(),
                    "download_and_extract() {name} is not supported by the native download transport"
                );
            }
        }
        let strings = url_strings(url)?;
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
        // validateChecksum (java:512-541): a malformed checksum is deferred
        // until after the download attempt (java:1094-1101, 1168-1170).
        let deferred = checksum_text_error(sha256, integrity);
        let expected = if deferred.is_some() {
            None
        } else {
            transport::checksums(sha256, integrity)
                .map_err(|error| anyhow!("{error}"))?
                .expected
        };
        let output = match output {
            Some(output) => context.path_of(output, "download_and_extract", "output")?,
            None => context.stage_bytes().to_vec(),
        };
        let relative = context.output(&output)?;
        context.facility("download");
        // The download lands in `<output>/temp<N>/<url basename>`
        // (java:1126, DownloadManager); errors and suffix checks name it.
        let downloaded = |url: &reqwest::Url| {
            let name = url
                .path_segments()
                .and_then(|mut segments| segments.next_back())
                .unwrap_or("");
            let mut path = output.clone();
            path.extend_from_slice(format!("/{}/{name}", temp_name()).as_bytes());
            (name.to_owned(), path)
        };
        let (fetched, bytes) = match context.fetch(&urls, expected.as_ref(), !integrity.is_empty())
        {
            Ok(fetched) => fetched,
            Err(TypedIoError::Completion(_)) if allow_fail => {
                return Ok(eval.heap().alloc(AllocStruct([("success", false)])));
            }
            Err(TypedIoError::Completion(message)) if message.starts_with(CHECKSUM_MISMATCH) => {
                bail!(
                    "java.io.IOException: Error downloading [{}] to {}: {message}",
                    urls.iter()
                        .map(reqwest::Url::as_str)
                        .collect::<Vec<_>>()
                        .join(", "),
                    display(&downloaded(&urls[0]).1)
                )
            }
            Err(error) => return Err(anyhow!("{error}")),
        };
        if let Some(error) = &deferred {
            bail!(
                "Checksum error in repository @@{}: {error}",
                context.canonical
            );
        }
        let (name, archive) = downloaded(&fetched);
        let kind = if r#type.is_empty() {
            decompressor_for_name(name.as_bytes(), &display(&archive))?
        } else {
            decompressor(r#type)?
        };
        context.extract_bytes(&bytes, kind, strip_prefix, &relative, &archive)?;
        let algorithm = expected
            .as_ref()
            .map_or(Algorithm::Sha256, |checksum| checksum.algorithm);
        let checksum = Checksum {
            algorithm,
            bytes: algorithm.hash(&bytes),
        };
        let heap = eval.heap();
        let mut fields = vec![
            ("success", heap.alloc(true)),
            ("integrity", heap.alloc(checksum.integrity())),
        ];
        if algorithm == Algorithm::Sha256 {
            fields.push(("sha256", heap.alloc(transport::hex(&checksum.bytes))));
        }
        fields.push(("size_bytes", heap.alloc(bytes.len() as i64)));
        Ok(heap.alloc(AllocStruct(fields)))
    }

    fn download<'v>(
        this: Value<'v>,
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: starlark::collections::SmallMap<String, Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = (this, args, kwargs);
        Err(not_implemented("download"))
    }

    fn patch<'v>(
        this: Value<'v>,
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: starlark::collections::SmallMap<String, Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = (this, args, kwargs);
        Err(not_implemented("patch"))
    }

    fn watch_tree<'v>(
        this: Value<'v>,
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: starlark::collections::SmallMap<String, Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = (this, args, kwargs);
        Err(not_implemented("watch_tree"))
    }

    fn repo_metadata<'v>(
        this: Value<'v>,
        #[starlark(args)] args: UnpackTuple<Value<'v>>,
        #[starlark(kwargs)] kwargs: starlark::collections::SmallMap<String, Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = (this, args, kwargs);
        Err(not_implemented("repo_metadata"))
    }
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
        Ok(fs::metadata(path_buf(&path_receiver(this)?.bytes)).is_ok())
    }

    #[starlark(attribute)]
    fn is_dir<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        Ok(fs::metadata(path_buf(&path_receiver(this)?.bytes)).is_ok_and(|m| m.is_dir()))
    }

    #[starlark(attribute)]
    fn realpath<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
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
            ensure!(
                entries.len() < MAX_DIRENTS,
                "readdir() exceeds {MAX_DIRENTS} entries"
            );
            let name = entry?.file_name();
            let mut bytes = path.bytes.clone();
            if bytes != b"/" {
                bytes.push(b'/');
            }
            bytes.extend_from_slice(name.as_bytes());
            entries.push(bytes);
            names.push(name.as_bytes().to_vec());
        }
        if watch != ShouldWatch::No && context.under_stage(&path.bytes).is_none() {
            context.watch(&path.bytes, watch)?;
            names.sort_unstable();
            let mut hash = Vec::new();
            for name in names {
                hash.extend_from_slice(&name);
                hash.push(0);
            }
            context.record(
                context.friendly(&path.bytes),
                "dirents",
                format!("dirents:{}", sha256(&hash)),
            )?;
        } else if watch == ShouldWatch::Yes {
            bail!("attempted to watch path under working directory");
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
/// the repository contents cache. The native stage is renamed to a
/// content-addressed directory whose name depends on these very links, so an
/// absolute link cannot name its final location; links into the stage are
/// therefore rewritten relative after the callback (same resolved contents).
fn replant_symlinks(stage: &Path) -> anyhow::Result<()> {
    fn visit(
        stage: &Path,
        directory: &Path,
        depth: usize,
        count: &mut usize,
    ) -> anyhow::Result<()> {
        ensure!(depth <= 64, "repository tree exceeds depth 64");
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
                visit(stage, &path, depth + 1, count)?;
            } else if kind.is_symlink() {
                let target = fs::read_link(&path)?;
                if let Ok(inside) = target.strip_prefix(stage) {
                    let parent_depth = path
                        .parent()
                        .and_then(|parent| parent.strip_prefix(stage).ok())
                        .map_or(0, |relative| relative.components().count());
                    let mut relative = PathBuf::new();
                    for _ in 0..parent_depth {
                        relative.push("..");
                    }
                    relative.push(inside);
                    if relative.as_os_str().is_empty() {
                        relative.push(".");
                    }
                    fs::remove_file(&path)?;
                    std::os::unix::fs::symlink(relative, &path)?;
                }
            }
        }
        Ok(())
    }
    visit(stage, stage, 0, &mut 0)
}

fn is_latin1(text: &str) -> bool {
    crate::repository_rule::is_latin1(text)
}

fn alloc_attrs<'v>(
    heap: Heap<'v>,
    attrs: &BTreeMap<String, AttrValue>,
    canonical_name: &str,
) -> anyhow::Result<Value<'v>> {
    let mut values = BTreeMap::new();
    for (name, attr) in attrs {
        let value = match attr {
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
            AttrValue::Ints(_) | AttrValue::LabelKeyedStringDict(_) => {
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
        };
        values.insert(name.clone(), value);
    }
    values.insert("name".to_owned(), heap.alloc(canonical_name));
    let fields = heap.alloc(AllocStruct(values));
    Ok(heap.alloc(RepoDefinitionGen { fields }))
}

/// Execute only into the caller's precreated private stage. Failures may leave
/// partial files there; callers must not publish them. Frozen owners remain live
/// throughout evaluation, including transitive/private helper functions.
pub fn execute(
    repositories: Arc<RepositorySet>,
    definition: &RuleDefinition,
    functions: &[OwnedFrozenValue],
    attrs: &BTreeMap<String, AttrValue>,
    canonical_name: &str,
    original_name: &str,
    definition_label: &Label,
    stage: &Path,
) -> Result<ExecutionInputs, String> {
    let run = || -> anyhow::Result<ExecutionInputs> {
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
        let context = Context {
            repositories,
            stage: checked_root(stage)?,
            workspace,
            label: definition_label.clone(),
            canonical: canonical_name.to_owned(),
            state: RefCell::new(State::default()),
            environment: RefCell::new(Watch::default()),
        };
        Module::with_temp_heap(|module| -> anyhow::Result<()> {
            let heap = module.heap();
            let host = context.repositories.host().map_err(anyhow::Error::msg)?;
            // repository_ctx.os.environ is the command's repoEnv; a snapshot
            // failure surfaces only when environ is actually read.
            let os = match context.repositories.environment().client_environment() {
                Ok(environment) => crate::host_os::alloc_with_environ(heap, host, environment)?,
                Err(error) => {
                    crate::host_os::alloc_with_environ_error(heap, host, error.to_string())?
                }
            };
            let ctx = heap.alloc(RepositoryCtxGen {
                attrs: alloc_attrs(heap, attrs, canonical_name)?,
                os,
                name: canonical_name.to_owned(),
                original_name: original_name.to_owned(),
                workspace_root: latin1_text(context.workspace.as_os_str().as_bytes()),
            });
            for function in functions {
                heap.access_owned_frozen_value(function);
            }
            let implementation = heap.access_owned_frozen_value(&definition.implementation);
            let mut eval = Evaluator::new(&module);
            eval.set_max_callstack_size(128)?;
            eval.set_max_tick_count(1_000_000)?;
            eval.set_max_heap_size(128 * 1024 * 1024)?;
            eval.extra = Some(&context);
            for name in definition.environ.names() {
                context
                    .environment
                    .borrow_mut()
                    .observe(context.repositories.environment(), name)?;
            }
            let returned = eval
                .eval_function(implementation, &[ctx], &[])
                .map_err(callback_error)?;
            ensure!(
                returned.is_none(),
                "repository implementation return values other than None are unsupported"
            );
            Ok(())
        })?;
        replant_symlinks(&context.stage)?;
        let watched_env = context.environment.borrow().inputs()?;
        let mut state = context.state.into_inner();
        state.inputs.watched_env = watched_env;
        Ok(state.inputs)
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

    struct Fixture {
        root: tempfile::TempDir,
        stage: tempfile::TempDir,
        repositories: Arc<RepositorySet>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir_in("/tmp/opencode").unwrap();
            let stage = tempfile::tempdir_in("/tmp/opencode").unwrap();
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
        for (path, expected) in [
            (
                "../escape",
                "Cannot write outside of the repository directory for path",
            ),
            (
                "/absolute-n21",
                "Cannot write outside of the repository directory for path /absolute-n21",
            ),
            ("back\\\\slash", "native store profile"),
            ("a\\nb", "native store profile"),
            ("nul\\000path", "native store profile"),
            ("colon:name", "native store profile"),
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
        assert_eq!(names, ["escape", "m", "x"]);
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
    fn label_templates_reject_symlink_files_parents_and_markers() {
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
        for label in ["//pkg:link", "//alias:link"] {
            let schema = format!("{{'src': attr.label(default='{label}')}}");
            let error = fixture
                .run_loaded(
                    fixture.load("ctx.template('x', ctx.attr.src)", &schema),
                    BTreeMap::new(),
                )
                .unwrap_err();
            assert!(error.contains("symlink"), "{error}");
        }
        fs::remove_file(fixture.root.path().join("pkg/BUILD")).unwrap();
        symlink(
            fixture.root.path().join("BUILD"),
            fixture.root.path().join("pkg/BUILD"),
        )
        .unwrap();
        let error = fixture
            .run_loaded(
                fixture.load(
                    "ctx.template('x', ctx.attr.src)",
                    "{'src': attr.label(default='//pkg:link')}",
                ),
                BTreeMap::new(),
            )
            .unwrap_err();
        assert!(error.contains("marker"), "{error}");
    }

    #[test]
    fn file_and_template_output_limits_checked_before_replacement() {
        let fixture = Fixture::new();
        fixture.error(
            "ctx.file('existing', 'keep')\nctx.file('existing', 'x' * (16 * 1024 * 1024 + 1))",
            "16 MiB",
        );
        assert_eq!(fixture.bytes("existing"), b"keep");
        fixture.error("ctx.file('in', 'aa')\nctx.template('existing', 'in', {'a': 'b' * (8 * 1024 * 1024 + 1)})", "16 MiB");
        assert_eq!(fixture.bytes("existing"), b"keep");
    }

    #[test]
    fn input_and_cumulative_write_limits() {
        let fixture = Fixture::new();
        let input = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(fixture.stage.path().join("huge"))
            .unwrap();
        input.set_len((MAX_FILE_BYTES + 1) as u64).unwrap();
        fixture.error("ctx.template('out', 'huge')", "input exceeds 16 MiB");
        fixture.error(
            "for i in range(5):\n    ctx.file('repeat', 'x' * (16 * 1024 * 1024))",
            "cumulative writes exceed 64 MiB",
        );
        assert_eq!(fixture.bytes("repeat").len(), MAX_FILE_BYTES);
    }

    #[test]
    fn rejects_overdeep_long_paths_and_excessive_operations() {
        let fixture = Fixture::new();
        fixture.error("ctx.file('/'.join(['a'] * 65))", "depth 64");
        fixture.error("ctx.file('a' * 4097)", "4096");
        fixture.error(
            "for i in range(10001):\n    ctx.file('empty')",
            "10000 writes",
        );
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

    fn executable(path: &Path, text: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
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
        let outside = tempfile::tempdir_in("/tmp/opencode").unwrap();
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
        let bin = tempfile::tempdir_in("/tmp/opencode").unwrap();
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
        let bin = tempfile::tempdir_in("/tmp/opencode").unwrap();
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
        let outside = tempfile::tempdir_in("/tmp/opencode").unwrap();
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
        fixture.error("ctx.download_and_extract('https://example.invalid/a.tar.gz', auth = {'https://example.invalid': {}})", "auth is not supported");
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
            "repository downloads are disabled",
        );
        disabled.ok("(not ctx.download_and_extract('https://example.invalid/a.tar.gz', allow_fail = True).success) or fail('allow_fail')");
        // A malformed checksum is deferred past a failed download (java:1159-1170).
        disabled.ok("(not ctx.download_and_extract('https://example.invalid/a.tar.gz', sha256 = 'bad', allow_fail = True).success) or fail('deferred')");
        disabled.error(
            "ctx.download_and_extract('https://example.invalid/a.tar.gz', sha256 = 'bad')",
            "repository downloads are disabled",
        );
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
m = ctx.download_and_extract("{url}/pkg.tar.gz", output = "mismatch", sha256 = "{zero}", allow_fail = True)
(not m.success) or fail("mismatch")"#,
            zero = "0".repeat(64)
        ));
        assert_eq!(fixture.bytes("dl/a.txt"), b"A");
        assert_eq!(fixture.bytes("dl/sub/b.txt"), b"B");
        assert_eq!(fixture.bytes("typed/top/a.txt"), b"A");
        assert!(!fixture.stage.path().join("mismatch/a.txt").exists());
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
        for method in ["download", "patch", "watch_tree", "repo_metadata"] {
            fixture.error(
                &format!("ctx.file('partial', 'private')\nctx.{method}('x')"),
                &format!("repository_ctx.{method}() is not implemented by the native profile"),
            );
            assert_eq!(fixture.bytes("partial"), b"private");
        }
        fixture.error("ctx.report_progress(1)", "report_progress");
        fixture.error("ctx.file('partial')\nreturn {}", "other than None");
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
}
