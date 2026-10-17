//! Proxy selection of Bazel 9.3's downloader (`ProxyHelper`,
//! `src/main/java/com/google/devtools/build/lib/bazel/repository/downloader/ProxyHelper.java`
//! in the pinned source) over the command's non-strict repository environment:
//! the client environment with `--repo_env` applied
//! (runtime/CommandEnvironment.java:342-416). Registry reads
//! (bazel/bzlmod/IndexRegistry.java:235), archive sources and
//! `repository_ctx`/`module_ctx` downloads
//! (bazel/repository/starlark/StarlarkBaseExternalContext.java:864-873) all
//! pass that map to the downloader, which builds its `ProxyHelper` from it
//! (HttpDownloader.java:280-282) and asks it for every connection, redirects
//! included (HttpConnector.java:125-136).
//!
//! The environment is installed once per process by the command-line entry
//! point. Without an installed environment (library use and unit tests) every
//! connection is direct, the behavior before rc evaluation.
//!
//! Java system properties (`http.nonProxyHosts`, `https.proxyHost`, …) are
//! never set for the native process, which matches a Bazel server started
//! without `--host_jvm_args` defining them. SOCKS proxies fail explicitly: the
//! HTTP transport has no SOCKS support. Proxy values are never printed (they
//! may carry credentials); Bazel's messages are kept with the address
//! redacted where Bazel would echo it with credentials.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::sync::OnceLock;

/// Environment variables that select a proxy for a Git child process.
/// Bazel's `git_repository` runs `git` through `repository_ctx.execute`,
/// whose environment is the repository environment, so Git (curl) sees the
/// same proxy variables (tools/build_defs/repo/git_worker.bzl).
const GIT_PROXY_VARIABLES: [&str; 8] = [
    "http_proxy",
    "HTTP_PROXY",
    "https_proxy",
    "HTTPS_PROXY",
    "all_proxy",
    "ALL_PROXY",
    "no_proxy",
    "NO_PROXY",
];

/// The proxy-relevant part of one command's repository environment.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProxyEnvironment {
    /// ProxyHelper.java:79-82: `no_proxy`, or `NO_PROXY` when that is unset
    /// or empty.
    no_proxy: Option<String>,
    /// ProxyHelper.java:131-140: the first non-null of `https_proxy`,
    /// `HTTPS_PROXY` (an empty `https_proxy` still wins).
    https_proxy: Option<String>,
    /// ProxyHelper.java:160-169: `http_proxy`, then `HTTP_PROXY`.
    http_proxy: Option<String>,
    /// The Git proxy variables, for the Git child process.
    git: Vec<(OsString, OsString)>,
}

impl fmt::Debug for ProxyEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProxyEnvironment(<redacted>)")
    }
}

impl ProxyEnvironment {
    /// The proxy settings of a repository environment map.
    pub fn from_environment<'a>(
        environment: impl IntoIterator<Item = (&'a OsStr, &'a OsStr)>,
    ) -> Self {
        let map: BTreeMap<&OsStr, &OsStr> = environment.into_iter().collect();
        let get = |name: &str| {
            map.get(OsStr::new(name))
                .map(|value| value.to_string_lossy().into_owned())
        };
        let no_proxy = match get("no_proxy") {
            Some(value) if !value.is_empty() => Some(value),
            _ => get("NO_PROXY"),
        };
        let git = GIT_PROXY_VARIABLES
            .iter()
            .filter_map(|name| {
                map.get(OsStr::new(name))
                    .map(|value| (OsString::from(name), value.to_os_string()))
            })
            .collect();
        Self {
            no_proxy,
            https_proxy: get("https_proxy").or_else(|| get("HTTPS_PROXY")),
            http_proxy: get("http_proxy").or_else(|| get("HTTP_PROXY")),
            git,
        }
    }

    /// ProxyHelper.createProxyIfNeeded (ProxyHelper.java:73-188) for a URL
    /// with this scheme and host (as `java.net.URI.getHost` returns it).
    pub fn route(&self, scheme: &str, host: &str) -> Result<Route, String> {
        if let Some(no_proxy) = self.no_proxy.as_deref().filter(|value| !value.is_empty()) {
            for entry in java_split_comma(no_proxy) {
                let exempt = if entry.starts_with('.') {
                    // Sub-domains only.
                    host.ends_with(entry)
                } else {
                    host == entry || host.ends_with(&format!(".{entry}"))
                };
                if exempt {
                    return Ok(Route::Direct);
                }
            }
        }
        // HttpUtils.isProtocol compares the scheme case-insensitively
        // (HttpUtils.java:40-44).
        let address = if scheme.eq_ignore_ascii_case("https") {
            self.https_proxy.as_deref()
        } else if scheme.eq_ignore_ascii_case("http") {
            self.http_proxy.as_deref()
        } else {
            None
        };
        create_proxy_info(address)
    }

    /// The Git proxy variables of this environment.
    pub fn git_environment(&self) -> &[(OsString, OsString)] {
        &self.git
    }
}

/// `String.split("\\s*,\\s*")` (Java regex `\s` is `[ \t\n\x0B\f\r]`;
/// trailing empty strings are removed).
fn java_split_comma(text: &str) -> Vec<&str> {
    let space = |c: char| matches!(c, ' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r');
    let pieces = text.split(',').collect::<Vec<_>>();
    let last = pieces.len() - 1;
    let mut entries = pieces
        .iter()
        .enumerate()
        .map(|(index, piece)| {
            let piece = if index > 0 {
                piece.trim_start_matches(space)
            } else {
                piece
            };
            if index < last {
                piece.trim_end_matches(space)
            } else {
                piece
            }
        })
        .collect::<Vec<_>>();
    // A single piece (no separator) is the whole input, never removed.
    if entries.len() > 1 {
        while entries.last() == Some(&"") {
            entries.pop();
        }
    }
    entries
}

/// Where one connection goes (ProxyInfo).
#[derive(Clone, PartialEq, Eq)]
pub enum Route {
    /// ProxyInfo.NO_PROXY.
    Direct,
    /// `Proxy.Type.HTTP`: plain HTTP to the proxy (Java never speaks TLS to
    /// an HTTP proxy, also for an `https://` proxy address).
    Http {
        host: String,
        port: u16,
        credentials: Option<(String, String)>,
    },
    /// `Proxy.Type.SOCKS` (unsupported natively).
    Socks { address: String },
}

impl fmt::Debug for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct => f.write_str("Direct"),
            Self::Http { .. } => f.write_str("Http(<redacted>)"),
            Self::Socks { .. } => f.write_str("Socks(<redacted>)"),
        }
    }
}

/// The groups of ProxyHelper.URL_PATTERN
/// `^(https?://|socks[45]?://)?(([^:@]+?)(?::([^@]+?))?@)?([^:]+)(?::(\d+))?/?$`
/// (ProxyHelper.java:333-335), found in the regex engine's priority order.
#[derive(Debug, PartialEq, Eq)]
struct Groups<'a> {
    protocol: Option<&'a str>,
    id_and_password: Option<&'a str>,
    user: Option<&'a str>,
    password: Option<&'a str>,
    host: &'a str,
    port: Option<&'a str>,
}

fn match_url_pattern(text: &str) -> Option<Groups<'_>> {
    let protocol = ["https://", "http://", "socks5://", "socks4://", "socks://"]
        .into_iter()
        .find(|prefix| text.starts_with(prefix));
    for protocol in protocol.into_iter().map(Some).chain([None]) {
        let rest = &text[protocol.map_or(0, str::len)..];
        if let Some(groups) = match_authority(rest) {
            return Some(Groups { protocol, ..groups });
        }
    }
    None
}

/// `(([^:@]+?)(?::([^@]+?))?@)?([^:]+)(?::(\d+))?/?$` on `rest`.
fn match_authority(rest: &str) -> Option<Groups<'_>> {
    let boundaries = || rest.char_indices().map(|(index, _)| index).skip(1);
    // The optional user-info group is tried first; the lazy user grows one
    // character at a time; the optional password is tried before its absence.
    for user_end in boundaries().chain([rest.len()]) {
        let user = &rest[..user_end];
        if user.contains([':', '@']) {
            break;
        }
        let after = &rest[user_end..];
        if let Some(password_text) = after.strip_prefix(':') {
            let start = user_end + 1;
            for (offset, _) in password_text.char_indices().skip(1) {
                let password = &password_text[..offset];
                if password.contains('@') {
                    break;
                }
                if password_text[offset..].starts_with('@')
                    && let Some(host) = match_host(&rest[start + offset + 1..])
                {
                    return Some(Groups {
                        id_and_password: Some(&rest[..start + offset + 1]),
                        user: Some(user),
                        password: Some(password),
                        ..host
                    });
                }
            }
        }
        if after.starts_with('@')
            && let Some(host) = match_host(&after[1..])
        {
            return Some(Groups {
                id_and_password: Some(&rest[..user_end + 1]),
                user: Some(user),
                ..host
            });
        }
    }
    match_host(rest)
}

/// `([^:]+)(?::(\d+))?/?$`.
fn match_host(text: &str) -> Option<Groups<'_>> {
    let host_end = text.find(':').unwrap_or(text.len());
    // Greedy host: the longest candidate first.
    let mut ends = text[..host_end]
        .char_indices()
        .map(|(index, c)| index + c.len_utf8())
        .collect::<Vec<_>>();
    ends.reverse();
    for end in ends {
        let host = &text[..end];
        let remainder = &text[end..];
        if let Some(port_text) = remainder.strip_prefix(':') {
            let digits = port_text.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 && matches!(&port_text[digits..], "" | "/") {
                return Some(Groups {
                    protocol: None,
                    id_and_password: None,
                    user: None,
                    password: None,
                    host,
                    port: Some(&port_text[..digits]),
                });
            }
        }
        if matches!(remainder, "" | "/") {
            return Some(Groups {
                protocol: None,
                id_and_password: None,
                user: None,
                password: None,
                host,
                port: None,
            });
        }
    }
    None
}

/// ProxyHelper.createProxyInfo(address, null, null) (ProxyHelper.java:213-330).
fn create_proxy_info(address: Option<&str>) -> Result<Route, String> {
    let Some(address) = address.filter(|address| !address.is_empty()) else {
        return Ok(Route::Direct);
    };
    let Some(groups) = match_url_pattern(address) else {
        // Bazel echoes the whole address, credentials included.
        return Err("Proxy address <redacted> is not a valid URL".to_owned());
    };
    let clean = match groups.id_and_password {
        Some(id) => address.replace(id, ""),
        None => address.to_owned(),
    };
    let (socks, default_port) = match groups.protocol {
        Some("https://") => (false, 443),
        Some("http://") | None => (false, 80),
        Some(_) => (true, 1080),
    };
    let port = match groups.port {
        // Integer.parseInt.
        Some(digits) => digits
            .parse::<i32>()
            .map_err(|_| format!("Error parsing proxy port: {clean}"))?,
        None => default_port,
    };
    // new InetSocketAddress(hostname, port): IllegalArgumentException, which
    // HttpConnector reports as is (HttpConnector.java:187-189).
    let port = u16::try_from(port).map_err(|_| format!("port out of range:{port}"))?;
    let credentials = match (groups.user, groups.password) {
        (Some(_), None) => return Err(format!("No password given for proxy {clean}")),
        (Some(user), Some(password)) => Some((url_decode(user)?, url_decode(password)?)),
        _ => None,
    };
    if socks {
        return Ok(Route::Socks { address: clean });
    }
    Ok(Route::Http {
        host: groups.host.to_owned(),
        port,
        credentials,
    })
}

/// `URLDecoder.decode(text, UTF_8)`: `+` is a space, `%XX` bytes are
/// UTF-8-decoded (malformed sequences become U+FFFD), malformed escapes fail.
fn url_decode(text: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut bytes = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        match c {
            '%' => {
                let hex = text.get(index + 1..index + 3);
                let Some(value) = hex.and_then(|hex| {
                    hex.bytes()
                        .all(|b| b.is_ascii_hexdigit())
                        .then(|| u8::from_str_radix(hex, 16).ok())
                        .flatten()
                }) else {
                    return Err(if text.len() < index + 3 {
                        "URLDecoder: Incomplete trailing escape (%) pattern".to_owned()
                    } else {
                        "URLDecoder: Illegal hex characters in escape (%) pattern".to_owned()
                    });
                };
                bytes.push(value);
                chars.next();
                chars.next();
                continue;
            }
            _ => {
                if !bytes.is_empty() {
                    out.push_str(&String::from_utf8_lossy(&bytes));
                    bytes.clear();
                }
                out.push(if c == '+' { ' ' } else { c });
            }
        }
    }
    out.push_str(&String::from_utf8_lossy(&bytes));
    Ok(out)
}

static INSTALLED: OnceLock<ProxyEnvironment> = OnceLock::new();

/// Install the command's proxy environment (once per process; the
/// command-line entry point calls this before any download).
pub fn install(environment: ProxyEnvironment) -> Result<(), String> {
    INSTALLED
        .set(environment)
        .map_err(|_| "the download proxy environment is already installed".to_owned())
}

fn installed() -> Option<&'static ProxyEnvironment> {
    INSTALLED.get()
}

/// The route of one request, or the downloader's error for it.
pub(crate) fn route(url: &reqwest::Url) -> Result<Route, String> {
    let Some(environment) = installed() else {
        return Ok(Route::Direct);
    };
    environment.route(url.scheme(), url.host_str().unwrap_or(""))
}

/// The proxy URL of an HTTP route, `None` when the host is no valid URL host
/// (Java resolves it as given and fails with UnknownHostException).
fn proxy_url(host: &str, port: u16) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(&format!("http://{host}:{port}")).ok()?;
    (url.host_str() == Some(host) && url.path() == "/").then_some(url)
}

/// Fail a request whose proxy Bazel would reject, before it is sent.
pub(crate) fn check(url: &reqwest::Url) -> Result<(), String> {
    match route(url)? {
        Route::Socks { .. } => Err(
            "SOCKS proxies (socks://, socks4://, socks5://) are not supported by the native downloader"
                .to_owned(),
        ),
        // HttpConnector.java:180-185.
        Route::Http { host, port, .. } if proxy_url(&host, port).is_none() => {
            Err(format!("Unknown host: {host}"))
        }
        Route::Direct | Route::Http { .. } => Ok(()),
    }
}

/// Route a client's requests like ProxyHelper. The builder must already have
/// `no_proxy()` (no system proxy discovery). A request whose route is an
/// error must be rejected by [`check`] first; such a request is sent
/// directly only if it was not checked.
pub(crate) fn configure(
    builder: reqwest::blocking::ClientBuilder,
) -> reqwest::blocking::ClientBuilder {
    if installed().is_none() {
        return builder;
    }
    let mut builder = builder;
    for scheme in ["https", "http"] {
        // One custom proxy per scheme carries that scheme's credentials.
        let credentials = match installed().map(|environment| {
            create_proxy_info(match scheme {
                "https" => environment.https_proxy.as_deref(),
                _ => environment.http_proxy.as_deref(),
            })
        }) {
            Some(Ok(Route::Http { credentials, .. })) => credentials,
            _ => None,
        };
        let mut proxy = reqwest::Proxy::custom(move |url| {
            if !url.scheme().eq_ignore_ascii_case(scheme) {
                return None;
            }
            match route(url) {
                Ok(Route::Http { host, port, .. }) => proxy_url(&host, port),
                _ => None,
            }
        });
        if let Some((user, password)) = credentials {
            proxy = proxy.basic_auth(&user, &password);
        }
        builder = builder.proxy(proxy);
    }
    builder
}

/// The proxy variables a Git child process receives.
pub(crate) fn git_environment() -> &'static [(OsString, OsString)] {
    installed().map_or(&[], ProxyEnvironment::git_environment)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(pairs: &[(&str, &str)]) -> ProxyEnvironment {
        let owned = pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect::<Vec<_>>();
        ProxyEnvironment::from_environment(
            owned.iter().map(|(k, v)| (k.as_os_str(), v.as_os_str())),
        )
    }

    fn http(host: &str, port: u16) -> Route {
        Route::Http {
            host: host.into(),
            port,
            credentials: None,
        }
    }

    #[test]
    fn variables_follow_proxy_helper_precedence() {
        let env = environment(&[
            ("HTTPS_PROXY", "http://upper:1"),
            ("https_proxy", "http://lower:2"),
            ("HTTP_PROXY", "plain:3"),
        ]);
        assert_eq!(env.route("https", "x.test"), Ok(http("lower", 2)));
        assert_eq!(env.route("HTTP", "x.test"), Ok(http("plain", 3)));
        assert_eq!(env.route("ftp", "x.test"), Ok(Route::Direct));
        // An empty lowercase variable is non-null and selects no proxy.
        let env = environment(&[("https_proxy", ""), ("HTTPS_PROXY", "http://upper:1")]);
        assert_eq!(env.route("https", "x.test"), Ok(Route::Direct));
        // An empty no_proxy falls back to NO_PROXY.
        let env = environment(&[
            ("https_proxy", "p:8"),
            ("no_proxy", ""),
            ("NO_PROXY", "x.test"),
        ]);
        assert_eq!(env.route("https", "x.test"), Ok(Route::Direct));
    }

    #[test]
    fn no_proxy_entries_match_like_bazel() {
        let env = environment(&[
            ("https_proxy", "https://p"),
            ("NO_PROXY", "your-storagebox.de , .sub.test,,exact.test"),
        ]);
        for (host, direct) in [
            ("your-storagebox.de", true),
            ("u1.your-storagebox.de", true),
            ("xyour-storagebox.de", false),
            ("a.sub.test", true),
            ("sub.test", false),
            ("exact.test", true),
            ("other.test", false),
        ] {
            let expected = if direct {
                Route::Direct
            } else {
                http("p", 443)
            };
            assert_eq!(env.route("https", host), Ok(expected), "{host}");
        }
        assert_eq!(java_split_comma("a , b,,c,,"), ["a", "b", "", "c"]);
        assert_eq!(java_split_comma(" a"), [" a"]);
        assert_eq!(java_split_comma(",a"), ["", "a"]);
        // No `*` wildcard (unlike curl).
        let env = environment(&[("http_proxy", "p"), ("no_proxy", "*")]);
        assert_eq!(env.route("http", "x"), Ok(http("p", 80)));
    }

    #[test]
    fn addresses_parse_with_url_pattern_semantics() {
        let route = |address: &str| create_proxy_info(Some(address));
        assert_eq!(
            route("http://prod-egress-proxy.tail70a9fd.ts.net:8080"),
            Ok(http("prod-egress-proxy.tail70a9fd.ts.net", 8080))
        );
        // `([^:]+)` is greedy: a trailing slash without a port stays in
        // the host, which then cannot be resolved.
        assert_eq!(route("https://p/"), Ok(http("p/", 443)));
        assert_eq!(route("https://p:8443/"), Ok(http("p", 8443)));
        assert_eq!(route("p"), Ok(http("p", 80)));
        assert_eq!(
            route("http://u%40x:p+w%21@h:9"),
            Ok(Route::Http {
                host: "h".into(),
                port: 9,
                credentials: Some(("u@x".into(), "p w!".into())),
            })
        );
        assert_eq!(
            route("http://user@h:9"),
            Err("No password given for proxy http://h:9".into())
        );
        for invalid in ["ftp://h", "http://u:pw@h:notaport", "h:1:2"] {
            assert_eq!(
                route(invalid),
                Err("Proxy address <redacted> is not a valid URL".into())
            );
        }
        // URL_PATTERN without a protocol takes `ftp` as the user.
        assert_eq!(
            route("ftp://user:pw@h"),
            Ok(Route::Http {
                host: "h".into(),
                port: 80,
                credentials: Some(("ftp".into(), "//user:pw".into())),
            })
        );
        assert_eq!(
            route("h:99999999999"),
            Err("Error parsing proxy port: h:99999999999".into())
        );
        assert_eq!(route("h:70000"), Err("port out of range:70000".into()));
        assert_eq!(
            route("socks5://s:1"),
            Ok(Route::Socks {
                address: "socks5://s:1".into()
            })
        );
        assert_eq!(route(""), Ok(Route::Direct));
        // Regex priority: without a valid user-info after the protocol, the
        // protocol-less alternative takes `http` as the user.
        assert_eq!(
            match_url_pattern("http://:pw@h").map(|g| (g.protocol, g.user, g.password, g.host)),
            Some((None, Some("http"), Some("//:pw"), "h"))
        );
        assert_eq!(
            match_url_pattern("http://a:b:c@h:1").map(|g| (g.user, g.password, g.host, g.port)),
            Some((Some("a"), Some("b:c"), "h", Some("1")))
        );
        assert_eq!(
            url_decode("%4"),
            Err("URLDecoder: Incomplete trailing escape (%) pattern".into())
        );
        assert_eq!(
            url_decode("%zz"),
            Err("URLDecoder: Illegal hex characters in escape (%) pattern".into())
        );
        assert_eq!(url_decode("%C3%A9"), Ok("é".into()));
    }

    /// ProxyHelper.createProxyIfNeeded (ProxyHelper.java:74-206), item by item.
    #[test]
    fn create_proxy_if_needed_details() {
        let proxied = |env: &ProxyEnvironment, scheme: &str, host: &str| {
            env.route(scheme, host).unwrap() != Route::Direct
        };
        // (a) `no_proxy` is read before `NO_PROXY` (:79-82): a non-empty
        // lowercase list hides the uppercase one; an empty one falls back.
        let env = environment(&[
            ("https_proxy", "p"),
            ("no_proxy", "a.test"),
            ("NO_PROXY", "b.test"),
        ]);
        assert!(!proxied(&env, "https", "a.test"));
        assert!(proxied(&env, "https", "b.test"));
        let env = environment(&[
            ("https_proxy", "p"),
            ("no_proxy", ""),
            ("NO_PROXY", "b.test"),
        ]);
        assert!(!proxied(&env, "https", "b.test"));
        // (b) `\s*,\s*` entries (:84); a leading '.' matches sub-domains only
        // by endsWith (:87-91); otherwise the exact host or `.entry` suffix
        // (:93-97). No `*` wildcard, no port, no CIDR, case-sensitive.
        let env = environment(&[
            ("http_proxy", "p"),
            (
                "NO_PROXY",
                " exact.test\t,\n.sub.test , *.star.test,port.test:8080,10.0.0.0/8,Case.test",
            ),
        ]);
        for (host, direct) in [
            (" exact.test", true), // leading whitespace of the whole list is kept
            ("exact.test", false),
            ("a.sub.test", true),
            ("sub.test", false),
            ("xsub.test", false),
            ("a.star.test", false),
            ("port.test", false),
            ("10.1.2.3", false),
            ("Case.test", true),
            ("case.test", false),
            ("www.Case.test", true),
        ] {
            assert_eq!(!proxied(&env, "http", host), direct, "{host:?}");
        }
        let env = environment(&[("http_proxy", "p"), ("NO_PROXY", "exact.test")]);
        assert!(!proxied(&env, "http", "exact.test"));
        assert!(!proxied(&env, "http", "www.exact.test"));
        assert!(proxied(&env, "http", "notexact.test"));
        // (c) the variable follows the request scheme (:130-188); the
        // lowercase variable wins whenever it is set, also when empty, and an
        // empty address is no proxy (:213-215).
        let env = environment(&[
            ("https_proxy", ""),
            ("HTTPS_PROXY", "upper-https"),
            ("http_proxy", ""),
            ("HTTP_PROXY", "upper-http"),
        ]);
        assert_eq!(env.route("https", "x"), Ok(Route::Direct));
        assert_eq!(env.route("http", "x"), Ok(Route::Direct));
        let env = environment(&[("HTTPS_PROXY", "s"), ("HTTP_PROXY", "h")]);
        assert_eq!(env.route("https", "x"), Ok(http("s", 80)));
        assert_eq!(env.route("http", "x"), Ok(http("h", 80)));
        let env = environment(&[("HTTPS_PROXY", "s")]);
        assert_eq!(env.route("http", "x"), Ok(Route::Direct));
        // (d) a user without a password (:282-284), printed without the
        // removed user-info like Bazel's cleanProxyAddress (:240-244).
        let env = environment(&[("https_proxy", "http://user@proxy:3128")]);
        assert_eq!(
            env.route("https", "x"),
            Err("No password given for proxy http://proxy:3128".into())
        );
        // NO_PROXY is checked first: an exempt host never parses the proxy.
        let env = environment(&[("https_proxy", "http://user@proxy:3128"), ("NO_PROXY", "x")]);
        assert_eq!(env.route("https", "x"), Ok(Route::Direct));
    }

    #[test]
    fn git_receives_only_proxy_variables() {
        let env = environment(&[("HTTPS_PROXY", "p"), ("NO_PROXY", "n"), ("HOME", "/h")]);
        let names = env
            .git_environment()
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, ["HTTPS_PROXY", "NO_PROXY"]);
        assert_eq!(format!("{env:?}"), "ProxyEnvironment(<redacted>)");
    }

    #[test]
    fn proxy_hosts_round_trip_or_fail() {
        assert!(proxy_url("p", 80).is_some());
        assert!(proxy_url("10.0.0.1", 3128).is_some());
        for bad in ["p/", "@h", "a b"] {
            assert!(proxy_url(bad, 80).is_none(), "{bad}");
        }
    }
}
