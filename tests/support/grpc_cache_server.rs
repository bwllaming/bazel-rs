// Shared by integration tests (`include!("support/grpc_cache_server.rs")`):
// a minimal in-process REAPI v2 gRPC cache on loopback (or a Unix socket,
// optionally TLS/mTLS) with an in-memory CAS and Action Cache, a request
// log and failure injection. Speaks gRPC over h2 directly and decodes the
// messages with bazel_rs::reapi's protobuf primitives.
//
// Services: Capabilities.GetCapabilities; ActionCache.GetActionResult and
// UpdateActionResult; ContentAddressableStorage.FindMissingBlobs,
// BatchUpdateBlobs and BatchReadBlobs; ByteStream Read (read_offset),
// Write (write_offset, resumable uploads) and QueryWriteStatus.

#[allow(dead_code)]
mod grpc_cache_server {
    use bazel_rs::reapi::{self, Digest, Value};
    use bytes::Bytes;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
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
        let parts = resource.split('/').collect::<Vec<_>>();
        let blobs = parts.iter().position(|part| *part == "blobs").unwrap();
        let instance_end = parts
            .iter()
            .position(|part| *part == "uploads" || *part == "blobs")
            .unwrap();
        (
            parts[..instance_end].join("/"),
            parts[blobs + 1].to_owned(),
            parts[blobs + 2].parse().unwrap(),
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
                let mut out = Vec::new();
                reapi::message(&mut out, 1, &cache);
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
                let (instance, hash, _) = parse_resource(&string_field(&request, 1));
                let offset = varint(&request, 2) as usize;
                let Some(data) = state.cas.get(&format!("{instance}|{hash}")).cloned() else {
                    return (Vec::new(), status(5, "blob not found"));
                };
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
                let (instance, hash, size) = parse_resource(&resource);
                let key = format!("{instance}|{hash}");
                if state.cas.contains_key(&key) {
                    let mut response = Vec::new();
                    reapi::varint_field(&mut response, 1, size);
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
                let (instance, hash, _) = parse_resource(&resource);
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
        let (responses, (code, message)) = dispatch(&state, &method, metadata, messages(&bytes));
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

        pub fn configure(&self, change: impl FnOnce(&mut Config)) {
            change(&mut self.state.lock().unwrap().config);
        }
    }
}
