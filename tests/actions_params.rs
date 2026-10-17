//! N23d CLI coverage (docs/ACTIONS_PARAMS.md): Args parameter files, tree
//! artifacts with execution-time Args expansion, actions.symlink,
//! expand_template/template_dict, Starlark spawn environments, and cache keys
//! that cover parameter-file and tree contents. Expected bytes follow the
//! paired genuine Bazel 9.3 capture (tests/fixtures/actions_params).
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("actions-params-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='actions_params_fixture')\n");
        for (name, text) in files {
            ws.file(name, text);
        }
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, text: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "120s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }

    /// Build and return (output path relative to the output root, path).
    fn build(&self, targets: &[&str]) -> (Vec<(String, PathBuf)>, String) {
        let mut args = vec!["build"];
        args.extend(targets);
        let output = self.run(&args);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stderr}");
        let outputs = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .filter_map(|(_, path)| {
                let relative = path.split_once("/bin/")?.1.to_owned();
                Some((relative, PathBuf::from(path)))
            })
            .collect();
        (outputs, stderr)
    }

    fn rejected(&self, target: &str, diagnostic: &str) {
        let output = self.run(&["build", target]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "unexpected success: {target}");
        assert!(stderr.contains(diagnostic), "{target}: {stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        // Published tree artifacts are read-only (0555) directories; make them
        // writable so the temporary directory can be removed.
        let _ = Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(self.path())
            .status();
    }
}

fn read(outputs: &[(String, PathBuf)], name: &str) -> String {
    let path = &outputs
        .iter()
        .find(|(relative, _)| relative == name)
        .unwrap_or_else(|| panic!("no output {name} in {outputs:?}"))
        .1;
    fs::read_to_string(path).unwrap()
}

fn mode(outputs: &[(String, PathBuf)], name: &str) -> u32 {
    let path = &outputs
        .iter()
        .find(|(relative, _)| relative == name)
        .unwrap()
        .1;
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

const DUMP: &str = r#"for a in "$@"; do printf '[%s]\n' "$a"; case "$a" in @*) f="${a#@}";; --params=*) f="${a#--params=}";; *) f=;; esac; if [ -n "$f" ]; then printf '<<%s>>\n' "${f##*/}"; cat "$f"; echo '<<end>>'; fi; done > "$OUT""#;

const DEFS: &str = r#"
def _base(f):
    return "base:" + f.basename

def _pair(f):
    return [f.basename, f.extension]

def _path(f):
    return f.path

def _dump(ctx, name, arguments, inputs = []):
    out = ctx.actions.declare_file(name)
    ctx.actions.run_shell(outputs = [out], inputs = inputs, command = DUMP, arguments = arguments, env = {"OUT": out.path})
    return out

def _params_impl(ctx):
    a = ctx.actions.args()
    a.add("--flag", "v w").add_all(["it's", ctx.attr.word])
    a.use_param_file("@%s", use_always = True)
    b = ctx.actions.args()
    b.set_param_file_format("flag_per_line")
    b.use_param_file("--params=%s", use_always = True)
    b.add("--k", "v w").add("pos").add_all("--many", ["1", "2"])
    c = ctx.actions.args().add_all(["small"]).use_param_file("@%s")
    big = ctx.actions.args().add_all(["x" * 100 for i in range(400)]).use_param_file("@%s")
    return [DefaultInfo(files = depset([
        _dump(ctx, "always.txt", [a]),
        _dump(ctx, "flag_per_line.txt", [b]),
        _dump(ctx, "parts.txt", ["lit", a, c, "end"]),
        _dump(ctx, "big.txt", [big]),
    ]))]

params = rule(implementation = _params_impl, attrs = {"word": attr.string()})

def _tree_impl(ctx):
    d = ctx.actions.declare_directory("tree")
    ctx.actions.run_shell(outputs = [d], command = "mkdir -p $1/sub/empty && printf %s > $1/a.txt && printf b > $1/sub/b.md" % ctx.attr.word, arguments = [d.path])
    a = ctx.actions.args()
    a.add_all([d], map_each = _base)
    a.add_joined([d], join_with = ",", map_each = _pair, uniquify = True)
    a.add_all([d], expand_directories = False)
    consumer = _dump(ctx, "consumer.txt", [a], inputs = [d])
    w = ctx.actions.declare_file("written.txt")
    ctx.actions.write(w, a)
    link = ctx.actions.declare_directory("tree_link")
    ctx.actions.symlink(output = link, target_file = d)
    return [DefaultInfo(files = depset([d, consumer, w, link]))]

tree = rule(implementation = _tree_impl, attrs = {"word": attr.string()})

def _files_impl(ctx):
    link = ctx.actions.declare_file("link.txt")
    ctx.actions.symlink(output = link, target_file = ctx.file.src)
    td = ctx.actions.template_dict()
    td.add("%K%", "kay")
    td.add_joined("%L%", depset([ctx.file.src]), join_with = ",", map_each = _path, format_joined = "<%s>")
    out = ctx.actions.declare_file("expanded.txt")
    ctx.actions.expand_template(template = ctx.file.src, output = out, substitutions = {"A": "AB", "B": "C"}, computed_substitutions = td)
    envs = []
    for name, default, env in [("env_default.txt", True, {"X": "1"}), ("env_plain.txt", False, {"X": "1"})]:
        e = ctx.actions.declare_file(name)
        ctx.actions.run_shell(outputs = [e], command = "env | LC_ALL=C sort | grep -v '^PWD=\\|^SHLVL=\\|^_=\\|^TMPDIR=' > $1 || true", arguments = [e.path], use_default_shell_env = default, env = env, execution_requirements = {"no-sandbox": "1"})
        envs.append(e)
    return [DefaultInfo(files = depset([link, out] + envs))]

files = rule(implementation = _files_impl, attrs = {"src": attr.label(allow_single_file = True)})

def _bad_impl(ctx):
    m = ctx.attr.mode
    o = ctx.actions.declare_file("o")
    if m == "param_arg":
        ctx.actions.args().use_param_file("@")
    elif m == "format_twice":
        ctx.actions.args().set_param_file_format("shell").set_param_file_format("shell")
    elif m == "symlink_path":
        ctx.actions.symlink(output = o, target_path = "x")
    elif m == "symlink_exec":
        ctx.actions.symlink(output = o, target_file = ctx.file.src, is_executable = True)
    elif m == "add_dir":
        ctx.actions.args().add(ctx.actions.declare_directory("d"))
    elif m == "dir_not_input":
        d = ctx.actions.declare_directory("d")
        ctx.actions.run_shell(outputs = [d], command = "true")
        ctx.actions.run_shell(outputs = [o], command = "true", arguments = [ctx.actions.args().add_all([d])])
    elif m == "declare_symlink":
        ctx.actions.declare_symlink("s")
    elif m == "transform":
        ctx.actions.transform_info_file(transform_func = _path, template = ctx.file.src, output_file_name = "info.txt")
    elif m == "file_dir":
        ctx.actions.declare_file("same")
        ctx.actions.declare_directory("same")
    elif m == "duplicate":
        td = ctx.actions.template_dict().add("K", "2")
        ctx.actions.expand_template(template = ctx.file.src, output = o, substitutions = {"K": "1"}, computed_substitutions = td)
    return [DefaultInfo(files = depset([o]))]

bad = rule(implementation = _bad_impl, attrs = {"mode": attr.string(), "src": attr.label(allow_single_file = True)})
"#;

const BUILD: &str = r#"
load(":defs.bzl", "bad", "files", "params", "tree")
params(name = "params", word = "one")
tree(name = "tree", word = "a")
files(name = "files", src = "src.txt")
[bad(name = m, mode = m, src = "src.txt") for m in ["param_arg", "format_twice", "symlink_path", "symlink_exec", "add_dir", "dir_not_input", "declare_symlink", "transform", "duplicate", "file_dir"]]
"#;

fn workspace() -> Workspace {
    let defs = format!("DUMP = {DUMP:?}\n{DEFS}");
    let ws = Workspace::new(&[
        ("pkg/defs.bzl", &defs),
        ("pkg/BUILD.bazel", BUILD),
        ("pkg/src.txt", "A B %K% %L%\n"),
    ]);
    fs::set_permissions(
        ws.path().join("pkg/src.txt"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    ws
}

#[test]
fn param_files_follow_bazel_naming_formats_and_threshold() {
    let ws = workspace();
    let (outputs, _) = ws.build(&["//pkg:params"]);
    assert_eq!(
        read(&outputs, "pkg/always.txt"),
        "[@bazel-out/k8-fastbuild/bin/pkg/always.txt-0.params]\n<<always.txt-0.params>>\n--flag\n'v w'\n'it'\\''s'\none\n<<end>>\n"
    );
    assert_eq!(
        read(&outputs, "pkg/flag_per_line.txt"),
        "[--params=bazel-out/k8-fastbuild/bin/pkg/flag_per_line.txt-0.params]\n<<flag_per_line.txt-0.params>>\n--k=v w\n--many=1 2\n<<end>>\n[pos]\n"
    );
    // Two parameter-file parts are numbered by their position among spilled
    // parts; a part under the threshold stays inline.
    assert_eq!(
        read(&outputs, "pkg/parts.txt"),
        "[lit]\n[@bazel-out/k8-fastbuild/bin/pkg/parts.txt-0.params]\n<<parts.txt-0.params>>\n--flag\n'v w'\n'it'\\''s'\none\n<<end>>\n[small]\n[end]\n"
    );
    let big = read(&outputs, "pkg/big.txt");
    assert!(big.starts_with("[@bazel-out/k8-fastbuild/bin/pkg/big.txt-0.params]\n"));
    assert_eq!(big.matches(&"x".repeat(100)).count(), 400);
    assert_eq!(mode(&outputs, "pkg/always.txt"), 0o555);

    // Parameter-file content is part of the action key.
    let (_, cached) = ws.build(&["//pkg:params"]);
    assert!(cached.contains("0 actions executed"), "{cached}");
    ws.file(
        "pkg/BUILD.bazel",
        &BUILD.replace("word = \"one\"", "word = \"two\""),
    );
    let (outputs, rebuilt) = ws.build(&["//pkg:params"]);
    assert!(rebuilt.contains("2 actions executed"), "{rebuilt}");
    assert!(read(&outputs, "pkg/always.txt").contains("\ntwo\n"));
}

#[test]
fn tree_artifacts_expand_in_args_written_args_and_symlinks() {
    let ws = workspace();
    let (outputs, _) = ws.build(&["//pkg:tree"]);
    assert_eq!(
        read(&outputs, "pkg/consumer.txt"),
        "[base:a.txt]\n[base:b.md]\n[a.txt,txt,b.md,md]\n[bazel-out/k8-fastbuild/bin/pkg/tree]\n"
    );
    assert_eq!(
        read(&outputs, "pkg/written.txt"),
        "base:a.txt\nbase:b.md\na.txt,txt,b.md,md\nbazel-out/k8-fastbuild/bin/pkg/tree\n"
    );
    for name in ["pkg/tree", "pkg/tree_link"] {
        let root = &outputs.iter().find(|(r, _)| r == name).unwrap().1;
        assert_eq!(mode(&outputs, name), 0o555);
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "a");
        assert_eq!(fs::read_to_string(root.join("sub/b.md")).unwrap(), "b");
        assert!(root.join("sub/empty").is_dir());
        let child = fs::metadata(root.join("sub")).unwrap().permissions().mode();
        assert_eq!(child & 0o7777, 0o555);
    }
    // Tree contents are part of the consumers' keys.
    ws.file(
        "pkg/BUILD.bazel",
        &BUILD.replace("word = \"a\"", "word = \"z\""),
    );
    let (outputs, rebuilt) = ws.build(&["//pkg:tree"]);
    assert!(rebuilt.contains("4 actions executed"), "{rebuilt}");
    let root = &outputs
        .iter()
        .find(|(r, _)| r == "pkg/tree_link")
        .unwrap()
        .1;
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "z");
}

#[test]
fn symlink_template_and_spawn_environments() {
    let ws = workspace();
    let (outputs, _) = ws.build(&["//pkg:files"]);
    assert_eq!(read(&outputs, "pkg/link.txt"), "A B %K% %L%\n");
    assert_eq!(mode(&outputs, "pkg/link.txt"), 0o644);
    assert_eq!(
        read(&outputs, "pkg/expanded.txt"),
        "AC C kay <pkg/src.txt>\n"
    );
    assert_eq!(mode(&outputs, "pkg/expanded.txt"), 0o555);
    assert_eq!(
        read(&outputs, "pkg/env_default.txt"),
        "PATH=/bin:/usr/bin:/usr/local/bin\nX=1\n"
    );
    assert_eq!(read(&outputs, "pkg/env_plain.txt"), "X=1\n");
}

#[test]
fn rejections_are_bazel_messages_or_explicit_native_gaps() {
    let ws = workspace();
    for (target, diagnostic) in [
        (
            "//pkg:param_arg",
            "Invalid value for parameter \"param_file_arg\": Expected string with a single \"%s\", got \"@\"",
        ),
        (
            "//pkg:format_twice",
            "set_param_file_format() may only be called once",
        ),
        (
            "//pkg:symlink_path",
            "symlink() with \"target_path\" param requires that \"output\" be declared as a symlink",
        ),
        (
            "//pkg:symlink_exec",
            "Creating symlink pkg/o failed: failed to create symbolic link 'bazel-out/k8-fastbuild/bin/pkg/o': file 'pkg/src.txt' is not executable",
        ),
        (
            "//pkg:add_dir",
            "Cannot add directories to Args#add since they may expand to multiple values.",
        ),
        (
            "//pkg:dir_not_input",
            "Action pkg/o failed: Failed to expand directory <generated file pkg/d>. Either add the directory as an input of the action",
        ),
        (
            "//pkg:declare_symlink",
            "not supported by the native profile",
        ),
        (
            "//pkg:transform",
            "file '//pkg:defs.bzl' cannot use private API",
        ),
        ("//pkg:duplicate", "Multiple entries with same key: K="),
        (
            "//pkg:file_dir",
            "The following directories were also declared as files:\npkg/same",
        ),
    ] {
        ws.rejected(target, diagnostic);
    }
}
