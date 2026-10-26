//! Bazel 9.3's credential helpers (N41, docs/REMOTE_CACHE.md).
//!
//! Pinned source (`src/main/java/com/google/devtools/build/lib/`):
//! `authandtls/AuthAndTLSOptions.java:202-336` (the three options and the
//! `--credential_helper` converter), `authandtls/GoogleAuthUtils.java:416-436`
//! and `runtime/CommandLinePathFactory.java` (helper path resolution),
//! `authandtls/credentialhelper/CredentialHelperProvider.java:66-116` (scope
//! matching), `CredentialHelper.java:69-172` (the subprocess protocol and its
//! error texts), `GetCredentialsRequest.java`/`GetCredentialsResponse.java`
//! (the JSON messages), `CredentialHelperCredentials.java:61-79` and
//! `CredentialCacheExpiry.java`/`CredentialModule.java` (the per-URI cache).
//!
//! Consumers: the remote cache (crate::remote_cache) and the downloader
//! (registry, archive and repository/extension downloads;
//! `bazel/BazelRepositoryModule.java:446-477`, `bazel/repository/downloader/
//! HttpConnectorMultiplexer.java:133-151`). The native process is one
//! invocation, so the cache lives for one command, like `bazel --batch`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One `--credential_helper` value (AuthAndTLSOptions.CredentialHelperOption).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperOption {
    /// The normalized scope: an ASCII domain name, optionally `*.`-prefixed.
    pub scope: Option<String>,
    /// The unparsed path.
    pub path: String,
}

/// The modelled `AuthAndTLSOptions` (credential helper options only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialOptions {
    pub helpers: Vec<HelperOption>,
    pub timeout: Duration,
    pub cache_duration: Duration,
}

impl Default for CredentialOptions {
    fn default() -> Self {
        Self {
            helpers: Vec::new(),
            // defaultValue = "10s" / "30m" (AuthAndTLSOptions.java:233, 248).
            timeout: Duration::from_secs(10),
            cache_duration: Duration::from_secs(30 * 60),
        }
    }
}

/// `name` and `old_name` value forms: `--name=V`, `--name V`.
fn option_value<'a>(
    arg: &'a str,
    next: Option<&'a str>,
    names: &[&str],
) -> Option<Result<(&'a str, bool), String>> {
    let body = arg.strip_prefix("--")?;
    for name in names {
        if body == *name {
            return Some(
                next.map(|value| (value, true))
                    .ok_or_else(|| format!("Expected value after --{name}")),
            );
        }
        if let Some(value) = body.strip_prefix(name).and_then(|v| v.strip_prefix('=')) {
            return Some(Ok((value, false)));
        }
    }
    None
}

/// Converters.DurationConverter (common/options/Converters.java:178-210).
pub fn parse_duration(input: &str) -> Result<Duration, String> {
    if input == "0" {
        return Ok(Duration::ZERO);
    }
    let illegal = || format!("Illegal duration '{input}'.");
    let split = input
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(illegal)?;
    let (digits, unit) = input.split_at(split);
    if digits.is_empty() {
        return Err(illegal());
    }
    // Long.parseLong: overflow is a NumberFormatException, i.e. a crash in
    // Bazel; natively an explicit error.
    let value: u64 = digits
        .parse::<i64>()
        .map_err(|_| format!("duration '{input}' is out of range"))?
        .try_into()
        .map_err(|_| illegal())?;
    let seconds = |factor: u64| {
        value
            .checked_mul(factor)
            .map(Duration::from_secs)
            .ok_or_else(|| format!("duration '{input}' is out of range"))
    };
    match unit {
        "d" => seconds(86_400),
        "h" => seconds(3_600),
        "m" => seconds(60),
        "s" => seconds(1),
        "ms" => Ok(Duration::from_millis(value)),
        "ns" => Ok(Duration::from_nanos(value)),
        _ => Err(illegal()),
    }
}

impl CredentialOptions {
    /// Parses one modelled option; `None` if `arg` is not one of them. The
    /// result says whether the separate value `next` was consumed.
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Option<Result<bool, String>> {
        if let Some(found) = option_value(
            arg,
            next,
            &["credential_helper", "experimental_credential_helper"],
        ) {
            return Some(found.and_then(|(value, used)| {
                let helper = parse_helper_option(value).map_err(|error| {
                    format!("While parsing option --credential_helper={value}: {error}")
                })?;
                self.helpers.push(helper);
                Ok(used)
            }));
        }
        for (names, timeout) in [
            (
                &[
                    "credential_helper_timeout",
                    "experimental_credential_helper_timeout",
                ],
                true,
            ),
            (
                &[
                    "credential_helper_cache_duration",
                    "experimental_credential_helper_cache_duration",
                ],
                false,
            ),
        ] {
            if let Some(found) = option_value(arg, next, names) {
                return Some(found.and_then(|(value, used)| {
                    let duration = parse_duration(value).map_err(|error| {
                        format!("While parsing option --{}={value}: {error}", names[0])
                    })?;
                    if timeout {
                        self.timeout = duration;
                    } else {
                        self.cache_duration = duration;
                    }
                    Ok(used)
                }));
            }
        }
        None
    }
}

/// CredentialHelperOptionConverter.convert (AuthAndTLSOptions.java:288-336).
pub fn parse_helper_option(input: &str) -> Result<HelperOption, String> {
    if let Some((scope, path)) = input.split_once('=') {
        let scope = parse_scope(scope)?;
        if path.is_empty() {
            return Err("Credential helper path must not be empty".into());
        }
        return Ok(HelperOption {
            scope: Some(scope),
            path: path.to_owned(),
        });
    }
    if input.is_empty() {
        return Err("Credential helper path must not be empty".into());
    }
    Ok(HelperOption {
        scope: None,
        path: input.to_owned(),
    })
}

fn parse_scope(scope: &str) -> Result<String, String> {
    if scope.is_empty() {
        return Err("Credential helper scope must not be empty".into());
    }
    let (wildcard, domain) = match scope.strip_prefix("*.") {
        Some(domain) if scope.len() > 2 => ("*.", domain),
        _ => ("", scope),
    };
    let invalid = || {
        format!(
            "Credential helper scope '{scope}' must be a valid domain name with an optional leading '*.' wildcard"
        )
    };
    if !domain.is_ascii() {
        // IDN.toASCII converts internationalized names to Punycode after
        // nameprep; the native profile does not implement nameprep.
        return Err(format!(
            "Credential helper scope '{scope}' is not ASCII; internationalized (Punycode) scopes are not supported natively"
        ));
    }
    std3_ascii(domain).ok_or_else(invalid)?;
    Ok(format!("{wildcard}{domain}"))
}

/// java.net.IDN.toASCII(domain, USE_STD3_ASCII_RULES) for an all-ASCII
/// domain: labels split at dots, each 1..=63 letters, digits or hyphens,
/// neither starting nor ending with a hyphen; a single trailing dot (the
/// root label) is allowed. ASCII labels are not case-folded.
fn std3_ascii(domain: &str) -> Option<()> {
    let body = domain.strip_suffix('.').unwrap_or(domain);
    if body.is_empty() {
        return None;
    }
    for label in body.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return None;
        }
    }
    Some(())
}

/// PathFragment normalization: empty and `.` segments dropped, `..` folded
/// into a preceding normal segment (kept at the start of a relative path,
/// dropped at the root of an absolute one).
fn normalize(path: &str) -> (bool, Vec<&str>) {
    let absolute = path.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.last().is_some_and(|last| *last != "..") {
                    segments.pop();
                } else if !absolute {
                    segments.push("..");
                }
            }
            other => segments.push(other),
        }
    }
    (absolute, segments)
}

/// CommandLinePathFactory.create (runtime/CommandLinePathFactory.java):
/// `%workspace%/`-relative, absolute, or a single segment looked up on the
/// client's PATH.
pub fn resolve_helper_path(
    value: &str,
    workspace: Option<&Path>,
    path_variable: &str,
) -> Result<PathBuf, String> {
    // REPLACEMENT_PATTERN `^(%([a-z_]+)%/+)?([^%].*)$`.
    let mut root = None;
    let mut rest = value;
    if let Some(after) = value.strip_prefix('%')
        && let Some(end) = after.find('%')
        && !after[..end].is_empty()
        && after[..end]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'_')
        && after[end + 1..].starts_with('/')
    {
        root = Some(&after[..end]);
        rest = after[end + 1..].trim_start_matches('/');
    }
    if rest.is_empty() || rest.starts_with('%') {
        // Preconditions.checkArgument(matcher.matches()): an unchecked
        // IllegalArgumentException (a crash) in Bazel.
        return Err(format!(
            "credential helper path '{value}' does not match Bazel's path syntax (an internal error in Bazel)"
        ));
    }
    let (absolute, segments) = normalize(rest);
    if segments.contains(&"..") {
        return Err(format!(
            "Path '{value}' must not contain any uplevel references ('..')"
        ));
    }
    if let Some(root) = root {
        let base = match (root, workspace) {
            ("workspace", Some(workspace)) => workspace.to_path_buf(),
            ("install_base", _) => {
                return Err(format!(
                    "credential helper path '{value}': the %install_base% root is not modelled by the native profile"
                ));
            }
            _ => return Err(format!("Unknown root {root}")),
        };
        return Ok(segments
            .iter()
            .fold(base, |path, segment| path.join(segment)));
    }
    if absolute {
        return Ok(segments
            .iter()
            .fold(PathBuf::from("/"), |path, segment| path.join(segment)));
    }
    if segments.len() > 1 {
        return Err(format!(
            "Path '{value}' must either be absolute or not contain any path separators"
        ));
    }
    let name = segments.first().copied().unwrap_or("");
    for directory in path_variable.split(':') {
        if directory.is_empty() || !directory.starts_with('/') {
            continue;
        }
        let candidate = Path::new(directory).join(name);
        if let Ok(metadata) = std::fs::metadata(&candidate) {
            use std::os::unix::fs::PermissionsExt;
            if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "Could not find file with name '{name}' on PATH '{path_variable}'"
    ))
}

/// GetCredentialsResponse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// Header names and values in the helper's order.
    pub headers: Vec<(String, Vec<String>)>,
    /// Seconds since the epoch.
    pub expires: Option<i64>,
}

impl Response {
    /// The request headers, one entry per value.
    pub fn pairs(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .flat_map(|(name, values)| values.iter().map(|value| (name.clone(), value.clone())))
            .collect()
    }
}

/// A credential helper failure (CredentialHelperException).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperError(pub String);

impl std::fmt::Display for HelperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `yyyy-MM-dd'T'HH:mm:ssXXX` with ResolverStyle.LENIENT
/// (GetCredentialsResponse.java:63-66): offsets `Z` or `+HH:MM`; out-of-range
/// fields roll over. Errors carry java.time's parse position.
pub fn parse_expires(text: &str) -> Result<i64, String> {
    let bytes = text.as_bytes();
    let fail = |index: usize| {
        Err(format!(
            "Text '{text}' could not be parsed at index {index}"
        ))
    };
    let mut position = 0;
    let number = |width: usize, position: &mut usize| -> Option<i64> {
        let digits = bytes.get(*position..*position + width)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        *position += width;
        Some(digits.iter().fold(0, |n, d| n * 10 + i64::from(d - b'0')))
    };
    let mut fields = [0i64; 6];
    for (index, (width, separator)) in [
        (4, Some(b'-')),
        (2, Some(b'-')),
        (2, Some(b'T')),
        (2, Some(b':')),
        (2, Some(b':')),
        (2, None),
    ]
    .into_iter()
    .enumerate()
    {
        let start = position;
        fields[index] = match number(width, &mut position) {
            Some(value) => value,
            None => return fail(start),
        };
        if let Some(separator) = separator {
            if bytes.get(position) != Some(&separator) {
                return fail(position);
            }
            position += 1;
        }
    }
    let offset = match bytes.get(position) {
        Some(b'Z') => {
            position += 1;
            0
        }
        Some(sign @ (b'+' | b'-')) => {
            let start = position;
            position += 1;
            let (Some(hours), Some(b':')) = (number(2, &mut position), bytes.get(position)) else {
                return fail(start);
            };
            position += 1;
            let Some(minutes) = number(2, &mut position) else {
                return fail(start);
            };
            let seconds = hours * 3600 + minutes * 60;
            if *sign == b'-' { -seconds } else { seconds }
        }
        _ => return fail(position),
    };
    if position != bytes.len() {
        return fail(position);
    }
    let [year, month, day, hour, minute, second] = fields;
    // Lenient resolution: months beyond 12 roll into years, days and times
    // add to the start of the month.
    let months = year * 12 + (month - 1);
    let (year, month) = (months.div_euclid(12), months.rem_euclid(12) + 1);
    let days = days_from_civil(year, month, 1) + day - 1;
    Ok(days * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// GetCredentialsResponse.GsonTypeAdapter.read: an object with an optional
/// `headers` object of string arrays and an optional RFC 3339 `expires`;
/// unknown keys are ignored. `None` for an empty document (Gson's null).
fn parse_response(stdout: &str) -> Result<Option<Response>, ()> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).map_err(|_| ())?;
    let object = value.as_object().ok_or(())?;
    let mut response = Response {
        headers: Vec::new(),
        expires: None,
    };
    for (key, value) in object {
        match key.as_str() {
            "headers" => {
                for (name, values) in value.as_object().ok_or(())? {
                    let values = values
                        .as_array()
                        .ok_or(())?
                        .iter()
                        .map(|value| value.as_str().map(str::to_owned).ok_or(()))
                        .collect::<Result<Vec<_>, _>>()?;
                    response.headers.push((name.clone(), values));
                }
            }
            "expires" => {
                response.expires = Some(parse_expires(value.as_str().ok_or(())?).map_err(|_| ())?);
            }
            _ => {}
        }
    }
    Ok(Some(response))
}

/// CredentialHelper.getCredentials: `HELPER get` in the workspace with the
/// client environment, `{"uri":"URI"}` on stdin, the response on stdout.
pub fn invoke(
    helper: &Path,
    uri: &str,
    workspace: Option<&Path>,
    timeout: Duration,
) -> Result<Response, HelperError> {
    let fail = |detail: String| {
        HelperError(format!(
            "Failed to get credentials for '{uri}' from helper '{}': {detail}",
            helper.display()
        ))
    };
    let mut command = Command::new(helper);
    command
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so a timeout kills the helper's descendants too.
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    if let Some(workspace) = workspace {
        command.current_dir(workspace);
    }
    // Started by the server itself (crate::signals::Defaults::Jvm).
    crate::signals::reset_in_child(&mut command, crate::signals::Defaults::Jvm);
    // A helper written just before may still be open for writing in another
    // concurrently forked child (ETXTBSY); retry like crate::action.
    let mut spawned = crate::spawn_lock::spawn(&mut command);
    for delay in [1, 2, 4, 8, 16, 32, 64] {
        match &spawned {
            Err(error) if error.raw_os_error() == Some(26) => {
                std::thread::sleep(Duration::from_millis(delay));
                spawned = crate::spawn_lock::spawn(&mut command);
            }
            _ => break,
        }
    }
    let mut child = spawned.map_err(|error| {
        // java.io.IOException texts: "error=2, No such file or directory".
        let detail = match error.raw_os_error() {
            Some(code) => format!(
                "Cannot run program \"{}\" (in directory \"{}\"): error={code}, {}",
                helper.display(),
                workspace
                    .map(|w| w.display().to_string())
                    .unwrap_or_default(),
                std::io::Error::from_raw_os_error(code)
                    .to_string()
                    .split(" (os error")
                    .next()
                    .unwrap_or_default()
            ),
            None => error.to_string(),
        };
        fail(detail)
    })?;
    let group = child.id();
    // GetCredentialsRequest.GsonTypeAdapter.write: {"uri":"..."} (Gson
    // escapes HTML characters by default).
    let request = format!("{{\"uri\":{}}}", gson_string(uri));
    let mut stdin = child.stdin.take().expect("piped stdin");
    // A helper may ignore stdin and exit (CredentialHelper.java:93-99). The
    // writer and readers are never joined unboundedly: a descendant that
    // keeps a pipe open must not hang the invocation.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(request.as_bytes());
    });
    let drain = |mut pipe: Box<dyn Read + Send>| {
        let shared = std::sync::Arc::new(Mutex::new(Vec::new()));
        let buffer = shared.clone();
        let thread = std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => buffer
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .extend_from_slice(&chunk[..n]),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
        (shared, thread)
    };
    let (out, out_thread) = drain(Box::new(child.stdout.take().expect("piped stdout")));
    let (err, err_thread) = drain(Box::new(child.stderr.take().expect("piped stderr")));
    let limited = !timeout.is_zero();
    let deadline = Instant::now() + timeout;
    let kill_group = || {
        if crate::git_process::kill_group(group).is_err() {
            // The leader at least.
            return false;
        }
        true
    };
    let mut status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if limited && Instant::now() >= deadline => {
                if !kill_group() {
                    let _ = child.kill();
                }
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                let _ = kill_group();
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail(error.to_string()));
            }
        }
    };
    // The leader is gone; its pipes close unless a descendant holds them.
    // Wait for EOF until the deadline, then kill the group and give the
    // readers a short grace period before detaching them.
    let finished = |deadline: Instant| {
        while !(out_thread.is_finished() && err_thread.is_finished()) {
            if limited && Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    };
    if !finished(deadline) {
        let _ = kill_group();
        if !finished(Instant::now() + Duration::from_millis(500)) {
            // Detach: a descendant outside the group still holds a pipe.
        }
        // Output without EOF is incomplete: the helper did not finish in time.
        status = None;
    }
    if writer.is_finished() {
        let _ = writer.join();
    }
    let take = |buffer: &std::sync::Arc<Mutex<Vec<u8>>>| {
        String::from_utf8_lossy(&buffer.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
    };
    let (stdout, stderr) = (take(&out), take(&err));
    let Some(status) = status else {
        return Err(fail("process timed out".into()));
    };
    if !status.success() {
        let code = status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            128 + status.signal().unwrap_or(0)
        });
        return Err(fail(format!(
            "process exited with code {code}. stderr: {stderr}"
        )));
    }
    match parse_response(&stdout) {
        Ok(Some(response)) => Ok(response),
        Ok(None) => Err(fail(format!(
            "process exited without output. stderr: {stderr}"
        ))),
        Err(()) => Err(fail(format!("error parsing output. stderr: {stderr}"))),
    }
}

/// A JSON string as Gson writes it (html-safe escaping).
fn gson_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '<' | '>' | '&' | '=' | '\'' | '\u{2028}' | '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The host of a URI string as java.net.URI.getHost returns it (no case
/// folding; IPv6 literals keep their brackets), or `None`.
pub fn uri_host(uri: &str) -> Option<&str> {
    let (_, rest) = uri.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if host_port.starts_with('[') {
        &host_port[..=host_port.find(']')?]
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    (!host.is_empty()).then_some(host)
}

/// The resolved helpers (CredentialHelperProvider) and the per-URI cache.
#[derive(Debug)]
pub struct Credentials {
    default_helper: Option<PathBuf>,
    hosts: HashMap<String, PathBuf>,
    suffixes: HashMap<String, PathBuf>,
    workspace: Option<PathBuf>,
    timeout: Duration,
    cache_duration: Duration,
    cache: Mutex<HashMap<String, (Response, Instant)>>,
    /// One lock per URI: concurrent requests for a URI wait for one helper
    /// invocation (Caffeine's `Cache.get(key, loader)` in
    /// CredentialHelperCredentials.getRequestMetadata).
    loading: Mutex<HashMap<String, std::sync::Arc<Mutex<()>>>>,
    invocations: Mutex<Vec<String>>,
}

impl Credentials {
    /// GoogleAuthUtils.newCredentialHelperProvider: every path resolves
    /// eagerly; later values for the same scope win.
    pub fn new(
        options: &CredentialOptions,
        workspace: Option<&Path>,
        path_variable: &str,
    ) -> Result<Self, String> {
        let mut credentials = Self {
            default_helper: None,
            hosts: HashMap::new(),
            suffixes: HashMap::new(),
            workspace: workspace.map(Path::to_path_buf),
            timeout: options.timeout,
            cache_duration: options.cache_duration,
            cache: Mutex::new(HashMap::new()),
            loading: Mutex::new(HashMap::new()),
            invocations: Mutex::new(Vec::new()),
        };
        for helper in &options.helpers {
            let path = resolve_helper_path(&helper.path, workspace, path_variable)?;
            match &helper.scope {
                None => credentials.default_helper = Some(path),
                Some(scope) => match scope.strip_prefix("*.") {
                    Some(suffix) => {
                        credentials.suffixes.insert(suffix.to_owned(), path);
                    }
                    None => {
                        credentials.hosts.insert(scope.clone(), path);
                    }
                },
            }
        }
        Ok(credentials)
    }

    pub fn is_empty(&self) -> bool {
        self.default_helper.is_none() && self.hosts.is_empty() && self.suffixes.is_empty()
    }

    /// CredentialHelperProvider.findCredentialHelper: exact host, then the
    /// nearest `*.` suffix (the domain itself included), then the default.
    pub fn helper_for(&self, uri: &str) -> Option<&Path> {
        let Some(host) = uri_host(uri) else {
            return self.default_helper.as_deref();
        };
        if let Some(path) = self.hosts.get(host) {
            return Some(path);
        }
        let mut domain = host;
        loop {
            if let Some(path) = self.suffixes.get(domain) {
                return Some(path);
            }
            match domain.split_once('.') {
                Some((_, parent)) => domain = parent,
                None => break,
            }
        }
        self.default_helper.as_deref()
    }

    /// CredentialHelperCredentials.getRequestMetadata without a fallback:
    /// `Ok(None)` when no helper applies to `uri`.
    pub fn headers(&self, uri: &str) -> Result<Option<Vec<(String, String)>>, HelperError> {
        let cached = || {
            let now = Instant::now();
            self.cache
                .lock()
                .expect("cache lock")
                .get(uri)
                .filter(|(_, expiry)| *expiry > now)
                .map(|(response, _)| response.pairs())
        };
        if let Some(pairs) = cached() {
            return Ok(Some(pairs));
        }
        let Some(helper) = self.helper_for(uri) else {
            return Ok(None);
        };
        let lock = self
            .loading
            .lock()
            .expect("loading lock")
            .entry(uri.to_owned())
            .or_default()
            .clone();
        let _loading = lock.lock().expect("uri lock");
        if let Some(pairs) = cached() {
            return Ok(Some(pairs));
        }
        let now = Instant::now();
        self.invocations
            .lock()
            .expect("invocation lock")
            .push(uri.to_owned());
        let response = invoke(helper, uri, self.workspace.as_deref(), self.timeout)?;
        // CredentialCacheExpiry: the response's expiry, else the default.
        let lifetime = match response.expires {
            Some(expires) => {
                let now_seconds = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                Duration::from_secs(expires.saturating_sub(now_seconds).max(0) as u64)
            }
            None => self.cache_duration,
        };
        let pairs = response.pairs();
        if !lifetime.is_zero() {
            self.cache
                .lock()
                .expect("cache lock")
                .insert(uri.to_owned(), (response, now + lifetime));
        }
        Ok(Some(pairs))
    }

    /// CredentialHelperCredentials.refresh: drops every cached response.
    pub fn refresh(&self) {
        self.cache.lock().expect("cache lock").clear();
    }

    /// The URIs the helpers were invoked for, in order (tests).
    pub fn invocations(&self) -> Vec<String> {
        self.invocations.lock().expect("invocation lock").clone()
    }
}

static GLOBAL: OnceLock<Credentials> = OnceLock::new();

/// Installs the invocation's credentials (once, from main).
pub fn install(credentials: Credentials) {
    let _ = GLOBAL.set(credentials);
}

/// The invocation's credentials, if installed.
pub fn global() -> Option<&'static Credentials> {
    GLOBAL.get()
}

/// The downloader's per-URL headers (HttpConnectorMultiplexer.getHeaderFunction,
/// :133-151): helper headers; a helper failure is a warning and the request
/// continues without credentials. Empty when no helper applies.
pub fn download_headers(url: &str) -> Vec<(String, String)> {
    download_credentials(url).unwrap_or_default()
}

/// CredentialHelperCredentials.getRequestMetadata (credentialhelper/
/// CredentialHelperCredentials.java:61-79) as the downloader sees it:
/// `Some` when a helper answers for `url` (its headers replace the static
/// `auth` fallback entirely), `Some(empty)` after a helper failure (warned;
/// getHeaderFunction continues without any credential headers), `None` when
/// no helper applies and the static credentials (repository_ctx/module_ctx
/// `auth`) are used instead.
pub fn download_credentials(url: &str) -> Option<Vec<(String, String)>> {
    let credentials = global()?;
    match credentials.headers(url) {
        Ok(headers) => headers,
        Err(error) => {
            crate::note!("WARNING: Error retrieving auth headers, continuing without: {error}");
            Some(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    include!("../tests/support/executable_script.rs");

    // Never fs::write a helper this process then executes (ETXTBSY race).
    fn helper(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        write_executable_script(&path, format!("#!/bin/bash\n{body}\n"), 0o755);
        path
    }

    #[test]
    fn scopes_follow_the_converter() {
        let ok = |input: &str| parse_helper_option(input).unwrap();
        assert_eq!(
            ok("example.com=/h"),
            HelperOption {
                scope: Some("example.com".into()),
                path: "/h".into()
            }
        );
        assert_eq!(
            ok("*.Example.com=/h").scope.as_deref(),
            Some("*.Example.com")
        );
        assert_eq!(ok("%workspace%/h").scope, None);
        assert_eq!(ok("127.0.0.1=h").scope.as_deref(), Some("127.0.0.1"));
        for (input, error) in [
            ("=x", "Credential helper scope must not be empty"),
            ("a=", "Credential helper path must not be empty"),
            ("", "Credential helper path must not be empty"),
            (
                "*=x",
                "Credential helper scope '*' must be a valid domain name with an optional leading '*.' wildcard",
            ),
            (
                "a..b=x",
                "Credential helper scope 'a..b' must be a valid domain name with an optional leading '*.' wildcard",
            ),
            (
                "*.=x",
                "Credential helper scope '*.' must be a valid domain name with an optional leading '*.' wildcard",
            ),
            (
                "-a.com=x",
                "Credential helper scope '-a.com' must be a valid domain name with an optional leading '*.' wildcard",
            ),
        ] {
            assert_eq!(parse_helper_option(input), Err(error.into()), "{input}");
        }
        assert!(
            parse_helper_option("bücher.de=x")
                .unwrap_err()
                .contains("not supported natively")
        );
    }

    #[test]
    fn paths_resolve_like_command_line_path_factory() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        assert_eq!(
            resolve_helper_path("%workspace%/bin/h", Some(ws), ""),
            Ok(ws.join("bin/h"))
        );
        assert_eq!(
            resolve_helper_path("%workspace%//bin/./h", Some(ws), ""),
            Ok(ws.join("bin/h"))
        );
        assert_eq!(
            resolve_helper_path("//usr//h", None, ""),
            Ok("/usr/h".into())
        );
        assert_eq!(
            resolve_helper_path("bin/helper", Some(ws), ""),
            Err(
                "Path 'bin/helper' must either be absolute or not contain any path separators"
                    .into()
            )
        );
        assert_eq!(
            resolve_helper_path("%workspace%/../x", Some(ws), ""),
            Err("Path '%workspace%/../x' must not contain any uplevel references ('..')".into())
        );
        assert_eq!(
            resolve_helper_path("%foo%/x", Some(ws), ""),
            Err("Unknown root foo".into())
        );
        assert_eq!(
            resolve_helper_path("nohelper", Some(ws), "/usr/bin:/bin"),
            Err("Could not find file with name 'nohelper' on PATH '/usr/bin:/bin'".into())
        );
        let found = helper(ws, "found", "true");
        assert_eq!(
            resolve_helper_path("found", None, &format!("rel::{}", ws.display())),
            Ok(found)
        );
    }

    #[test]
    fn matching_prefers_host_then_nearest_wildcard_then_default() {
        let dir = tempfile::tempdir().unwrap();
        let options = CredentialOptions {
            helpers: [
                "/default",
                "a.example.com=/exact",
                "*.example.com=/wild",
                "*.b.example.com=/b",
            ]
            .iter()
            .map(|value| parse_helper_option(value).unwrap())
            .collect(),
            ..CredentialOptions::default()
        };
        let credentials = Credentials::new(&options, Some(dir.path()), "").unwrap();
        for (uri, helper) in [
            ("https://a.example.com/x", "/exact"),
            ("https://example.com:443", "/wild"),
            ("https://c.a.example.com/x", "/wild"),
            ("https://x.b.example.com/x", "/b"),
            ("https://b.example.com", "/b"),
            ("https://user:pw@other.org:8/x", "/default"),
            ("unix:///socket", "/default"),
            ("https://A.example.com/x", "/wild"),
        ] {
            assert_eq!(
                credentials.helper_for(uri),
                Some(Path::new(helper)),
                "{uri}"
            );
        }
        assert_eq!(uri_host("http://[::1]:8080/x"), Some("[::1]"));
    }

    #[test]
    fn protocol_cache_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let good = helper(
            dir.path(),
            "good",
            &format!(
                "printf '%s|%s|%s\\n' \"$1\" \"$PWD\" \"$(cat)\" >> {}\necho '{{\"headers\":{{\"Authorization\":[\"Bearer t\"],\"X\":[\"a\",\"b\"]}},\"ignored\":1}}'",
                log.display()
            ),
        );
        let options = CredentialOptions {
            helpers: vec![HelperOption {
                scope: None,
                path: good.display().to_string(),
            }],
            ..CredentialOptions::default()
        };
        let credentials = Credentials::new(&options, Some(dir.path()), "").unwrap();
        let expected = vec![
            ("Authorization".to_owned(), "Bearer t".to_owned()),
            ("X".into(), "a".into()),
            ("X".into(), "b".into()),
        ];
        for _ in 0..2 {
            assert_eq!(
                credentials.headers("http://h:1/a=b").unwrap(),
                Some(expected.clone())
            );
        }
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            format!(
                "get|{}|{{\"uri\":\"http://h:1/a\\u003db\"}}\n",
                dir.path().display()
            )
        );
        let count = std::cell::Cell::new(0);
        let invoke_with = |body: &str, timeout: Duration| {
            count.set(count.get() + 1);
            let path = helper(dir.path(), &format!("h{}", count.get()), body);
            invoke(&path, "https://x", Some(dir.path()), timeout)
                .unwrap_err()
                .0
                .replace(&path.display().to_string(), "H")
        };
        let second = Duration::from_secs(5);
        assert_eq!(
            invoke_with("echo oops >&2; exit 3", second),
            "Failed to get credentials for 'https://x' from helper 'H': process exited with code 3. stderr: oops\n"
        );
        assert_eq!(
            invoke_with("echo '{bad'", second),
            "Failed to get credentials for 'https://x' from helper 'H': error parsing output. stderr: "
        );
        assert_eq!(
            invoke_with("cat >/dev/null", second),
            "Failed to get credentials for 'https://x' from helper 'H': process exited without output. stderr: "
        );
        assert_eq!(
            invoke_with("echo '{\"headers\":{\"a\":\"b\"}}'", second),
            "Failed to get credentials for 'https://x' from helper 'H': error parsing output. stderr: "
        );
        assert_eq!(
            invoke_with("sleep 5", Duration::from_millis(100)),
            "Failed to get credentials for 'https://x' from helper 'H': process timed out"
        );
    }

    #[test]
    fn descendants_holding_pipes_cannot_hang_the_invocation() {
        let dir = tempfile::tempdir().unwrap();
        let timeout = Duration::from_millis(300);
        for (name, body) in [
            // A grandchild in the helper's group keeps stdout open: killed
            // with the group at the deadline.
            ("grandchild", "sleep 30 &\necho '{\"headers\":{}}'"),
            // One that escaped the group (setsid): its reader is detached.
            ("escaped", "setsid sleep 3 &\necho '{\"headers\":{}}'"),
            // The helper itself hangs: the group is killed.
            ("hang", "sleep 30 | cat"),
        ] {
            let path = helper(dir.path(), name, body);
            let start = Instant::now();
            let error = invoke(&path, "https://x", Some(dir.path()), timeout).unwrap_err();
            assert!(error.0.ends_with("process timed out"), "{name}: {error}");
            // Below the 30 s the hanging fixtures sleep (an implementation
            // waiting for them still fails), above a loaded host's delays.
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "{name}: {:?}",
                start.elapsed()
            );
        }
        // A helper whose short-lived descendant (like jq) writes the output.
        let path = helper(
            dir.path(),
            "piped",
            "echo '{\"headers\":{\"A\":[\"b\"]}}' | cat",
        );
        assert_eq!(
            invoke(&path, "https://x", Some(dir.path()), Duration::from_secs(5))
                .unwrap()
                .pairs(),
            [("A".to_owned(), "b".to_owned())]
        );
    }

    #[test]
    fn expires_parses_rfc3339_like_java_time() {
        assert_eq!(parse_expires("1970-01-01T00:00:00Z"), Ok(0));
        assert_eq!(
            parse_expires("2026-10-17T12:00:00+02:00"),
            Ok(1_792_231_200)
        );
        assert_eq!(parse_expires("1970-13-01T00:00:00Z"), Ok(365 * 86_400));
        assert_eq!(
            parse_expires("2026-10-17T12:00:00.5Z"),
            Err("Text '2026-10-17T12:00:00.5Z' could not be parsed at index 19".into())
        );
        assert_eq!(
            parse_expires("2026-10-17"),
            Err("Text '2026-10-17' could not be parsed at index 10".into())
        );
    }

    #[test]
    fn durations_follow_the_converter() {
        assert_eq!(parse_duration("0"), Ok(Duration::ZERO));
        assert_eq!(parse_duration("30m"), Ok(Duration::from_secs(1800)));
        assert_eq!(parse_duration("5ms"), Ok(Duration::from_millis(5)));
        assert_eq!(parse_duration("10"), Err("Illegal duration '10'.".into()));
        assert_eq!(parse_duration("s"), Err("Illegal duration 's'.".into()));
        let mut options = CredentialOptions::default();
        assert_eq!(
            options.parse("--credential_helper_timeout=5s", None),
            Some(Ok(false))
        );
        assert_eq!(options.timeout, Duration::from_secs(5));
        assert_eq!(
            options.parse("--experimental_credential_helper", Some("x.com=/h")),
            Some(Ok(true))
        );
        assert_eq!(options.helpers.len(), 1);
        assert_eq!(
            options.parse("--credential_helper_cache_duration=x", None),
            Some(Err(
                "While parsing option --credential_helper_cache_duration=x: Illegal duration 'x'."
                    .into()
            ))
        );
        assert_eq!(options.parse("--remote_cache=x", None), None);
    }
}
