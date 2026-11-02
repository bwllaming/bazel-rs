//! The Remote Execution API v2 messages the HTTP cache exchanges with Bazel
//! 9.3 (N41, docs/REMOTE_CACHE.md), hand-encoded in protobuf's canonical
//! form (fields in number order, proto3 defaults omitted), so that digests
//! equal the ones genuine Bazel computes (`remote/util/DigestUtil.java`:
//! SHA-256 of `message.toByteString()`).
//!
//! Field numbers: `build/bazel/remote/execution/v2/remote_execution.proto`
//! (third_party/remoteapis in the pinned source) and
//! `src/main/protobuf/cache_salt.proto:25-43`.

use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;

/// `Digest`: lowercase hex SHA-256 and size.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest {
    pub hash: String,
    pub size: u64,
}

impl Digest {
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            hash: hex(&Sha256::digest(bytes)),
            size: bytes.len() as u64,
        }
    }

    /// The encoded `Digest` message.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        string(&mut out, 1, &self.hash);
        varint_field(&mut out, 2, self.size);
        out
    }

    /// A decoded `Digest` message.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut digest = Digest {
            hash: String::new(),
            size: 0,
        };
        for field in fields(bytes)? {
            match field {
                (1, Value::Bytes(value)) => digest.hash = utf8(value)?,
                (2, Value::Varint(value)) => digest.size = value,
                _ => {}
            }
        }
        Ok(digest)
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.hash, self.size)
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A protobuf varint.
pub fn varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A field key (number and wire type).
pub fn key(out: &mut Vec<u8>, field: u32, wire: u8) {
    varint(out, (u64::from(field) << 3) | u64::from(wire));
}

/// A length-delimited field, written even when empty.
pub fn bytes_field(out: &mut Vec<u8>, field: u32, value: &[u8]) {
    key(out, field, 2);
    varint(out, value.len() as u64);
    out.extend_from_slice(value);
}

/// A proto3 singular string/bytes field: omitted when empty.
pub fn string(out: &mut Vec<u8>, field: u32, value: &str) {
    if !value.is_empty() {
        bytes_field(out, field, value.as_bytes());
    }
}

/// A proto3 singular scalar: omitted when zero.
pub fn varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
    if value != 0 {
        key(out, field, 0);
        varint(out, value);
    }
}

/// A singular message field: present even when the message is empty.
pub fn message(out: &mut Vec<u8>, field: u32, value: &[u8]) {
    bytes_field(out, field, value);
}

/// A decoded field value (fixed-width values are skipped).
pub enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

fn read_varint(bytes: &[u8], position: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*position).ok_or("truncated protobuf varint")?;
        *position += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("malformed protobuf varint".into())
}

/// The fields of an encoded message, in wire order.
pub fn fields(bytes: &[u8]) -> Result<Vec<(u64, Value<'_>)>, String> {
    let mut position = 0;
    let mut out = Vec::new();
    while position < bytes.len() {
        let tag = read_varint(bytes, &mut position)?;
        let (field, wire) = (tag >> 3, tag & 7);
        if field == 0 {
            return Err("invalid protobuf field number 0".into());
        }
        let value = match wire {
            0 => Value::Varint(read_varint(bytes, &mut position)?),
            1 | 5 => {
                let width = if wire == 1 { 8 } else { 4 };
                if position + width > bytes.len() {
                    return Err("truncated protobuf fixed field".into());
                }
                position += width;
                Value::Fixed
            }
            2 => {
                let length = usize::try_from(read_varint(bytes, &mut position)?)
                    .map_err(|_| "protobuf length overflow")?;
                let end = position
                    .checked_add(length)
                    .filter(|end| *end <= bytes.len())
                    .ok_or("truncated protobuf length-delimited field")?;
                let value = &bytes[position..end];
                position = end;
                Value::Bytes(value)
            }
            _ => return Err(format!("unsupported protobuf wire type {wire}")),
        };
        out.push((field, value));
    }
    Ok(out)
}

/// A protobuf string's text.
pub fn utf8(bytes: &[u8]) -> Result<String, String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| "invalid UTF-8 in protobuf string".into())
}

/// `Command` for an HTTP cache (RemoteExecutionService.buildCommand,
/// remote/RemoteExecutionService.java:269-332, with API >= 2.1 output_paths
/// and no platform or working directory).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Command {
    pub arguments: Vec<String>,
    /// Sorted by name (`TreeSet<String>`).
    pub environment: BTreeMap<String, String>,
    /// Sorted output paths (files and directories).
    pub output_paths: Vec<String>,
    /// The members of `output_paths` that are tree artifacts: REAPI 2.0's
    /// `output_directories` (field 4) when `output_paths` are not used.
    pub output_directories: std::collections::BTreeSet<String>,
    /// `Platform` properties, sorted (PlatformUtils.sortPlatformProperties).
    pub platform: Vec<(String, String)>,
}

/// The properties of an encoded `Platform`.
pub fn decode_platform(bytes: &[u8]) -> Result<Vec<(String, String)>, String> {
    let mut properties = Vec::new();
    for field in fields(bytes)? {
        if let (1, Value::Bytes(property)) = field {
            let (mut name, mut value) = (String::new(), String::new());
            for field in fields(property)? {
                match field {
                    (1, Value::Bytes(text)) => name = utf8(text)?,
                    (2, Value::Bytes(text)) => value = utf8(text)?,
                    _ => {}
                }
            }
            properties.push((name, value));
        }
    }
    Ok(properties)
}

fn platform(properties: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in properties {
        let mut property = Vec::new();
        string(&mut property, 1, name);
        string(&mut property, 2, value);
        message(&mut out, 1, &property);
    }
    out
}

impl Command {
    /// Arguments, environment and output paths of an encoded Command
    /// (other fields are ignored).
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut command = Command::default();
        for field in fields(bytes)? {
            match field {
                (1, Value::Bytes(value)) => command.arguments.push(utf8(value)?),
                (2, Value::Bytes(variable)) => {
                    let (mut name, mut value) = (String::new(), String::new());
                    for field in fields(variable)? {
                        match field {
                            (1, Value::Bytes(text)) => name = utf8(text)?,
                            (2, Value::Bytes(text)) => value = utf8(text)?,
                            _ => {}
                        }
                    }
                    command.environment.insert(name, value);
                }
                (3 | 7, Value::Bytes(value)) => command.output_paths.push(utf8(value)?),
                (4, Value::Bytes(value)) => {
                    let path = utf8(value)?;
                    command.output_directories.insert(path.clone());
                    command.output_paths.push(path);
                }
                (5, Value::Bytes(value)) => command.platform = decode_platform(value)?,
                _ => {}
            }
        }
        Ok(command)
    }

    pub fn encode(&self) -> Vec<u8> {
        self.encode_with(true)
    }

    /// With `output_paths` false, the outputs are REAPI 2.0's
    /// `output_files` (field 3) and `output_directories` (field 4;
    /// RemoteExecutionService.buildCommand for a server whose highest API
    /// version is below 2.1).
    pub fn encode_with(&self, output_paths: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for argument in &self.arguments {
            bytes_field(&mut out, 1, argument.as_bytes());
        }
        for (name, value) in &self.environment {
            let mut variable = Vec::new();
            string(&mut variable, 1, name);
            string(&mut variable, 2, value);
            message(&mut out, 2, &variable);
        }
        if !output_paths {
            for path in &self.output_paths {
                if !self.output_directories.contains(path) {
                    bytes_field(&mut out, 3, path.as_bytes());
                }
            }
            for path in &self.output_paths {
                if self.output_directories.contains(path) {
                    bytes_field(&mut out, 4, path.as_bytes());
                }
            }
        }
        if !self.platform.is_empty() {
            message(&mut out, 5, &platform(&self.platform));
        }
        if output_paths {
            for path in &self.output_paths {
                bytes_field(&mut out, 7, path.as_bytes());
            }
        }
        out
    }
}

/// `Action` (remote/util/Utils.java:430-453).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub command_digest: Digest,
    pub input_root_digest: Digest,
    /// `timeout` in whole seconds (Spawns.getTimeout; `None` for zero).
    pub timeout_seconds: Option<u64>,
    /// `do_not_cache` (`!Spawns.mayBeCachedRemotely`).
    pub do_not_cache: bool,
    /// `CacheSalt.may_be_executed_remotely`
    /// (RemoteExecutionService.buildSalt, :373-391).
    pub may_be_executed_remotely: bool,
    pub platform: Vec<(String, String)>,
}

impl Action {
    /// The command digest of an encoded Action.
    pub fn decode_command_digest(bytes: &[u8]) -> Result<Digest, String> {
        for field in fields(bytes)? {
            if let (1, Value::Bytes(value)) = field {
                return Digest::decode(value);
            }
        }
        Err("Action without a command digest".into())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        message(&mut out, 1, &self.command_digest.encode());
        message(&mut out, 2, &self.input_root_digest.encode());
        if let Some(seconds) = self.timeout_seconds.filter(|seconds| *seconds != 0) {
            let mut duration = Vec::new();
            varint_field(&mut duration, 1, seconds);
            message(&mut out, 6, &duration);
        }
        varint_field(&mut out, 7, u64::from(self.do_not_cache));
        let mut salt = Vec::new();
        varint_field(&mut salt, 1, u64::from(self.may_be_executed_remotely));
        // `bytes salt = 9`: an empty CacheSalt serializes to no bytes.
        if !salt.is_empty() {
            bytes_field(&mut out, 9, &salt);
        }
        if !self.platform.is_empty() {
            message(&mut out, 10, &platform(&self.platform));
        }
        out
    }
}

/// One input of the Merkle tree: a file (`digest`) or, with `symlink`, an
/// unresolved symlink artifact by its link text (a `SymlinkNode`,
/// MerkleTreeComputer: `addSymlinksBuilder().setName(..).setTarget(..)`).
#[derive(Clone, Debug)]
pub struct InputFile {
    pub digest: Digest,
    pub symlink: Option<String>,
}

/// The input root and every `Directory` blob of a spawn's inputs
/// (remote/merkletree/MerkleTreeComputer): entries sorted by name, files
/// before directories, every file `is_executable` (Bazel marks all inputs
/// executable), no node properties; symlinks (`Directory.symlinks`, field
/// 3) after the directories.
pub fn input_root(files: &BTreeMap<String, InputFile>) -> Result<(Digest, Vec<Vec<u8>>), String> {
    let (digest, blobs) = input_tree(files, &std::collections::BTreeSet::new())?;
    Ok((digest, blobs.into_iter().map(|(_, blob)| blob).collect()))
}

/// One encoded `Directory` with its digest.
pub type DirectoryBlob = (Digest, Vec<u8>);

/// [`input_root`] with `directories` that must exist even when nothing is
/// below them: an empty tree artifact or source directory input, and every
/// tree artifact output (MerkleTreeComputer.doBuildForSpawn's
/// `EmptyInputDirectory`, a `DirectoryNode` with the empty digest). Returns
/// every `Directory` blob with its digest, the root's last.
pub fn input_tree(
    files: &BTreeMap<String, InputFile>,
    directories: &std::collections::BTreeSet<String>,
) -> Result<(Digest, Vec<DirectoryBlob>), String> {
    #[derive(Default)]
    struct Node<'a> {
        files: BTreeMap<&'a str, &'a InputFile>,
        directories: BTreeMap<&'a str, Node<'a>>,
    }
    fn segments(path: &str) -> Result<Vec<&str>, String> {
        let segments = path.split('/').collect::<Vec<_>>();
        if segments
            .iter()
            .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
        {
            return Err(format!("invalid input path '{path}'"));
        }
        Ok(segments)
    }
    let mut root = Node::default();
    for (path, file) in files {
        let mut node = &mut root;
        let mut segments = segments(path)?.into_iter().peekable();
        while let Some(segment) = segments.next() {
            if segments.peek().is_none() {
                if node.directories.contains_key(segment)
                    || node.files.insert(segment, file).is_some()
                {
                    return Err(format!("conflicting input path '{path}'"));
                }
            } else {
                if node.files.contains_key(segment) {
                    return Err(format!("conflicting input path '{path}'"));
                }
                node = node.directories.entry(segment).or_default();
            }
        }
    }
    for path in directories {
        let mut node = &mut root;
        for segment in segments(path)? {
            if node.files.contains_key(segment) {
                return Err(format!("conflicting input path '{path}'"));
            }
            node = node.directories.entry(segment).or_default();
        }
    }
    fn encode(node: &Node<'_>, blobs: &mut Vec<DirectoryBlob>) -> Digest {
        let mut out = Vec::new();
        for (name, file) in node.files.iter().filter(|(_, file)| file.symlink.is_none()) {
            let mut entry = Vec::new();
            string(&mut entry, 1, name);
            message(&mut entry, 2, &file.digest.encode());
            varint_field(&mut entry, 4, 1);
            message(&mut out, 1, &entry);
        }
        for (name, directory) in &node.directories {
            let digest = encode(directory, blobs);
            let mut entry = Vec::new();
            string(&mut entry, 1, name);
            message(&mut entry, 2, &digest.encode());
            message(&mut out, 2, &entry);
        }
        for (name, target) in node
            .files
            .iter()
            .filter_map(|(name, file)| Some((name, file.symlink.as_ref()?)))
        {
            let mut entry = Vec::new();
            string(&mut entry, 1, name);
            string(&mut entry, 2, target);
            message(&mut out, 3, &entry);
        }
        let digest = Digest::of(&out);
        blobs.push((digest.clone(), out));
        digest
    }
    let mut blobs = Vec::new();
    let digest = encode(&root, &mut blobs);
    Ok((digest, blobs))
}

/// A decoded `Directory`: files `(name, digest, is_executable)`,
/// subdirectories `(name, digest)` and symlinks `(name, target)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Directory {
    pub files: Vec<(String, Digest, bool)>,
    pub directories: Vec<(String, Digest)>,
    pub symlinks: Vec<(String, String)>,
}

impl Directory {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut directory = Directory::default();
        for field in fields(bytes)? {
            let (number, Value::Bytes(entry)) = field else {
                continue;
            };
            let (mut name, mut digest, mut executable, mut target) =
                (String::new(), None, false, String::new());
            for field in fields(entry)? {
                match field {
                    (1, Value::Bytes(value)) => name = utf8(value)?,
                    (2, Value::Bytes(value)) if number == 3 => target = utf8(value)?,
                    (2, Value::Bytes(value)) => digest = Some(Digest::decode(value)?),
                    (4, Value::Varint(value)) if number == 1 => executable = value != 0,
                    _ => {}
                }
            }
            let digest = || {
                digest
                    .clone()
                    .ok_or_else(|| format!("Directory entry '{name}' without a digest"))
            };
            match number {
                1 => directory.files.push((name.clone(), digest()?, executable)),
                2 => directory.directories.push((name.clone(), digest()?)),
                3 => directory.symlinks.push((name, target)),
                _ => {}
            }
        }
        Ok(directory)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, digest, executable) in &self.files {
            let mut entry = Vec::new();
            string(&mut entry, 1, name);
            message(&mut entry, 2, &digest.encode());
            varint_field(&mut entry, 4, u64::from(*executable));
            message(&mut out, 1, &entry);
        }
        for (name, digest) in &self.directories {
            let mut entry = Vec::new();
            string(&mut entry, 1, name);
            message(&mut entry, 2, &digest.encode());
            message(&mut out, 2, &entry);
        }
        for (name, target) in &self.symlinks {
            let mut entry = Vec::new();
            string(&mut entry, 1, name);
            string(&mut entry, 2, target);
            message(&mut out, 3, &entry);
        }
        out
    }
}

/// A decoded `Tree` (an output directory): the root and every
/// descendant `Directory`, by digest.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub root: Directory,
    pub children: BTreeMap<Digest, Directory>,
}

impl Tree {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut tree = Tree::default();
        for field in fields(bytes)? {
            match field {
                (1, Value::Bytes(root)) => tree.root = Directory::decode(root)?,
                (2, Value::Bytes(child)) => {
                    tree.children
                        .insert(Digest::of(child), Directory::decode(child)?);
                }
                _ => {}
            }
        }
        Ok(tree)
    }

    /// Every file and symlink below the root by relative path, and every
    /// directory (empty ones included).
    pub fn entries(&self) -> Result<TreeEntries, String> {
        let mut entries = TreeEntries::default();
        fn walk(
            tree: &Tree,
            directory: &Directory,
            prefix: &str,
            depth: usize,
            entries: &mut TreeEntries,
        ) -> Result<(), String> {
            if depth > 4096 {
                return Err("output directory Tree is too deep".into());
            }
            let join = |name: &str| -> Result<String, String> {
                if name.is_empty() || name == "." || name == ".." || name.contains('/') {
                    return Err(format!("invalid output directory entry name '{name}'"));
                }
                Ok(if prefix.is_empty() {
                    name.to_owned()
                } else {
                    format!("{prefix}/{name}")
                })
            };
            for (name, digest, executable) in &directory.files {
                entries
                    .files
                    .push((join(name)?, digest.clone(), *executable));
            }
            for (name, target) in &directory.symlinks {
                entries.symlinks.push((join(name)?, target.clone()));
            }
            for (name, digest) in &directory.directories {
                let path = join(name)?;
                let child = tree.children.get(digest).ok_or_else(|| {
                    format!("output directory Tree lacks the Directory {digest} of '{path}'")
                })?;
                entries.directories.push(path.clone());
                walk(tree, child, &path, depth + 1, entries)?;
            }
            Ok(())
        }
        walk(self, &self.root, "", 0, &mut entries)?;
        Ok(entries)
    }
}

/// The flattened contents of a [`Tree`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeEntries {
    pub files: Vec<(String, Digest, bool)>,
    pub symlinks: Vec<(String, String)>,
    pub directories: Vec<String>,
}

/// `ActionResult.output_directories` entry (path, tree_digest,
/// is_topologically_sorted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputDirectory {
    pub path: String,
    pub tree_digest: Digest,
    /// The `Tree` lists parents before children ([`output_tree`]'s order).
    pub is_topologically_sorted: bool,
}

/// UploadManifest.DirectoryBuilder.build (remote/UploadManifest.java:
/// 416-468): the `Tree` of an output directory from its regular files
/// (`files`: path below the tree and digest; every FileNode
/// `is_executable`, `--incompatible_remote_preserve_executable_bit` being
/// off), symlinks (path, link text) and directories (every one, empty
/// ones included). Each Directory lists its entries sorted by name; the
/// Directory messages are computed children first (paths in reverse
/// order), deduplicated keeping the first, and written parents first —
/// the root, then the children (topologically sorted).
pub fn output_tree(
    files: &[(String, Digest)],
    symlinks: &[(String, String)],
    directories: &[String],
) -> Vec<u8> {
    #[derive(Default)]
    struct Node {
        files: BTreeMap<String, Digest>,
        symlinks: BTreeMap<String, String>,
        directories: std::collections::BTreeSet<String>,
    }
    fn split(path: &str) -> (&str, &str) {
        path.rsplit_once('/').unwrap_or(("", path))
    }
    fn ensure<'a>(nodes: &'a mut BTreeMap<String, Node>, path: &str) -> &'a mut Node {
        if !nodes.contains_key(path) {
            nodes.insert(path.to_owned(), Node::default());
            if !path.is_empty() {
                let (parent, name) = split(path);
                ensure(nodes, parent).directories.insert(name.to_owned());
            }
        }
        nodes.get_mut(path).expect("inserted above")
    }
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
    ensure(&mut nodes, "");
    for directory in directories {
        ensure(&mut nodes, directory);
    }
    for (path, digest) in files {
        let (parent, name) = split(path);
        ensure(&mut nodes, parent)
            .files
            .insert(name.to_owned(), digest.clone());
    }
    for (path, target) in symlinks {
        let (parent, name) = split(path);
        ensure(&mut nodes, parent)
            .symlinks
            .insert(name.to_owned(), target.clone());
    }
    let mut digests: BTreeMap<&str, Digest> = BTreeMap::new();
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (path, node) in nodes.iter().rev() {
        let child = |name: &str| {
            if path.is_empty() {
                name.to_owned()
            } else {
                format!("{path}/{name}")
            }
        };
        let blob = Directory {
            files: node
                .files
                .iter()
                .map(|(name, digest)| (name.clone(), digest.clone(), true))
                .collect(),
            directories: node
                .directories
                .iter()
                .map(|name| (name.clone(), digests[child(name).as_str()].clone()))
                .collect(),
            symlinks: node
                .symlinks
                .iter()
                .map(|(name, target)| (name.clone(), target.clone()))
                .collect(),
        }
        .encode();
        digests.insert(path, Digest::of(&blob));
        if seen.insert(blob.clone()) {
            blobs.push(blob);
        }
    }
    let mut out = Vec::new();
    for (index, blob) in blobs.iter().rev().enumerate() {
        message(&mut out, if index == 0 { 1 } else { 2 }, blob);
    }
    out
}

/// `ActionResult.output_files` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputFile {
    pub path: String,
    pub digest: Digest,
    pub is_executable: bool,
    /// Inlined contents, if any.
    pub contents: Vec<u8>,
}

/// `ActionResult.output_symlinks` entry (OutputSymlink: path, target).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputSymlink {
    pub path: String,
    pub target: String,
}

/// `Timestamp`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timestamp {
    pub seconds: i64,
    pub nanos: i32,
}

impl Timestamp {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        varint_field(&mut out, 1, self.seconds as u64);
        varint_field(&mut out, 2, self.nanos as u32 as u64);
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut timestamp = Timestamp::default();
        for field in fields(bytes)? {
            match field {
                (1, Value::Varint(value)) => timestamp.seconds = value as i64,
                (2, Value::Varint(value)) => timestamp.nanos = value as i32,
                _ => {}
            }
        }
        Ok(timestamp)
    }
}

/// The `ActionResult` subset Bazel's HTTP cache writes and reads
/// (remote/UploadManifest.java:106-153).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionResult {
    pub output_files: Vec<OutputFile>,
    /// Symlink outputs (UploadManifest.addFileSymbolicLink writes each to
    /// `output_file_symlinks` and `output_symlinks`; a reader takes both).
    pub output_symlinks: Vec<OutputSymlink>,
    /// `output_directories` (field 3).
    pub output_directories: Vec<OutputDirectory>,
    /// Paths of `output_directory_symlinks` (field 11).
    pub other_outputs: Vec<String>,
    pub exit_code: i32,
    pub stdout_raw: Vec<u8>,
    pub stdout_digest: Option<Digest>,
    pub stderr_raw: Vec<u8>,
    pub stderr_digest: Option<Digest>,
    /// worker/execution start and completion (ExecutedActionMetadata 3, 4, 7, 8).
    pub execution: Option<(Timestamp, Timestamp)>,
}

impl ActionResult {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for file in &self.output_files {
            let mut entry = Vec::new();
            string(&mut entry, 1, &file.path);
            message(&mut entry, 2, &file.digest.encode());
            varint_field(&mut entry, 4, u64::from(file.is_executable));
            if !file.contents.is_empty() {
                bytes_field(&mut entry, 5, &file.contents);
            }
            message(&mut out, 2, &entry);
        }
        for directory in &self.output_directories {
            let mut entry = Vec::new();
            string(&mut entry, 1, &directory.path);
            message(&mut entry, 3, &directory.tree_digest.encode());
            varint_field(&mut entry, 4, u64::from(directory.is_topologically_sorted));
            message(&mut out, 3, &entry);
        }
        varint_field(&mut out, 4, i64::from(self.exit_code) as u64);
        if !self.stdout_raw.is_empty() {
            bytes_field(&mut out, 5, &self.stdout_raw);
        }
        if let Some(digest) = &self.stdout_digest {
            message(&mut out, 6, &digest.encode());
        }
        if !self.stderr_raw.is_empty() {
            bytes_field(&mut out, 7, &self.stderr_raw);
        }
        if let Some(digest) = &self.stderr_digest {
            message(&mut out, 8, &digest.encode());
        }
        if let Some((start, end)) = &self.execution {
            let mut metadata = Vec::new();
            message(&mut metadata, 3, &start.encode());
            message(&mut metadata, 4, &end.encode());
            message(&mut metadata, 7, &start.encode());
            message(&mut metadata, 8, &end.encode());
            message(&mut out, 9, &metadata);
        }
        for field in [10, 12] {
            for symlink in &self.output_symlinks {
                let mut entry = Vec::new();
                string(&mut entry, 1, &symlink.path);
                string(&mut entry, 2, &symlink.target);
                message(&mut out, field, &entry);
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut result = ActionResult::default();
        for field in fields(bytes)? {
            match field {
                (2, Value::Bytes(entry)) => {
                    let mut file = OutputFile {
                        path: String::new(),
                        digest: Digest {
                            hash: String::new(),
                            size: 0,
                        },
                        is_executable: false,
                        contents: Vec::new(),
                    };
                    for field in fields(entry)? {
                        match field {
                            (1, Value::Bytes(value)) => file.path = utf8(value)?,
                            (2, Value::Bytes(value)) => file.digest = Digest::decode(value)?,
                            (4, Value::Varint(value)) => file.is_executable = value != 0,
                            (5, Value::Bytes(value)) => file.contents = value.to_vec(),
                            _ => {}
                        }
                    }
                    result.output_files.push(file);
                }
                // output_file_symlinks, output_symlinks
                // (RemoteExecutionService.parseActionResultMetadata takes
                // both, one entry per path).
                (10 | 12, Value::Bytes(entry)) => {
                    let mut symlink = OutputSymlink {
                        path: String::new(),
                        target: String::new(),
                    };
                    for field in fields(entry)? {
                        match field {
                            (1, Value::Bytes(value)) => symlink.path = utf8(value)?,
                            (2, Value::Bytes(value)) => symlink.target = utf8(value)?,
                            _ => {}
                        }
                    }
                    match result
                        .output_symlinks
                        .iter()
                        .find(|known| known.path == symlink.path)
                    {
                        Some(known) if known.target != symlink.target => {
                            return Err(format!(
                                "Symlink path collision: '{}' is mapped to both '{}' and '{}'",
                                symlink.path, known.target, symlink.target
                            ));
                        }
                        Some(_) => {}
                        None => result.output_symlinks.push(symlink),
                    }
                }
                (3, Value::Bytes(entry)) => {
                    let mut directory = OutputDirectory {
                        path: String::new(),
                        tree_digest: Digest::of(b""),
                        is_topologically_sorted: false,
                    };
                    for field in fields(entry)? {
                        match field {
                            (1, Value::Bytes(value)) => directory.path = utf8(value)?,
                            (3, Value::Bytes(value)) => {
                                directory.tree_digest = Digest::decode(value)?
                            }
                            (4, Value::Varint(value)) => {
                                directory.is_topologically_sorted = value != 0
                            }
                            _ => {}
                        }
                    }
                    result.output_directories.push(directory);
                }
                // output_directory_symlinks: path is field 1.
                (11, Value::Bytes(entry)) => {
                    let mut path = String::new();
                    for field in fields(entry)? {
                        if let (1, Value::Bytes(value)) = field {
                            path = utf8(value)?;
                        }
                    }
                    result.other_outputs.push(path);
                }
                (4, Value::Varint(value)) => result.exit_code = value as i32,
                (5, Value::Bytes(value)) => result.stdout_raw = value.to_vec(),
                (6, Value::Bytes(value)) => result.stdout_digest = Some(Digest::decode(value)?),
                (7, Value::Bytes(value)) => result.stderr_raw = value.to_vec(),
                (8, Value::Bytes(value)) => result.stderr_digest = Some(Digest::decode(value)?),
                // ExecutedActionMetadata: the execution start and completion
                // (fields 7 and 8), Utils.createSpawnResult's wall time.
                (9, Value::Bytes(metadata)) => {
                    let (mut start, mut end) = (None, None);
                    for field in fields(metadata)? {
                        match field {
                            (7, Value::Bytes(value)) => start = Some(Timestamp::decode(value)?),
                            (8, Value::Bytes(value)) => end = Some(Timestamp::decode(value)?),
                            _ => {}
                        }
                    }
                    result.execution = start.zip(end);
                }
                _ => {}
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Genuine Bazel 9.3.0's ActionResult of a spawn whose declared symlink
    /// output `pkg/s` is `ln -s ../does/not/exist` (tests/remote_cache.rs
    /// `unresolved_symlinks_enter_the_remote_cache`), without its execution
    /// metadata (field 9, timestamps): UploadManifest.addFileSymbolicLink
    /// writes the symlink to output_file_symlinks (10) and output_symlinks
    /// (12).
    #[test]
    fn output_symlinks_equal_genuine_bazel() {
        let genuine = "32420a406533623063343432393866633163313439616662663463383939366662393234323761653431653436343962393334636134393539393162373835326238353542420a406533623063343432393866633163313439616662663463383939366662393234323761653431653436343962393334636134393539393162373835326238353552350a2062617a656c2d6f75742f6b382d666173746275696c642f62696e2f706b672f7312112e2e2f646f65732f6e6f742f657869737462350a2062617a656c2d6f75742f6b382d666173746275696c642f62696e2f706b672f7312112e2e2f646f65732f6e6f742f6578697374";
        let bytes = (0..genuine.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&genuine[at..at + 2], 16).unwrap())
            .collect::<Vec<_>>();
        let decoded = ActionResult::decode(&bytes).unwrap();
        assert_eq!(
            decoded.output_symlinks,
            [OutputSymlink {
                path: "bazel-out/k8-fastbuild/bin/pkg/s".into(),
                target: "../does/not/exist".into(),
            }]
        );
        assert!(decoded.output_files.is_empty() && decoded.other_outputs.is_empty());
        assert_eq!(decoded.encode(), bytes);
        // One path with two targets is Bazel's collision error.
        let mut collision = decoded.clone();
        collision.output_symlinks[0].target = "other".into();
        let mut both = bytes.clone();
        both.extend(&collision.encode()[136..]);
        assert!(
            ActionResult::decode(&both)
                .unwrap_err()
                .contains("Symlink path collision")
        );
    }

    // Genuine Bazel 9.3.0 blobs from the N41 probe (docs/REMOTE_CACHE.md):
    // the run_shell action of //pkg:sh with input pkg/in.txt ("hello\n").
    #[test]
    fn digests_equal_genuine_bazel() {
        let input = Digest::of(b"hello\n");
        let files = BTreeMap::from([(
            "pkg/in.txt".to_owned(),
            InputFile {
                digest: input,
                symlink: None,
            },
        )]);
        let (root, blobs) = input_root(&files).unwrap();
        assert_eq!(
            root,
            Digest {
                hash: "7b28be45fb7bee9ecee43927ae9231f95d71a7a7ecb2c4a2aea7948f3af5f890".into(),
                size: 77
            }
        );
        assert_eq!(blobs.len(), 2);
        let command = Command {
            arguments: [
                "/bin/bash",
                "-c",
                "tr a-z A-Z < $1 > $2",
                "",
                "pkg/in.txt",
                "bazel-out/k8-fastbuild/bin/pkg/sh.out",
            ]
            .map(str::to_owned)
            .to_vec(),
            environment: BTreeMap::new(),
            output_paths: vec!["bazel-out/k8-fastbuild/bin/pkg/sh.out".into()],
            ..Command::default()
        };
        let command_digest = Digest::of(&command.encode());
        assert_eq!(
            command_digest,
            Digest {
                hash: "a01e81fb05f23fc6128c71b2c090e51bfd5ee84e4c82c8ceeac31f3b0cc5df22".into(),
                size: 129
            }
        );
        let action = Action {
            command_digest,
            input_root_digest: root,
            timeout_seconds: None,
            do_not_cache: false,
            may_be_executed_remotely: true,
            platform: Vec::new(),
        };
        assert_eq!(
            Digest::of(&action.encode()),
            Digest {
                hash: "215a0cea76512319330fd7d9ff08cee13aaca1f345089f37d42323c2d69be62e".into(),
                size: 145
            }
        );
    }

    #[test]
    fn commands_and_actions_decode() {
        let command = Command {
            arguments: vec!["a".into(), String::new()],
            environment: BTreeMap::from([("K".to_owned(), "v".to_owned())]),
            output_paths: vec!["o".into()],
            ..Command::default()
        };
        assert_eq!(Command::decode(&command.encode()).unwrap(), command);
        let digest = Digest::of(&command.encode());
        let action = Action {
            command_digest: digest.clone(),
            input_root_digest: Digest::of(b""),
            timeout_seconds: None,
            do_not_cache: false,
            may_be_executed_remotely: true,
            platform: Vec::new(),
        };
        assert_eq!(
            Action::decode_command_digest(&action.encode()).unwrap(),
            digest
        );
    }

    #[test]
    fn genrule_command_digest_equals_genuine_bazel() {
        let command = Command {
            arguments: vec![
                "/bin/bash".into(),
                "-c".into(),
                "source external/bazel_tools/tools/genrule/genrule-setup.sh; cat pkg/in.txt > bazel-out/k8-fastbuild/bin/pkg/gen.txt; echo done >> bazel-out/k8-fastbuild/bin/pkg/gen.txt".into(),
            ],
            environment: BTreeMap::from([(
                "PATH".to_owned(),
                "/bin:/usr/bin:/usr/local/bin".to_owned(),
            )]),
            output_paths: vec!["bazel-out/k8-fastbuild/bin/pkg/gen.txt".into()],
            ..Command::default()
        };
        assert_eq!(
            Digest::of(&command.encode()).hash,
            "b0163f748153226032b0932f75fbbf56739b70d54624c4bee53b1427f3fe222a"
        );
    }

    #[test]
    fn action_results_round_trip_and_skip_unknown_fields() {
        let result = ActionResult {
            output_files: vec![OutputFile {
                path: "bazel-out/k8-fastbuild/bin/pkg/gen.txt".into(),
                digest: Digest::of(b"hello\ndone\n"),
                is_executable: true,
                contents: Vec::new(),
            }],
            stdout_digest: Some(Digest::of(b"")),
            stderr_digest: Some(Digest::of(b"")),
            execution: Some((
                Timestamp {
                    seconds: 1_792_199_058,
                    nanos: 159_613_792,
                },
                Timestamp {
                    seconds: 1_792_199_058,
                    nanos: 165_613_792,
                },
            )),
            ..ActionResult::default()
        };
        let bytes = result.encode();
        // The genuine entry of //pkg:gen is 304 bytes with these fields.
        assert_eq!(bytes.len(), 304);
        assert_eq!(ActionResult::decode(&bytes).unwrap(), result);
        assert!(ActionResult::decode(&[0x0a, 0x05, 0x01]).is_err());
    }
}
