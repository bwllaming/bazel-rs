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

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        string(&mut out, 1, &self.hash);
        varint_field(&mut out, 2, self.size);
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
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

fn varint(out: &mut Vec<u8>, mut value: u64) {
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

fn key(out: &mut Vec<u8>, field: u32, wire: u8) {
    varint(out, (u64::from(field) << 3) | u64::from(wire));
}

fn bytes_field(out: &mut Vec<u8>, field: u32, value: &[u8]) {
    key(out, field, 2);
    varint(out, value.len() as u64);
    out.extend_from_slice(value);
}

/// A proto3 singular string/bytes field: omitted when empty.
fn string(out: &mut Vec<u8>, field: u32, value: &str) {
    if !value.is_empty() {
        bytes_field(out, field, value.as_bytes());
    }
}

/// A proto3 singular scalar: omitted when zero.
fn varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
    if value != 0 {
        key(out, field, 0);
        varint(out, value);
    }
}

/// A singular message field: present even when the message is empty.
fn message(out: &mut Vec<u8>, field: u32, value: &[u8]) {
    bytes_field(out, field, value);
}

enum Value<'a> {
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

fn fields(bytes: &[u8]) -> Result<Vec<(u64, Value<'_>)>, String> {
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

fn utf8(bytes: &[u8]) -> Result<String, String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| "invalid UTF-8 in protobuf string".into())
}

/// `Command` for an HTTP cache (RemoteExecutionService.buildCommand,
/// remote/RemoteExecutionService.java:269-332, with API >= 2.1 output_paths
/// and no platform or working directory).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub arguments: Vec<String>,
    /// Sorted by name (`TreeSet<String>`).
    pub environment: BTreeMap<String, String>,
    /// Sorted output paths.
    pub output_paths: Vec<String>,
    /// `Platform` properties, sorted (PlatformUtils.sortPlatformProperties).
    pub platform: Vec<(String, String)>,
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
        let mut command = Command {
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            output_paths: Vec::new(),
            platform: Vec::new(),
        };
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
                (7, Value::Bytes(value)) => command.output_paths.push(utf8(value)?),
                _ => {}
            }
        }
        Ok(command)
    }

    pub fn encode(&self) -> Vec<u8> {
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
        if !self.platform.is_empty() {
            message(&mut out, 5, &platform(&self.platform));
        }
        for path in &self.output_paths {
            bytes_field(&mut out, 7, path.as_bytes());
        }
        out
    }
}

/// `Action` (remote/util/Utils.java:430-453).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub command_digest: Digest,
    pub input_root_digest: Digest,
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

/// One input file of the Merkle tree.
#[derive(Clone, Debug)]
pub struct InputFile {
    pub digest: Digest,
}

/// The input root and every `Directory` blob of a spawn's inputs
/// (remote/merkletree/MerkleTreeComputer): entries sorted by name, files
/// before directories, every file `is_executable` (Bazel marks all inputs
/// executable), no node properties.
pub fn input_root(files: &BTreeMap<String, InputFile>) -> Result<(Digest, Vec<Vec<u8>>), String> {
    #[derive(Default)]
    struct Node<'a> {
        files: BTreeMap<&'a str, &'a InputFile>,
        directories: BTreeMap<&'a str, Node<'a>>,
    }
    let mut root = Node::default();
    for (path, file) in files {
        let mut node = &mut root;
        let mut segments = path.split('/').peekable();
        while let Some(segment) = segments.next() {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(format!("invalid input path '{path}'"));
            }
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
    fn encode(node: &Node<'_>, blobs: &mut Vec<Vec<u8>>) -> Digest {
        let mut out = Vec::new();
        for (name, file) in &node.files {
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
        let digest = Digest::of(&out);
        blobs.push(out);
        digest
    }
    let mut blobs = Vec::new();
    let digest = encode(&root, &mut blobs);
    Ok((digest, blobs))
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

/// `Timestamp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
}

/// The `ActionResult` subset Bazel's HTTP cache writes and reads
/// (remote/UploadManifest.java:106-153).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionResult {
    pub output_files: Vec<OutputFile>,
    /// Paths of outputs that are not plain files (directories, symlinks).
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
                // output_directories, output_file_symlinks,
                // output_directory_symlinks, output_symlinks: path is field 1.
                (3 | 10 | 11 | 12, Value::Bytes(entry)) => {
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
                _ => {}
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Genuine Bazel 9.3.0 blobs from the N41 probe (docs/REMOTE_CACHE.md):
    // the run_shell action of //pkg:sh with input pkg/in.txt ("hello\n").
    #[test]
    fn digests_equal_genuine_bazel() {
        let input = Digest::of(b"hello\n");
        let files = BTreeMap::from([("pkg/in.txt".to_owned(), InputFile { digest: input })]);
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
            platform: Vec::new(),
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
            platform: Vec::new(),
        };
        assert_eq!(Command::decode(&command.encode()).unwrap(), command);
        let digest = Digest::of(&command.encode());
        let action = Action {
            command_digest: digest.clone(),
            input_root_digest: Digest::of(b""),
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
            platform: Vec::new(),
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
        let mut decoded = ActionResult::decode(&bytes).unwrap();
        decoded.execution = result.execution;
        assert_eq!(decoded, result);
        assert!(ActionResult::decode(&[0x0a, 0x05, 0x01]).is_err());
    }
}
