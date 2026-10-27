//! Real CLI scenarios for repository_ctx.patch, repository_ctx.repo_metadata,
//! implementation return values, Latin-1 octal escapes in executed .bzl code
//! and .bzl loads through symlinks (N26). The http_archive scenario runs the
//! embedded @bazel_tools http.bzl against a private loopback HTTP listener.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
include!("support/verify_env.rs");
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;
include!("support/explicit_modules.rs");

/// Serves files below `root` with Content-Length and Connection: close.
struct Server {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn new(root: PathBuf) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("private listener failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&request).into_owned();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let bytes = (path.starts_with('/') && !path.contains(".."))
                    .then(|| fs::read(root.join(&path[1..])).ok())
                    .flatten();
                let status = if bytes.is_some() {
                    "200 OK"
                } else {
                    "404 Not Found"
                };
                let bytes = bytes.unwrap_or_default();
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&bytes);
            }
        });
        Self {
            address,
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("n26-cli-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        fs::create_dir(f.0.path().join("home")).unwrap();
        f
    }

    fn file(&self, name: &str, data: impl AsRef<[u8]>) {
        let path = self.0.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    /// Exact environment: no client credentials or netrc can be consulted.
    fn run(&self, args: &[&str]) -> Output {
        let registry = format!(
            "--registry=file://{}",
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/builtin_modules/registry")
                .display()
        );
        Command::new("/usr/bin/timeout")
            .env_clear()
            .envs(verifier_environment())
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("LC_ALL", "C")
            .args([
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
            ])
            .args(&args[..1])
            .args(["--experimental-builtin-modules", registry.as_str()])
            .args(&args[1..])
            .current_dir(self.0.path())
            .explicit_modules()
            .output()
            .unwrap()
    }

    fn built(&self, target: &str) -> Vec<u8> {
        let output = self.run(&["build", target]);
        assert!(output.status.success(), "{}", stderr(&output));
        let stdout = String::from_utf8(output.stdout).unwrap();
        let path = stdout.lines().next().unwrap().split_once('\t').unwrap().1;
        fs::read(path).unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn rejected(output: &Output, marker: &str) {
    assert!(!output.status.success(), "{}", stderr(output));
    assert_ne!(output.status.code(), Some(124), "request timed out");
    assert!(!stderr(output).contains("panicked"), "{}", stderr(output));
    assert!(stderr(output).contains(marker), "{}", stderr(output));
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

const GAZELLE_BEFORE: &str = "load(\n    \"@bazel_skylib//lib:shell.bzl\",\n    \"shell\",\n)\n\nDEFAULT_LANGUAGES = []\n\ndef proto_gazelle(name):\n    native.sh_binary(\n        name = name,\n    )\n";
const GAZELLE_AFTER: &str = "load(\n    \"@bazel_skylib//lib:shell.bzl\",\n    \"shell\",\n)\nload(\n    \"@rules_shell//shell:sh_binary.bzl\",\n    \"sh_binary\",\n)\n\nDEFAULT_LANGUAGES = []\n\ndef proto_gazelle(name):\n    sh_binary(\n        name = name,\n    )\n";
/// thirdparty/build_stack_rules_proto/proto_gazelle_bazel9.patch shape:
/// a/b prefixes, tab-separated timestamps, no `diff --git` header.
const GAZELLE_PATCH: &str = "--- a/rules/proto_gazelle.bzl\t2024-05-14 13:52:59.000000000 +0000\n+++ b/rules/proto_gazelle.bzl\t2026-06-04 19:22:29.815049510 +0000\n@@ -2,6 +2,10 @@\n     \"@bazel_skylib//lib:shell.bzl\",\n     \"shell\",\n )\n+load(\n+    \"@rules_shell//shell:sh_binary.bzl\",\n+    \"sh_binary\",\n+)\n \n DEFAULT_LANGUAGES = []\n \n@@ -8,4 +12,4 @@\n def proto_gazelle(name):\n-    native.sh_binary(\n+    sh_binary(\n         name = name,\n     )\n";
const COMPILE_PATCH: &str = "--- a/rules/proto_compile.bzl\t2024-05-14 13:52:59.000000000 +0000\n+++ b/rules/proto_compile.bzl\t2026-06-04 19:22:29.815049510 +0000\n@@ -1,2 +1,2 @@\n-tools = ctx.resolve_tools()\n+tools = None\n keep = 1\n";

#[test]
fn http_archive_applies_label_patches_and_returns_repo_metadata() {
    let f = Fixture::new();
    let served = f.0.path().join("served");
    let archive = tar_gz(&[
        (
            "rules_proto-grav/rules/proto_gazelle.bzl",
            GAZELLE_BEFORE.as_bytes(),
        ),
        (
            "rules_proto-grav/rules/proto_compile.bzl",
            b"tools = ctx.resolve_tools()\nkeep = 1\n",
        ),
    ]);
    let sha256 = format!("{:x}", Sha256::digest(&archive));
    fs::create_dir(&served).unwrap();
    fs::write(served.join("grav.tar.gz"), &archive).unwrap();
    let server = Server::new(served);
    f.file(
        "MODULE.bazel",
        format!(
            r#"module(name = "main")
http_archive = use_repo_rule("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
http_archive(
    name = "build_stack_rules_proto",
    sha256 = "{sha256}",
    strip_prefix = "rules_proto-grav",
    urls = ["{url}/grav.tar.gz"],
    build_file_content = "exports_files(glob(['rules/*.bzl']))\n",
    patches = [
        "@@//thirdparty:build_stack_rules_proto/proto_compile_bazel9.patch",
        "@@//thirdparty:build_stack_rules_proto/proto_gazelle_bazel9.patch",
    ],
    patch_args = ["-p1"],
)
"#,
            url = server.url()
        ),
    );
    f.file(
        "thirdparty/BUILD.bazel",
        "exports_files(['build_stack_rules_proto/proto_compile_bazel9.patch', 'build_stack_rules_proto/proto_gazelle_bazel9.patch'])\n",
    );
    f.file(
        "thirdparty/build_stack_rules_proto/proto_gazelle_bazel9.patch",
        GAZELLE_PATCH,
    );
    f.file(
        "thirdparty/build_stack_rules_proto/proto_compile_bazel9.patch",
        COMPILE_PATCH,
    );
    f.file(
        "BUILD.bazel",
        "genrule(name = 'gazelle', srcs = ['@build_stack_rules_proto//:rules/proto_gazelle.bzl'], outs = ['gazelle.bzl'], cmd = 'cat $(SRCS) > $@')\ngenrule(name = 'compile', srcs = ['@build_stack_rules_proto//:rules/proto_compile.bzl'], outs = ['compile.bzl'], cmd = 'cat $(SRCS) > $@')\n",
    );
    // http.bzl:191 returns ctx.repo_metadata(reproducible = True) (sha256 set);
    // utils.bzl patch() applies both label patches natively with strip 1.
    assert_eq!(f.built("//:gazelle"), GAZELLE_AFTER.as_bytes());
    assert_eq!(f.built("//:compile"), b"tools = None\nkeep = 1\n");

    // A two-line hunk whose context does not match is skipped silently by
    // applyFuzzy(…, 2) (PatchUtil.java:221-235): genuine Bazel succeeds with
    // the file unpatched (captured A1/broken_compile_patch).
    f.file(
        "thirdparty/build_stack_rules_proto/proto_compile_bazel9.patch",
        COMPILE_PATCH.replace("ctx.resolve_tools()", "ctx.other()"),
    );
    assert_eq!(
        f.built("//:compile"),
        b"tools = ctx.resolve_tools()\nkeep = 1\n"
    );
    // Without -p1 the a/ b/ names are not found (captured B5/patch_args_p0).
    f.file(
        "MODULE.bazel",
        fs::read_to_string(f.0.path().join("MODULE.bazel"))
            .unwrap()
            .replace("patch_args = [\"-p1\"],", ""),
    );
    let output = f.run(&["build", "//:compile"]);
    rejected(
        &output,
        &format!(
            "Error applying patch {}/thirdparty/build_stack_rules_proto/proto_compile_bazel9.patch: Cannot find file to patch (near line 1), old file name (a/rules/proto_compile.bzl) doesn't exist, new file name (b/rules/proto_compile.bzl) doesn't exist.",
            f.0.path().display()
        ),
    );
}

const STRINGS_BZL_HEADER: &str = "def chr(i):\n    if i < 0 or i > 255:\n        fail(\"expected a int between 0 and 255 (inclusive)\")\n    return INT_TO_CHAR[i]\n\ndef ord(c):\n    if len(c) != 1:\n        fail(\"expected a string with a single character\")\n    return CHAR_TO_INT.get(c)\n";

/// bazel_lib lib/private/{strings,base64}.bzl shape: octal-escape tables
/// (strings.bzl:262-518) behind user-defined chr/ord, and base64.bzl
/// decode()/encode() that load them (base64.bzl:9-35).
fn byte_helpers(f: &Fixture) {
    let table: Vec<String> = (0..=255).map(|i| format!("    \"\\{i:03o}\",")).collect();
    let dict: Vec<String> = (0..=255)
        .map(|i| format!("    \"\\{i:03o}\": {i},"))
        .collect();
    f.file("lib/BUILD.bazel", "");
    f.file(
        "lib/strings.bzl",
        format!(
            "CHAR_TO_INT = {{\n{}\n}}\n\nINT_TO_CHAR = [\n{}\n]\n\n{STRINGS_BZL_HEADER}",
            dict.join("\n"),
            table.join("\n")
        ),
    );
    f.file(
        "lib/base64.bzl",
        r#"load(":strings.bzl", "chr", "ord")

BASE64_CHARS = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"

def _int_to_binary(i, digits = 8):
    return "".join([str((i >> (digits - 1 - k)) & 1) for k in range(digits)])

def _chunk(data, length):
    return [data[i:i + length] for i in range(0, len(data), length)]

def decode(data):
    padding = data.count("=")
    data = data.replace("=", "A")
    binstring = ""
    for i in range(len(data)):
        index = BASE64_CHARS.find(data[i])
        if index == -1:
            fail("expected a base64 encoded string")
        binstring += _int_to_binary(index, 6)
    outstring = ""
    for chunk in _chunk(binstring, 8):
        outstring += chr(int(chunk, 2))
    return outstring if padding == 0 else outstring[:-padding]

def encode(data):
    padding = 0
    if len(data) % 3 != 0:
        padding = (len(data) + 3 - len(data) % 3) - len(data)
    data += "\0" * padding
    binstring = ""
    for chunk in _chunk(data, 3):
        for i in range(len(chunk)):
            binstring += _int_to_binary(ord(chunk[i]))
    outstring = ""
    for element in _chunk(binstring, 6):
        outstring += BASE64_CHARS[int(element, 2)]
    return outstring if padding == 0 else outstring[:-padding] + "=" * padding
"#,
    );
}

#[test]
fn executed_bzl_octal_byte_tables_and_user_chr_write_latin1_bytes() {
    let f = Fixture::new();
    byte_helpers(&f);
    f.file(
        "MODULE.bazel",
        "module(name = 'main')\nmake = use_repo_rule('//:defs.bzl', 'make')\nmake(name = 'made', payload = '/wDpw6k=', escaped = '\\377\\200A')\n",
    );
    f.file(
        "defs.bzl",
        r#"load("//lib:base64.bzl", "decode", "encode")
load("//lib:strings.bzl", "chr", "ord")

def _impl(ctx):
    raw = decode(ctx.attr.payload)
    len(raw) == 5 or fail("decoded length %d" % len(raw))
    [ord(c) for c in raw.elems()] == [255, 0, 233, 195, 169] or fail("decoded bytes")
    encode(raw) == ctx.attr.payload or fail("round trip " + encode(raw))
    len("\377") == 1 or fail("octal is one char")
    ctx.file("BUILD.bazel", "exports_files(['decoded', 'literal', 'attr'])\n")
    ctx.file("decoded", raw, executable = False)
    ctx.file("literal", "\377\200" + chr(65) + chr(233) + "\351", executable = False)
    ctx.file("attr", ctx.attr.escaped + str(len(ctx.attr.escaped)), executable = False)
    return ctx.repo_metadata(attrs_for_reproducibility = {"payload": ctx.attr.payload})

make = repository_rule(_impl, attrs = {"payload": attr.string(), "escaped": attr.string()})
"#,
    );
    f.file("BUILD.bazel", "");
    assert_eq!(f.built("@made//:decoded"), [0xff, 0x00, 0xe9, 0xc3, 0xa9]);
    assert_eq!(f.built("@made//:literal"), [0xff, 0x80, b'A', 0xe9, 0xe9]);
    assert_eq!(f.built("@made//:attr"), [0xff, 0x80, b'A', b'3']);

    // Bazel's universe has no chr: a free reference fails in the resolver.
    f.file(
        "lib/free.bzl",
        "def unused():\n    return str(chr)\nvalue = 'ASCII output'\n",
    );
    f.file(
        "defs.bzl",
        "load('//lib:free.bzl', 'value')\ndef _impl(ctx):\n    ctx.file('BUILD.bazel', '')\nmake = repository_rule(_impl, attrs = {'payload': attr.string(), 'escaped': attr.string()})\n",
    );
    let output = f.run(&["build", "@made//:decoded"]);
    rejected(&output, "free.bzl:2:16");
    rejected(&output, "name 'chr' is not defined");
    // Hexadecimal escapes are Bazel lexer errors, also in executed code.
    f.file("lib/free.bzl", "value = '\\x41'\n");
    rejected(
        &f.run(&["build", "@made//:decoded"]),
        "invalid escape sequence: \\x. Use '\\\\' to insert '\\'.",
    );
}

#[test]
fn repository_rules_patch_with_strip_directory_and_return_values() {
    let f = Fixture::new();
    f.file(
        "MODULE.bazel",
        "module(name = 'main')\nmake = use_repo_rule('//:defs.bzl', 'make')\nmake(name = 'made', result = 'dict')\nmake(name = 'bad', result = 'bad_dict')\nmake(name = 'int', result = 'int')\n",
    );
    f.file("patches/BUILD.bazel", "exports_files(['fix.patch'])\n");
    f.file(
        "patches/fix.patch",
        "diff --git a/p.txt b/p.txt\nindex 1111111..2222222 100644\n--- a/p.txt\n+++ b/p.txt\n@@ -1 +1 @@\n-a\n+b\n",
    );
    f.file(
        "defs.bzl",
        r#"def _impl(ctx):
    ctx.file("BUILD.bazel", "exports_files(['p.txt', 'sub/p.txt'])\n")
    ctx.file("p.txt", "a\n", executable = False)
    ctx.patch(Label("//patches:fix.patch"), strip = 1)
    ctx.file("sub/p.txt", "a\n", executable = False)
    ctx.patch(Label("//patches:fix.patch"), 1, directory = "sub", watch_patch = "no")
    if ctx.attr.result == "dict":
        return {"name": ctx.name, "result": "dict"}
    if ctx.attr.result == "bad_dict":
        return {1: "x"}
    return 42

make = repository_rule(_impl, attrs = {"result": attr.string()})
"#,
    );
    f.file("BUILD.bazel", "");
    assert_eq!(f.built("@made//:p.txt"), b"b\n");
    assert_eq!(f.built("@made//:sub/p.txt"), b"b\n");
    assert_eq!(f.built("@int//:p.txt"), b"b\n");
    rejected(
        &f.run(&["build", "@bad//:p.txt"]),
        "got dict<int, string> for 'return value', want dict<string, unknown>",
    );
}

#[test]
fn generated_bzl_symlinks_load_inside_and_into_materialized_repositories() {
    let f = Fixture::new();
    let outside = tempfile::Builder::new()
        .prefix("n26-outside-")
        .tempdir_in(crate::test_temp::root())
        .unwrap();
    fs::write(outside.path().join("config.bzl"), "VALUE = 'outside'\n").unwrap();
    f.file(
        "MODULE.bazel",
        format!(
            "module(name = 'main')\nbazel_dep(name = 'cclike', version = '1')\nlocal_path_override(module_name = 'cclike', path = 'cclike')\nmake = use_repo_rule('//:defs.bzl', 'make')\nmake(name = 'made')\ncross = use_repo_rule('//:defs.bzl', 'cross')\ncross(name = 'cross', target = '//tools:config.bzl')\ncross(name = 'cc_config', target = '@cclike//cc/private:config.bzl')\nhost = use_repo_rule('//:defs.bzl', 'host')\nhost(name = 'host', target = {:?})\n",
            outside.path().join("config.bzl").display().to_string()
        ),
    );
    f.file(
        "cclike/MODULE.bazel",
        "module(name = 'cclike', version = '1')\n",
    );
    f.file(
        "cclike/cc/private/BUILD.bazel",
        "exports_files(['config.bzl'])\n",
    );
    f.file("cclike/cc/private/config.bzl", "VALUE = 'from cclike'\n");
    f.file("tools/BUILD.bazel", "exports_files(['config.bzl'])\n");
    f.file("tools/config.bzl", "VALUE = 'from main'\n");
    f.file(
        "defs.bzl",
        r#"def _impl(ctx):
    ctx.file("private/real.bzl", "VALUE = 'linked'\n")
    ctx.symlink("private/real.bzl", "config.bzl")
    ctx.file("BUILD.bazel", "load(':config.bzl', 'VALUE')\ngenrule(name = 'out', outs = ['out.txt'], cmd = 'echo %s > $@' % VALUE)\n")

make = repository_rule(_impl)

def _cross(ctx):
    # unix_cc_configure.bzl:362-370 shape: link a .bzl of another repository.
    ctx.symlink(Label(ctx.attr.target), "config.bzl")
    ctx.file("BUILD.bazel", "load(':config.bzl', 'VALUE')\ngenrule(name = 'out', outs = ['out.txt'], cmd = 'echo %s > $@' % VALUE)\n")

cross = repository_rule(_cross, attrs = {"target": attr.string()})

def _host(ctx):
    ctx.symlink(ctx.attr.target, "config.bzl")
    ctx.file("BUILD.bazel", "load(':config.bzl', 'VALUE')\ngenrule(name = 'out', outs = ['out.txt'], cmd = 'echo %s > $@' % VALUE)\n")

host = repository_rule(_host, attrs = {"target": attr.string()})
"#,
    );
    f.file("BUILD.bazel", "");
    assert_eq!(f.built("@made//:out"), b"linked\n");
    // Links into a repository materialized in this invocation are read
    // through under the logical label (main repository and a module).
    assert_eq!(f.built("@cross//:out"), b"from main\n");
    assert_eq!(f.built("@cc_config//:out"), b"from cclike\n");
    // A host path linked by a repository rule is read like Bazel (N28d
    // LinkPolicy host reads, docs/GLOB_SYMLINKS.md).
    assert_eq!(f.built("@host//:out"), b"outside\n");
    // A checked-in main-repository link to the same host file stays an
    // explicit error.
    f.file(
        "linked/BUILD.bazel",
        "load(':config.bzl', 'VALUE')\ngenrule(name = 'out', outs = ['out.txt'], cmd = 'echo %s > $@' % VALUE)\n",
    );
    std::os::unix::fs::symlink(
        outside.path().join("config.bzl"),
        f.0.path().join("linked/config.bzl"),
    )
    .unwrap();
    rejected(
        &f.run(&["build", "//linked:out"]),
        "outside its repository root",
    );
}
