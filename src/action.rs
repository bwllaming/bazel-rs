//! Native action execution for an initial Bazel subset, not full Bazel.
//!
//! A fresh working directory is isolation for build state, NOT a security
//! sandbox. Arbitrary shell commands can access the host, network, clock, and
//! absolute paths; they are not hermetic. Sources and the cache must not be
//! modified by an adversary racing filesystem checks. Symlink checks prevent
//! accidental escapes, but are not a substitute for OS sandboxing.
//!
//! No caller environment is inherited: otherwise secrets could leak and an
//! unkeyed environment variable could silently change a cached result. TMPDIR
//! defaults to a fresh absolute directory; its spelling (and the working directory's)
//! is intentionally not stable, so actions must not embed these paths in output.
//!
//! Tree artifacts (declared directories, docs/ACTIONS_PARAMS.md) are directory
//! outputs and inputs: their regular files and subdirectories are keyed,
//! staged and published; symlinks inside a tree are rejected explicitly.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// The key identity stays `native-action-v5`: actions without N23d features
// (no tree artifacts, parameter files, exact environment, execution
// requirements or transforms) keep their keys; those features are framed in
// an extra block. Manifests changed shape, so their format is v6.
const FORMAT: &str = "native-action-v5";
const MANIFEST_FORMAT: &str = "native-action-v6";
// Bazel 9.3 OutputPermissions.READONLY: all completed regular action outputs,
// including non-executable writes, normalize after execution, not during it.
const OUTPUT_MODE: u32 = 0o555;
// Completed tree artifacts: every entry below the root (files and
// subdirectories) is chmod'ed by
// ActionOutputMetadataStore.constructTreeArtifactValueFromFilesystem
// (skyframe/ActionOutputMetadataStore.java:245-281); genuine Bazel 9.3 also
// leaves the root read-only (0555, observed by the paired capture,
// tests/fixtures/actions_params/reference_receipt.json).
const TREE_ROOT_MODE: u32 = 0o555;
const SHELL: &str = "/bin/bash";
const SHELL_FLAGS: [&str; 4] = ["-e", "-o", "pipefail", "-c"];
const LOG_TAIL_BYTES: usize = 64 * 1024;
const ENV: [(&str, &str); 3] = [("PATH", "/usr/bin:/bin"), ("LANG", "C"), ("LC_ALL", "C")];
const SPAWN_RETRY_DELAYS_MS: [u64; 7] = [1, 2, 4, 8, 16, 32, 64];
static NEXT_WORK: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct Action {
    pub id: String,
    pub command: String,
    pub inputs: Vec<(PathBuf, PathBuf)>,
    pub outputs: Vec<PathBuf>,
    /// Native write, mutually exclusive with `program` and shell commands.
    pub write: Option<WriteAction>,
    /// Direct executable; `None` selects Bash unless this is a native write.
    pub program: Option<String>,
    /// Literal process arguments, or shell positional parameters starting at `$1`.
    pub arguments: Vec<String>,
    /// Explicit overrides applied after controlled defaults, including TMPDIR.
    pub environment: BTreeMap<String, String>,
    /// Enable genrule-style errexit and pipefail; ignored for writes/direct runs.
    pub shell_strict: bool,
    /// Starlark-action features (docs/ACTIONS_PARAMS.md).
    pub extra: ActionExtra,
}

#[derive(Clone, Debug, Default)]
pub struct ActionExtra {
    /// Outputs (members of `Action::outputs`) that are tree artifacts.
    pub tree_outputs: BTreeSet<PathBuf>,
    /// Input destinations that are tree artifacts (directories); every other
    /// input must be a regular file.
    pub tree_inputs: BTreeSet<PathBuf>,
    /// Parameter files (CommandLines.ParamFileActionInput): written below
    /// the execution root before the process starts; content-keyed.
    pub param_files: Vec<(PathBuf, Vec<u8>)>,
    /// Starlark spawn environment: exactly `environment` plus TMPDIR
    /// (PosixLocalEnvProvider), without the PATH/LANG/LC_ALL defaults.
    pub exact_environment: bool,
    /// Execution requirements: keyed, no local effect.
    pub execution_info: BTreeMap<String, String>,
    /// A native, process-free transform of one input into the only output.
    pub transform: Option<Transform>,
}

#[derive(Clone, Debug)]
pub enum Transform {
    /// SymlinkAction: the output observes the target's bytes and permission
    /// bits (published as a copy, not a filesystem symlink).
    Symlink {
        target: PathBuf,
        require_executable: bool,
        /// The expanded progress message that prefixes execution failures
        /// (`<progress message> failed: ...`); not keyed, like Bazel.
        progress: String,
    },
    /// TemplateExpansionAction: literal, sequential replacement of internal
    /// bytes (LocalTemplateExpansionStrategy.getExpandedTemplateUnsafe).
    Template {
        template: PathBuf,
        substitutions: Vec<(Vec<u8>, Vec<u8>)>,
        executable: bool,
    },
    /// An action whose execution is an explicit native gap (its outputs are
    /// declared and registered, e.g. workspace-status transforms).
    Unsupported { message: String },
}

#[derive(Clone, Debug)]
pub struct WriteAction {
    pub content: Vec<u8>,
    pub executable: bool,
}

#[derive(Debug)]
pub struct ActionResult {
    pub outputs: Vec<PathBuf>,
    pub cache_hit: bool,
    /// Declaration-order output index of each tree output: its regular files,
    /// relative to the tree root, in byte order (TreeArtifactValue children).
    pub trees: BTreeMap<usize, Vec<PathBuf>>,
}

#[derive(Clone, PartialEq, Eq)]
struct Input {
    bytes: Vec<u8>,
    digest: String,
    executable: u32,
    mode: u32,
}

#[derive(Clone, PartialEq, Eq)]
struct Tree {
    files: BTreeMap<PathBuf, Input>,
    directories: BTreeSet<PathBuf>,
}

#[derive(Clone, PartialEq, Eq)]
enum Staged {
    File(Input),
    Tree(Tree),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: String,
    key: String,
    outputs: Vec<Output>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Output {
    // Hex encoding supports even non-UTF-8 Unix filenames.
    name: String,
    digest: String,
    mode: u32,
    tree: Option<TreeManifest>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TreeManifest {
    files: Vec<(String, String)>,
    directories: Vec<String>,
}

struct WorkDir(PathBuf);

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = remove_directory(&self.0);
    }
}

fn io_error(context: &str, path: &Path, error: io::Error) -> String {
    format!("{context} '{}': {error}", path.display())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}

fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

// Length-prefix every field, including list lengths, to avoid concatenation
// collisions and make the key independent of input/output declaration order.
fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn frame_count(hash: &mut Sha256, count: usize) {
    frame(hash, &(count as u64).to_le_bytes());
}

fn validate_relative(path: &Path) -> Result<(), String> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty()
        || bytes.contains(&0)
        || bytes
            .split(|&b| b == b'/')
            .any(|c| c.is_empty() || c == b"." || c == b"..")
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!(
            "unsafe mapped path '{}' (expected relative normal components)",
            path.display()
        ));
    }
    Ok(())
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

// Check all ancestors, not just the leaf: dir/link/file must not escape via
// dir/link. This intentionally rejects source paths through symlink directories.
fn no_linked_ancestor(path: &Path) -> Result<fs::Metadata, String> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        let metadata =
            fs::symlink_metadata(&prefix).map_err(|e| io_error("cannot inspect", &prefix, e))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("symlink is not allowed: '{}'", prefix.display()));
        }
    }
    fs::symlink_metadata(path).map_err(|e| io_error("cannot inspect", path, e))
}

fn regular_file(path: &Path) -> Result<fs::Metadata, String> {
    let metadata = no_linked_ancestor(path)?;
    if !metadata.is_file() {
        return Err(format!("expected regular file: '{}'", path.display()));
    }
    Ok(metadata)
}

fn real_directory(path: &Path) -> Result<fs::Metadata, String> {
    let metadata = no_linked_ancestor(path)?;
    if !metadata.is_dir() {
        return Err(format!("expected directory: '{}'", path.display()));
    }
    Ok(metadata)
}

fn read_input(path: &Path) -> Result<Input, String> {
    let metadata = regular_file(path)?;
    let bytes = fs::read(path).map_err(|e| io_error("cannot read", path, e))?;
    Ok(Input {
        digest: digest(&bytes),
        bytes,
        executable: metadata.permissions().mode() & 0o111,
        mode: metadata.permissions().mode() & 0o7777,
    })
}

/// Walk a directory without following symlinks. Entries are visited in byte
/// order; a symlink or special file below the root is an explicit error.
fn read_tree(root: &Path) -> Result<Tree, String> {
    real_directory(root)?;
    let mut tree = Tree {
        files: BTreeMap::new(),
        directories: BTreeSet::new(),
    };
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let directory = root.join(&relative);
        let mut entries = fs::read_dir(&directory)
            .map_err(|e| io_error("cannot list directory", &directory, e))?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io_error("cannot list directory", &directory, e))?;
        entries.sort();
        for name in entries {
            let child = relative.join(&name);
            let path = root.join(&child);
            let metadata =
                fs::symlink_metadata(&path).map_err(|e| io_error("cannot inspect", &path, e))?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "tree artifact entry '{}' is a symlink; symlinks inside tree artifacts are not supported by the native profile",
                    path.display()
                ));
            } else if metadata.is_dir() {
                tree.directories.insert(child.clone());
                pending.push(child);
            } else if metadata.is_file() {
                tree.files.insert(child, read_input(&path)?);
            } else {
                return Err(format!(
                    "tree artifact entry '{}' is not a regular file or directory",
                    path.display()
                ));
            }
        }
    }
    Ok(tree)
}

fn read_staged(path: &Path, tree: bool) -> Result<Staged, String> {
    if tree {
        Ok(Staged::Tree(read_tree(path)?))
    } else {
        Ok(Staged::File(read_input(path)?))
    }
}

/// Byte-ordered relative paths of a tree's regular files.
fn tree_listing(tree: &Tree) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = tree.files.keys().cloned().collect();
    files.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
    files
}

type PreparedAction = (BTreeMap<PathBuf, Staged>, Vec<PathBuf>, String);

fn check_one_output(action: &Action, kind: &str) -> Result<(), String> {
    if !action.command.is_empty()
        || action.program.is_some()
        || !action.arguments.is_empty()
        || !action.environment.is_empty()
        || !action.extra.param_files.is_empty()
        || action.outputs.len() != 1
    {
        return Err(format!(
            "{kind} action requires exactly one output, no program, and empty command, arguments, environment, and parameter files"
        ));
    }
    Ok(())
}

/// Validate commands and mapped input/output namespaces without filesystem I/O.
///
/// Writes require one output and no program, command, arguments, environment, or inputs.
/// Their content is arbitrary bytes, including NULs, with no argv size limit.
/// Direct runs require a nonempty, NUL-free program and an empty shell command.
/// Empty output lists are permitted for process actions. Duplicate input names
/// are permitted here; preparation checks that their contents and modes agree.
/// Transforms (symlink, template) require one output and their source among
/// the input destinations; parameter files must not overlap other paths.
pub fn validate_structure(action: &Action) -> Result<(), String> {
    if action.write.is_some()
        && (!action.command.is_empty()
            || action.program.is_some()
            || !action.arguments.is_empty()
            || !action.environment.is_empty()
            // Only tree inputs, which written Args expand (ParameterFileWriteAction).
            || action
                .inputs
                .iter()
                .any(|(_, name)| !action.extra.tree_inputs.contains(name))
            || !action.extra.param_files.is_empty()
            || action.extra.transform.is_some()
            || action.outputs.len() != 1)
    {
        return Err("write action requires exactly one output, no program, and empty command, arguments, environment, and inputs".into());
    }
    if action.write.is_some() && !action.extra.tree_outputs.is_empty() {
        return Err("write action output cannot be a tree artifact".into());
    }
    match &action.extra.transform {
        Some(Transform::Symlink { target, .. }) => {
            check_one_output(action, "symlink")?;
            if !action.inputs.iter().any(|(_, name)| name == target) {
                return Err("symlink action target must be an input".into());
            }
        }
        Some(Transform::Template { template, .. }) => {
            check_one_output(action, "template")?;
            if !action.inputs.iter().any(|(_, name)| name == template) {
                return Err("template action template must be an input".into());
            }
            if !action.extra.tree_outputs.is_empty() {
                return Err("template action output cannot be a tree artifact".into());
            }
        }
        Some(Transform::Unsupported { .. }) => check_one_output(action, "unsupported")?,
        None => {}
    }
    for tree in &action.extra.tree_inputs {
        if !action.inputs.iter().any(|(_, name)| name == tree) {
            return Err(format!(
                "tree input '{}' is not an action input",
                tree.display()
            ));
        }
    }
    for tree in &action.extra.tree_outputs {
        if !action.outputs.contains(tree) {
            return Err(format!(
                "tree output '{}' is not an action output",
                tree.display()
            ));
        }
    }
    if let Some(program) = &action.program {
        if program.is_empty() || program.as_bytes().contains(&0) {
            return Err("direct program must be nonempty and contain no NUL byte".into());
        }
        if !action.command.is_empty() {
            return Err("direct action requires an empty shell command".into());
        }
    }
    if action.command.as_bytes().contains(&0) {
        return Err("command contains a NUL byte".into());
    }
    for argument in &action.arguments {
        if argument.as_bytes().contains(&0) {
            return Err("argument contains a NUL byte".into());
        }
    }
    for (name, value) in action
        .environment
        .iter()
        .chain(&action.extra.execution_info)
    {
        if name.is_empty() || name.as_bytes().contains(&b'=') || name.as_bytes().contains(&0) {
            return Err("environment key must be nonempty and contain neither '=' nor NUL".into());
        }
        if value.as_bytes().contains(&0) {
            return Err("environment value contains a NUL byte".into());
        }
    }
    // Validate the complete namespace before reading inputs or invoking processes.
    let mut input_names: Vec<PathBuf> = Vec::new();
    for (_, destination) in &action.inputs {
        validate_relative(destination)?;
        if input_names
            .iter()
            .any(|p| p != destination && overlaps(p, destination))
        {
            return Err(format!(
                "overlapping input destination '{}'",
                destination.display()
            ));
        }
        input_names.push(destination.clone());
    }
    let mut outputs: Vec<PathBuf> = Vec::new();
    for output in &action.outputs {
        validate_relative(output)?;
        if input_names
            .iter()
            .chain(outputs.iter())
            .any(|p| overlaps(p, output))
        {
            return Err(format!(
                "overlapping input/output path '{}'",
                output.display()
            ));
        }
        outputs.push(output.clone());
    }
    let mut params: Vec<&PathBuf> = Vec::new();
    for (path, _) in &action.extra.param_files {
        validate_relative(path)?;
        if input_names
            .iter()
            .chain(outputs.iter())
            .chain(params.iter().copied())
            .any(|p| overlaps(p, path))
        {
            return Err(format!(
                "parameter file '{}' overlaps another action path",
                path.display()
            ));
        }
        params.push(path);
    }
    Ok(())
}

fn frame_input(hash: &mut Sha256, input: &Input) {
    frame(hash, input.digest.as_bytes());
    frame(hash, &input.executable.to_le_bytes());
}

fn frame_staged(hash: &mut Sha256, staged: &Staged) {
    match staged {
        Staged::File(input) => frame_input(hash, input),
        Staged::Tree(tree) => {
            frame(hash, b"tree");
            frame_count(hash, tree.files.len());
            for (name, input) in &tree.files {
                frame(hash, name.as_os_str().as_bytes());
                frame_input(hash, input);
            }
            frame_count(hash, tree.directories.len());
            for name in &tree.directories {
                frame(hash, name.as_os_str().as_bytes());
            }
        }
    }
}

fn prepare(action: &Action) -> Result<PreparedAction, String> {
    validate_structure(action)?;
    let mut outputs = action.outputs.clone();
    outputs.sort();
    let mut inputs: BTreeMap<PathBuf, Staged> = BTreeMap::new();
    for (source, destination) in &action.inputs {
        let input = read_staged(source, action.extra.tree_inputs.contains(destination))?;
        if let Some(previous) = inputs.get(destination) {
            let same = match (previous, &input) {
                (Staged::File(a), Staged::File(b)) => {
                    a.digest == b.digest && a.executable == b.executable
                }
                (a, b) => a == b,
            };
            if !same {
                return Err(format!(
                    "conflicting duplicate input destination '{}'",
                    destination.display()
                ));
            }
        } else {
            inputs.insert(destination.clone(), input);
        }
    }
    let mut hash = Sha256::new();
    for value in [FORMAT, std::env::consts::OS, std::env::consts::ARCH] {
        frame(&mut hash, value.as_bytes());
    }
    for (name, value) in ENV {
        frame(&mut hash, name.as_bytes());
        frame(&mut hash, value.as_bytes());
    }
    frame(&mut hash, b"TMPDIR=fresh-absolute-work-tmp");
    frame(&mut hash, action.id.as_bytes());
    match (&action.write, &action.program, &action.extra.transform) {
        (Some(write), _, _) => {
            frame(&mut hash, b"write");
            frame(&mut hash, &write.content);
            frame(&mut hash, &[u8::from(write.executable)]);
        }
        (
            None,
            _,
            Some(Transform::Symlink {
                target,
                require_executable,
                ..
            }),
        ) => {
            frame(&mut hash, b"symlink");
            frame(&mut hash, target.as_os_str().as_bytes());
            frame(&mut hash, &[u8::from(*require_executable)]);
            // The published copy carries the target's complete permission bits.
            if let Some(Staged::File(input)) = inputs.get(target) {
                frame(&mut hash, &input.mode.to_le_bytes());
            }
        }
        (
            None,
            _,
            Some(Transform::Template {
                template,
                substitutions,
                executable,
            }),
        ) => {
            frame(&mut hash, b"template");
            frame(&mut hash, template.as_os_str().as_bytes());
            frame_count(&mut hash, substitutions.len());
            for (key, value) in substitutions {
                frame(&mut hash, key);
                frame(&mut hash, value);
            }
            frame(&mut hash, &[u8::from(*executable)]);
        }
        (None, _, Some(Transform::Unsupported { message })) => {
            frame(&mut hash, b"unsupported");
            frame(&mut hash, message.as_bytes());
        }
        (None, Some(program), None) => {
            frame(&mut hash, b"direct");
            frame(&mut hash, program.as_bytes());
        }
        (None, None, None) => {
            frame(&mut hash, b"shell");
            frame(&mut hash, SHELL.as_bytes());
            frame(&mut hash, &[u8::from(action.shell_strict)]);
            let flags: &[&str] = if action.shell_strict {
                &SHELL_FLAGS
            } else {
                &["-c"]
            };
            frame(&mut hash, &(flags.len() as u64).to_le_bytes());
            for flag in flags {
                frame(&mut hash, flag.as_bytes());
            }
        }
    }
    frame(&mut hash, action.command.as_bytes());
    frame(&mut hash, &(action.arguments.len() as u64).to_le_bytes());
    for argument in &action.arguments {
        frame(&mut hash, argument.as_bytes());
    }
    frame(&mut hash, &(action.environment.len() as u64).to_le_bytes());
    for (name, value) in &action.environment {
        frame(&mut hash, name.as_bytes());
        frame(&mut hash, value.as_bytes());
    }
    frame(&mut hash, &(inputs.len() as u64).to_le_bytes());
    for (name, input) in &inputs {
        frame(&mut hash, name.as_os_str().as_bytes());
        frame_staged(&mut hash, input);
    }
    frame(&mut hash, &(outputs.len() as u64).to_le_bytes());
    for name in &outputs {
        frame(&mut hash, name.as_os_str().as_bytes());
    }
    let extra = &action.extra;
    if !extra.tree_outputs.is_empty()
        || !extra.tree_inputs.is_empty()
        || !extra.param_files.is_empty()
        || extra.exact_environment
        || !extra.execution_info.is_empty()
        || extra.transform.is_some()
    {
        frame(&mut hash, b"n23d-extra");
        frame_count(&mut hash, extra.tree_outputs.len());
        for name in &extra.tree_outputs {
            frame(&mut hash, name.as_os_str().as_bytes());
        }
        frame_count(&mut hash, extra.tree_inputs.len());
        for name in &extra.tree_inputs {
            frame(&mut hash, name.as_os_str().as_bytes());
        }
        frame(&mut hash, &[u8::from(extra.exact_environment)]);
        frame_count(&mut hash, extra.execution_info.len());
        for (name, value) in &extra.execution_info {
            frame(&mut hash, name.as_bytes());
            frame(&mut hash, value.as_bytes());
        }
        let mut params: Vec<&(PathBuf, Vec<u8>)> = extra.param_files.iter().collect();
        params.sort_by(|a, b| a.0.cmp(&b.0));
        frame_count(&mut hash, params.len());
        for (path, content) in params {
            frame(&mut hash, path.as_os_str().as_bytes());
            frame(&mut hash, content);
        }
    }
    Ok((inputs, outputs, hex(&hash.finalize())))
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| io_error("cannot create directory", path, e))?;
    let metadata =
        fs::symlink_metadata(path).map_err(|e| io_error("cannot inspect directory", path, e))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!("expected real directory: '{}'", path.display()));
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| io_error("cannot set permissions", path, e))
}

fn write_file(path: &Path, bytes: &[u8], executable: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        ensure_directory(parent)?;
    }
    fs::write(path, bytes).map_err(|e| io_error("cannot write", path, e))?;
    set_mode(path, 0o644 | executable)
}

/// Remove a directory tree, first making read-only (0555) tree directories
/// writable again. Symlinks are never followed.
fn remove_directory(path: &Path) -> io::Result<()> {
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() {
            continue;
        }
        if metadata.permissions().mode() & 0o700 != 0o700 {
            fs::set_permissions(
                &directory,
                fs::Permissions::from_mode(metadata.permissions().mode() | 0o700),
            )?;
        }
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    fs::remove_dir_all(path)
}

fn stage_tree(root: &Path, tree: &Tree) -> Result<(), String> {
    ensure_directory(root)?;
    for directory in &tree.directories {
        ensure_directory(&root.join(directory))?;
    }
    for (name, input) in &tree.files {
        write_file(&root.join(name), &input.bytes, input.executable)?;
    }
    Ok(())
}

/// Publish a tree with Bazel's completed-tree permissions: every file and
/// subdirectory 0555, the root TREE_ROOT_MODE. Directories are made read-only
/// deepest first, after their contents are written.
fn publish_tree(root: &Path, tree: &Tree) -> Result<(), String> {
    stage_tree(root, tree)?;
    for name in tree.files.keys() {
        set_mode(&root.join(name), OUTPUT_MODE)?;
    }
    let mut directories: Vec<&PathBuf> = tree.directories.iter().collect();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        set_mode(&root.join(directory), OUTPUT_MODE)?;
    }
    set_mode(root, TREE_ROOT_MODE)
}

fn tree_manifest(tree: &Tree) -> TreeManifest {
    TreeManifest {
        files: tree
            .files
            .iter()
            .map(|(name, input)| (hex(name.as_os_str().as_bytes()), input.digest.clone()))
            .collect(),
        directories: tree
            .directories
            .iter()
            .map(|name| hex(name.as_os_str().as_bytes()))
            .collect(),
    }
}

fn cache_valid(
    directory: &Path,
    key: &str,
    outputs: &[PathBuf],
    trees: &BTreeSet<PathBuf>,
) -> Option<BTreeMap<PathBuf, Vec<PathBuf>>> {
    let manifest_path = directory.join("manifest.json");
    let check = || -> Result<Option<BTreeMap<PathBuf, Vec<PathBuf>>>, String> {
        regular_file(&manifest_path)?;
        let bytes = fs::read(&manifest_path)
            .map_err(|e| io_error("cannot read manifest", &manifest_path, e))?;
        let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if manifest.format != MANIFEST_FORMAT
            || manifest.key != key
            || manifest.outputs.len() != outputs.len()
        {
            return Ok(None);
        }
        let mut listings = BTreeMap::new();
        for (entry, name) in manifest.outputs.iter().zip(outputs) {
            if entry.name != hex(name.as_os_str().as_bytes()) {
                return Ok(None);
            }
            let path = directory.join("outputs").join(name);
            match (&entry.tree, trees.contains(name)) {
                (Some(expected), true) => {
                    if real_directory(&path)?.permissions().mode() & 0o7777 != TREE_ROOT_MODE {
                        return Ok(None);
                    }
                    let tree = read_tree(&path)?;
                    if tree_manifest(&tree) != *expected
                        || tree.files.values().any(|input| input.mode != OUTPUT_MODE)
                    {
                        return Ok(None);
                    }
                    for directory in &tree.directories {
                        let mode = fs::symlink_metadata(path.join(directory))
                            .map_err(|e| e.to_string())?
                            .permissions()
                            .mode();
                        if mode & 0o7777 != OUTPUT_MODE {
                            return Ok(None);
                        }
                    }
                    listings.insert(name.clone(), tree_listing(&tree));
                }
                (None, false) => {
                    let actual = read_input(&path)?;
                    if actual.mode != entry.mode || actual.digest != entry.digest {
                        return Ok(None);
                    }
                }
                _ => return Ok(None),
            }
        }
        Ok(Some(listings))
    };
    check().unwrap_or(None)
}

fn fresh_work(parent: &Path, key: &str) -> Result<WorkDir, String> {
    loop {
        let n = NEXT_WORK.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("{key}-{}-{n}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(WorkDir(path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_error("cannot create work directory", &path, e)),
        }
    }
}

fn remove_stale(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
                remove_directory(path)
            } else {
                fs::remove_file(path)
            };
            result.map_err(|e| io_error("cannot remove invalid cache entry", path, e))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error("cannot inspect cache entry", path, e)),
    }
}

// Read through the original file handle, not an action-controlled path. Limit
// reads even if a background child continues appending after the shell exits.
fn log_tail(file: &mut File) -> io::Result<String> {
    let length = file.metadata()?.len();
    let start = length.saturating_sub(LOG_TAIL_BYTES as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(LOG_TAIL_BYTES as u64).read_to_end(&mut bytes)?;
    let mut result = String::new();
    if start != 0 {
        result.push_str("[truncated: showing last 65536 bytes]\n");
    }
    result.push_str(&String::from_utf8_lossy(&bytes));
    Ok(result)
}

fn create_log(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| io_error("cannot create action log", path, e))
}

// On Linux an executable can transiently remain open for writing in another
// concurrently spawned child, even after our own fs::write has closed its fd.
// Retry only an ETXTBSY spawn error: exec has not succeeded, so the action has
// not run. Never retry wait errors or exit statuses. The injected backoff keeps
// retry-count/error-classification tests deterministic and free of sleeps.
fn retry_spawn<T>(
    mut spawn: impl FnMut() -> io::Result<T>,
    mut backoff: impl FnMut(Duration),
) -> io::Result<T> {
    let mut delays = SPAWN_RETRY_DELAYS_MS.into_iter();
    loop {
        match spawn() {
            Err(error) if cfg!(target_os = "linux") && error.raw_os_error() == Some(26) => {
                let Some(delay) = delays.next() else {
                    return Err(error);
                };
                backoff(Duration::from_millis(delay));
            }
            result => return result,
        }
    }
}

/// StringUtilities.replaceAllLiteral (util/StringUtilities.java:70-88) on
/// internal bytes: non-overlapping, left to right; an empty key is a no-op.
fn replace_all_literal(input: &[u8], literal: &[u8], replacement: &[u8]) -> Vec<u8> {
    if literal.is_empty() {
        return input.to_vec();
    }
    let mut result = Vec::with_capacity(input.len());
    let mut start = 0;
    while let Some(offset) = input[start..]
        .windows(literal.len())
        .position(|window| window == literal)
    {
        result.extend_from_slice(&input[start..start + offset]);
        result.extend_from_slice(replacement);
        start += offset + literal.len();
    }
    result.extend_from_slice(&input[start..]);
    result
}

/// Run a native transform; the output is left in the execution root.
fn run_transform(
    action: &Action,
    transform: &Transform,
    inputs: &BTreeMap<PathBuf, Staged>,
    execroot: &Path,
) -> Result<(), String> {
    let output = execroot.join(&action.outputs[0]);
    match transform {
        Transform::Unsupported { message } => Err(message.clone()),
        Transform::Template {
            template,
            substitutions,
            executable,
        } => {
            let Some(Staged::File(input)) = inputs.get(template) else {
                return Err(format!(
                    "failed to load template file '{}': not a regular file",
                    template.display()
                ));
            };
            let mut text = input.bytes.clone();
            for (key, value) in substitutions {
                text = replace_all_literal(&text, key, value);
            }
            write_file(&output, &text, if *executable { 0o111 } else { 0 })
        }
        Transform::Symlink {
            target,
            require_executable,
            progress,
        } => {
            let tree_output = action.extra.tree_outputs.contains(&action.outputs[0]);
            match inputs.get(target) {
                Some(Staged::File(input)) if !tree_output => {
                    // SymlinkAction.maybeVerifyTargetIsExecutable
                    // (analysis/actions/SymlinkAction.java:296-330): S_IXUSR.
                    if *require_executable && input.mode & 0o100 == 0 {
                        return Err(format!(
                            "{progress} failed: failed to create symbolic link '{}': file '{}' is not executable",
                            action.outputs[0].display(),
                            target.display()
                        ));
                    }
                    write_file(&output, &input.bytes, input.executable)
                }
                Some(Staged::Tree(tree)) if tree_output => {
                    if output.exists() {
                        remove_directory(&output)
                            .map_err(|e| io_error("cannot replace", &output, e))?;
                    }
                    stage_tree(&output, tree)
                }
                _ => Err(format!(
                    "symlink target '{}' and output '{}' must both be files or both be directories",
                    target.display(),
                    action.outputs[0].display()
                )),
            }
        }
    }
}

enum Collected {
    File(Input),
    Tree(Tree),
}

/// Execute an action or return its verified cached outputs in declaration order.
///
/// Executable bits are preserved and keyed. Other permission bits, timestamps,
/// and source path spellings are not inputs. Only declared regular output files
/// and tree outputs are published; undeclared files are discarded. The cache uses per-key
/// cross-process locks, so unrelated actions may execute concurrently.
/// Shell commands are trusted and unsandboxed, run with Bash; `shell_strict` adds
/// -e -o pipefail for genrules, while custom run_shell actions use only -c.
/// All shell actions get an empty `$0`, then literal `$1`, `$2`, ... . Explicit environment
/// entries override PATH/LANG/LC_ALL/TMPDIR defaults; no caller variables leak in.
/// Starlark actions (`exact_environment`) get exactly their environment plus TMPDIR.
/// Direct programs receive arguments verbatim, without a shell or strictness flags.
/// Relative programs containing a slash resolve in the action working directory;
/// bare names search PATH, and absolute paths are accepted. Host executables are
/// trusted, unsandboxed, and not content-keyed unless declared as action inputs.
/// Native writes spawn no subprocess and set output permissions to 0644 or 0755.
/// Logs are spooled to temporary files; at most the last 64 KiB per stream is
/// decoded (lossily) for diagnostics. Successful log tails are replayed to stderr,
/// keeping stdout artifact-only. Cache hits do not replay logs. Disk usage is not
/// capped; logs are removed with the working directory.
/// Linux ETXTBSY spawn failures get at most seven retries (127 ms total requested
/// backoff). Once a child starts, its wait result and exit status are never retried.
pub fn execute(cache_root: &Path, action: &Action) -> Result<ActionResult, String> {
    let (inputs, outputs, key) = prepare(action)?;
    fs::create_dir_all(cache_root).map_err(|e| io_error("cannot create cache", cache_root, e))?;
    let root = fs::canonicalize(cache_root)
        .map_err(|e| io_error("cannot resolve cache", cache_root, e))?;
    let actions = root.join("actions");
    let locks = root.join("locks");
    let work_parent = root.join("work");
    for path in [&actions, &locks, &work_parent] {
        ensure_directory(path)?;
    }
    let lock_path = locks.join(format!("{key}.lock"));
    // Locks are never unlinked: removing a held lock would permit two distinct
    // inodes (and thus two owners) for the same action key.
    if let Ok(metadata) = fs::symlink_metadata(&lock_path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err(format!(
            "expected regular lock file: '{}'",
            lock_path.display()
        ));
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| io_error("cannot open lock", &lock_path, e))?;
    FileExt::lock_exclusive(&lock).map_err(|e| io_error("cannot lock", &lock_path, e))?;
    // File drop releases the advisory lock on every success/error path.
    let cached = actions.join(&key);
    let result = |listings: BTreeMap<PathBuf, Vec<PathBuf>>, cache_hit: bool| ActionResult {
        outputs: action
            .outputs
            .iter()
            .map(|p| cached.join("outputs").join(p))
            .collect(),
        cache_hit,
        trees: action
            .outputs
            .iter()
            .enumerate()
            .filter_map(|(index, name)| Some((index, listings.get(name)?.clone())))
            .collect(),
    };
    if let Some(listings) = cache_valid(&cached, &key, &outputs, &action.extra.tree_outputs) {
        return Ok(result(listings, true));
    }
    remove_stale(&cached)?;
    let work = fresh_work(&work_parent, &key)?;
    let execroot = work.0.join("execroot");
    let tmp = work.0.join("tmp");
    ensure_directory(&execroot)?;
    ensure_directory(&tmp)?;
    for (name, input) in &inputs {
        match input {
            Staged::File(input) => {
                write_file(&execroot.join(name), &input.bytes, input.executable)?
            }
            Staged::Tree(tree) => stage_tree(&execroot.join(name), tree)?,
        }
    }
    for (name, content) in &action.extra.param_files {
        write_file(&execroot.join(name), content, 0)?;
    }
    for name in &outputs {
        if let Some(parent) = execroot.join(name).parent() {
            ensure_directory(parent)?;
        }
        // Bazel creates tree artifact directories before execution.
        if action.extra.tree_outputs.contains(name) {
            ensure_directory(&execroot.join(name))?;
        }
    }
    if let Some(write) = &action.write {
        write_file(
            &execroot.join(&outputs[0]),
            &write.content,
            if write.executable { 0o111 } else { 0 },
        )?;
    } else if let Some(transform) = &action.extra.transform {
        run_transform(action, transform, &inputs, &execroot)?;
    } else {
        let mut stdout = create_log(&work.0.join("stdout.log"))?;
        let mut stderr = create_log(&work.0.join("stderr.log"))?;
        let mut command = if let Some(program) = &action.program {
            let mut command = Command::new(program);
            command.args(&action.arguments);
            command
        } else {
            let mut command = Command::new(SHELL);
            if action.shell_strict {
                command.args(SHELL_FLAGS);
            } else {
                command.arg("-c");
            }
            command.arg(&action.command).arg("").args(&action.arguments);
            command
        };
        command.current_dir(&execroot).env_clear();
        if !action.extra.exact_environment {
            command.envs(ENV);
        }
        command
            .env("TMPDIR", &tmp)
            .envs(&action.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout.try_clone().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(stderr.try_clone().map_err(|e| e.to_string())?));
        let mut child = retry_spawn(|| command.spawn(), std::thread::sleep)
            .map_err(|e| format!("cannot execute action '{}': {e}", action.id))?;
        let status = child
            .wait()
            .map_err(|e| format!("cannot wait for action '{}': {e}", action.id))?;
        let stdout =
            log_tail(&mut stdout).map_err(|e| format!("cannot read action stdout: {e}"))?;
        let stderr =
            log_tail(&mut stderr).map_err(|e| format!("cannot read action stderr: {e}"))?;
        if !status.success() {
            return Err(format!(
                "action '{}' failed ({}):\nstdout:\n{}\nstderr:\n{}",
                action.id, status, stdout, stderr
            ));
        }
        if !stdout.is_empty() || !stderr.is_empty() {
            eprintln!(
                "action '{}' logs:\nstdout:\n{}\nstderr:\n{}",
                action.id, stdout, stderr
            );
        }
    }
    // Validate every output before creating the completed directory. Snapshot
    // output contents too, so the manifest describes exactly what is published.
    let symlink_mode = match &action.extra.transform {
        Some(Transform::Symlink { target, .. }) => match inputs.get(target) {
            Some(Staged::File(input)) => Some(input.mode),
            _ => None,
        },
        _ => None,
    };
    let mut collected = Vec::new();
    for name in &outputs {
        let path = execroot.join(name);
        let snapshot = || -> Result<(Collected, u32), String> {
            if action.extra.tree_outputs.contains(name) {
                return Ok((Collected::Tree(read_tree(&path)?), TREE_ROOT_MODE));
            }
            // Match metadata collection order: a successful action may leave a
            // regular output unreadable. Normalize before reading its bytes,
            // but never chmod a symlink or a path through a linked ancestor.
            regular_file(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(OUTPUT_MODE))
                .map_err(|e| io_error("cannot normalize action output permissions", &path, e))?;
            Ok((
                Collected::File(read_input(&path)?),
                symlink_mode.unwrap_or(OUTPUT_MODE),
            ))
        };
        let (input, mode) = snapshot().map_err(|e| {
            format!(
                "action '{}' has missing or invalid output '{}': {e}",
                action.id,
                name.display()
            )
        })?;
        collected.push((name, input, mode));
    }
    let completed = work.0.join("completed");
    ensure_directory(&completed.join("outputs"))?;
    let mut manifest = Manifest {
        format: MANIFEST_FORMAT.into(),
        key: key.clone(),
        outputs: Vec::new(),
    };
    let mut listings = BTreeMap::new();
    for (name, input, mode) in collected {
        let path = completed.join("outputs").join(name);
        match input {
            Collected::File(input) => {
                write_file(&path, &input.bytes, 0o111)?;
                fs::set_permissions(&path, fs::Permissions::from_mode(mode)).map_err(|e| {
                    io_error("cannot normalize action output permissions", &path, e)
                })?;
                manifest.outputs.push(Output {
                    name: hex(name.as_os_str().as_bytes()),
                    digest: input.digest,
                    mode,
                    tree: None,
                });
            }
            Collected::Tree(tree) => {
                if let Some(parent) = path.parent() {
                    ensure_directory(parent)?;
                }
                publish_tree(&path, &tree)?;
                listings.insert(name.clone(), tree_listing(&tree));
                manifest.outputs.push(Output {
                    name: hex(name.as_os_str().as_bytes()),
                    digest: String::new(),
                    mode,
                    tree: Some(tree_manifest(&tree)),
                });
            }
        }
    }
    let manifest_bytes =
        serde_json::to_vec(&manifest).map_err(|e| format!("cannot encode manifest: {e}"))?;
    write_file(&completed.join("manifest.json"), &manifest_bytes, 0)?;
    // Same-filesystem rename publishes all declared files and the manifest at
    // once. Crash durability is not promised; damaged entries rebuild on use.
    fs::rename(&completed, &cached).map_err(|e| io_error("cannot publish action", &cached, e))?;
    Ok(result(listings, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::{Arc, Barrier};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir();
            loop {
                let path = root.join(format!(
                    "native-action-test-{}-{}",
                    std::process::id(),
                    NEXT_WORK.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(fs::canonicalize(path).unwrap()),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("{e}"),
                }
            }
        }

        fn cache(&self) -> PathBuf {
            self.0.join("cache")
        }

        fn action(&self) -> Action {
            let source = self.0.join("source");
            fs::write(&source, b"hello\n").unwrap();
            fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
            Action {
                id: "copy".into(),
                command: "cat src/input > out/result".into(),
                inputs: vec![(source, PathBuf::from("src/input"))],
                outputs: vec![PathBuf::from("out/result")],
                write: None,
                program: None,
                arguments: Vec::new(),
                environment: BTreeMap::new(),
                shell_strict: true,
                extra: ActionExtra::default(),
            }
        }

        fn assert_unpublished(&self) {
            for name in ["actions", "work"] {
                let path = self.cache().join(name);
                if path.exists() {
                    assert_eq!(fs::read_dir(path).unwrap().count(), 0);
                }
            }
        }

        fn write_action(&self, content: Vec<u8>) -> Action {
            Action {
                id: "write".into(),
                command: String::new(),
                inputs: Vec::new(),
                outputs: vec!["out/result".into()],
                write: Some(WriteAction {
                    content,
                    executable: false,
                }),
                program: None,
                arguments: Vec::new(),
                environment: BTreeMap::new(),
                shell_strict: true,
                extra: ActionExtra::default(),
            }
        }

        fn direct_action(&self, script: &str) -> Action {
            let source = self.0.join("executable-source");
            fs::write(&source, script).unwrap();
            fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
            Action {
                id: "direct".into(),
                command: String::new(),
                inputs: vec![(source, "tools/quoted '$ tool".into())],
                outputs: vec!["out/result".into()],
                write: None,
                program: Some("./tools/quoted '$ tool".into()),
                arguments: Vec::new(),
                environment: BTreeMap::new(),
                shell_strict: true,
                extra: ActionExtra::default(),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // Published tree artifacts are read-only (0555) directories.
            let _ = remove_directory(&self.0);
        }
    }

    #[test]
    fn native_write_large_binary_and_shell_text_are_not_executed() {
        let fixture = Fixture::new();
        let marker = fixture.0.join("subprocess-ran");
        let mut content = format!(
            "#!/bin/bash\ntouch '{}'\n$(touch '{}')\n",
            marker.display(),
            marker.display()
        )
        .into_bytes();
        content.extend((0..300 * 1024).map(|i| (i % 256) as u8));
        let action = fixture.write_action(content.clone());
        let first = execute(&fixture.cache(), &action).unwrap();
        assert!(!first.cache_hit);
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), content);
        assert_eq!(
            fs::metadata(&first.outputs[0])
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o555
        );
        assert!(!marker.exists());
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert_eq!(
            fs::read_dir(fixture.cache().join("work")).unwrap().count(),
            0
        );
    }

    #[test]
    fn native_write_content_mode_and_variant_are_keyed() {
        let fixture = Fixture::new();
        let mut action = fixture.write_action(b"original\0".to_vec());
        let first = execute(&fixture.cache(), &action).unwrap();
        action.write.as_mut().unwrap().executable = true;
        let executable = execute(&fixture.cache(), &action).unwrap();
        assert!(!executable.cache_hit);
        assert_ne!(first.outputs, executable.outputs);
        assert_eq!(
            fs::metadata(&executable.outputs[0])
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o555
        );
        action.write.as_mut().unwrap().content.push(0xff);
        let changed = execute(&fixture.cache(), &action).unwrap();
        assert!(!changed.cache_hit);
        assert_ne!(executable.outputs, changed.outputs);
        assert_eq!(fs::read(&changed.outputs[0]).unwrap(), b"original\0\xff");
        let mut empty = fixture.write_action(Vec::new());
        let write_key = prepare(&empty).unwrap().2;
        empty.write = None;
        assert_ne!(write_key, prepare(&empty).unwrap().2);
    }

    #[test]
    fn native_write_tampered_bytes_and_mode_rebuild() {
        let fixture = Fixture::new();
        let action = fixture.write_action(b"restored\0".to_vec());
        let first = execute(&fixture.cache(), &action).unwrap();
        let path = &first.outputs[0];
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(path, b"tampered").unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert_eq!(fs::read(path).unwrap(), b"restored\0");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o555
        );
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
    }

    #[test]
    fn invalid_write_shapes_rejected_before_cache_io() {
        let fixture = Fixture::new();
        let original = fixture.write_action(vec![0]);
        for case in 0..6 {
            let mut action = original.clone();
            match case {
                0 => action.command = "true".into(),
                1 => action.arguments.push(String::new()),
                2 => {
                    action.environment.insert("KEY".into(), "value".into());
                }
                3 => action
                    .inputs
                    .push((fixture.0.join("missing"), "input".into())),
                4 => action.outputs.clear(),
                5 => action.outputs.push("other".into()),
                _ => unreachable!(),
            }
            assert!(
                validate_structure(&action)
                    .unwrap_err()
                    .contains("write action")
            );
            assert!(
                execute(&fixture.cache(), &action)
                    .unwrap_err()
                    .contains("write action")
            );
            assert!(!fixture.cache().exists());
        }
    }

    #[test]
    fn direct_executable_input_preserves_literal_arguments_and_environment() {
        let fixture = Fixture::new();
        let mut action = fixture.direct_action(
            "#!/bin/bash\nfalse\nfalse | true\ntest \"$#\" = 4 || exit 12\ntest \"$PATH\" = /usr/bin:/bin || exit 13\ntest -z \"${HOME+x}\" || exit 14\n[[ $TMPDIR = /* ]] && test -d \"$TMPDIR\" || exit 15\nprintf '%s\\0' \"$0\" \"$1\" \"$2\" \"$3\" \"$4\" \"$VALUE\" > out/result\n",
        );
        let marker = fixture.0.join("expanded");
        action.arguments = vec![
            String::new(),
            "雪 'quotes'".into(),
            "$HOME *".into(),
            format!("$(touch '{}')", marker.display()),
        ];
        action
            .environment
            .insert("VALUE".into(), "$literal 雪".into());
        let first = execute(&fixture.cache(), &action).unwrap();
        let expected: Vec<u8> = std::iter::once(action.program.as_ref().unwrap())
            .chain(action.arguments.iter())
            .chain(std::iter::once(&action.environment["VALUE"]))
            .flat_map(|value| value.bytes().chain(std::iter::once(0)))
            .collect();
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), expected);
        assert!(!marker.exists());
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.shell_strict = false;
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.arguments.swap(0, 1);
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.environment.insert("VALUE".into(), "changed".into());
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
    }

    #[test]
    fn direct_program_content_mode_path_and_variant_are_keyed() {
        let fixture = Fixture::new();
        let mut action = fixture.direct_action("#!/bin/sh\nprintf first > out/result\n");
        let first = execute(&fixture.cache(), &action).unwrap();
        fs::set_permissions(&action.inputs[0].0, fs::Permissions::from_mode(0o744)).unwrap();
        let mode = execute(&fixture.cache(), &action).unwrap();
        assert!(!mode.cache_hit);
        assert_ne!(first.outputs, mode.outputs);
        fs::write(
            &action.inputs[0].0,
            "#!/bin/sh\nprintf changed > out/result\n",
        )
        .unwrap();
        let content = execute(&fixture.cache(), &action).unwrap();
        assert!(!content.cache_hit);
        assert_eq!(fs::read(&content.outputs[0]).unwrap(), b"changed");
        action.program = Some("tools/quoted '$ tool".into());
        let path = execute(&fixture.cache(), &action).unwrap();
        assert!(!path.cache_hit);
        assert_ne!(content.outputs, path.outputs);
        let direct_key = prepare(&action).unwrap().2;
        action.program = None;
        assert_ne!(direct_key, prepare(&action).unwrap().2);
    }

    #[test]
    fn direct_program_searches_explicit_path_and_accepts_absolute_paths() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command.clear();
        action.arguments = vec!["src/input".into(), "out/result".into()];
        action.program = Some("cp".into());
        action.environment.insert("PATH".into(), "/bin".into());
        let searched = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&searched.outputs[0]).unwrap(), b"hello\n");
        action.program = Some("/bin/cp".into());
        let absolute = execute(&fixture.cache(), &action).unwrap();
        assert!(!absolute.cache_hit);
        assert_eq!(fs::read(&absolute.outputs[0]).unwrap(), b"hello\n");
        action.program = Some("cp".into());
        action
            .environment
            .insert("PATH".into(), "/no/such/directory".into());
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("cannot execute action")
        );
    }

    #[test]
    fn direct_failed_process_has_bounded_logs_and_no_publication() {
        let fixture = Fixture::new();
        let action = fixture.direct_action(
            "#!/bin/sh\nprintf partial > out/result\nhead -c 100000 /dev/zero\nprintf stdout-end\nhead -c 100000 /dev/zero >&2\nprintf stderr-end >&2\nexit 7\n",
        );
        let error = execute(&fixture.cache(), &action).unwrap_err();
        assert!(error.contains("failed (exit status: 7)"));
        assert!(error.contains("stdout-end") && error.contains("stderr-end"));
        assert_eq!(error.matches("[truncated:").count(), 2);
        assert!(error.len() < 2 * LOG_TAIL_BYTES + 1024);
        fixture.assert_unpublished();
        fs::write(
            &action.inputs[0].0,
            "#!/bin/sh\nprintf recovered > out/result\n",
        )
        .unwrap();
        let recovered = execute(&fixture.cache(), &action).unwrap();
        fs::set_permissions(&recovered.outputs[0], fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&recovered.outputs[0], "tampered").unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert_eq!(fs::read(&recovered.outputs[0]).unwrap(), b"recovered");
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
    }

    #[test]
    fn invalid_direct_shapes_are_rejected_before_cache_io() {
        let fixture = Fixture::new();
        let original = fixture.direct_action("#!/bin/sh\nexit 0\n");
        fs::remove_file(&original.inputs[0].0).unwrap();
        for case in 0..6 {
            let mut action = original.clone();
            match case {
                0 => action.program = Some(String::new()),
                1 => action.program = Some("bad\0program".into()),
                2 => action.command = "true".into(),
                3 => action.arguments.push("bad\0argument".into()),
                4 => {
                    action.environment.insert("bad=key".into(), "value".into());
                }
                5 => {
                    action.inputs.clear();
                    action.write = Some(WriteAction {
                        content: vec![],
                        executable: false,
                    });
                }
                _ => unreachable!(),
            }
            let structural = validate_structure(&action).unwrap_err();
            assert_eq!(execute(&fixture.cache(), &action).unwrap_err(), structural);
            assert!(!fixture.cache().exists());
        }
    }

    #[test]
    fn shell_strictness_controls_errexit_and_pipeline_status() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        for command in [
            "false; printf ok > out/result",
            "false | printf ok > out/result",
        ] {
            action.command = command.into();
            action.shell_strict = false;
            let lax = execute(&fixture.cache(), &action).unwrap();
            assert!(!lax.cache_hit);
            assert_eq!(fs::read(&lax.outputs[0]).unwrap(), b"ok");
            action.shell_strict = true;
            assert!(
                execute(&fixture.cache(), &action)
                    .unwrap_err()
                    .contains("failed (exit status: 1)")
            );
        }
        action.shell_strict = false;
        action.command = "printf partial > out/result; true | false".into();
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("failed (exit status: 1)")
        );
    }

    #[test]
    fn lax_shell_always_has_empty_zero_and_literal_first_argument() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.shell_strict = false;
        action.command = "test \"$0\" = '' && test \"$#\" = 0 && printf ok > out/result".into();
        execute(&fixture.cache(), &action).unwrap();
        action.arguments = vec!["雪 $HOME 'literal'".into()];
        action.command =
            "test \"$0\" = '' && test \"$#\" = 1 && printf '%s' \"$1\" > out/result".into();
        let result = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(
            fs::read(&result.outputs[0]).unwrap(),
            action.arguments[0].as_bytes()
        );
    }

    #[test]
    fn shell_strictness_is_keyed_only_for_shell_actions() {
        let fixture = Fixture::new();
        let mut shell = fixture.action();
        let strict = execute(&fixture.cache(), &shell).unwrap();
        shell.shell_strict = false;
        let lax = execute(&fixture.cache(), &shell).unwrap();
        assert!(!lax.cache_hit);
        assert_ne!(strict.outputs, lax.outputs);
        let mut write = fixture.write_action(b"native".to_vec());
        let first = execute(&fixture.cache(), &write).unwrap();
        write.shell_strict = false;
        let second = execute(&fixture.cache(), &write).unwrap();
        assert!(second.cache_hit);
        assert_eq!(first.outputs, second.outputs);
    }

    #[test]
    fn shell_arguments_are_literal_ordered_and_keyed() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        let marker = fixture.0.join("argument-ran");
        action.arguments = vec![
            String::new(),
            "雪 🦀".into(),
            "$HOME 'quoted' \\\n".into(),
            format!("$(touch '{}')", marker.display()),
        ];
        action.command = "test \"$0\" = '' && test \"$#\" = 4 && printf '%s\\0' \"$1\" \"$2\" \"$3\" \"$4\" > out/result".into();
        let expected: Vec<u8> = action
            .arguments
            .iter()
            .flat_map(|arg| arg.bytes().chain(std::iter::once(0)))
            .collect();
        let first = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), expected);
        assert!(!marker.exists());
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.arguments.swap(0, 1);
        let reordered = execute(&fixture.cache(), &action).unwrap();
        assert!(!reordered.cache_hit);
        assert_ne!(first.outputs, reordered.outputs);
        action.arguments.clear();
        action.command = "test \"$0\" = '' && test \"$#\" = 0 && printf ok > out/result".into();
        execute(&fixture.cache(), &action).unwrap();
    }

    #[test]
    fn shell_environment_overlays_defaults_and_is_keyed() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.environment = BTreeMap::from([
            ("VALUE".into(), "literal $HOME 雪".into()),
            ("PATH".into(), "/explicit/path".into()),
            ("LANG".into(), "POSIX".into()),
            ("LC_ALL".into(), "POSIX".into()),
            ("TMPDIR".into(), "/explicit/tmp".into()),
            ("EMPTY".into(), String::new()),
        ]);
        action.command = "test \"$PATH\" = /explicit/path && test \"$LANG\" = POSIX && test \"$LC_ALL\" = POSIX && test \"$TMPDIR\" = /explicit/tmp && test \"${EMPTY+x}\" = x && test -z \"$EMPTY\" && test -z \"${HOME+x}\" && printf '%s' \"$VALUE\" > out/result".into();
        let first = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(
            fs::read(&first.outputs[0]).unwrap(),
            "literal $HOME 雪".as_bytes()
        );
        let reordered: BTreeMap<_, _> = action
            .environment
            .iter()
            .rev()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        action.environment = reordered;
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.environment.insert("VALUE".into(), "changed".into());
        let changed = execute(&fixture.cache(), &action).unwrap();
        assert!(!changed.cache_hit);
        assert_ne!(first.outputs, changed.outputs);
        assert_eq!(fs::read(&changed.outputs[0]).unwrap(), b"changed");
        action
            .environment
            .insert("UNUSED".into(), "also-keyed".into());
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
    }

    #[test]
    fn invalid_shell_arguments_and_environment_rejected_before_cache_io() {
        let fixture = Fixture::new();
        let original = fixture.action();
        for (name, value) in [
            ("", "value"),
            ("a=b", "value"),
            ("a\0b", "value"),
            ("key", "a\0b"),
        ] {
            let mut action = original.clone();
            action.environment.insert(name.into(), value.into());
            assert!(
                execute(&fixture.cache(), &action)
                    .unwrap_err()
                    .contains("environment")
            );
            assert!(!fixture.cache().exists());
        }
        let mut action = original;
        action.arguments.push("a\0b".into());
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("argument contains a NUL")
        );
        assert!(!fixture.cache().exists());
    }

    #[test]
    fn successful_hit_and_output_order() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command += "; printf second > other; chmod 751 out/result";
        action.outputs.push("other".into());
        let first = execute(&fixture.cache(), &action).unwrap();
        assert!(!first.cache_hit);
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), b"hello\n");
        assert_eq!(fs::read(&first.outputs[1]).unwrap(), b"second");
        assert_eq!(
            fs::metadata(&first.outputs[0])
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0o111
        );
        let second = execute(&fixture.cache(), &action).unwrap();
        assert!(second.cache_hit);
        assert_eq!(first.outputs, second.outputs);
        action.outputs.reverse();
        let reordered = execute(&fixture.cache(), &action).unwrap();
        assert!(reordered.cache_hit);
        assert_eq!(reordered.outputs[0], first.outputs[1]);
    }

    #[test]
    fn content_command_id_and_mode_invalidation() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        let first = execute(&fixture.cache(), &action).unwrap();
        fs::write(&action.inputs[0].0, b"changed").unwrap();
        let changed = execute(&fixture.cache(), &action).unwrap();
        assert!(!changed.cache_hit);
        assert_ne!(first.outputs, changed.outputs);
        assert_eq!(fs::read(&changed.outputs[0]).unwrap(), b"changed");
        action.command += "; printf ! >> out/result";
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        fs::set_permissions(&action.inputs[0].0, fs::Permissions::from_mode(0o755)).unwrap();
        action.command += "; test -x src/input";
        let executable = execute(&fixture.cache(), &action).unwrap();
        assert!(!executable.cache_hit);
        // Change only mode, not command or contents.
        fs::set_permissions(&action.inputs[0].0, fs::Permissions::from_mode(0o744)).unwrap();
        let mode_changed = execute(&fixture.cache(), &action).unwrap();
        assert!(!mode_changed.cache_hit);
        assert_ne!(executable.outputs, mode_changed.outputs);
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.id += "-new";
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
    }

    #[test]
    fn tampered_missing_mode_and_manifest_rebuild() {
        let fixture = Fixture::new();
        let action = fixture.action();
        let result = execute(&fixture.cache(), &action).unwrap();
        let path = &result.outputs[0];
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(path, "tampered").unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert_eq!(fs::read(path).unwrap(), b"hello\n");
        for mode in [0o444, 0o644, 0o755, 0o4755] {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o7777,
                OUTPUT_MODE
            );
        }
        fs::remove_file(path).unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        let entry = path.parent().unwrap().parent().unwrap().parent().unwrap();
        fs::write(entry.join("manifest.json"), b"{}").unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        fs::remove_file(path).unwrap();
        symlink(&action.inputs[0].0, path).unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert!(!fs::symlink_metadata(path).unwrap().file_type().is_symlink());
    }

    #[test]
    fn failures_do_not_publish_and_can_retry() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command =
            "echo stdout-message; echo stderr-message >&2; echo partial > out/result; exit 7"
                .into();
        let error = execute(&fixture.cache(), &action).unwrap_err();
        assert!(
            error.contains("stdout-message")
                && error.contains("stderr-message")
                && error.contains('7')
        );
        fixture.assert_unpublished();
        action.command = "true".into();
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("missing or invalid output")
        );
        fixture.assert_unpublished();
        // The same key retries cleanly when an external prerequisite changes.
        let prerequisite = fixture.0.join("ready");
        action.command = format!(
            "test -f '{}' && cat src/input > out/result",
            prerequisite.display()
        );
        assert!(execute(&fixture.cache(), &action).is_err());
        fixture.assert_unpublished();
        fs::write(prerequisite, b"").unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
    }

    #[test]
    fn shell_failures_and_pipeline_failures_do_not_publish() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        let marker = fixture.0.join("continued");
        for command in ["false; echo ok > out/result", "false | cat > out/result"] {
            action.command = format!("{command}; touch '{}'", marker.display());
            let error = execute(&fixture.cache(), &action).unwrap_err();
            assert!(error.contains("failed (exit status: 1)"), "{error}");
            assert!(!marker.exists());
            fixture.assert_unpublished();
        }
    }

    #[test]
    fn substantial_logs_succeed_or_fail_with_bounded_tails() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        let logs = "printf stdout-start; head -c 1048576 /dev/zero | tr '\\0' x; printf stdout-end; printf stderr-start >&2; head -c 1048576 /dev/zero | tr '\\0' y >&2; printf stderr-end >&2";
        action.command = format!("{logs}; printf artifact > out/result");
        let result = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&result.outputs[0]).unwrap(), b"artifact");
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert_eq!(
            fs::read_dir(fixture.cache().join("work")).unwrap().count(),
            0
        );

        let failed_fixture = Fixture::new();
        action.command = format!("{logs}; printf partial > out/result; false");
        let error = execute(&failed_fixture.cache(), &action).unwrap_err();
        assert!(error.len() < 2 * LOG_TAIL_BYTES + 1024);
        assert_eq!(error.matches("[truncated:").count(), 2);
        assert!(error.contains("stdout-end") && error.contains("stderr-end"));
        assert!(!error.contains("stdout-start") && !error.contains("stderr-start"));
        failed_fixture.assert_unpublished();
    }

    #[test]
    fn log_tails_are_bounded_and_mark_only_truncation() {
        let fixture = Fixture::new();
        let path = fixture.0.join("log");
        for length in [0, 10, LOG_TAIL_BYTES, LOG_TAIL_BYTES + 1] {
            fs::write(&path, vec![b'x'; length]).unwrap();
            let tail = log_tail(&mut File::open(&path).unwrap()).unwrap();
            assert_eq!(tail.contains("[truncated:"), length > LOG_TAIL_BYTES);
            assert_eq!(tail.matches('x').count(), length.min(LOG_TAIL_BYTES));
            assert!(tail.len() <= LOG_TAIL_BYTES + 100);
        }
        fs::write(&path, vec![0xff; LOG_TAIL_BYTES + 1]).unwrap();
        let tail = log_tail(&mut File::open(&path).unwrap()).unwrap();
        assert!(tail.starts_with("[truncated:"));
        assert!(tail.len() <= 3 * LOG_TAIL_BYTES + 100);
    }

    #[test]
    fn structure_validation_does_not_require_sources_or_create_cache() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        fs::remove_file(&action.inputs[0].0).unwrap();
        validate_structure(&action).unwrap();
        action
            .inputs
            .push((fixture.0.join("also-missing"), "src/input".into()));
        validate_structure(&action).unwrap();
        action.outputs.clear();
        validate_structure(&action).unwrap();

        action.command = "true\0; echo unreachable".into();
        assert!(validate_structure(&action).unwrap_err().contains("NUL"));
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("NUL")
        );
        assert!(!fixture.cache().exists());
    }

    #[test]
    fn unsafe_paths_rejected_before_shell() {
        let fixture = Fixture::new();
        let original = fixture.action();
        fs::remove_file(&original.inputs[0].0).unwrap();
        let marker = fixture.0.join("ran");
        for name in [
            "",
            "/absolute",
            "../escape",
            "a/../b",
            "./a",
            "a/./b",
            "a//b",
            "a/",
            "a\0b",
        ] {
            for input in [false, true] {
                let mut action = original.clone();
                action.command = format!("touch '{}'", marker.display());
                if input {
                    action.inputs[0].1 = name.into();
                } else {
                    action.outputs[0] = name.into();
                }
                assert!(validate_structure(&action).is_err(), "accepted {name:?}");
                assert!(
                    execute(&fixture.cache(), &action).is_err(),
                    "accepted {name:?}"
                );
                assert!(!marker.exists());
            }
        }
        for name in ["src", "src/input", "src/input/child"] {
            let mut action = original.clone();
            action.outputs = vec![name.into()];
            assert!(validate_structure(&action).is_err());
            assert!(execute(&fixture.cache(), &action).is_err());
        }
        let mut action = original.clone();
        action.outputs = vec!["a".into(), "a/b".into()];
        assert!(validate_structure(&action).is_err());
        assert!(execute(&fixture.cache(), &action).is_err());
        action.outputs = vec!["a".into(), "a".into()];
        assert!(validate_structure(&action).is_err());
        action = original.clone();
        action
            .inputs
            .push((action.inputs[0].0.clone(), "src".into()));
        assert!(validate_structure(&action).is_err());
        assert!(execute(&fixture.cache(), &action).is_err());
        assert!(!fixture.cache().exists());
        fixture.assert_unpublished();
    }

    #[test]
    fn duplicate_mappings_and_input_order() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action
            .inputs
            .push((action.inputs[0].0.clone(), "second".into()));
        let first = execute(&fixture.cache(), &action).unwrap();
        action.inputs.reverse();
        action.inputs.push(action.inputs[0].clone());
        let second = execute(&fixture.cache(), &action).unwrap();
        assert!(second.cache_hit);
        assert_eq!(first.outputs, second.outputs);
        let identical = fixture.0.join("identical");
        fs::write(&identical, b"hello\n").unwrap();
        fs::set_permissions(&identical, fs::Permissions::from_mode(0o644)).unwrap();
        action.inputs.push((identical, "src/input".into()));
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        let conflicting = fixture.0.join("different");
        fs::write(&conflicting, "different").unwrap();
        action.inputs.push((conflicting, "src/input".into()));
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("conflicting duplicate")
        );
    }

    #[test]
    fn reject_nonregular_and_symlink_inputs_outputs() {
        let fixture = Fixture::new();
        let original = fixture.action();
        let link = fixture.0.join("link");
        symlink(&original.inputs[0].0, &link).unwrap();
        let mut action = original.clone();
        action.inputs[0].0 = link;
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("symlink")
        );
        let dirlink = fixture.0.join("dirlink");
        symlink(&fixture.0, &dirlink).unwrap();
        action.inputs[0].0 = dirlink.join("source");
        assert!(execute(&fixture.cache(), &action).is_err());
        action.inputs[0].0 = fixture.0.clone();
        assert!(execute(&fixture.cache(), &action).is_err());
        action.inputs[0].0 = fixture.0.join("missing");
        assert!(execute(&fixture.cache(), &action).is_err());
        for command in [
            format!("ln -s '{}' out/result", original.inputs[0].0.display()),
            format!("rmdir out; ln -s '{}' out", fixture.0.display()),
            "mkdir out/result".into(),
            "mkfifo out/result".into(),
        ] {
            action = original.clone();
            action.command = command;
            assert!(execute(&fixture.cache(), &action).is_err());
            fixture.assert_unpublished();
        }
    }

    #[test]
    fn controlled_environment_and_absolute_tmpdir() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command = "test \"$PATH\" = /usr/bin:/bin && test \"$LANG\" = C && test \"$LC_ALL\" = C && test -z \"${HOME+x}\" && test -z \"${CARGO_HOME+x}\" && [[ $TMPDIR = /* ]] && test -d \"$TMPDIR\" && printf ok > out/result".into();
        assert!(execute(&fixture.cache(), &action).is_ok());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn spawn_retry_is_bounded_and_only_handles_etxtbsy() {
        let mut calls = 0;
        let mut delays = Vec::new();
        let error = retry_spawn::<()>(
            || {
                calls += 1;
                Err(io::Error::from_raw_os_error(26))
            },
            |delay| delays.push(delay.as_millis()),
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(26));
        assert_eq!(calls, 8);
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 32, 64]);
        assert_eq!(delays.iter().sum::<u128>(), 127);

        calls = 0;
        delays.clear();
        let result = retry_spawn(
            || {
                calls += 1;
                if calls < 3 {
                    Err(io::Error::from_raw_os_error(26))
                } else {
                    Ok(42)
                }
            },
            |delay| delays.push(delay.as_millis()),
        )
        .unwrap();
        assert_eq!(result, 42);
        assert_eq!(calls, 3);
        assert_eq!(delays, vec![1, 2]);

        for errno in [1, 2, 4, 11, 13, 24] {
            calls = 0;
            let error = retry_spawn::<()>(
                || {
                    calls += 1;
                    Err(io::Error::from_raw_os_error(errno))
                },
                |_| panic!("must not back off for errno {errno}"),
            )
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(errno));
            assert_eq!(calls, 1);
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn spawn_retry_recovers_from_real_writable_executable_without_duplicate_execution() {
        let fixture = Fixture::new();
        let action = fixture.direct_action(
            "#!/bin/sh\nprintf 'run\\n' >> \"$1\"\nprintf stdout-message\nprintf stderr-message >&2\n",
        );
        let executable = &action.inputs[0].0;
        let marker = fixture.0.join("ran");
        let mut writer = Some(OpenOptions::new().write(true).open(executable).unwrap());
        let mut stdout = create_log(&fixture.0.join("stdout")).unwrap();
        let mut stderr = create_log(&fixture.0.join("stderr")).unwrap();
        let mut command = Command::new(executable);
        command
            .arg(&marker)
            .stdout(Stdio::from(stdout.try_clone().unwrap()))
            .stderr(Stdio::from(stderr.try_clone().unwrap()));
        let mut attempts = 0;
        let mut waits = 0;
        let mut child = retry_spawn(
            || {
                attempts += 1;
                command.spawn()
            },
            |delay| {
                waits += 1;
                assert!(!marker.exists());
                drop(writer.take());
                std::thread::sleep(delay);
            },
        )
        .unwrap();
        assert!(child.wait().unwrap().success());
        assert!((2..=8).contains(&attempts));
        assert_eq!(waits, attempts - 1);
        assert_eq!(fs::read_to_string(marker).unwrap(), "run\n");
        assert_eq!(log_tail(&mut stdout).unwrap(), "stdout-message");
        assert_eq!(log_tail(&mut stderr).unwrap(), "stderr-message");
    }

    #[test]
    fn direct_nonzero_exit_is_not_a_spawn_error_and_is_never_retried() {
        let fixture = Fixture::new();
        let mut action = fixture.direct_action("#!/bin/sh\nprintf run >> \"$1\"\nexit 26\n");
        let marker = fixture.0.join("ran");
        action.arguments = vec![marker.to_str().unwrap().into()];
        let error = execute(&fixture.cache(), &action).unwrap_err();
        assert!(error.contains("failed (exit status: 26)"), "{error}");
        assert_eq!(fs::read_to_string(marker).unwrap(), "run");
        fixture.assert_unpublished();
    }

    #[test]
    fn concurrent_direct_actions_in_distinct_workdirs_run_once() {
        const WORKERS: usize = 16;
        const ROUNDS: usize = 64;
        let barrier = Arc::new(Barrier::new(WORKERS));
        let mut workers = Vec::new();
        for _ in 0..WORKERS {
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                let fixture = Fixture::new();
                let marker = fixture.0.join("executions");
                let mut action = fixture.direct_action(
                    "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$2\"\nprintf '%s' \"$1\" > out/result\n",
                );
                barrier.wait();
                for round in 0..ROUNDS {
                    // Every round gets a fresh cache key and copied executable.
                    action.arguments = vec![round.to_string(), marker.to_str().unwrap().into()];
                    let first = execute(&fixture.cache(), &action).unwrap();
                    assert!(!first.cache_hit);
                    assert_eq!(
                        fs::read_to_string(&first.outputs[0]).unwrap(),
                        round.to_string()
                    );
                    assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
                }
                let expected: String = (0..ROUNDS).map(|round| format!("{round}\n")).collect();
                assert_eq!(fs::read_to_string(marker).unwrap(), expected);
            }));
        }
        // Join every worker even if one panics; no background test work leaks.
        let results: Vec<_> = workers.into_iter().map(|worker| worker.join()).collect();
        assert!(results.into_iter().all(|result| result.is_ok()));
    }

    #[test]
    fn concurrent_same_key_runs_once() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        let count = fixture.0.join("count");
        action.command = format!(
            "echo run >> '{}'; sleep 0.1; cat src/input > out/result",
            count.display()
        );
        let barrier = Arc::new(Barrier::new(4));
        let mut threads = Vec::new();
        for _ in 0..4 {
            let action = action.clone();
            let cache = fixture.cache();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                execute(&cache, &action).unwrap()
            }));
        }
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| !r.cache_hit).count(), 1);
        assert_eq!(fs::read_to_string(count).unwrap(), "run\n");
        assert!(results.iter().all(|r| r.outputs == results[0].outputs));
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn tree_outputs_are_published_keyed_and_staged_as_tree_inputs() {
        let fixture = Fixture::new();
        let mut producer = fixture.action();
        producer.inputs.clear();
        producer.outputs = vec!["out/tree".into()];
        producer.extra.tree_outputs = BTreeSet::from([PathBuf::from("out/tree")]);
        producer.command =
            "test -d out/tree && mkdir -p out/tree/sub/empty && printf a > out/tree/a && printf b > out/tree/sub/b && chmod 600 out/tree/a".into();
        let first = execute(&fixture.cache(), &producer).unwrap();
        let tree = &first.outputs[0];
        assert_eq!(mode(tree), TREE_ROOT_MODE);
        for (name, expected) in [
            ("a", OUTPUT_MODE),
            ("sub", OUTPUT_MODE),
            ("sub/b", OUTPUT_MODE),
            ("sub/empty", OUTPUT_MODE),
        ] {
            assert_eq!(mode(&tree.join(name)), expected, "{name}");
        }
        assert_eq!(
            first.trees[&0],
            [PathBuf::from("a"), PathBuf::from("sub/b")]
        );
        let hit = execute(&fixture.cache(), &producer).unwrap();
        assert!(hit.cache_hit);
        assert_eq!(hit.trees, first.trees);
        // Tampering with a published tree entry rebuilds it.
        fs::set_permissions(tree.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(tree.join("sub/b"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(tree.join("sub/b"), "tampered").unwrap();
        assert!(!execute(&fixture.cache(), &producer).unwrap().cache_hit);
        assert_eq!(fs::read(tree.join("sub/b")).unwrap(), b"b");
        // A consumer stages the tree; its key covers the tree contents.
        let mut consumer = fixture.action();
        consumer.inputs = vec![(tree.clone(), "in/tree".into())];
        consumer.extra.tree_inputs = BTreeSet::from([PathBuf::from("in/tree")]);
        consumer.command =
            "cat in/tree/a in/tree/sub/b > out/result; test -d in/tree/sub/empty".into();
        let consumed = execute(&fixture.cache(), &consumer).unwrap();
        assert_eq!(fs::read(&consumed.outputs[0]).unwrap(), b"ab");
        producer.command = producer.command.replace("printf b", "printf c");
        let changed = execute(&fixture.cache(), &producer).unwrap();
        consumer.inputs = vec![(changed.outputs[0].clone(), "in/tree".into())];
        let reconsumed = execute(&fixture.cache(), &consumer).unwrap();
        assert!(!reconsumed.cache_hit);
        assert_eq!(fs::read(&reconsumed.outputs[0]).unwrap(), b"ac");
        // Without the tree flag a directory input is still rejected.
        consumer.extra.tree_inputs.clear();
        assert!(
            execute(&fixture.cache(), &consumer)
                .unwrap_err()
                .contains("expected regular file")
        );
        // Removing a cache entry with read-only directories works.
        drop(fixture);
    }

    #[test]
    fn tree_outputs_reject_symlinks_and_missing_directories() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.inputs.clear();
        action.outputs = vec!["out/tree".into()];
        action.extra.tree_outputs = BTreeSet::from([PathBuf::from("out/tree")]);
        action.command = "printf x > out/tree/real && ln -s real out/tree/link".into();
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("symlinks inside tree artifacts are not supported by the native profile")
        );
        action.command = "rmdir out/tree && printf x > out/tree".into();
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("expected directory")
        );
        fixture.assert_unpublished();
    }

    #[test]
    fn param_files_are_written_before_execution_and_keyed_by_content() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.extra.param_files = vec![("out/result-0.params".into(), b"'a b'\n".to_vec())];
        action.command = "cat out/result-0.params > out/result".into();
        let first = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), b"'a b'\n");
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.extra.param_files[0].1 = b"changed\n".to_vec();
        let changed = execute(&fixture.cache(), &action).unwrap();
        assert!(!changed.cache_hit);
        assert_eq!(fs::read(&changed.outputs[0]).unwrap(), b"changed\n");
        action
            .extra
            .param_files
            .push(("src/input".into(), Vec::new()));
        assert!(
            validate_structure(&action)
                .unwrap_err()
                .contains("overlaps")
        );
    }

    #[test]
    fn exact_environment_and_execution_info_are_keyed() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.extra.exact_environment = true;
        action.environment.insert("X".into(), "1".into());
        action.command = "test -z \"${LANG+x}\" && test -z \"${LC_ALL+x}\" && [[ $TMPDIR = /* ]] && printf '%s' \"$X\" > out/result".into();
        let first = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), b"1");
        action
            .extra
            .execution_info
            .insert("no-sandbox".into(), "1".into());
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.extra.exact_environment = false;
        assert!(execute(&fixture.cache(), &action).is_err());
    }

    #[test]
    fn symlink_transform_copies_bytes_and_target_mode_and_checks_executable() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command.clear();
        action.extra.transform = Some(Transform::Symlink {
            target: "src/input".into(),
            require_executable: false,
            progress: "Creating symlink out/result".into(),
        });
        let first = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), b"hello\n");
        assert_eq!(mode(&first.outputs[0]), 0o644);
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.extra.transform = Some(Transform::Symlink {
            target: "src/input".into(),
            require_executable: true,
            progress: "Creating symlink out/result".into(),
        });
        let error = execute(&fixture.cache(), &action).unwrap_err();
        assert!(
            error.contains(
                "failed to create symbolic link 'out/result': file 'src/input' is not executable"
            ),
            "{error}"
        );
        fs::set_permissions(&action.inputs[0].0, fs::Permissions::from_mode(0o755)).unwrap();
        let executable = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(mode(&executable.outputs[0]), 0o755);
        action.extra.transform = Some(Transform::Symlink {
            target: "missing".into(),
            require_executable: false,
            progress: "Creating symlink out/result".into(),
        });
        assert!(
            validate_structure(&action)
                .unwrap_err()
                .contains("target must be an input")
        );
    }

    #[test]
    fn template_transform_replaces_sequentially_and_literally() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        fs::write(&action.inputs[0].0, b"A B $x \xff\n").unwrap();
        action.command.clear();
        action.extra.transform = Some(Transform::Template {
            template: "src/input".into(),
            substitutions: vec![
                (b"A".to_vec(), b"AB".to_vec()),
                (b"B".to_vec(), b"C".to_vec()),
                (Vec::new(), b"never".to_vec()),
                (b"$x".to_vec(), b"$y".to_vec()),
            ],
            executable: true,
        });
        let first = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&first.outputs[0]).unwrap(), b"AC C $y \xff\n");
        assert_eq!(mode(&first.outputs[0]), OUTPUT_MODE);
        assert_eq!(replace_all_literal(b"aaa", b"aa", b"b"), b"ba");
        if let Some(Transform::Template { executable, .. }) = action.extra.transform.as_mut() {
            *executable = false;
        }
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.extra.transform = Some(Transform::Unsupported {
            message: "explicit gap".into(),
        });
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("explicit gap")
        );
    }
}
