//! N48: Bazel 9.3 rejects recursive calls (`function 'NAME' called
//! recursively`, net/starlark/java/eval/StarlarkFunction.java:571-574 and
//! StarlarkThread.isRecursiveCall), with the error located at the re-entered
//! function (`def` identifier or `lambda` start), Bazel's innermost traceback
//! frame. Callbacks Bazel runs in a new Starlark thread (symbolic macro
//! implementations, `map_each`) start a new recursion scope. Expected texts and
//! locations are those of the paired capture
//! (tests/fixtures/no_recursion/reference_receipt.json).
use std::fs;
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("no-recursion-cli-")
                .tempdir_in(std::env::temp_dir())
                .unwrap(),
        );
        for (name, text) in [("MODULE.bazel", "module(name = 'no_recursion')\n")]
            .iter()
            .chain(files)
        {
            let path = ws.0.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        ws
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.0.path().join("private-cache").display()
            ))
            .args(args)
            .current_dir(self.0.path())
            .output()
            .unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Fails with `function 'NAME' called recursively` located at `location`
/// (`//pkg:file:line:column`), after the traceback frames `frames` (in order).
fn assert_recursion(output: &Output, name: &str, location: &str, frames: &[&str]) {
    let err = stderr(output);
    assert!(!output.status.success(), "{err}");
    let message = format!("error: function '{name}' called recursively\n");
    let at = err
        .find(&message)
        .unwrap_or_else(|| panic!("missing {message:?} in:\n{err}"));
    let arrow = err[at + message.len()..].lines().next().unwrap_or_default();
    assert_eq!(arrow.trim_start(), format!("--> {location}"), "{err}");
    let mut rest = err.as_str();
    for frame in frames {
        let at = rest
            .find(frame)
            .unwrap_or_else(|| panic!("missing frame {frame:?} in order in:\n{err}"));
        rest = &rest[at + frame.len()..];
    }
    assert!(
        rest.starts_with('\n') || rest.contains(&message),
        "frames must precede the error:\n{err}"
    );
}

fn bzl(code: &str) -> Workspace {
    Workspace::new(&[
        ("p/BUILD", "load(\":t.bzl\", \"x\")\n"),
        ("p/t.bzl", &format!("{code}\nx = 1\n")),
    ])
}

#[test]
fn recursion_at_bzl_top_level_is_rejected_at_the_function() {
    let cases: &[(&str, &str, &str, &[&str])] = &[
        (
            "def f(n):\n    return f(n - 1) if n else 0\n\nf(3)",
            "f",
            "//p:t.bzl:1:5",
            &["* //p:t.bzl:4, in <module>", "* //p:t.bzl:2, in f"],
        ),
        (
            "def a(n):\n    return b(n)\n\ndef b(n):\n    return c(n)\n\ndef c(n):\n    return a(n - 1) if n else 0\n\nb(2)",
            "b",
            "//p:t.bzl:4:5",
            &[
                "* //p:t.bzl:10, in <module>",
                "* //p:t.bzl:5, in b",
                "* //p:t.bzl:8, in c",
                "* //p:t.bzl:2, in a",
            ],
        ),
        (
            "f = lambda n: f(n - 1) if n else 0\nf(2)",
            "lambda",
            "//p:t.bzl:1:5",
            &["* //p:t.bzl:2, in <module>", "* //p:t.bzl:1, in lambda"],
        ),
        (
            "def key(x):\n    return sorted([1, 2], key = key)[0]\n\ndef run():\n    return key(1)\n\nrun()",
            "key",
            "//p:t.bzl:1:5",
            &[
                "* //p:t.bzl:7, in <module>",
                "* //p:t.bzl:5, in run",
                "* //p:t.bzl:2, in key",
                "File <builtin>, in sorted",
            ],
        ),
        (
            "def outer(n):\n    def inner():\n        return outer(n - 1)\n    return inner() if n else 0\n\nouter(2)",
            "outer",
            "//p:t.bzl:1:5",
            &[
                "* //p:t.bzl:6, in <module>",
                "* //p:t.bzl:4, in outer",
                "* //p:t.bzl:3, in inner",
            ],
        ),
        // Code identity: two closures of one nested def.
        (
            "def mk():\n    def inner(k, n):\n        return k(k, n - 1) if n else 0\n    return inner\n\nmk()(mk(), 2)",
            "inner",
            "//p:t.bzl:2:9",
            &["* //p:t.bzl:6, in <module>", "* //p:t.bzl:3, in inner"],
        ),
        (
            "def f(n):\n    return [f(i) for i in range(n)]\n\nf(2)",
            "f",
            "//p:t.bzl:1:5",
            &["* //p:t.bzl:4, in <module>", "* //p:t.bzl:2, in f"],
        ),
    ];
    for (code, name, location, frames) in cases {
        let ws = bzl(code);
        assert_recursion(&ws.run(&["query", "//p:all"]), name, location, frames);
    }
}

#[test]
fn recursion_through_a_loaded_function_and_provider_init() {
    let ws = Workspace::new(&[
        ("p/BUILD", "load(\":t.bzl\", \"x\")\n"),
        ("p/u.bzl", "def apply(g, n):\n    return g(n)\n"),
        (
            "p/t.bzl",
            "load(\":u.bzl\", \"apply\")\n\ndef h(n):\n    return apply(h, n - 1) if n else 0\n\nh(2)\nx = 1\n",
        ),
    ]);
    assert_recursion(
        &ws.run(&["query", "//p:all"]),
        "h",
        "//p:t.bzl:3:5",
        &[
            "* //p:t.bzl:6, in <module>",
            "* //p:t.bzl:4, in h",
            "* //p:u.bzl:2, in apply",
        ],
    );
    let ws = bzl(
        "def _init(**kwargs):\n    return {\"v\": P(**kwargs)}\n\nP, _new = provider(init = _init)\nP(a = 1)",
    );
    assert_recursion(
        &ws.run(&["query", "//p:all"]),
        "_init",
        "//p:t.bzl:1:5",
        &["* //p:t.bzl:5, in <module>", "* //p:t.bzl:2, in _init"],
    );
}

#[test]
fn functions_may_be_called_again_and_refer_to_each_other() {
    let ws = bzl(concat!(
        "calls = []\n\ndef yin(x):\n    calls.append(\"yin\")\n    if x:\n        yang(False)\n\n",
        "def yang(x):\n    calls.append(\"yang\")\n    if x:\n        yin(False)\n\n",
        "yin(True)\nyang(True)\n\ndef one():\n    return 1\n\n",
        "print(\"N48PRINT\", calls, [one() for _ in range(3)], sorted([3, 1, 2], key = lambda v: -v))",
    ));
    let output = ws.run(&["query", "//p:all"]);
    let err = stderr(&output);
    assert!(output.status.success(), "{err}");
    assert!(
        err.contains("N48PRINT [\"yin\", \"yang\", \"yang\", \"yin\"] [1, 1, 1] [3, 2, 1]"),
        "{err}"
    );
}

#[test]
fn argument_binding_fails_before_the_recursion_check() {
    let ws = bzl("def f(a):\n    f()\n\nf(1)");
    let output = ws.run(&["query", "//p:all"]);
    let err = stderr(&output);
    assert!(!output.status.success(), "{err}");
    assert!(
        err.contains("error: f() missing 1 required positional argument: a"),
        "{err}"
    );
    assert!(!err.contains("called recursively"), "{err}");
}

#[test]
fn recursion_in_legacy_and_symbolic_macros() {
    let ws = Workspace::new(&[
        ("p/BUILD", "load(\":t.bzl\", \"m\")\n\nm(name = \"x\")\n"),
        (
            "p/t.bzl",
            "def m(name, depth = 0):\n    if depth < 2:\n        m(name = name, depth = depth + 1)\n    native.filegroup(name = name)\n",
        ),
    ]);
    assert_recursion(
        &ws.run(&["query", "//p:all"]),
        "m",
        "//p:t.bzl:1:5",
        &["p/BUILD:3, in <module>", "* //p:t.bzl:3, in m"],
    );
    let ws = Workspace::new(&[
        ("p/BUILD", "load(\":t.bzl\", \"m\")\n\nm(name = \"x\")\n"),
        (
            "p/t.bzl",
            "def _helper(n):\n    return _helper(n - 1) if n else None\n\ndef _impl(name, visibility):\n    _helper(2)\n    native.filegroup(name = name)\n\nm = macro(implementation = _impl)\n",
        ),
    ]);
    assert_recursion(
        &ws.run(&["query", "//p:all"]),
        "_helper",
        "//p:t.bzl:1:5",
        &["* //p:t.bzl:5, in _impl", "* //p:t.bzl:2, in _helper"],
    );
}

/// Bazel runs a symbolic macro's implementation in a new Starlark thread
/// (MacroClass.executeMacroImplementation), so a helper that instantiated
/// the macro may be called again by its implementation.
#[test]
fn symbolic_macro_implementation_starts_a_new_recursion_scope() {
    let ws = Workspace::new(&[
        ("p/BUILD", "load(\":t.bzl\", \"lm\")\n\nlm(name = \"x\")\n"),
        (
            "p/t.bzl",
            concat!(
                "def _make(name, symbolic):\n    if symbolic:\n        m(name = name)\n",
                "    else:\n        native.filegroup(name = name + \"_fg\")\n\n",
                "def _impl(name, visibility):\n    _make(name, False)\n\n",
                "m = macro(implementation = _impl)\n\ndef lm(name):\n    _make(name, True)\n",
            ),
        ),
    ]);
    let output = ws.run(&["query", "//p:all"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "//p:x_fg\n");
}

const RULES: &str = r#"
def _helper(ctx, n):
    return _helper(ctx, n - 1) if n else []

def _helper_impl(ctx):
    return _helper(ctx, 2)

helper_rule = rule(implementation = _helper_impl)

def _self_impl(ctx, n = 1):
    if n:
        _self_impl(ctx, n - 1)
    return []

self_rule = rule(implementation = _self_impl)

def _fmt(x):
    if type(x) != "ctx":
        return str(x)
    ctx = x
    args = ctx.actions.args()
    args.add_all([1, 2], map_each = _fmt)
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, args)
    return [DefaultInfo(files = depset([out]))]

def _map_each_impl(ctx):
    return _fmt(ctx)

map_each_rule = rule(implementation = _map_each_impl)
"#;

#[test]
fn recursion_in_rule_implementations() {
    let ws = Workspace::new(&[
        (
            "p/BUILD",
            concat!(
                "load(\":t.bzl\", \"helper_rule\", \"map_each_rule\", \"self_rule\")\n\n",
                "helper_rule(name = \"helper\")\n\nself_rule(name = \"self\")\n\n",
                "map_each_rule(name = \"map_each\")\n",
            ),
        ),
        ("p/t.bzl", RULES),
    ]);
    assert_recursion(
        &ws.run(&["build", "//p:helper"]),
        "_helper",
        "//p:t.bzl:2:5",
        &[
            "* //p:t.bzl:6, in _helper_impl",
            "* //p:t.bzl:3, in _helper",
        ],
    );
    assert_recursion(
        &ws.run(&["build", "//p:self"]),
        "_self_impl",
        "//p:t.bzl:10:5",
        &["* //p:t.bzl:12, in _self_impl"],
    );
    // Bazel calls map_each in its own Starlark thread while expanding the
    // command line, not within the implementation that registered it.
    let output = ws.run(&["build", "//p:map_each"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout.trim_end().split_once('\t').unwrap().1;
    assert_eq!(fs::read_to_string(path).unwrap(), "1\n2\n");
}
