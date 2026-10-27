// Shared by integration tests (`include!("support/grpc_cache_server.rs")`):
// a minimal in-process REAPI v2 gRPC cache on loopback (or a Unix socket,
// optionally TLS/mTLS) with an in-memory CAS and Action Cache, a request
// log and failure injection. Speaks gRPC over h2 directly and decodes the
// messages with bazel_rs::reapi's protobuf primitives.
//
// Services: Capabilities.GetCapabilities; ActionCache.GetActionResult and
// UpdateActionResult; ContentAddressableStorage.FindMissingBlobs,
// BatchUpdateBlobs and BatchReadBlobs; ByteStream Read (read_offset),
// Write (write_offset, resumable uploads) and QueryWriteStatus; and
// Execution.Execute/WaitExecution (`Config::execution`): the Action's
// Command runs locally in a fresh directory holding its input root (every
// blob must be in the CAS, else FAILED_PRECONDITION with MISSING
// violations, like a real executor), its outputs, stdout and stderr go to
// the CAS and a successful cacheable result to the Action Cache. Scripted
// `ExecStep`s inject stream breaks, operation errors and response
// statuses.

#[allow(dead_code)]
mod grpc_cache_server {
    use bazel_rs::reapi::{self, Digest, Value};
    use bytes::Bytes;
    use std::collections::{BTreeMap, VecDeque};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// One logged call: the method (`Service/Method`), its metadata and a
    /// short description of the request.
    #[derive(Clone, Debug)]
    pub struct Logged {
        pub method: String,
        pub metadata: Vec<(String, String)>,
        pub detail: String,
    }

    impl Logged {
        pub fn header(&self, name: &str) -> Vec<&str> {
            self.metadata
                .iter()
                .filter(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .collect()
        }
    }

    /// Server behaviour.
    #[derive(Clone, Debug)]
    pub struct Config {
        pub digest_functions: Vec<u64>,
        pub update_enabled: bool,
        pub low_api: (u64, u64),
        pub high_api: (u64, u64),
        /// ByteStream.Read response chunk size.
        pub read_chunk: usize,
        /// Fail the next Read with UNAVAILABLE after this many bytes.
        pub fail_read_after: Option<usize>,
        /// Fail the next Write with UNAVAILABLE after committing this many bytes.
        pub fail_write_after: Option<usize>,
        pub query_write_status: bool,
        /// Answer every call of this method (`Service/Method`) with a status.
        pub fail_method: Option<(String, u32, String)>,
        /// Advertise and serve the Execution service.
        pub execution: bool,
        /// ExecutionCapabilities.digest_functions.
        pub exec_digest_functions: Vec<u64>,
        /// One step per Execute/WaitExecution call, then `ExecStep::Run`.
        pub exec_script: VecDeque<ExecStep>,
        /// CacheCapabilities.supported_compressors (1: ZSTD); with ZSTD the
        /// ByteStream serves `compressed-blobs/zstd/` resources.
        pub compressors: Vec<u64>,
    }

    /// How the server answers one Execute or WaitExecution call.
    #[derive(Clone, Debug)]
    pub enum ExecStep {
        /// Run the action (or answer from the Action Cache).
        Run,
        /// Send an EXECUTING operation, then end the stream with this
        /// status (0: OK); the operation finishes for WaitExecution.
        Break(u32),
        /// Fail the call with this status before any operation.
        Call(u32, String),
        /// A done operation with this `error` status.
        OperationError(u32, String),
        /// A done operation whose ExecuteResponse has this status, the
        /// encoded `google.rpc.Status` details and a `message`.
        Response {
            code: u32,
            message: String,
            details: Vec<(String, Vec<u8>)>,
            response_message: String,
        },
        /// Answer WaitExecution with NOT_FOUND (the operation was lost).
        Lost,
    }

    /// One executed (or answered) Execute request.
    #[derive(Clone, Debug)]
    pub struct Executed {
        pub instance: String,
        pub action_digest: Digest,
        pub skip_cache_lookup: bool,
        pub action: Vec<u8>,
        pub command: reapi::Command,
        /// The input root's files (path -> digest and executable bit),
        /// symlinks (path -> target) and directories.
        pub files: BTreeMap<String, (Digest, bool)>,
        pub symlinks: BTreeMap<String, String>,
        pub directories: Vec<String>,
        pub cached: bool,
    }

    impl Default for Config {
        fn default() -> Self {
            Self {
                digest_functions: vec![1],
                update_enabled: true,
                low_api: (2, 0),
                high_api: (2, 3),
                read_chunk: 64 * 1024,
                fail_read_after: None,
                fail_write_after: None,
                query_write_status: true,
                fail_method: None,
                execution: false,
                exec_digest_functions: vec![1],
                exec_script: VecDeque::new(),
                compressors: Vec::new(),
            }
        }
    }

    #[derive(Default)]
    pub struct State {
        /// `instance|hash` -> ActionResult.
        pub ac: BTreeMap<String, Vec<u8>>,
        /// `instance|hash` -> blob.
        pub cas: BTreeMap<String, Vec<u8>>,
        /// Upload resource -> bytes committed so far.
        pub uploads: BTreeMap<String, Vec<u8>>,
        pub log: Vec<Logged>,
        pub config: Config,
        /// The largest `data` of one WriteRequest.
        pub max_write_chunk: usize,
        /// Execute requests in arrival order.
        pub executed: Vec<Executed>,
        /// Finished operations by name (for WaitExecution).
        pub operations: BTreeMap<String, Vec<u8>>,
        /// Operations waiting for WaitExecution: name -> request.
        pub pending: BTreeMap<String, Vec<u8>>,
        pub next_operation: usize,
    }

    pub struct Tls {
        pub certificate: PathBuf,
        pub key: PathBuf,
        /// Require client certificates signed by this CA.
        pub client_ca: Option<PathBuf>,
    }

    pub struct Server {
        pub port: u16,
        pub socket: Option<PathBuf>,
        pub state: Arc<Mutex<State>>,
        _runtime: tokio::runtime::Runtime,
    }

    /// The canonical test temporary parent (support/temp_root.rs).
    pub fn fs_canonical_tmp() -> PathBuf {
        crate::test_temp::root()
    }

    fn status(code: u32, message: &str) -> (u32, String) {
        (code, message.to_owned())
    }

    fn string_field(bytes: &[u8], number: u64) -> String {
        reapi::fields(bytes)
            .unwrap()
            .into_iter()
            .find_map(|field| match field {
                (n, Value::Bytes(value)) if n == number => Some(reapi::utf8(value).unwrap()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn varint(bytes: &[u8], number: u64) -> u64 {
        reapi::fields(bytes)
            .unwrap()
            .into_iter()
            .find_map(|field| match field {
                (n, Value::Varint(value)) if n == number => Some(value),
                _ => None,
            })
            .unwrap_or(0)
    }

    fn message_field(bytes: &[u8], number: u64) -> Vec<Vec<u8>> {
        reapi::fields(bytes)
            .unwrap()
            .into_iter()
            .filter_map(|field| match field {
                (n, Value::Bytes(value)) if n == number => Some(value.to_vec()),
                _ => None,
            })
            .collect()
    }

    fn frame(message: &[u8]) -> Bytes {
        let mut out = Vec::with_capacity(message.len() + 5);
        out.push(0);
        out.extend_from_slice(&(message.len() as u32).to_be_bytes());
        out.extend_from_slice(message);
        Bytes::from(out)
    }

    fn messages(body: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut position = 0;
        while position + 5 <= body.len() {
            let length =
                u32::from_be_bytes(body[position + 1..position + 5].try_into().unwrap()) as usize;
            out.push(body[position + 5..position + 5 + length].to_vec());
            position += 5 + length;
        }
        out
    }

    /// `[INSTANCE/]blobs/HASH/SIZE` or `[INSTANCE/]uploads/UUID/blobs/HASH/SIZE`.
    fn parse_resource(resource: &str) -> (String, String, u64) {
        let (instance, hash, size, _) = parse_resource_compressed(resource);
        (instance, hash, size)
    }

    /// [`parse_resource`] that also accepts `compressed-blobs/zstd/HASH/SIZE`
    /// (and whether the resource is compressed).
    fn parse_resource_compressed(resource: &str) -> (String, String, u64, bool) {
        let parts = resource.split('/').collect::<Vec<_>>();
        let (blobs, compressed) = match parts.iter().position(|part| *part == "blobs") {
            Some(blobs) => (blobs, false),
            None => (
                parts
                    .iter()
                    .position(|part| *part == "compressed-blobs")
                    .unwrap()
                    + 1,
                true,
            ),
        };
        let instance_end = parts
            .iter()
            .position(|part| *part == "uploads" || *part == "blobs" || *part == "compressed-blobs")
            .unwrap();
        (
            parts[..instance_end].join("/"),
            parts[blobs + 1].to_owned(),
            parts[blobs + 2].parse().unwrap(),
            compressed,
        )
    }

    fn semver(major: u64, minor: u64) -> Vec<u8> {
        let mut out = Vec::new();
        reapi::varint_field(&mut out, 1, major);
        reapi::varint_field(&mut out, 2, minor);
        out
    }

    /// The response messages and final status of one call.
    fn dispatch(
        state: &Mutex<State>,
        method: &str,
        metadata: Vec<(String, String)>,
        requests: Vec<Vec<u8>>,
    ) -> (Vec<Vec<u8>>, (u32, String)) {
        let mut state = state.lock().unwrap();
        let request = requests.first().cloned().unwrap_or_default();
        let detail = match method {
            "google.bytestream.ByteStream/Read" => format!(
                "{} offset={}",
                string_field(&request, 1),
                varint(&request, 2)
            ),
            "google.bytestream.ByteStream/Write" => format!(
                "{} offset={} requests={}",
                string_field(&request, 1),
                varint(&request, 2),
                requests.len()
            ),
            "google.bytestream.ByteStream/QueryWriteStatus" => string_field(&request, 1),
            "build.bazel.remote.execution.v2.ContentAddressableStorage/FindMissingBlobs" => {
                format!(
                    "instance={} digests={}",
                    string_field(&request, 1),
                    message_field(&request, 2).len()
                )
            }
            _ => format!(
                "instance={} key={}",
                string_field(&request, 1),
                message_field(&request, 2)
                    .first()
                    .map(|digest| Digest::decode(digest).unwrap().hash)
                    .unwrap_or_default()
            ),
        };
        state.log.push(Logged {
            method: method.to_owned(),
            metadata,
            detail,
        });
        if let Some((failing, code, message)) = &state.config.fail_method
            && failing == method
        {
            return (Vec::new(), (*code, message.clone()));
        }
        let ok = || status(0, "");
        match method {
            "build.bazel.remote.execution.v2.Capabilities/GetCapabilities" => {
                let config = &state.config;
                let mut cache = Vec::new();
                let mut packed = Vec::new();
                for value in &config.digest_functions {
                    reapi::varint(&mut packed, *value);
                }
                reapi::bytes_field(&mut cache, 1, &packed);
                let mut update = Vec::new();
                reapi::varint_field(&mut update, 1, u64::from(config.update_enabled));
                reapi::message(&mut cache, 2, &update);
                reapi::varint_field(&mut cache, 4, 4 << 20);
                let mut packed = Vec::new();
                for value in &config.compressors {
                    reapi::varint(&mut packed, *value);
                }
                if !packed.is_empty() {
                    reapi::bytes_field(&mut cache, 6, &packed);
                }
                let mut out = Vec::new();
                reapi::message(&mut out, 1, &cache);
                if config.execution {
                    let mut execution = Vec::new();
                    reapi::varint_field(&mut execution, 2, 1);
                    let mut packed = Vec::new();
                    for value in &config.exec_digest_functions {
                        reapi::varint(&mut packed, *value);
                    }
                    reapi::bytes_field(&mut execution, 5, &packed);
                    reapi::message(&mut out, 2, &execution);
                }
                reapi::message(&mut out, 4, &semver(config.low_api.0, config.low_api.1));
                reapi::message(&mut out, 5, &semver(config.high_api.0, config.high_api.1));
                (vec![out], ok())
            }
            "build.bazel.remote.execution.v2.ActionCache/GetActionResult" => {
                let instance = string_field(&request, 1);
                let key = Digest::decode(&message_field(&request, 2)[0]).unwrap();
                match state.ac.get(&format!("{instance}|{}", key.hash)) {
                    Some(result) => (vec![result.clone()], ok()),
                    None => (Vec::new(), status(5, "no such action")),
                }
            }
            "build.bazel.remote.execution.v2.ActionCache/UpdateActionResult" => {
                let instance = string_field(&request, 1);
                let key = Digest::decode(&message_field(&request, 2)[0]).unwrap();
                let result = message_field(&request, 3).pop().unwrap_or_default();
                state
                    .ac
                    .insert(format!("{instance}|{}", key.hash), result.clone());
                (vec![result], ok())
            }
            "build.bazel.remote.execution.v2.ContentAddressableStorage/FindMissingBlobs" => {
                let instance = string_field(&request, 1);
                let mut out = Vec::new();
                for digest in message_field(&request, 2) {
                    let decoded = Digest::decode(&digest).unwrap();
                    if decoded.size != 0
                        && !state
                            .cas
                            .contains_key(&format!("{instance}|{}", decoded.hash))
                    {
                        reapi::message(&mut out, 2, &digest);
                    }
                }
                (vec![out], ok())
            }
            "build.bazel.remote.execution.v2.ContentAddressableStorage/BatchUpdateBlobs" => {
                let instance = string_field(&request, 1);
                let mut out = Vec::new();
                for entry in message_field(&request, 2) {
                    let digest = message_field(&entry, 1).pop().unwrap();
                    let data = message_field(&entry, 2).pop().unwrap_or_default();
                    let decoded = Digest::decode(&digest).unwrap();
                    let mut response = Vec::new();
                    reapi::message(&mut response, 1, &digest);
                    let mut rpc_status = Vec::new();
                    if Digest::of(&data) == decoded {
                        state
                            .cas
                            .insert(format!("{instance}|{}", decoded.hash), data);
                    } else {
                        reapi::varint_field(&mut rpc_status, 1, 3);
                    }
                    reapi::message(&mut response, 2, &rpc_status);
                    reapi::message(&mut out, 1, &response);
                }
                (vec![out], ok())
            }
            "build.bazel.remote.execution.v2.ContentAddressableStorage/BatchReadBlobs" => {
                let instance = string_field(&request, 1);
                let mut out = Vec::new();
                for digest in message_field(&request, 2) {
                    let decoded = Digest::decode(&digest).unwrap();
                    let mut response = Vec::new();
                    reapi::message(&mut response, 1, &digest);
                    let mut rpc_status = Vec::new();
                    match state.cas.get(&format!("{instance}|{}", decoded.hash)) {
                        Some(data) => reapi::bytes_field(&mut response, 2, data),
                        None => reapi::varint_field(&mut rpc_status, 1, 5),
                    }
                    reapi::message(&mut response, 3, &rpc_status);
                    reapi::message(&mut out, 1, &response);
                }
                (vec![out], ok())
            }
            "google.bytestream.ByteStream/Read" => {
                let (instance, hash, _, compressed) =
                    parse_resource_compressed(&string_field(&request, 1));
                let offset = varint(&request, 2) as usize;
                let Some(data) = state.cas.get(&format!("{instance}|{hash}")).cloned() else {
                    return (Vec::new(), status(5, "blob not found"));
                };
                if compressed {
                    if !state.config.compressors.contains(&1) {
                        return (Vec::new(), status(3, "compression is not supported"));
                    }
                    // The zstd stream of the blob from the (uncompressed)
                    // read offset on.
                    let stream =
                        bazel_rs::remote_grpc::zstd_encode(&data[offset.min(data.len())..]);
                    let mut out = Vec::new();
                    for piece in stream.chunks(state.config.read_chunk.max(1)) {
                        let mut response = Vec::new();
                        reapi::bytes_field(&mut response, 10, piece);
                        out.push(response);
                    }
                    return (out, ok());
                }
                let limit = state
                    .config
                    .fail_read_after
                    .filter(|limit| offset + limit < data.len());
                if limit.is_some() {
                    state.config.fail_read_after = None;
                }
                let chunk = state.config.read_chunk;
                let mut out = Vec::new();
                let mut position = offset.min(data.len());
                while position < data.len() {
                    if let Some(limit) = limit
                        && position >= offset + limit
                    {
                        return (out, status(14, "injected read failure"));
                    }
                    let end = (position + chunk).min(data.len());
                    let mut response = Vec::new();
                    reapi::bytes_field(&mut response, 10, &data[position..end]);
                    out.push(response);
                    position = end;
                }
                (out, ok())
            }
            "google.bytestream.ByteStream/Write" => {
                let resource = string_field(&request, 1);
                let (instance, hash, size, compressed) = parse_resource_compressed(&resource);
                let key = format!("{instance}|{hash}");
                if state.cas.contains_key(&key) {
                    let mut response = Vec::new();
                    // -1 for compressed uploads (REAPI).
                    reapi::varint_field(&mut response, 1, if compressed { u64::MAX } else { size });
                    return (vec![response], ok());
                }
                if compressed {
                    if !state.config.compressors.contains(&1) {
                        return (Vec::new(), status(3, "compression is not supported"));
                    }
                    let mut committed = state.uploads.remove(&resource).unwrap_or_default();
                    let base = varint(&requests[0], 2) as usize;
                    if base != committed.len() {
                        return (Vec::new(), status(3, "write_offset mismatch"));
                    }
                    let mut stream = Vec::new();
                    for request in &requests {
                        let offset = varint(request, 2) as usize;
                        let data = message_field(request, 10).pop().unwrap_or_default();
                        state.max_write_chunk = state.max_write_chunk.max(data.len());
                        if offset != base + stream.len() {
                            return (Vec::new(), status(3, "write_offset mismatch"));
                        }
                        stream.extend_from_slice(&data);
                    }
                    let (decoded, complete) = bazel_rs::remote_grpc::zstd_decode(&stream);
                    if !complete {
                        return (Vec::new(), status(3, "truncated zstd stream"));
                    }
                    committed.extend_from_slice(&decoded);
                    if Digest::of(&committed).hash != hash {
                        return (Vec::new(), status(3, "digest mismatch"));
                    }
                    let mut response = Vec::new();
                    reapi::varint_field(&mut response, 1, (base + stream.len()) as u64);
                    state.cas.insert(key, committed);
                    return (vec![response], ok());
                }
                let mut committed = state.uploads.remove(&resource).unwrap_or_default();
                let limit = state
                    .config
                    .fail_write_after
                    .filter(|limit| (*limit as u64) < size);
                if limit.is_some() {
                    state.config.fail_write_after = None;
                }
                for request in &requests {
                    let offset = varint(request, 2) as usize;
                    let data = message_field(request, 10).pop().unwrap_or_default();
                    state.max_write_chunk = state.max_write_chunk.max(data.len());
                    if offset != committed.len() {
                        return (Vec::new(), status(3, "write_offset mismatch"));
                    }
                    committed.extend_from_slice(&data);
                    if let Some(limit) = limit
                        && committed.len() >= limit
                        && committed.len() < size as usize
                    {
                        state.uploads.insert(resource.clone(), committed);
                        return (Vec::new(), status(14, "injected write failure"));
                    }
                }
                let mut response = Vec::new();
                reapi::varint_field(&mut response, 1, committed.len() as u64);
                if Digest::of(&committed).hash != hash {
                    return (Vec::new(), status(3, "digest mismatch"));
                }
                state.cas.insert(key, committed);
                (vec![response], ok())
            }
            "google.bytestream.ByteStream/QueryWriteStatus" => {
                if !state.config.query_write_status {
                    return (
                        Vec::new(),
                        status(12, "QueryWriteStatus is not implemented"),
                    );
                }
                let resource = string_field(&request, 1);
                let (instance, hash, _, _) = parse_resource_compressed(&resource);
                let complete = state.cas.contains_key(&format!("{instance}|{hash}"));
                let committed = state.uploads.get(&resource).map_or(0, Vec::len);
                let mut response = Vec::new();
                reapi::varint_field(&mut response, 1, committed as u64);
                reapi::varint_field(&mut response, 2, u64::from(complete));
                (vec![response], ok())
            }
            _ => (Vec::new(), status(12, "unknown method")),
        }
    }

    const EXECUTE_RESPONSE: &str =
        "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse";
    const METADATA: &str =
        "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteOperationMetadata";
    const PRECONDITION_FAILURE: &str = "type.googleapis.com/google.rpc.PreconditionFailure";

    fn any(type_url: &str, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        reapi::string(&mut out, 1, type_url);
        reapi::bytes_field(&mut out, 2, value);
        out
    }

    fn rpc_status(code: u32, message: &str, details: &[(String, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        reapi::varint_field(&mut out, 1, u64::from(code));
        reapi::string(&mut out, 2, message);
        for (type_url, value) in details {
            reapi::message(&mut out, 3, &any(type_url, value));
        }
        out
    }

    /// An EXECUTING operation (ExecuteOperationMetadata.stage = 3).
    fn executing(name: &str, action: &Digest) -> Vec<u8> {
        let mut metadata = Vec::new();
        reapi::varint_field(&mut metadata, 1, 3);
        reapi::message(&mut metadata, 2, &action.encode());
        let mut out = Vec::new();
        reapi::string(&mut out, 1, name);
        reapi::message(&mut out, 2, &any(METADATA, &metadata));
        out
    }

    fn done(name: &str, error: Option<Vec<u8>>, response: Option<Vec<u8>>) -> Vec<u8> {
        let mut out = Vec::new();
        reapi::string(&mut out, 1, name);
        reapi::varint_field(&mut out, 3, 1);
        if let Some(error) = error {
            reapi::message(&mut out, 4, &error);
        }
        if let Some(response) = response {
            reapi::message(&mut out, 5, &any(EXECUTE_RESPONSE, &response));
        }
        out
    }

    fn execute_response(
        result: Option<&[u8]>,
        cached: bool,
        status: Option<Vec<u8>>,
        message: &str,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(result) = result {
            reapi::message(&mut out, 1, result);
        }
        reapi::varint_field(&mut out, 2, u64::from(cached));
        if let Some(status) = status {
            reapi::message(&mut out, 3, &status);
        }
        reapi::string(&mut out, 5, message);
        out
    }

    fn duration_seconds(bytes: &[u8]) -> u64 {
        varint(bytes, 1)
    }

    /// A materialized input root: files, symlinks, directories; `Err` lists
    /// the missing blobs.
    fn walk_input(
        state: &State,
        instance: &str,
        digest: &Digest,
        prefix: &str,
        executed: &mut Executed,
        missing: &mut Vec<Digest>,
        blobs: &mut Vec<(String, Vec<u8>, bool)>,
    ) {
        let Some(bytes) = state.cas.get(&format!("{instance}|{}", digest.hash)) else {
            if digest.size != 0 {
                missing.push(digest.clone());
                return;
            }
            return;
        };
        let directory = reapi::Directory::decode(bytes).unwrap();
        let join = |name: &str| {
            if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            }
        };
        for (name, file, executable) in &directory.files {
            let path = join(name);
            executed
                .files
                .insert(path.clone(), (file.clone(), *executable));
            match state.cas.get(&format!("{instance}|{}", file.hash)) {
                Some(data) => blobs.push((path, data.clone(), *executable)),
                None if file.size == 0 => blobs.push((path, Vec::new(), *executable)),
                None => missing.push(file.clone()),
            }
        }
        for (name, target) in &directory.symlinks {
            executed.symlinks.insert(join(name), target.clone());
        }
        for (name, child) in &directory.directories {
            let path = join(name);
            executed.directories.push(path.clone());
            walk_input(state, instance, child, &path, executed, missing, blobs);
        }
    }

    fn put_blob(state: &mut State, instance: &str, data: &[u8]) -> Digest {
        let digest = Digest::of(data);
        state
            .cas
            .insert(format!("{instance}|{}", digest.hash), data.to_vec());
        digest
    }

    /// The Tree of an output directory, its blobs stored.
    fn output_tree(state: &mut State, instance: &str, path: &Path) -> Vec<u8> {
        fn directory(
            state: &mut State,
            instance: &str,
            path: &Path,
            children: &mut Vec<Vec<u8>>,
        ) -> Vec<u8> {
            let mut entries = std::fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            let mut value = reapi::Directory::default();
            for entry in entries {
                let name = entry.file_name().to_string_lossy().into_owned();
                let file_type = entry.file_type().unwrap();
                if file_type.is_symlink() {
                    let target = std::fs::read_link(entry.path()).unwrap();
                    value
                        .symlinks
                        .push((name, target.to_string_lossy().into_owned()));
                } else if file_type.is_dir() {
                    let child = directory(state, instance, &entry.path(), children);
                    let digest = Digest::of(&child);
                    children.push(child);
                    value.directories.push((name, digest));
                } else {
                    use std::os::unix::fs::PermissionsExt as _;
                    let data = std::fs::read(entry.path()).unwrap();
                    let executable = entry.metadata().unwrap().permissions().mode() & 0o100 != 0;
                    let digest = put_blob(state, instance, &data);
                    value.files.push((name, digest, executable));
                }
            }
            value.encode()
        }
        let mut children = Vec::new();
        let root = directory(state, instance, path, &mut children);
        let mut out = Vec::new();
        reapi::message(&mut out, 1, &root);
        for child in &children {
            reapi::message(&mut out, 2, child);
        }
        out
    }

    static NEXT_EXECUTION: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// Runs one Execute request: `(operations, status)`.
    fn run_execute(state: &Mutex<State>, request: &[u8]) -> (Vec<Vec<u8>>, (u32, String)) {
        let step = state
            .lock()
            .unwrap()
            .config
            .exec_script
            .pop_front()
            .unwrap_or(ExecStep::Run);
        run_step(state, request, step)
    }

    fn run_step(
        state: &Mutex<State>,
        request: &[u8],
        step: ExecStep,
    ) -> (Vec<Vec<u8>>, (u32, String)) {
        let instance = string_field(request, 1);
        let skip_cache_lookup = varint(request, 3) != 0;
        let action_digest = Digest::decode(&message_field(request, 6)[0]).unwrap();
        let mut guard = state.lock().unwrap();
        let name = format!("operations/{}", guard.next_operation);
        guard.next_operation += 1;
        let missing_status = |missing: &[Digest]| {
            let mut failure = Vec::new();
            for digest in missing {
                let mut violation = Vec::new();
                reapi::string(&mut violation, 1, "MISSING");
                reapi::string(
                    &mut violation,
                    2,
                    &format!("blobs/{}/{}", digest.hash, digest.size),
                );
                reapi::message(&mut failure, 1, &violation);
            }
            rpc_status(
                9,
                "missing input blobs",
                &[(PRECONDITION_FAILURE.to_owned(), failure)],
            )
        };
        match &step {
            ExecStep::Call(code, message) => return (Vec::new(), (*code, message.clone())),
            ExecStep::OperationError(code, message) => {
                return (
                    vec![done(&name, Some(rpc_status(*code, message, &[])), None)],
                    (0, String::new()),
                );
            }
            ExecStep::Response {
                code,
                message,
                details,
                response_message,
            } => {
                let response = execute_response(
                    Some(&[]),
                    false,
                    Some(rpc_status(*code, message, details)),
                    response_message,
                );
                return (vec![done(&name, None, Some(response))], (0, String::new()));
            }
            ExecStep::Break(code) => {
                guard.pending.insert(name.clone(), request.to_vec());
                return (
                    vec![executing(&name, &action_digest)],
                    (
                        *code,
                        if *code == 0 {
                            String::new()
                        } else {
                            "stream broken".into()
                        },
                    ),
                );
            }
            ExecStep::Run | ExecStep::Lost => {}
        }
        let Some(action) = guard
            .cas
            .get(&format!("{instance}|{}", action_digest.hash))
            .cloned()
        else {
            let status = missing_status(std::slice::from_ref(&action_digest));
            return (
                vec![done(
                    &name,
                    None,
                    Some(execute_response(None, false, Some(status), "")),
                )],
                (0, String::new()),
            );
        };
        let command_digest = Digest::decode(&message_field(&action, 1)[0]).unwrap();
        let input_root = Digest::decode(&message_field(&action, 2)[0]).unwrap();
        let do_not_cache = varint(&action, 7) != 0;
        let timeout = message_field(&action, 6)
            .first()
            .map(|duration| duration_seconds(duration));
        let mut executed = Executed {
            instance: instance.clone(),
            action_digest: action_digest.clone(),
            skip_cache_lookup,
            action: action.clone(),
            command: reapi::Command::default(),
            files: BTreeMap::new(),
            symlinks: BTreeMap::new(),
            directories: Vec::new(),
            cached: false,
        };
        let mut missing = Vec::new();
        let command = guard
            .cas
            .get(&format!("{instance}|{}", command_digest.hash))
            .cloned();
        match &command {
            Some(command) => executed.command = reapi::Command::decode(command).unwrap(),
            None => missing.push(command_digest.clone()),
        }
        let mut blobs = Vec::new();
        walk_input(
            &guard,
            &instance,
            &input_root,
            "",
            &mut executed,
            &mut missing,
            &mut blobs,
        );
        if !missing.is_empty() {
            guard.executed.push(executed);
            let status = missing_status(&missing);
            return (
                vec![done(
                    &name,
                    None,
                    Some(execute_response(None, false, Some(status), "")),
                )],
                (0, String::new()),
            );
        }
        if !skip_cache_lookup
            && let Some(result) = guard
                .ac
                .get(&format!("{instance}|{}", action_digest.hash))
                .cloned()
        {
            executed.cached = true;
            guard.executed.push(executed);
            let response = execute_response(Some(&result), true, None, "");
            let operation = done(&name, None, Some(response));
            guard.operations.insert(name, operation.clone());
            return (vec![operation], (0, String::new()));
        }
        let lost = matches!(step, ExecStep::Lost);
        drop(guard);
        // Materialize the input root.
        let root = crate::test_temp::root().join(format!(
            "native-reapi-exec-{}-{}",
            std::process::id(),
            NEXT_EXECUTION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for directory in &executed.directories {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        for (path, data, executable) in &blobs {
            use std::os::unix::fs::PermissionsExt as _;
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, data).unwrap();
            std::fs::set_permissions(
                &file,
                std::fs::Permissions::from_mode(if *executable { 0o755 } else { 0o644 }),
            )
            .unwrap();
        }
        for (path, target) in &executed.symlinks {
            let link = root.join(path);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, link).unwrap();
        }
        let command = executed.command.clone();
        for output in &command.output_paths {
            std::fs::create_dir_all(root.join(output).parent().unwrap()).unwrap();
        }
        let program = &command.arguments[0];
        let mut process = if program.contains('/') && !program.starts_with('/') {
            let mut process = std::process::Command::new(root.join(program));
            std::os::unix::process::CommandExt::arg0(&mut process, program);
            process
        } else {
            std::process::Command::new(program)
        };
        process
            .args(&command.arguments[1..])
            .current_dir(&root)
            .env_clear()
            .envs(&command.environment)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = process.spawn().unwrap();
        let started = std::time::Instant::now();
        let mut timed_out = false;
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if let Some(timeout) = timeout
                && started.elapsed() > std::time::Duration::from_secs(timeout)
            {
                let _ = child.kill();
                timed_out = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        let mut guard = state.lock().unwrap();
        let mut result = reapi::ActionResult::default();
        for path in &command.output_paths {
            let full = root.join(path);
            let Ok(metadata) = std::fs::symlink_metadata(&full) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                result.output_symlinks.push(reapi::OutputSymlink {
                    path: path.clone(),
                    target: std::fs::read_link(&full)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                });
            } else if metadata.is_dir() {
                let tree = output_tree(&mut guard, &instance, &full);
                let tree_digest = put_blob(&mut guard, &instance, &tree);
                result.output_directories.push(reapi::OutputDirectory {
                    path: path.clone(),
                    tree_digest,
                });
            } else {
                use std::os::unix::fs::PermissionsExt as _;
                let data = std::fs::read(&full).unwrap();
                let digest = put_blob(&mut guard, &instance, &data);
                result.output_files.push(reapi::OutputFile {
                    path: path.clone(),
                    digest,
                    is_executable: metadata.permissions().mode() & 0o100 != 0,
                    contents: Vec::new(),
                });
            }
        }
        result.exit_code = output.status.code().unwrap_or(137);
        result.stdout_digest = Some(put_blob(&mut guard, &instance, &output.stdout));
        result.stderr_digest = Some(put_blob(&mut guard, &instance, &output.stderr));
        let result = result.encode();
        if result_cacheable(&result) && !do_not_cache && !timed_out {
            guard
                .ac
                .insert(format!("{instance}|{}", action_digest.hash), result.clone());
        }
        guard.executed.push(executed);
        let status = timed_out.then(|| rpc_status(4, "execution timed out", &[]));
        let operation = done(
            &name,
            None,
            Some(execute_response(Some(&result), false, status, "")),
        );
        if !lost {
            guard.operations.insert(name.clone(), operation.clone());
        }
        drop(guard);
        let _ = std::fs::remove_dir_all(&root);
        (
            vec![executing(&name, &action_digest), operation],
            (0, String::new()),
        )
    }

    fn result_cacheable(result: &[u8]) -> bool {
        reapi::ActionResult::decode(result).unwrap().exit_code == 0
    }

    /// WaitExecution: the finished operation, a pending one run now, or
    /// NOT_FOUND.
    fn run_wait(state: &Mutex<State>, request: &[u8]) -> (Vec<Vec<u8>>, (u32, String)) {
        let name = string_field(request, 1);
        let mut guard = state.lock().unwrap();
        if matches!(guard.config.exec_script.front(), Some(ExecStep::Lost)) {
            guard.config.exec_script.pop_front();
            guard.pending.remove(&name);
            return (Vec::new(), (5, format!("operation {name} not found")));
        }
        if let Some(operation) = guard.operations.get(&name).cloned() {
            return (vec![operation], (0, String::new()));
        }
        let Some(execute) = guard.pending.remove(&name) else {
            return (Vec::new(), (5, format!("operation {name} not found")));
        };
        drop(guard);
        let (mut operations, status) = run_step(state, &execute, ExecStep::Run);
        // The finished operation keeps the waited-for name.
        let last = operations.pop().unwrap();
        let mut renamed = Vec::new();
        reapi::string(&mut renamed, 1, &name);
        for field in reapi::fields(&last).unwrap() {
            match field {
                (1, _) => {}
                (number, Value::Bytes(value)) => {
                    reapi::bytes_field(&mut renamed, number as u32, value)
                }
                (number, Value::Varint(value)) => {
                    reapi::varint_field(&mut renamed, number as u32, value)
                }
                (_, Value::Fixed) => {}
            }
        }
        (vec![renamed], status)
    }

    fn log_execution(
        state: &Mutex<State>,
        method: &str,
        metadata: Vec<(String, String)>,
        request: &[u8],
    ) {
        let detail = if method.ends_with("/Execute") {
            format!(
                "instance={} action={} skip_cache_lookup={}",
                string_field(request, 1),
                Digest::decode(&message_field(request, 6)[0]).unwrap().hash,
                varint(request, 3) != 0
            )
        } else {
            string_field(request, 1)
        };
        state.lock().unwrap().log.push(Logged {
            method: method.to_owned(),
            metadata,
            detail,
        });
    }

    async fn handle(
        state: Arc<Mutex<State>>,
        request: http::Request<h2::RecvStream>,
        mut respond: h2::server::SendResponse<Bytes>,
    ) {
        let method = request.uri().path().trim_start_matches('/').to_owned();
        let metadata = request
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or("").to_owned(),
                )
            })
            .collect::<Vec<_>>();
        let mut body = request.into_body();
        let mut bytes = Vec::new();
        while let Some(chunk) = body.data().await {
            let Ok(chunk) = chunk else { return };
            let _ = body.flow_control().release_capacity(chunk.len());
            bytes.extend_from_slice(&chunk);
        }
        let requests = messages(&bytes);
        let execution = matches!(
            method.as_str(),
            "build.bazel.remote.execution.v2.Execution/Execute"
                | "build.bazel.remote.execution.v2.Execution/WaitExecution"
        );
        let (responses, (code, message)) = if execution {
            let request = requests.first().cloned().unwrap_or_default();
            log_execution(&state, &method, metadata, &request);
            let state = state.clone();
            let wait = method.ends_with("/WaitExecution");
            tokio::task::spawn_blocking(move || {
                if wait {
                    run_wait(&state, &request)
                } else {
                    run_execute(&state, &request)
                }
            })
            .await
            .unwrap()
        } else {
            dispatch(&state, &method, metadata, requests)
        };
        let mut trailers = http::HeaderMap::new();
        trailers.insert("grpc-status", code.to_string().parse().unwrap());
        if !message.is_empty() {
            trailers.insert("grpc-message", message.replace(' ', "%20").parse().unwrap());
        }
        if responses.is_empty() {
            // Trailers-only.
            let mut response = http::Response::builder()
                .status(200)
                .header("content-type", "application/grpc");
            for (name, value) in &trailers {
                response = response.header(name, value);
            }
            let _ = respond.send_response(response.body(()).unwrap(), true);
            return;
        }
        let response = http::Response::builder()
            .status(200)
            .header("content-type", "application/grpc")
            .body(())
            .unwrap();
        let Ok(mut stream) = respond.send_response(response, false) else {
            return;
        };
        for message in responses {
            stream.reserve_capacity(message.len() + 5);
            if stream.send_data(frame(&message), false).is_err() {
                return;
            }
        }
        let _ = stream.send_trailers(trailers);
    }

    async fn serve_connection<T>(state: Arc<Mutex<State>>, io: T)
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let Ok(mut connection) = h2::server::Builder::new()
            .initial_window_size(1 << 20)
            .initial_connection_window_size(1 << 24)
            .handshake::<_, Bytes>(io)
            .await
        else {
            return;
        };
        while let Some(Ok((request, respond))) = connection.accept().await {
            tokio::spawn(handle(state.clone(), request, respond));
        }
    }

    fn tls_acceptor(tls: &Tls) -> tokio_rustls::TlsAcceptor {
        use rustls_pki_types::pem::PemObject as _;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let chain = rustls_pki_types::CertificateDer::pem_file_iter(&tls.certificate)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = rustls_pki_types::PrivateKeyDer::from_pem_file(&tls.key).unwrap();
        let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap();
        let builder = match &tls.client_ca {
            Some(ca) => {
                let mut roots = rustls::RootCertStore::empty();
                for certificate in rustls_pki_types::CertificateDer::pem_file_iter(ca).unwrap() {
                    roots.add(certificate.unwrap()).unwrap();
                }
                builder.with_client_cert_verifier(
                    rustls::server::WebPkiClientVerifier::builder_with_provider(
                        Arc::new(roots),
                        provider,
                    )
                    .build()
                    .unwrap(),
                )
            }
            None => builder.with_no_client_auth(),
        };
        let mut config = builder.with_single_cert(chain, key).unwrap();
        config.alpn_protocols = vec![b"h2".to_vec()];
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
    }

    impl Server {
        pub fn new() -> Self {
            Self::start(Config::default(), None, None)
        }

        pub fn with_config(config: Config) -> Self {
            Self::start(config, None, None)
        }

        pub fn tls(config: Config, tls: Tls) -> Self {
            Self::start(config, Some(tls), None)
        }

        pub fn unix(config: Config, socket: PathBuf) -> Self {
            Self::start(config, None, Some(socket))
        }

        fn start(config: Config, tls: Option<Tls>, socket: Option<PathBuf>) -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let state = Arc::new(Mutex::new(State {
                config,
                ..State::default()
            }));
            let accept_state = state.clone();
            let mut port = 0;
            if let Some(path) = &socket {
                let _ = std::fs::remove_file(path);
                let listener = runtime.block_on(async { tokio::net::UnixListener::bind(path) });
                let listener = listener.unwrap();
                runtime.spawn(async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        tokio::spawn(serve_connection(accept_state.clone(), stream));
                    }
                });
            } else {
                let listener = runtime
                    .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                    .unwrap();
                port = listener.local_addr().unwrap().port();
                let acceptor = tls.as_ref().map(tls_acceptor);
                runtime.spawn(async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        let state = accept_state.clone();
                        match acceptor.clone() {
                            Some(acceptor) => {
                                tokio::spawn(async move {
                                    if let Ok(stream) = acceptor.accept(stream).await {
                                        serve_connection(state, stream).await;
                                    }
                                });
                            }
                            None => {
                                tokio::spawn(serve_connection(state, stream));
                            }
                        }
                    }
                });
            }
            Self {
                port,
                socket,
                state,
                _runtime: runtime,
            }
        }

        pub fn url(&self) -> String {
            match &self.socket {
                Some(path) => format!("unix:{}", path.display()),
                None => format!("grpc://127.0.0.1:{}", self.port),
            }
        }

        pub fn take_log(&self) -> Vec<Logged> {
            std::mem::take(&mut self.state.lock().unwrap().log)
        }

        /// The logged methods, sorted, with counts: `Service/Method xN`.
        pub fn method_counts(&self, log: &[Logged]) -> BTreeMap<String, usize> {
            let mut counts = BTreeMap::new();
            for call in log {
                let short = call.method.rsplit('.').next().unwrap().to_owned();
                *counts.entry(short).or_insert(0) += 1;
            }
            counts
        }

        /// The Action Cache keys of `instance`.
        pub fn ac_keys(&self, instance: &str) -> Vec<String> {
            self.state
                .lock()
                .unwrap()
                .ac
                .keys()
                .filter_map(|key| key.strip_prefix(&format!("{instance}|")).map(str::to_owned))
                .collect()
        }

        pub fn cas_blob(&self, instance: &str, hash: &str) -> Option<Vec<u8>> {
            self.state
                .lock()
                .unwrap()
                .cas
                .get(&format!("{instance}|{hash}"))
                .cloned()
        }

        /// The Execute requests so far.
        pub fn executed(&self) -> Vec<Executed> {
            self.state.lock().unwrap().executed.clone()
        }

        pub fn configure(&self, change: impl FnOnce(&mut Config)) {
            change(&mut self.state.lock().unwrap().config);
        }
    }
}
