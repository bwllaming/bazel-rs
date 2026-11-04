//! Remote execution (`--remote_executor`, docs/REMOTE_EXECUTION.md,
//! crate::remote_execution) against the in-process loopback REAPI server of
//! tests/support, whose Execution service runs each Command locally in a
//! fresh directory built from the uploaded input root. Nothing contacts a
//! real executor. The ignored paired check runs genuine Bazel 9.3 against
//! the same server.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

include!("support/executable_script.rs");
include!("support/verify_env.rs");
include!("support/explicit_modules.rs");
include!("support/grpc_cache_server.rs");

use grpc_cache_server::{Config, ExecStep, Server};

static NEXT: AtomicUsize = AtomicUsize::new(0);

const DEFS: &str = r#"def _runner(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run(executable = ctx.file.tool, arguments = [ctx.file.src.path, out.path], inputs = [ctx.file.src], outputs = [out], mnemonic = "RunTool")
    return [DefaultInfo(files = depset([out]))]
runner = rule(_runner, attrs = {"src": attr.label(allow_single_file = True), "tool": attr.label(allow_single_file = True)})
def _shell(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(command = "tr a-z A-Z < $1 > $2", arguments = [ctx.file.src.path, out.path], inputs = [ctx.file.src], outputs = [out])
    return [DefaultInfo(files = depset([out]))]
shell = rule(_shell, attrs = {"src": attr.label(allow_single_file = True)})
def _tree(ctx):
    out = ctx.actions.declare_directory(ctx.label.name + ".d")
    ctx.actions.run_shell(command = "mkdir -p $1/sub && echo a > $1/a.txt && echo b > $1/sub/b.txt && ln -s a.txt $1/link", arguments = [out.path], outputs = [out])
    empty = ctx.actions.declare_directory(ctx.label.name + ".empty")
    ctx.actions.run_shell(command = "true", arguments = [empty.path], outputs = [empty])
    listing = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(command = "(cd $1 && find . | sort && cat a.txt sub/b.txt) > $3; ls -A $2 | wc -l >> $3", arguments = [out.path, empty.path, listing.path], inputs = [out, empty], outputs = [listing])
    return [DefaultInfo(files = depset([out, listing]))]
tree = rule(_tree)
def _timed(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".out")
    ctx.actions.run_shell(command = "sleep 5; echo late > $1", arguments = [out.path], outputs = [out], execution_requirements = {"timeout": "1"})
    return [DefaultInfo(files = depset([out]))]
timed = rule(_timed)
"#;

const BUILD: &str = r#"load(":defs.bzl", "runner", "shell", "timed", "tree")
genrule(name = "gen", srcs = ["in.txt"], outs = ["gen.txt"], cmd = "cat $< > $@; echo done >> $@")
runner(name = "run", src = "in.txt", tool = "tool.sh")
shell(name = "sh", src = "in.txt")
genrule(name = "ne", outs = ["ne.txt"], cmd = "echo ne > $@", tags = ["no-remote-exec"])
genrule(name = "lo", outs = ["lo.txt"], cmd = "echo lo > $@", tags = ["local"])
genrule(name = "nc", outs = ["nc.txt"], cmd = "echo nc > $@", tags = ["no-remote-cache"])
genrule(name = "props", outs = ["props.txt"], cmd = "echo props > $@", exec_properties = {"pool": "big"})
genrule(name = "fails", outs = ["fails.txt"], cmd = "echo oops; echo bad >&2; exit 3")
tree(name = "tree")
timed(name = "timed")
"#;

// The genuine Bazel 9.3.0 keys of //pkg:gen, //pkg:run and //pkg:sh (the
// N41 HTTP capture, tests/remote_cache.rs): a remotely executed spawn has
// the key its cache entry has.
const GEN_KEY: &str = "e99888bbd1421b4dc6fd361854c07ef76d5bb586d761766704e0063d578b4dd9";
const RUN_KEY: &str = "55a9a20019c12f38767dc9dc43875a8a327cd33249897231f72ffede7936e030";
const SH_KEY: &str = "215a0cea76512319330fd7d9ff08cee13aaca1f345089f37d42323c2d69be62e";

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let root = crate::test_temp::root().join(format!(
            "native-remote-exec-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("ws/pkg")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        let ws = Self(root);
        ws.file("MODULE.bazel", "module(name = \"rc\")\n");
        ws.file("pkg/BUILD.bazel", BUILD);
        ws.file("pkg/defs.bzl", DEFS);
        ws.file("pkg/in.txt", "hello\n");
        write_executable_script(
            &ws.0.join("ws/pkg/tool.sh"),
            "#!/bin/bash\ncp \"$1\" \"$2\"\n",
            0o755,
        );
        ws
    }

    fn file(&self, relative: &str, text: &str) {
        let path = self.0.join("ws").join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn run(&self, cache: &str, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.0.join(cache).display()))
            .args(args)
            .current_dir(self.0.join("ws"))
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.join("home"));
        command.explicit_modules().output().unwrap()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !line.starts_with("bazel-rs: memory limit defaulted to "))
        .map(|line| format!("{line}\n"))
        .collect()
}

fn outputs(output: &Output) -> BTreeMap<String, String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (label, path) = line.split_once('\t')?;
            Some((
                label.to_owned(),
                fs::read_to_string(path).unwrap_or_default(),
            ))
        })
        .collect()
}

/// The built output paths by file name.
fn output_paths(output: &Output) -> BTreeMap<String, PathBuf> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (_, path) = line.split_once('\t')?;
            let path = PathBuf::from(path);
            Some((path.file_name()?.to_string_lossy().into_owned(), path))
        })
        .collect()
}

fn executor() -> Server {
    Server::with_config(Config {
        execution: true,
        ..Config::default()
    })
}

/// The Execution methods called since the last log read.
fn execution_calls(server: &Server) -> Vec<String> {
    server
        .take_log()
        .iter()
        .filter(|call| call.method.contains("Execution/"))
        .map(|call| call.method.rsplit('/').next().unwrap().to_owned())
        .collect()
}

fn counts(server: &Server) -> BTreeMap<String, usize> {
    server.method_counts(&server.take_log())
}

#[test]
fn spawns_execute_remotely_with_genuine_keys_and_hit_afterwards() {
    let ws = Workspace::new();
    let server = executor();
    let flag = format!("--remote_executor={}", server.url());
    let built = ws.run(
        "c1",
        &["build", &flag, "//pkg:gen", "//pkg:run", "//pkg:sh"],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    assert!(
        stderr(&built).contains(
            "3 actions executed, 0 cache hits, 0 remote cache hits (experimental local execution; 3 executed remotely"
        ),
        "{}",
        stderr(&built)
    );
    let built = outputs(&built);
    assert_eq!(built["//pkg:gen"], "hello\ndone\n");
    assert_eq!(built["//pkg:run"], "hello\n");
    assert_eq!(built["//pkg:sh"], "HELLO\n");
    let mut keys = server
        .executed()
        .iter()
        .map(|executed| executed.action_digest.hash.clone())
        .collect::<Vec<_>>();
    keys.sort();
    let mut expected = vec![GEN_KEY, RUN_KEY, SH_KEY];
    expected.sort();
    assert_eq!(keys, expected);
    // The input roots hold the inputs (genrule-setup.sh included), all
    // executable; the server ran them and stored the results.
    let gen_spawn = server
        .executed()
        .into_iter()
        .find(|executed| executed.action_digest.hash == GEN_KEY)
        .unwrap();
    assert_eq!(
        gen_spawn.files.keys().collect::<Vec<_>>(),
        [
            "external/bazel_tools/tools/genrule/genrule-setup.sh",
            "pkg/in.txt"
        ]
    );
    assert!(gen_spawn.files.values().all(|(_, executable)| *executable));
    assert!(!gen_spawn.skip_cache_lookup);
    let mut ac = server.ac_keys("");
    ac.sort();
    assert_eq!(ac, expected);
    let calls = counts(&server);
    assert_eq!(calls["Capabilities/GetCapabilities"], 1);
    assert_eq!(calls["ActionCache/GetActionResult"], 3);
    assert_eq!(calls["Execution/Execute"], 3);
    assert_eq!(calls["ContentAddressableStorage/FindMissingBlobs"], 3);
    // A fresh native cache: every spawn is a remote cache hit.
    let exec_log = ws.0.join("exec.json");
    let log_flag = format!("--execution_log_json_file={}", exec_log.display());
    let again = ws.run(
        "c2",
        &[
            "build",
            &flag,
            &log_flag,
            "//pkg:gen",
            "//pkg:run",
            "//pkg:sh",
        ],
    );
    assert!(again.status.success(), "{}", stderr(&again));
    assert!(
        stderr(&again).contains("0 actions executed, 0 cache hits, 3 remote cache hits"),
        "{}",
        stderr(&again)
    );
    assert_eq!(outputs(&again), built);
    let calls = counts(&server);
    assert!(!calls.contains_key("Execution/Execute"), "{calls:?}");
    // The execution log records the remote runner's cache hits.
    let logged = fs::read_to_string(&exec_log).unwrap();
    assert_eq!(
        logged.matches("\"runner\": \"remote\"").count(),
        3,
        "{logged}"
    );
    assert_eq!(logged.matches("\"cacheHit\": true").count(), 3, "{logged}");
}

#[test]
fn tags_platforms_and_shared_endpoints() {
    let ws = Workspace::new();
    let server = executor();
    let flag = format!("--remote_executor={}", server.url());
    let built = ws.run(
        "c1",
        &[
            "build",
            &flag,
            "--remote_default_exec_properties=OSFamily=linux",
            "--remote_default_exec_properties=pool=small",
            "--remote_exec_header=x-exec=e",
            "--remote_cache_header=x-cache=c",
            "//pkg:ne",
            "//pkg:lo",
            "//pkg:nc",
            "//pkg:props",
        ],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    let built_outputs = outputs(&built);
    assert_eq!(built_outputs["//pkg:ne"], "ne\n");
    assert_eq!(built_outputs["//pkg:lo"], "lo\n");
    let executed = server.executed();
    // `no-remote-exec` and `local` run locally; `no-remote-cache` runs
    // remotely with do_not_cache and no lookup.
    assert_eq!(executed.len(), 2, "{executed:?}");
    let platform = |name: &str| {
        executed
            .iter()
            .find(|executed| {
                executed
                    .command
                    .output_paths
                    .iter()
                    .any(|path| path.ends_with(name))
            })
            .unwrap()
            .command
            .platform
            .clone()
    };
    let pair = |name: &str, value: &str| (name.to_owned(), value.to_owned());
    // The defaults, then the target's exec_properties over them, sorted.
    assert_eq!(
        platform("nc.txt"),
        [pair("OSFamily", "linux"), pair("pool", "small")]
    );
    assert_eq!(
        platform("props.txt"),
        [pair("OSFamily", "linux"), pair("pool", "big")]
    );
    let nc = executed
        .iter()
        .find(|executed| executed.command.output_paths[0].ends_with("nc.txt"))
        .unwrap();
    // Action.do_not_cache (field 7) and the platform (field 10).
    let fields = bazel_rs::reapi::fields(&nc.action)
        .unwrap()
        .into_iter()
        .map(|(number, _)| number)
        .collect::<Vec<_>>();
    assert_eq!(fields, [1, 2, 7, 9, 10]);
    assert!(nc.skip_cache_lookup);
    // The shared endpoint carries --remote_header/--remote_exec_header on
    // every call, never --remote_cache_header.
    let log = server.take_log();
    for call in &log {
        assert_eq!(call.header("x-exec"), ["e"], "{call:?}");
        assert!(call.header("x-cache").is_empty(), "{call:?}");
    }
    // The no-remote-exec genrule used the remote cache (one lookup), the
    // local one did not; the nc genrule was not looked up.
    let lookups = log
        .iter()
        .filter(|call| call.method.ends_with("/GetActionResult"))
        .count();
    assert_eq!(lookups, 2, "{log:?}");
}

#[test]
fn trees_symlinks_and_failures_come_back() {
    let ws = Workspace::new();
    let server = executor();
    let flag = format!("--remote_executor={}", server.url());
    let built = ws.run("c1", &["build", &flag, "//pkg:tree"]);
    assert!(built.status.success(), "{}", stderr(&built));
    let paths = output_paths(&built);
    let listing = fs::read_to_string(&paths["tree.txt"]).unwrap();
    assert_eq!(
        listing,
        ".\n./a.txt\n./link\n./sub\n./sub/b.txt\na\nb\n0\n",
        "{}",
        stderr(&built)
    );
    let tree = &paths["tree.d"];
    assert_eq!(fs::read_to_string(tree.join("sub/b.txt")).unwrap(), "b\n");
    // The tree's symlink came back as one; publication materializes it,
    // like a local tree's.
    assert_eq!(fs::read_to_string(tree.join("link")).unwrap(), "a\n");
    // The consumer's input root holds the tree's files and the empty tree
    // as an empty directory; the producers' hold their (empty) output
    // directories.
    let executed = server.executed();
    let consumer = executed
        .iter()
        .find(|executed| executed.command.output_paths[0].ends_with("tree.txt"))
        .unwrap();
    assert!(
        consumer
            .directories
            .iter()
            .any(|directory| directory.ends_with("pkg/tree.empty")),
        "{consumer:?}"
    );
    assert!(
        consumer
            .files
            .keys()
            .any(|path| path.ends_with("pkg/tree.d/sub/b.txt")),
        "{consumer:?}"
    );
    let producer = executed
        .iter()
        .find(|executed| executed.command.output_paths[0].ends_with("tree.d"))
        .unwrap();
    assert!(
        producer
            .directories
            .iter()
            .any(|directory| directory.ends_with("pkg/tree.d")),
        "{producer:?}"
    );
    // A non-zero exit fails the action with its output.
    let failed = ws.run("c1", &["build", &flag, "//pkg:fails"]);
    assert!(!failed.status.success());
    assert!(
        stderr(&failed).contains("failed: (Exit 3): ") && stderr(&failed).contains("\nbad\n"),
        "{}",
        stderr(&failed)
    );
    assert!(
        String::from_utf8_lossy(&failed.stdout).contains("oops\n"),
        "{}",
        String::from_utf8_lossy(&failed.stdout)
    );
    // An execution timeout (Action.timeout from the `timeout` requirement).
    let timed = ws.run("c1", &["build", &flag, "//pkg:timed"]);
    assert!(!timed.status.success());
    assert!(
        stderr(&timed).contains("failed: (Exit 142): remote spawn timed out"),
        "{}",
        stderr(&timed)
    );
    let action = &server
        .executed()
        .into_iter()
        .find(|executed| executed.command.output_paths[0].ends_with("timed.out"))
        .unwrap()
        .action;
    assert!(
        bazel_rs::reapi::fields(action)
            .unwrap()
            .iter()
            .any(|(number, _)| *number == 6)
    );
}

#[test]
fn streams_reconnect_retry_and_report_failures() {
    let ws = Workspace::new();
    let server = executor();
    let flag = format!("--remote_executor={}", server.url());
    // A broken stream continues with WaitExecution.
    server.configure(|config| {
        config.exec_script = [ExecStep::Break(14)].into_iter().collect();
    });
    let built = ws.run(
        "c1",
        &["build", &flag, "--remote_retry_max_delay=10ms", "//pkg:gen"],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    assert_eq!(outputs(&built)["//pkg:gen"], "hello\ndone\n");
    assert_eq!(execution_calls(&server), ["Execute", "WaitExecution"]);
    // A stream that ends before the operation is done continues too; a
    // lost operation (NOT_FOUND) starts over with Execute.
    let other = executor();
    other.configure(|config| {
        config.exec_script = [ExecStep::Break(0), ExecStep::Lost].into_iter().collect();
    });
    let other_flag = format!("--remote_executor={}", other.url());
    let built = ws.run(
        "c1b",
        &[
            "build",
            &other_flag,
            "--remote_retry_max_delay=10ms",
            "//pkg:gen",
        ],
    );
    assert!(built.status.success(), "{}", stderr(&built));
    assert_eq!(
        execution_calls(&other),
        ["Execute", "WaitExecution", "Execute"]
    );
    // MISSING inputs: retried at once, the inputs uploaded again.
    let mut violation = Vec::new();
    bazel_rs::reapi::string(&mut violation, 1, "MISSING");
    bazel_rs::reapi::string(&mut violation, 2, "blobs/x/1");
    let mut failure = Vec::new();
    bazel_rs::reapi::message(&mut failure, 1, &violation);
    server.configure(|config| {
        config.exec_script = [ExecStep::Response {
            code: 9,
            message: "missing".into(),
            details: vec![(
                "type.googleapis.com/google.rpc.PreconditionFailure".into(),
                failure,
            )],
            response_message: String::new(),
        }]
        .into_iter()
        .collect();
    });
    let built = ws.run("c2", &["build", &flag, "//pkg:sh"]);
    assert!(built.status.success(), "{}", stderr(&built));
    let calls = counts(&server);
    assert_eq!(calls["Execution/Execute"], 2);
    assert_eq!(calls["ContentAddressableStorage/FindMissingBlobs"], 2);
    // A server error: the inner retrier retries transient codes
    // (--remote_retries), then the action fails with Bazel's text.
    server.configure(|config| {
        config.exec_script = (0..3)
            .map(|_| ExecStep::OperationError(14, "workers are gone".into()))
            .collect();
    });
    let failed = ws.run(
        "c3",
        &[
            "build",
            &flag,
            "--remote_retries=2",
            "--remote_retry_max_delay=10ms",
            "//pkg:run",
        ],
    );
    // ExitCode.REMOTE_ERROR.
    assert_eq!(failed.status.code(), Some(34), "{}", stderr(&failed));
    assert!(
        stderr(&failed).contains(
            "failed: (Exit 34): Remote Execution Failure:\nUnavailable: workers are gone\n"
        ),
        "{}",
        stderr(&failed)
    );
    assert_eq!(counts(&server)["Execution/Execute"], 3);
    // With --remote_local_fallback the spawn runs locally instead.
    server.configure(|config| {
        config.exec_script = [ExecStep::Response {
            code: 3,
            message: "bad action".into(),
            details: Vec::new(),
            response_message: "see the server log".into(),
        }]
        .into_iter()
        .collect();
    });
    let fallback = ws.run(
        "c4",
        // No upload: the next run must reach the executor, not a cached result.
        &[
            "build",
            &flag,
            "--remote_local_fallback",
            "--noremote_upload_local_results",
            "//pkg:run",
        ],
    );
    assert!(fallback.status.success(), "{}", stderr(&fallback));
    assert_eq!(outputs(&fallback)["//pkg:run"], "hello\n");
    // Without it: the message and the response's message.
    server.configure(|config| {
        config.exec_script = [ExecStep::Response {
            code: 3,
            message: "bad action".into(),
            details: Vec::new(),
            response_message: "see the server log".into(),
        }]
        .into_iter()
        .collect();
    });
    let failed = ws.run("c5", &["build", &flag, "//pkg:run"]);
    assert!(!failed.status.success());
    assert!(
        stderr(&failed).contains(
            "failed: (Exit 34): Remote Execution Failure:\nInvalid Argument: bad action\n\nsee the server log"
        ),
        "{}",
        stderr(&failed)
    );
}

#[test]
fn capabilities_strategies_and_options() {
    let ws = Workspace::new();
    // A cache without execution.
    let cache_only = Server::new();
    let flag = format!("--remote_executor={}", cache_only.url());
    let failed = ws.run("c1", &["build", &flag, "//pkg:gen"]);
    // CAPABILITIES_QUERY_FAILURE: exit code 34.
    assert_eq!(failed.status.code(), Some(34), "{}", stderr(&failed));
    assert!(
        stderr(&failed).contains(
            "Failed to query remote execution capabilities: Remote execution is not supported by the remote server, or the current account is not authorized to use remote execution."
        ),
        "{}",
        stderr(&failed)
    );
    // --spawn_strategy=local keeps spawns local (the remote cache serves
    // them); `remote` without an executor is no registered strategy.
    let server = executor();
    let flag = format!("--remote_executor={}", server.url());
    let local = ws.run(
        "c2",
        &["build", &flag, "--spawn_strategy=local", "//pkg:gen"],
    );
    assert!(local.status.success(), "{}", stderr(&local));
    assert!(server.executed().is_empty());
    let unregistered = ws.run("c3", &["build", "--spawn_strategy=remote", "//pkg:gen"]);
    assert!(!unregistered.status.success());
    assert!(
        stderr(&unregistered).contains(
            "'remote' was requested for explicit default strategies but no strategy with that identifier was registered"
        ),
        "{}",
        stderr(&unregistered)
    );
    // --strategy=Genrule=remote with a `local` genrule cannot execute it.
    let strict = ws.run(
        "c4",
        &["build", &flag, "--strategy=Genrule=remote", "//pkg:lo"],
    );
    assert!(!strict.status.success());
    assert!(
        stderr(&strict).contains(
            "Genrule spawn cannot be executed with any of the available strategies: [remote]"
        ),
        "{}",
        stderr(&strict)
    );
    for (option, error) in [
        (
            "--remote_cache=http://127.0.0.1:1",
            "Cannot combine gRPC based remote execution with HTTP-based caching",
        ),
        (
            "--remote_default_exec_properties=novalue",
            "While parsing option --remote_default_exec_properties=novalue: Variable definitions must be in the form of a 'name=value' assignment",
        ),
        (
            "--remote_print_execution_messages=some",
            "Not a valid execution message print mode",
        ),
    ] {
        let failed = ws.run("c5", &["build", &flag, option, "//pkg:gen"]);
        assert!(!failed.status.success(), "{option}");
        assert!(
            stderr(&failed).contains(error),
            "{option}: {}",
            stderr(&failed)
        );
    }
}

// --- The paired checks with genuine Bazel 9.3 (ignored; run explicitly) ---
//
// BAZEL_RS_GENUINE_BAZEL=<genuine 9.3.0 ELF> [BAZEL_RS_REMOTE_EXEC_RECEIPT=<out.json>]
// cargo test --test remote_execution genuine_bazel_sends -- --ignored
//
// Genuine Bazel and native build the same fixture with --remote_executor
// against fresh in-process servers; the Execute requests' Action digests,
// Commands and input roots must be identical.
//
// BAZEL_RS_GENUINE_BAZEL=<ELF> BAZEL_RS_NATIVELINK=<nativelink 1.8.2 binary>
// [BAZEL_RS_NATIVELINK_RECEIPT=<out.json>]
// cargo test --test remote_execution genuine_bazel_and_native_share -- --ignored
//
// The same fixture against a real open-source REAPI server (NativeLink, its
// GitHub release binary, scheduler and worker in one process on loopback):
// both write the same Action Cache keys, and each reads the other's.

const PAIRED_BUILD: &str = r#"load(":defs.bzl", "runner", "shell", "tree")
genrule(name = "gen", srcs = ["in.txt"], outs = ["gen.txt"], cmd = "cat $< > $@; echo done >> $@")
runner(name = "run", src = "in.txt", tool = "tool.sh")
shell(name = "sh", src = "in.txt")
genrule(name = "chain", srcs = [":gen"], outs = ["chain.txt"], cmd = "tr a-z A-Z < $< > $@")
genrule(name = "mk", outs = ["mk.sh"], cmd = "printf '#!/bin/bash\\necho made > $$1\\n' > $@", executable = True)
genrule(name = "use", outs = ["use.txt"], tools = [":mk"], cmd = "$(location :mk) $@")
genrule(name = "nc", outs = ["nc.txt"], cmd = "echo nc > $@", tags = ["no-remote-cache"])
genrule(name = "props", outs = ["props.txt"], cmd = "echo props > $@", exec_properties = {"pool": "big"})
tree(name = "tree")
"#;

const PAIRED_TARGETS: [&str; 9] = [
    "//pkg:gen",
    "//pkg:run",
    "//pkg:sh",
    "//pkg:chain",
    "//pkg:mk",
    "//pkg:use",
    "//pkg:nc",
    "//pkg:props",
    "//pkg:tree",
];

fn paired_flags(server: &str) -> Vec<String> {
    vec![
        format!("--remote_executor={server}"),
        "--remote_default_exec_properties=OSFamily=linux".into(),
    ]
}

fn genuine(binary: &str, ws: &Workspace, our: &str, registry: &str, server: &str) -> Output {
    Command::new(binary)
        .args([
            "--batch",
            "--ignore_all_rc_files",
            &format!("--output_user_root={}", ws.0.join(our).display()),
            "build",
            &format!("--registry={registry}"),
            "--lockfile_mode=off",
        ])
        .args(paired_flags(server))
        .args(PAIRED_TARGETS)
        .current_dir(ws.0.join("ws"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", ws.0.join("home"))
        .output()
        .unwrap()
}

fn native(ws: &Workspace, cache_dir: &str, registry: &str, server: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
        .arg("--ignore_all_rc_files")
        .arg(format!("--cache-dir={}", ws.0.join(cache_dir).display()))
        .args([
            "build",
            "--experimental-builtin-modules",
            &format!("--registry={registry}"),
            "--lockfile_mode=off",
        ])
        .args(paired_flags(server))
        .args(PAIRED_TARGETS)
        .current_dir(ws.0.join("ws"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", ws.0.join("home"))
        .output()
        .unwrap()
}

/// Per Execute request: the action digest, its Command (arguments,
/// environment, outputs, platform) and input root, by output path.
fn execute_view(server: &Server) -> BTreeMap<String, serde_json::Value> {
    server
        .executed()
        .into_iter()
        .map(|executed| {
            (
                executed.command.output_paths.join(","),
                serde_json::json!({
                    "action": executed.action_digest.to_string(),
                    "arguments": executed.command.arguments,
                    "environment": executed.command.environment,
                    "outputs": executed.command.output_paths,
                    "platform": executed.command.platform,
                    "files": executed.files.iter().map(|(path, (digest, executable))| format!("{path} {digest} {executable}")).collect::<Vec<_>>(),
                    "symlinks": executed.symlinks,
                    "directories": executed.directories,
                    "skip_cache_lookup": executed.skip_cache_lookup,
                }),
            )
        })
        .collect()
}

fn bazel_bin(ws: &Workspace) -> BTreeMap<String, String> {
    let bin = ws.0.join("ws/bazel-bin/pkg");
    let mut out = BTreeMap::new();
    for name in [
        "gen.txt",
        "run.out",
        "sh.out",
        "chain.txt",
        "use.txt",
        "nc.txt",
        "props.txt",
        "tree.txt",
    ] {
        out.insert(
            name.to_owned(),
            fs::read_to_string(bin.join(name)).unwrap_or_default(),
        );
    }
    out
}

#[test]
#[ignore = "needs genuine Bazel 9.3 (BAZEL_RS_GENUINE_BAZEL)"]
fn genuine_bazel_sends_identical_execute_requests() {
    let binary = std::env::var("BAZEL_RS_GENUINE_BAZEL").expect("BAZEL_RS_GENUINE_BAZEL");
    let registry = format!(
        "file://{}",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/builtin_modules/registry")
            .display()
    );
    let ws = Workspace::new();
    ws.file("pkg/BUILD.bazel", PAIRED_BUILD);
    let genuine_server = executor();
    let built = genuine(&binary, &ws, "our-1", &registry, &genuine_server.url());
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let genuine_outputs = bazel_bin(&ws);
    let genuine_view = execute_view(&genuine_server);
    let genuine_log = genuine_server.method_counts(&genuine_server.take_log());
    let _ = fs::remove_dir_all(ws.0.join("our-1"));
    let native_server = executor();
    let built = native(&ws, "ncache-1", &registry, &native_server.url());
    assert!(built.status.success(), "{}", stderr(&built));
    let native_outputs = output_paths(&built)
        .into_iter()
        .filter(|(name, _)| genuine_outputs.contains_key(name))
        .map(|(name, path)| (name, fs::read_to_string(path).unwrap_or_default()))
        .collect::<BTreeMap<_, _>>();
    let native_view = execute_view(&native_server);
    let native_log = native_server.method_counts(&native_server.take_log());
    // Native reads genuine's results: a fresh native cache against the
    // server genuine populated hits every cacheable spawn.
    let read = native(&ws, "ncache-2", &registry, &genuine_server.url());
    assert!(read.status.success(), "{}", stderr(&read));
    let read_log = genuine_server.method_counts(&genuine_server.take_log());
    let receipt = serde_json::json!({
        "server": "in-process tests/support/grpc_cache_server.rs (Execution service)",
        "genuine_execute": genuine_view,
        "native_execute": native_view,
        "genuine_calls": genuine_log,
        "native_calls": native_log,
        "native_read_calls": read_log,
        "genuine_outputs": genuine_outputs,
        "native_outputs": native_outputs,
        "native_read_stderr_summary": stderr(&read).lines().filter(|line| line.contains("actions executed")).collect::<Vec<_>>(),
    });
    if let Ok(path) = std::env::var("BAZEL_RS_REMOTE_EXEC_RECEIPT") {
        fs::write(path, serde_json::to_string_pretty(&receipt).unwrap() + "\n").unwrap();
    }
    assert_eq!(
        native_view.keys().collect::<Vec<_>>(),
        genuine_view.keys().collect::<Vec<_>>()
    );
    for (outputs, genuine) in &genuine_view {
        assert_eq!(&native_view[outputs], genuine, "{outputs}");
    }
    assert_eq!(native_outputs, genuine_outputs);
    // Only the no-remote-cache genrule executes again (do_not_cache).
    assert_eq!(read_log.get("Execution/Execute"), Some(&1), "{read_log:?}");
}

/// A NativeLink server (CAS, AC, scheduler and one local worker) on two
/// free loopback ports with private directories.
struct NativeLink {
    child: std::process::Child,
    port: u16,
    dir: PathBuf,
}

impl NativeLink {
    fn start(binary: &str, dir: PathBuf) -> Self {
        let free = || {
            std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        };
        let (port, worker) = (free(), free());
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let d = dir.display();
        let config = format!(
            r#"{{
  stores: [
    {{ name: "AC", filesystem: {{ content_path: "{d}/ac", temp_path: "{d}/ac-tmp", eviction_policy: {{ max_bytes: 1000000000 }} }} }},
    {{ name: "CAS", fast_slow: {{ fast: {{ filesystem: {{ content_path: "{d}/cas", temp_path: "{d}/cas-tmp", eviction_policy: {{ max_bytes: 10000000000 }} }} }}, slow: {{ noop: {{}} }} }} }},
  ],
  schedulers: [ {{ name: "S", simple: {{ supported_platform_properties: {{ OSFamily: "priority", pool: "priority" }} }} }} ],
  workers: [ {{ local: {{ worker_api_endpoint: {{ uri: "grpc://127.0.0.1:{worker}" }}, cas_fast_slow_store: "CAS", upload_action_result: {{ ac_store: "AC" }}, work_directory: "{d}/work", platform_properties: {{ OSFamily: {{ values: [""] }}, pool: {{ values: [""] }} }} }} }} ],
  servers: [
    {{ name: "public", listener: {{ http: {{ socket_address: "127.0.0.1:{port}" }} }}, services: {{
        cas: [ {{ instance_name: "", cas_store: "CAS" }} ],
        ac: [ {{ instance_name: "", ac_store: "AC" }} ],
        execution: [ {{ instance_name: "", cas_store: "CAS", scheduler: "S" }} ],
        capabilities: [ {{ instance_name: "", remote_execution: {{ scheduler: "S" }} }} ],
        bytestream: [ {{ instance_name: "", cas_store: "CAS" }} ] }} }},
    {{ name: "workers", listener: {{ http: {{ socket_address: "127.0.0.1:{worker}" }} }}, services: {{ worker_api: {{ scheduler: "S" }}, health: {{}} }} }},
  ],
}}
"#
        );
        let path = dir.join("config.json5");
        fs::write(&path, config).unwrap();
        let child = Command::new(binary)
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(fs::File::create(dir.join("nativelink.log")).unwrap())
            .spawn()
            .unwrap();
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // The local worker registers with the scheduler.
        std::thread::sleep(std::time::Duration::from_millis(500));
        Self { child, port, dir }
    }

    fn url(&self) -> String {
        format!("grpc://127.0.0.1:{}", self.port)
    }

    /// The Action Cache keys (the AC store's `HASH-SIZE` file names).
    fn ac_keys(&self) -> Vec<String> {
        let mut keys = Vec::new();
        let mut pending = vec![self.dir.join("ac")];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                if entry.file_type().unwrap().is_dir() {
                    pending.push(entry.path());
                } else {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let name = name.rsplit('/').next().unwrap().to_owned();
                    let hash = name
                        .split(['-', '_'])
                        .find(|part| part.len() == 64)
                        .unwrap_or(&name)
                        .to_owned();
                    keys.push(hash);
                }
            }
        }
        keys.sort();
        keys
    }
}

impl Drop for NativeLink {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
#[ignore = "needs genuine Bazel 9.3 (BAZEL_RS_GENUINE_BAZEL) and NativeLink (BAZEL_RS_NATIVELINK)"]
fn genuine_bazel_and_native_share_a_nativelink_executor() {
    let binary = std::env::var("BAZEL_RS_GENUINE_BAZEL").expect("BAZEL_RS_GENUINE_BAZEL");
    let nativelink = std::env::var("BAZEL_RS_NATIVELINK").expect("BAZEL_RS_NATIVELINK");
    let registry = format!(
        "file://{}",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/builtin_modules/registry")
            .display()
    );
    let ws = Workspace::new();
    ws.file("pkg/BUILD.bazel", PAIRED_BUILD);
    // Genuine executes on a fresh server.
    let first = NativeLink::start(&nativelink, ws.0.join("nativelink-genuine"));
    let built = genuine(&binary, &ws, "our-1", &registry, &first.url());
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let genuine_outputs = bazel_bin(&ws);
    let genuine_keys = first.ac_keys();
    // Native reads every cacheable result genuine's spawns left.
    let read = native(&ws, "ncache-1", &registry, &first.url());
    assert!(read.status.success(), "{}", stderr(&read));
    let native_read_summary = stderr(&read)
        .lines()
        .filter(|line| line.contains("actions executed"))
        .collect::<String>();
    drop(first);
    // Native executes on a fresh server; genuine reads its results.
    let second = NativeLink::start(&nativelink, ws.0.join("nativelink-native"));
    let built = native(&ws, "ncache-2", &registry, &second.url());
    assert!(built.status.success(), "{}", stderr(&built));
    let native_outputs = output_paths(&built)
        .into_iter()
        .filter(|(name, _)| genuine_outputs.contains_key(name))
        .map(|(name, path)| (name, fs::read_to_string(path).unwrap_or_default()))
        .collect::<BTreeMap<_, _>>();
    let native_keys = second.ac_keys();
    let _ = fs::remove_dir_all(ws.0.join("our-1"));
    let read_back = genuine(&binary, &ws, "our-2", &registry, &second.url());
    assert!(
        read_back.status.success(),
        "{}",
        String::from_utf8_lossy(&read_back.stderr)
    );
    let genuine_read_summary = String::from_utf8_lossy(&read_back.stderr)
        .lines()
        .filter(|line| line.contains(" processes: "))
        .collect::<String>();
    let receipt = serde_json::json!({
        "server": format!("NativeLink {nativelink} (loopback, CAS + AC + scheduler + local worker)"),
        "genuine_keys": genuine_keys,
        "native_keys": native_keys,
        "native_read_summary": native_read_summary,
        "genuine_read_summary": genuine_read_summary,
        "genuine_outputs": genuine_outputs,
        "native_outputs": native_outputs,
    });
    if let Ok(path) = std::env::var("BAZEL_RS_NATIVELINK_RECEIPT") {
        fs::write(path, serde_json::to_string_pretty(&receipt).unwrap() + "\n").unwrap();
    }
    // 12 spawns: NativeLink also stores the do_not_cache result of the
    // no-remote-cache genrule, which neither client reads back.
    assert_eq!(genuine_keys.len(), 12, "{genuine_keys:?}");
    assert_eq!(native_keys, genuine_keys);
    assert_eq!(native_outputs, genuine_outputs);
    assert!(
        native_read_summary.contains("11 remote cache hits"),
        "{native_read_summary}"
    );
    assert!(
        genuine_read_summary.contains("11 remote cache hit"),
        "{genuine_read_summary}"
    );
}

const TESTS_BZL: &str = "def _t(ctx):\n    out = ctx.actions.declare_file(ctx.label.name + \".sh\")\n    ctx.actions.write(out, \"#!/bin/bash\\necho testing; cat pkg/in.txt; echo out > $TEST_UNDECLARED_OUTPUTS_DIR/u.txt; exit $1\\n\", is_executable = True)\n    return [DefaultInfo(executable = out, runfiles = ctx.runfiles(files = ctx.files.data))]\nt_test = rule(_t, test = True, attrs = {\"data\": attr.label_list(allow_files = True)})\n";

const TESTS_BUILD: &str = "load(\":tests.bzl\", \"t_test\")\nt_test(name = \"ok\", data = [\"in.txt\"], args = [\"0\"])\nt_test(name = \"bad\", data = [\"in.txt\"], args = [\"1\"])\n";

#[test]
fn tests_execute_remotely_with_their_xml_spawn() {
    let ws = Workspace::new();
    ws.file("pkg/tests.bzl", TESTS_BZL);
    ws.file("pkg/BUILD.bazel", TESTS_BUILD);
    let server = executor();
    let flag = format!("--remote_executor={}", server.url());
    let tested = ws.run("c1", &["test", &flag, "//pkg:ok", "//pkg:bad"]);
    assert_eq!(tested.status.code(), Some(3), "{}", stderr(&tested));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&tested.stdout),
        stderr(&tested)
    );
    assert!(
        text.contains("//pkg:ok") && text.contains("PASSED"),
        "{text}"
    );
    assert!(
        text.contains("//pkg:bad") && text.contains("FAILED"),
        "{text}"
    );
    // Per test: the test spawn and the XML generating spawn.
    let executed = server.executed();
    assert_eq!(executed.len(), 4, "{executed:?}");
    let test_spawn = executed
        .iter()
        .find(|executed| executed.command.arguments[1] == "pkg/ok.sh")
        .unwrap();
    assert_eq!(
        test_spawn.command.arguments[0],
        "external/bazel_tools/tools/test/test-setup.sh"
    );
    assert!(
        test_spawn
            .files
            .contains_key("bazel-out/k8-fastbuild/bin/pkg/ok.sh.runfiles/_main/pkg/in.txt"),
        "{test_spawn:?}"
    );
    assert!(
        test_spawn
            .directories
            .iter()
            .any(|directory| directory == "bazel-out/k8-fastbuild/testlogs/pkg/ok/test.outputs")
    );
    // The test log is the spawn's stdout; test.xml came from the XML spawn.
    let testlogs = ws.run("c1", &["info", "bazel-testlogs"]);
    let testlogs = PathBuf::from(String::from_utf8_lossy(&testlogs.stdout).trim());
    let log = fs::read_to_string(testlogs.join("pkg/ok/test.log")).unwrap();
    assert!(log.contains("testing\nhello\n"), "{log}");
    let xml = fs::read_to_string(testlogs.join("pkg/ok/test.xml")).unwrap();
    assert!(xml.contains("<testsuites>"), "{xml}");
    // The undeclared outputs came back (zipped by default, like Bazel's
    // --zip_undeclared_test_outputs).
    assert!(
        testlogs.join("pkg/ok/test.outputs/u.txt").exists()
            || testlogs.join("pkg/ok/test.outputs/outputs.zip").exists()
            || testlogs.join("pkg/ok/test.outputs.zip").exists(),
        "{:?}",
        fs::read_dir(testlogs.join("pkg/ok")).map(|entries| entries
            .flatten()
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>())
    );
    // A fresh native cache: the passing test is a remote cache hit (with its
    // XML spawn); the failing one runs again.
    server.take_log();
    let again = ws.run("c2", &["test", &flag, "//pkg:ok", "//pkg:bad"]);
    assert_eq!(again.status.code(), Some(3), "{}", stderr(&again));
    let log = server.take_log();
    let executes = log
        .iter()
        .filter(|call| call.method.ends_with("/Execute"))
        .count();
    let calls = log
        .iter()
        .map(|call| format!("{} {}", call.method, call.detail))
        .collect::<Vec<_>>();
    // bad's test spawn and its XML spawn (a new duration may change the
    // XML spawn's key) at most.
    assert!(executes <= 2, "{executes} {calls:#?}");
    // The passing test's spawn was answered from the Action Cache.
    let ok_hit = log.iter().any(|call| {
        call.method.ends_with("/GetActionResult")
            && server
                .ac_keys("")
                .iter()
                .any(|key| call.detail.ends_with(key.as_str()))
    });
    assert!(ok_hit, "{calls:#?}");
}

#[test]
#[ignore = "needs genuine Bazel 9.3 (BAZEL_RS_GENUINE_BAZEL)"]
fn genuine_bazel_sends_identical_test_spawns() {
    let binary = std::env::var("BAZEL_RS_GENUINE_BAZEL").expect("BAZEL_RS_GENUINE_BAZEL");
    let registry = format!(
        "file://{}",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/builtin_modules/registry")
            .display()
    );
    let ws = Workspace::new();
    ws.file("pkg/tests.bzl", TESTS_BZL);
    ws.file("pkg/BUILD.bazel", TESTS_BUILD);
    let run = |native: bool, server: &Server| -> Output {
        let flags = [
            "test".to_owned(),
            format!("--registry={registry}"),
            "--lockfile_mode=off".to_owned(),
            format!("--remote_executor={}", server.url()),
            "//pkg:ok".to_owned(),
            "//pkg:bad".to_owned(),
        ];
        let mut command = if native {
            let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
            command
                .arg("--ignore_all_rc_files")
                .arg(format!("--cache-dir={}", ws.0.join("ncache").display()));
            command
        } else {
            let mut command = Command::new(&binary);
            command.args([
                "--batch",
                "--ignore_all_rc_files",
                &format!("--output_user_root={}", ws.0.join("our").display()),
            ]);
            command
        };
        command.args(&flags[..1]);
        if native {
            command.arg("--experimental-builtin-modules");
        }
        command
            .args(&flags[1..])
            .current_dir(ws.0.join("ws"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", ws.0.join("home"))
            .output()
            .unwrap()
    };
    // Only the test spawns: the XML spawns' keys carry the measured
    // duration.
    let test_spawns = |server: &Server| {
        execute_view(server)
            .into_iter()
            .filter(|(outputs, _)| outputs.contains("test.exited_prematurely"))
            .collect::<BTreeMap<_, _>>()
    };
    let genuine_server = executor();
    let genuine = run(false, &genuine_server);
    assert_eq!(
        genuine.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&genuine.stderr)
    );
    let native_server = executor();
    let native = run(true, &native_server);
    assert_eq!(native.status.code(), Some(3), "{}", stderr(&native));
    let genuine_view = test_spawns(&genuine_server);
    let native_view = test_spawns(&native_server);
    if let Ok(path) = std::env::var("BAZEL_RS_REMOTE_TEST_RECEIPT") {
        let receipt = serde_json::json!({
            "server": "in-process tests/support/grpc_cache_server.rs (Execution service)",
            "genuine_test_spawns": genuine_view,
            "native_test_spawns": native_view,
        });
        fs::write(path, serde_json::to_string_pretty(&receipt).unwrap() + "\n").unwrap();
    }
    assert_eq!(genuine_view.len(), 2);
    assert_eq!(native_view, genuine_view);
}
