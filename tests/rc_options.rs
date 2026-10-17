//! N40: the options of real rc files (~/code's .bazelrc and .bazelrc.coder)
//! through the CLI with rc evaluation (docs/RC_OPTIONS.md). Every scenario
//! runs with an exact environment (no inherited proxy variables), a private
//! HOME and no BAZELRC; proxies are local test servers only.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("rc-options-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        fs::create_dir_all(f.0.path().join("home")).unwrap();
        f.file(
            "ws/MODULE.bazel",
            "module(name='main')\nmake = use_repo_rule('//:defs.bzl','make')\nmake(name='made')\n",
        );
        f.file(
            "ws/defs.bzl",
            "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.file('data.txt', ctx.getenv('N40_VALUE', 'unset') + '|' + ctx.os.environ.get('N40_OTHER', 'none') + '\\n', executable=False)\nmake = repository_rule(_impl)\n",
        );
        f.file(
            "ws/BUILD.bazel",
            "genrule(name='copy',srcs=['@made//:data.txt'],outs=['copy.txt'],cmd='cat $(SRCS) > $@')\ngenrule(name='plain',outs=['plain.txt'],cmd='echo plain > $@')\n",
        );
        f
    }

    fn file(&self, name: &str, data: impl AsRef<[u8]>) {
        let path = self.0.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    fn ws(&self) -> PathBuf {
        self.0.path().join("ws")
    }

    /// Run with rc evaluation (no --ignore_all_rc_files) and exactly
    /// `environment` on top of PATH/HOME.
    fn run_env(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        let cache = format!("--cache-dir={}", self.0.path().join("cache").display());
        let mut command = Command::new("/usr/bin/timeout");
        command
            .args(["60s", env!("CARGO_BIN_EXE_bazel-rs"), &cache])
            .args(args)
            .current_dir(self.ws())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("LC_ALL", "C");
        for (name, value) in environment {
            command.env(name, value);
        }
        command.output().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(args, &[])
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn ok(output: &Output) -> String {
    assert!(output.status.success(), "{}", stderr(output));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn rejected(output: &Output, marker: &str) {
    assert!(!output.status.success(), "unexpected success");
    assert_ne!(output.status.code(), Some(124), "timed out");
    assert!(!stderr(output).contains("panicked"), "{}", stderr(output));
    assert!(stderr(output).contains(marker), "{}", stderr(output));
}

/// The built file of a one-target build.
fn built(output: &Output) -> String {
    let stdout = ok(output);
    let path = stdout
        .lines()
        .next()
        .and_then(|line| line.split_once('\t'))
        .map(|(_, path)| PathBuf::from(path))
        .expect("native build output path");
    fs::read_to_string(path).unwrap()
}

/// ~/code's .bazelrc without its remote-cache lines (N41) and without
/// `--java_runtime_version` (modelled on main by N28b), plus .bazelrc.coder's
/// proxy lines with local values.
const CODE_RC: &str = "build --incompatible_strict_action_env
test --test_output=errors
test --test_summary=terse
common --attempt_to_print_relative_paths=true
build --repo_env=JAVA_HOME=../bazel_tools/jdk
build --macos_sdk_version=10.13
build:dev --noremote_upload_local_results
common --experimental_isolated_extension_usages
common --incompatible_enable_proto_toolchain_resolution
build:linux_amd64 --platforms=//platforms:linux_amd64
build:linux_amd64 --sandbox_add_mount_pair=/tmp
build:buildbarn --remote_executor=grpc://buildbarn.invalid:8980
build:buildbarn --remote_instance_name=dev
build:buildbarn --remote_default_exec_properties=OSFamily=linux
";

#[test]
fn code_rc_options_reach_query_and_build() {
    let f = Fixture::new();
    f.file("ws/.bazelrc", CODE_RC);
    assert_eq!(ok(&f.run(&["query", "//:copy"])), "//:copy\n");
    assert_eq!(built(&f.run(&["build", "//:plain"])), "plain\n");
    assert_eq!(
        built(&f.run(&["build", "--config=dev", "//:plain"])),
        "plain\n"
    );
    let rc = f.ws().join(".bazelrc").display().to_string();
    rejected(
        &f.run(&["build", "--config=linux_amd64", "//:plain"]),
        &format!(
            "unsupported option --sandbox_add_mount_pair from rc file {rc}:11 (config definition 'build:linux_amd64'): sandboxed execution is not implemented"
        ),
    );
    let buildbarn = f.run(&["build", "--config=buildbarn", "//:plain"]);
    rejected(
        &buildbarn,
        &format!(
            "unsupported option --remote_executor from rc file {rc}:12 (config definition 'build:buildbarn'): remote execution is not implemented"
        ),
    );
    assert!(!stderr(&buildbarn).contains("buildbarn.invalid"));
    // The `test` group's options reach N23g's test command (and never
    // build/query): a non-test target is built and Bazel's exit 4 follows.
    let test = f.run(&["test", "//:plain"]);
    assert_eq!(test.status.code(), Some(4), "{}", stderr(&test));
    assert!(
        stderr(&test).contains("ERROR: No test targets were found, yet testing was requested"),
        "{}",
        stderr(&test)
    );
}

#[test]
fn defaults_are_accepted_and_other_values_are_explicit() {
    let f = Fixture::new();
    for (args, marker) in [
        // --noincompatible_strict_action_env is modelled since N42
        // (tests/action_env.rs); query still does not know it.
        (
            &["query", "--noincompatible_strict_action_env", "//:plain"][..],
            "--noincompatible_strict_action_env :: Unrecognized option: --noincompatible_strict_action_env",
        ),
        (
            &[
                "query",
                "--incompatible_enable_proto_toolchain_resolution=false",
                "//:plain",
            ][..],
            "the legacy --proto_compiler",
        ),
        (
            &["query", "--test_output=errors", "//:plain"][..],
            "--test_output=errors :: Unrecognized option: --test_output=errors",
        ),
        (
            &["build", "--test_output=chatty", "//:plain"][..],
            "While parsing option --test_output=chatty: Not a valid test output: 'chatty' (should be summary, errors, all or streamed)",
        ),
        (
            &["build", "--macos_sdk_version=x", "//:plain"][..],
            "While parsing option --macos_sdk_version=x: Dotted version components must all start with the form \\d+([a-z0-9]*?)?(\\d+)? but got 'x'",
        ),
    ] {
        rejected(&f.run(args), marker);
    }
    assert_eq!(
        built(&f.run(&["build", "--noincompatible_strict_action_env", "//:plain"])),
        "plain\n"
    );
    assert_eq!(
        built(&f.run(&[
            "build",
            "--experimental_strict_action_env",
            "--test_summary=detailed",
            "--remote_upload_local_results=false",
            "//:plain"
        ])),
        "plain\n"
    );
}

#[test]
fn repo_env_reaches_repository_rules_and_invalidates_them() {
    let f = Fixture::new();
    let value = |args: &[&str], environment: &[(&str, &str)]| {
        let mut all = vec!["build"];
        all.extend_from_slice(args);
        all.push("@made//:data.txt");
        built(&f.run_env(&all, environment))
    };
    assert_eq!(value(&[], &[]), "unset|none\n");
    assert_eq!(value(&["--repo_env=N40_VALUE=one"], &[]), "one|none\n");
    // A changed value re-executes the repository rule.
    assert_eq!(value(&["--repo_env", "N40_VALUE=two"], &[]), "two|none\n");
    // Unset, inherit, %bazel_workspace% and os.environ.
    let client = [("N40_VALUE", "client"), ("N40_OTHER", "o")];
    assert_eq!(value(&["--repo_env==N40_VALUE"], &client), "unset|o\n");
    assert_eq!(
        value(&["--repo_env==N40_VALUE", "--repo_env=N40_VALUE"], &client),
        "client|o\n"
    );
    let workspace = fs::canonicalize(f.ws()).unwrap().display().to_string();
    assert_eq!(
        value(
            &[
                "--repo_env=N40_VALUE=%bazel_workspace%/x",
                "--repo_env==N40_OTHER"
            ],
            &client
        ),
        format!("{workspace}/x|none\n")
    );
    // From an rc file (`common`), also for query's repository loading.
    f.file("ws/.bazelrc", "common --repo_env=N40_VALUE=rc\n");
    assert_eq!(value(&[], &[]), "rc|none\n");
    assert_eq!(value(&["--repo_env=N40_VALUE=cli"], &[]), "cli|none\n");
    rejected(
        &f.run(&["build", "--repo_env=", "//:plain"]),
        "While parsing option --repo_env=: Variable definitions must be in the form of a 'name=value', 'name', or '=name' assignment",
    );
}

#[test]
fn repo_env_reaches_module_ctx() {
    // N43's module_ctx shares repository_ctx's environment: getenv,
    // os.environ and execute see --repo_env (N40) like repository rules.
    let f = Fixture::new();
    f.file(
        "ws/MODULE.bazel",
        "module(name='main')\next = use_extension('//:ext.bzl', 'ext')\nuse_repo(ext, 'from_ext')\n",
    );
    f.file(
        "ws/ext.bzl",
        "def _repo(rctx):\n    rctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    rctx.file('data.txt', rctx.attr.value, executable=False)\nrepo = repository_rule(_repo, attrs={'value': attr.string()})\ndef _ext(mctx):\n    shell = mctx.execute(['/bin/sh', '-c', 'printf %s \"$N40_VALUE\"']).stdout\n    repo(name='from_ext', value=mctx.getenv('N40_VALUE', 'unset') + '|' + mctx.os.environ.get('N40_OTHER', 'none') + '|' + shell + '\\n')\next = module_extension(_ext)\n",
    );
    let value = |args: &[&str], environment: &[(&str, &str)]| {
        let mut all = vec!["build"];
        all.extend_from_slice(args);
        all.push("@from_ext//:data.txt");
        built(&f.run_env(&all, environment))
    };
    assert_eq!(value(&[], &[]), "unset|none|\n");
    assert_eq!(
        value(&["--repo_env=N40_VALUE=one", "--repo_env=N40_OTHER=o"], &[]),
        "one|o|one\n"
    );
    let client = [("N40_VALUE", "client")];
    assert_eq!(value(&["--repo_env==N40_VALUE"], &client), "unset|none|\n");
    assert_eq!(value(&[], &client), "client|none|client\n");
}

#[test]
fn repo_env_values_are_never_printed() {
    let f = Fixture::new();
    f.file(
        "ws/.bazelrc",
        "common --repo_env=N40_SECRET=sentinel-value\ncommon --announce_rc\n",
    );
    let output = f.run(&[
        "--repo_env=N40_SECRET=startup-sentinel",
        "query",
        "//:plain",
    ]);
    rejected(
        &output,
        "Unknown startup option: '--repo_env=N40_SECRET=<redacted>'",
    );
    let output = f.run(&["query", "//:plain"]);
    ok(&output);
    assert!(!stderr(&output).contains("sentinel"), "{}", stderr(&output));
    assert!(stderr(&output).contains("--repo_env=N40_SECRET=<redacted>"));
}

#[test]
fn isolate_requires_the_experimental_flag() {
    let f = Fixture::new();
    f.file(
        "ws/MODULE.bazel",
        "module(name='main')\ne = use_extension('//:ext.bzl', 'ext', isolate = True)\n",
    );
    rejected(
        &f.run(&["query", "//:plain"]),
        "in call to use_extension(), parameter 'isolate' is experimental and thus unavailable with the current flags. It may be enabled by setting --experimental_isolated_extension_usages",
    );
    f.file(
        "ws/.bazelrc",
        "common --experimental_isolated_extension_usages\n",
    );
    // ModuleFileFunction.java:573-584 (paired capture isolate-with-flag).
    let workspace = fs::canonicalize(f.ws()).unwrap().display().to_string();
    rejected(
        &f.run(&["query", "//:plain"]),
        &format!(
            "bazel-rs: Error computing the main repository mapping: the isolated usage at {workspace}/MODULE.bazel:2:18 of extension ext defined in @main//:ext.bzl has no effect as no repositories are imported from it. Either import one or more repositories generated by the extension with use_repo or remove the usage."
        ),
    );
    f.file(
        "ws/MODULE.bazel",
        "module(name='main')\ne = use_extension('//:ext.bzl', 'ext', isolate = True)\nuse_repo(e, 'made')\n",
    );
    assert_eq!(ok(&f.run(&["query", "//:plain"])), "//:plain\n");
    // A dev usage under --ignore_dev_dependency creates no usage.
    f.file(
        "ws/MODULE.bazel",
        "module(name='main')\ne = use_extension('//:ext.bzl', 'ext', isolate = True, dev_dependency = True)\n",
    );
    assert_eq!(
        ok(&f.run(&["query", "--ignore_dev_dependency", "//:plain"])),
        "//:plain\n"
    );
}

#[test]
fn relative_paths_rewrite_workspace_event_locations() {
    let f = Fixture::new();
    f.file(
        "ws/BUILD.bazel",
        "filegroup(name='bad', restricted_to=[])\n",
    );
    let workspace = fs::canonicalize(f.ws()).unwrap().display().to_string();
    let absolute = f.run(&["build", "//:bad"]);
    rejected(
        &absolute,
        &format!("bazel-rs: {workspace}/BUILD.bazel:1:10: in restricted_to attribute"),
    );
    f.file(
        "ws/.bazelrc",
        "common --attempt_to_print_relative_paths=true\n",
    );
    let relative = f.run(&["build", "//:bad"]);
    rejected(
        &relative,
        "bazel-rs: BUILD.bazel:1:10: in restricted_to attribute",
    );
    assert!(!stderr(&relative).contains(&format!("{workspace}/BUILD.bazel")));
}

#[test]
fn apple_sdk_versions_are_configuration_options() {
    let f = Fixture::new();
    f.file(
        "ws/BUILD.bazel",
        "config_setting(name='mac', values={'macos_sdk_version': '10.13'})\ngenrule(name='g', outs=['g.txt'], cmd=select({':mac': 'echo mac > $@', '//conditions:default': 'echo other > $@'}))\n",
    );
    assert_eq!(built(&f.run(&["build", "//:g"])), "other\n");
    assert_eq!(
        built(&f.run(&["build", "--macos_sdk_version=10.13", "//:g"])),
        "mac\n"
    );
    // DottedVersion.Option equality is by string representation.
    assert_eq!(
        built(&f.run(&["build", "--macos_sdk_version=10.13.0", "//:g"])),
        "other\n"
    );
    assert_eq!(
        built(&f.run(&[
            "build",
            "--ios_sdk_version=17.0",
            "--tvos_sdk_version=1a2",
            "--watchos_sdk_version=10.beta",
            "//:g"
        ])),
        "other\n"
    );
}

/// A minimal HTTP/1.1 server: records request lines and answers each with
/// `respond(request_line)`.
struct Server {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn new(respond: fn(&str) -> (u16, &'static str)) -> Self {
        Self::with(move |line| (respond(line).0, respond(line).1.to_owned()))
    }

    /// `respond` returns (status, body); a 3xx body is the Location.
    fn with(respond: impl Fn(&str) -> (u16, String) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let line = line.trim_end().to_owned();
                let mut header = String::new();
                while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
                    header.clear();
                }
                seen.lock().unwrap().push(line.clone());
                let (status, body) = respond(&line);
                let (location, body) = if (300..400).contains(&status) {
                    (format!("Location: {body}\r\n"), String::new())
                } else {
                    (String::new(), body)
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\n{location}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.flush();
                let mut rest = Vec::new();
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(50)));
                let _ = stream.read_to_end(&mut rest);
            }
        });
        Self { url, requests }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn registry(line: &str) -> (u16, &'static str) {
    if line.contains("/modules/dep/1.0/MODULE.bazel ") {
        (200, "module(name='dep', version='1.0')\n")
    } else {
        (404, "")
    }
}

fn refuse(_: &str) -> (u16, &'static str) {
    (502, "MUST_NOT_CONTACT_PROXY")
}

fn graph(f: &Fixture, registry: &str, flags: &[&str], environment: &[(&str, &str)]) -> Output {
    let registry = format!("--registry={registry}");
    let mut args = vec![
        "mod",
        "graph",
        "--output=json",
        registry.as_str(),
        "--lockfile_mode=off",
    ];
    args.extend_from_slice(flags);
    f.run_env(&args, environment)
}

#[test]
fn downloads_use_proxy_helper_over_the_repository_environment() {
    let f = Fixture::new();
    f.file(
        "ws/MODULE.bazel",
        "module(name='main')\nbazel_dep(name='dep', version='1.0')\n",
    );
    // An HTTP registry through an HTTP proxy from --repo_env (absolute-form
    // request); the registry host itself is never resolved.
    let proxy = Server::new(registry);
    let via = format!("--repo_env=http_proxy={}", proxy.url);
    let output = graph(&f, "http://registry.n40.invalid", &[&via], &[]);
    assert!(
        output.status.success(),
        "{} {:?}",
        stderr(&output),
        proxy.requests()
    );
    assert!(ok(&output).contains("dep@1.0"), "{}", ok(&output));
    assert!(
        proxy
            .requests()
            .iter()
            .any(|line| line
                == "GET http://registry.n40.invalid/modules/dep/1.0/MODULE.bazel HTTP/1.1"),
        "{:?}",
        proxy.requests()
    );
    // The client environment selects the proxy too (Bazel's clientEnv).
    let client = Server::new(registry);
    let output = graph(
        &f,
        "http://registry.n40.invalid",
        &[],
        &[("HTTP_PROXY", &client.url)],
    );
    ok(&output);
    assert!(!client.requests().is_empty());
    // NO_PROXY (from --repo_env, like .bazelrc.coder) bypasses the proxy.
    let direct = Server::new(registry);
    let forbidden = Server::new(refuse);
    let via = format!("--repo_env=HTTP_PROXY={}", forbidden.url);
    let output = graph(
        &f,
        &direct.url,
        &[&via, "--repo_env=NO_PROXY=127.0.0.1"],
        &[],
    );
    ok(&output);
    assert!(forbidden.requests().is_empty());
    assert!(!direct.requests().is_empty());
    // --repo_env==NAME removes a client proxy.
    let forbidden = Server::new(refuse);
    let direct = Server::new(registry);
    let output = graph(
        &f,
        &direct.url,
        &["--repo_env==http_proxy"],
        &[("http_proxy", &forbidden.url)],
    );
    ok(&output);
    assert!(forbidden.requests().is_empty());
    // HTTPS targets tunnel through CONNECT (the local proxy refuses).
    let tunnel = Server::new(refuse);
    let via = format!("--repo_env=HTTPS_PROXY={}", tunnel.url);
    let output = graph(&f, "https://registry.n40.invalid", &[&via], &[]);
    assert!(!output.status.success());
    let requests = tunnel.requests();
    assert!(!requests.is_empty());
    assert!(
        requests
            .iter()
            .all(|line| line == "CONNECT registry.n40.invalid:443 HTTP/1.1"),
        "{requests:?}"
    );
    // Invalid and SOCKS proxy addresses fail explicitly, without the value.
    let output = graph(
        &f,
        "http://registry.n40.invalid",
        &["--repo_env=http_proxy=http://u:pw@h:notaport"],
        &[],
    );
    rejected(&output, "Proxy address <redacted> is not a valid URL");
    assert!(!stderr(&output).contains("pw"));
    let output = graph(
        &f,
        "http://registry.n40.invalid",
        &["--repo_env=http_proxy=socks5://127.0.0.1:1"],
        &[],
    );
    rejected(&output, "SOCKS proxies");
}

#[test]
fn proxies_are_chosen_again_for_every_redirect_target() {
    // HttpConnector asks ProxyHelper for each connection, redirects included
    // (HttpConnector.java:125-136).
    let f = Fixture::new();
    f.file(
        "ws/MODULE.bazel",
        "module(name='main')\nbazel_dep(name='dep', version='1.0')\n",
    );
    // A NO_PROXY-exempt registry redirects to a host that needs the proxy.
    let proxy = Server::new(registry);
    let direct = Server::with(|line| {
        if line.contains("/modules/dep/1.0/MODULE.bazel ") {
            (
                302,
                "http://moved.n40.invalid/modules/dep/1.0/MODULE.bazel".to_owned(),
            )
        } else {
            (404, String::new())
        }
    });
    let via = format!("--repo_env=http_proxy={}", proxy.url);
    let output = graph(
        &f,
        &direct.url,
        &[&via, "--repo_env=NO_PROXY=127.0.0.1"],
        &[],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        proxy.requests().contains(
            &"GET http://moved.n40.invalid/modules/dep/1.0/MODULE.bazel HTTP/1.1".to_owned()
        ),
        "{:?}",
        proxy.requests()
    );
    // And back: a proxied request redirected to an exempt host goes direct.
    let target = Server::new(registry);
    let location = format!("{}/modules/dep/1.0/MODULE.bazel", target.url);
    let redirecting = Server::with(move |line| {
        if line.contains("/modules/dep/1.0/MODULE.bazel ") {
            (302, location.clone())
        } else {
            (404, String::new())
        }
    });
    let via = format!("--repo_env=http_proxy={}", redirecting.url);
    let output = graph(
        &f,
        "http://registry.n40.invalid",
        &[&via, "--repo_env=NO_PROXY=127.0.0.1"],
        &[],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(redirecting.requests().contains(
        &"GET http://registry.n40.invalid/modules/dep/1.0/MODULE.bazel HTTP/1.1".to_owned()
    ));
    assert!(
        target
            .requests()
            .contains(&"GET /modules/dep/1.0/MODULE.bazel HTTP/1.1".to_owned()),
        "{:?}",
        target.requests()
    );
    // A redirect target whose proxy is rejected (SOCKS) fails before any
    // connection: `localhost` is not NO_PROXY-exempt (exact host match),
    // so a direct fallback would have reached the target server.
    let target = Server::new(registry);
    let port = target.url.rsplit(':').next().unwrap().to_owned();
    let to_localhost = Server::with(move |line| {
        if line.contains("/modules/dep/1.0/MODULE.bazel ") {
            (
                302,
                format!("http://localhost:{port}/modules/dep/1.0/MODULE.bazel"),
            )
        } else {
            (404, String::new())
        }
    });
    let output = graph(
        &f,
        &to_localhost.url,
        &[
            "--repo_env=http_proxy=socks5://127.0.0.1:1",
            "--repo_env=NO_PROXY=127.0.0.1",
        ],
        &[],
    );
    assert!(!output.status.success());
    assert!(!to_localhost.requests().is_empty());
    assert!(target.requests().is_empty(), "{:?}", target.requests());
}
