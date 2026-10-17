//! `bazel query` language (docs/QUERY.md) over tests/fixtures/query_language/ws.
//! Traversals pass `--notool_deps`: the fixture's exec-configured implicit
//! dependencies live in `@bazel_tools`, which needs the genuine built-in
//! profile and its registry (tests/fixtures/query_language/reference_receipt.json
//! records the genuine Bazel 9.3 comparison with them).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Workspace(PathBuf);

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

impl Workspace {
    fn new() -> Self {
        Self::fixture("ws")
    }

    fn fixture(name: &str) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!(
            "bazel-rs-query-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        copy_tree(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/query_language")
                .join(name),
            &path,
        );
        Self(fs::canonicalize(path).unwrap())
    }

    fn query(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .arg("--ignore_all_rc_files")
            .arg(format!(
                "--cache-dir={}",
                self.0.join("bazel-rs-out").display()
            ))
            .arg("query")
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.query(args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        String::from_utf8(output.stdout).unwrap()
    }

    fn labels(&self, args: &[&str]) -> Vec<String> {
        self.ok(args).lines().map(str::to_owned).collect()
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn path(&self) -> String {
        self.0.display().to_string()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn target_patterns_and_set_algebra() {
    let ws = Workspace::new();
    assert_eq!(
        ws.labels(&["//lib:all"]),
        ["//lib:base", "//lib:gen", "//lib:tool", "//lib:util"]
    );
    assert_eq!(
        ws.labels(&["//lib:*"]),
        [
            "//lib:BUILD.bazel",
            "//lib:base",
            "//lib:base.txt",
            "//lib:fast.txt",
            "//lib:gen",
            "//lib:gen.out",
            "//lib:slow.txt",
            "//lib:tool",
            "//lib:tool.sh",
            "//lib:util",
            "//lib:util.txt",
        ]
    );
    // Equal precedence, left associative: (a + b) - c.
    assert_eq!(
        ws.labels(&["//lib:base + //lib:util - //lib:base"]),
        ["//lib:util"]
    );
    assert_eq!(
        ws.labels(&["//lib:all intersect //lib:util union //cyc:a"]),
        ["//cyc:a", "//lib:util"]
    );
    assert_eq!(
        ws.labels(&["set(//lib:base //cyc:a) except //cyc:a"]),
        ["//lib:base"]
    );
    // The residue is joined with spaces (QueryOptionHelper.readQuery).
    assert_eq!(
        ws.labels(&["//lib:base", "+", "//cyc:b"]),
        ["//cyc:b", "//lib:base"]
    );
    assert_eq!(
        ws.labels(&["let v = //lib:util in $v + //cyc:a"]),
        ["//cyc:a", "//lib:util"]
    );
}

#[test]
fn deps_follow_every_select_branch_implicit_and_visibility_edges() {
    let ws = Workspace::new();
    assert_eq!(
        ws.labels(&["--notool_deps", "deps(//app:bin)"]),
        [
            "//alias:util_alias",
            "//app:bin",
            "//app:main.txt",
            "//conf:fast",
            "//lib:base",
            "//lib:base.txt",
            "//lib:fast.txt",
            "//lib:gen",
            "//lib:gen.out",
            "//lib:slow.txt",
            "//lib:tool",
            "//lib:tool.sh",
            "//lib:util",
            "//lib:util.txt",
            "//vis:friends",
            "//vis:more",
        ]
    );
    assert_eq!(
        ws.labels(&["--notool_deps", "deps(//app:bin, 1)"]),
        [
            "//alias:util_alias",
            "//app:bin",
            "//app:main.txt",
            "//lib:gen.out",
            "//lib:tool",
            "//lib:util",
            "//vis:friends",
        ]
    );
    // --noimplicit_deps drops `_tool` and the package default visibility.
    assert_eq!(
        ws.labels(&["--noimplicit_deps", "--notool_deps", "deps(//lib:util)"]),
        [
            "//conf:fast",
            "//lib:base",
            "//lib:base.txt",
            "//lib:fast.txt",
            "//lib:slow.txt",
            "//lib:util",
            "//lib:util.txt",
        ]
    );
    // Cycles are ordinary graph edges for query.
    assert_eq!(ws.labels(&["deps(//cyc:a)"]), ["//cyc:a", "//cyc:b"]);
    // Legacy macro outputs depend on the generated rule.
    assert_eq!(
        ws.labels(&["--notool_deps", "deps(//app:wrapped)"]),
        [
            "//app:wrapped",
            "//app:wrapped_impl",
            "//lib:base",
            "//lib:base.txt",
            "//lib:tool",
            "//lib:tool.sh",
        ]
    );
}

#[test]
fn reverse_and_path_functions() {
    let ws = Workspace::new();
    let universe = "//app/... + //lib/... + //alias/...";
    assert_eq!(
        ws.labels(&["--notool_deps", &format!("rdeps({universe}, //lib:base)")]),
        // test_suite $implicit_tests are explicit dependency edges.
        [
            "//alias:util_alias",
            "//app:all_tests",
            "//app:bin",
            "//app:bin_test",
            "//app:unit_tests",
            "//app:wrapped",
            "//app:wrapped_impl",
            "//lib:base",
            "//lib:util",
        ]
    );
    assert_eq!(
        ws.labels(&[
            "--notool_deps",
            &format!("rdeps({universe}, //lib:base, 1)")
        ]),
        ["//app:wrapped_impl", "//lib:base", "//lib:util"]
    );
    assert_eq!(
        ws.labels(&["--notool_deps", "allpaths(//app:bin, //lib:base)"]),
        [
            "//alias:util_alias",
            "//app:bin",
            "//lib:base",
            "//lib:util"
        ]
    );
    // A top-level somepath prints the path in order, not sorted.
    assert_eq!(
        ws.labels(&["--notool_deps", "somepath(//app:bin, //lib:base.txt)"]),
        ["//app:bin", "//lib:gen.out", "//lib:gen", "//lib:base.txt"]
    );
    assert_eq!(
        ws.labels(&["same_pkg_direct_rdeps(//lib:base)"]),
        ["//lib:util"]
    );
}

#[test]
fn filters_attributes_tests_and_files() {
    let ws = Workspace::new();
    assert_eq!(
        ws.labels(&["kind('my_.* rule', //...)"]),
        [
            "//app:bin",
            "//app:bin_test",
            "//app:manual_test",
            "//app:wrapped_impl",
            "//lib:base",
            "//lib:util",
        ]
    );
    assert_eq!(
        ws.labels(&["filter('_test$', //app:all)"]),
        ["//app:bin_test", "//app:manual_test"]
    );
    assert_eq!(
        ws.labels(&["attr(flavor, spicy, //lib:all)"]),
        ["//lib:util"]
    );
    // attr.bool given 1 in BUILD (Type.BOOLEAN.convert); booleans match as 0/1.
    assert_eq!(ws.labels(&["attr(stripped, 1, //lib:all)"]), ["//lib:util"]);
    assert_eq!(
        ws.labels(&["attr(srcs, 'fast', //lib:all)"]),
        ["//lib:util"],
        "every select() branch is a possible value"
    );
    assert_eq!(
        ws.labels(&["labels(srcs, //lib:util)"]),
        ["//lib:fast.txt", "//lib:slow.txt", "//lib:util.txt"]
    );
    // test_suite expansion: $implicit_tests skip manual tests; tags filter.
    assert_eq!(ws.labels(&["tests(//app:all_tests)"]), ["//app:bin_test"]);
    assert_eq!(ws.labels(&["tests(//app:unit_tests)"]), ["//app:bin_test"]);
    assert_eq!(
        ws.labels(&["tests(//app:all)"]),
        ["//app:bin_test", "//app:manual_test"]
    );
    assert_eq!(
        ws.labels(&["some(//lib:util + //lib:base)"]),
        ["//lib:util"]
    );
    assert_eq!(
        ws.labels(&["siblings(//conf:fast)"]),
        ["//conf:BUILD.bazel", "//conf:fast"]
    );
    assert_eq!(
        ws.labels(&["visible(//app:bin, //lib:all + //cyc:a)"]),
        ["//lib:base", "//lib:gen", "//lib:tool", "//lib:util"]
    );
    assert_eq!(
        ws.labels(&["buildfiles(//app:bin)"]),
        [
            "//:BUILD.bazel",
            "//:defs.bzl",
            "//app:BUILD.bazel",
            "//macros:BUILD.bazel",
            "//macros:inner.bzl",
            "//macros:wrap.bzl",
        ]
    );
    assert_eq!(
        ws.labels(&["loadfiles(//app:bin)"]),
        ["//:defs.bzl", "//macros:inner.bzl", "//macros:wrap.bzl"]
    );
    assert_eq!(ws.labels(&["executables(//app:all)"]), Vec::<String>::new());
}

#[test]
fn output_formatters() {
    let ws = Workspace::new();
    let root = ws.path();
    assert_eq!(
        ws.ok(&[
            "--output=label_kind",
            "//lib:util + //lib:util.txt + //lib:gen.out + //vis:friends"
        ]),
        "generated file //lib:gen.out\nmy_library rule //lib:util\nsource file //lib:util.txt\npackage group //vis:friends\n"
    );
    assert_eq!(
        ws.ok(&["--output=package", "deps(//lib:util, 1)"]),
        "conf\nlib\n"
    );
    assert_eq!(
        ws.ok(&[
            "--output=location",
            "//conf:fast + //lib:base.txt + //vis:friends"
        ]),
        format!(
            "{root}/conf/BUILD.bazel:1:15: config_setting rule //conf:fast\n{root}/lib/base.txt:1:1: source file //lib:base.txt\n{root}/vis/BUILD.bazel:1:14: package group //vis:friends\n"
        )
    );
    assert_eq!(
        ws.ok(&["--output=location", "--relative_locations", "//conf:fast"]),
        "conf/BUILD.bazel:1:15: config_setting rule //conf:fast\n"
    );
    assert_eq!(
        ws.ok(&["--notool_deps", "--output=minrank", "deps(//lib:util)"]),
        "0 //lib:util\n1 //conf:fast\n1 //lib:base\n1 //lib:fast.txt\n1 //lib:slow.txt\n1 //lib:tool\n1 //lib:util.txt\n2 //lib:base.txt\n2 //lib:tool.sh\n"
    );
    assert_eq!(
        ws.ok(&["--notool_deps", "--output=maxrank", "deps(//lib:util)"]),
        "0 //lib:util\n1 //conf:fast\n1 //lib:base\n1 //lib:fast.txt\n1 //lib:slow.txt\n1 //lib:util.txt\n2 //lib:base.txt\n2 //lib:tool\n3 //lib:tool.sh\n"
    );
    assert_eq!(
        ws.ok(&["--output=graph", "--order_output=full", "deps(//cyc:a)"]),
        "digraph mygraph {\n  node [shape=box];\n  \"//cyc:a\"\n  \"//cyc:a\" -> \"//cyc:b\"\n  \"//cyc:b\"\n  \"//cyc:b\" -> \"//cyc:a\"\n}\n"
    );
    // Ordered (deps/full) label output is a topological order.
    assert_eq!(
        ws.ok(&["--notool_deps", "--order_output=full", "deps(//lib:base)"]),
        "//lib:base\n//lib:tool\n//lib:tool.sh\n//lib:base.txt\n"
    );
    assert_eq!(
        ws.ok(&["--output=build", "//lib:util"]),
        format!(
            "# {root}/lib/BUILD.bazel:10:11\nmy_library(\n  name = \"util\",\n  srcs = [\"//lib:util.txt\"] + select({{\"//conf:fast\": [\"//lib:fast.txt\"], \"//conditions:default\": [\"//lib:slow.txt\"]}}),\n  deps = [\"//lib:base\"],\n  flavor = \"spicy\",\n  stripped = True,\n)\n# Rule util instantiated at (most recent call last):\n#   {root}/lib/BUILD.bazel:10:11 in <toplevel>\n# Rule my_library defined at (most recent call last):\n#   {root}/defs.bzl:8:18 in <toplevel>\n\n"
        )
    );
    assert_eq!(ws.ok(&["--null", "//cyc:all"]), "//cyc:a\0//cyc:b\0");
}

#[test]
fn errors_exit_codes_and_keep_going() {
    let ws = Workspace::new();
    let syntax = ws.query(&["deps(//lib:util"]);
    assert_eq!(syntax.status.code(), Some(2));
    assert_eq!(
        stderr(&syntax),
        "ERROR: Error while parsing 'deps(//lib:util': premature end of input\n"
    );
    let unknown = ws.query(&["allrdeps(//lib:util)"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(stderr(&unknown).contains("unknown function 'allrdeps'"));
    let format = ws.query(&["--output=json", "//lib:util"]);
    assert_eq!(format.status.code(), Some(2));
    assert!(stderr(&format).contains("Invalid output format 'json'. Valid values are: label, label_kind, build, minrank, maxrank, package, location, graph, xml, proto, streamed_jsonproto, streamed_proto"));
    let missing = ws.query(&["//nope:x"]);
    assert_eq!(missing.status.code(), Some(7));
    assert!(stderr(&missing).starts_with("ERROR: no such package 'nope': BUILD file not found"));
    // Unbounded visitation preloads first: its first failure, no events
    // (QueryTransitivePackagePreloader, paired receipt error.deps_missing_edges).
    let broken = ws.query(&["deps(//tests:broken)"]);
    assert_eq!(broken.status.code(), Some(7));
    assert_eq!(
        stderr(&broken),
        "ERROR: Evaluation of query \"deps(//tests:broken)\" failed: preloading transitive closure failed: no such package 'nope': BUILD file not found in any of the following directories. Add a BUILD file to a directory to mark it as a package.\n - nope\n"
    );
    assert!(broken.stdout.is_empty());
    // A bounded visitation reports each missing edge, then fails.
    let bounded = ws.query(&["deps(//tests:broken, 1)"]);
    assert_eq!(bounded.status.code(), Some(7));
    let text = stderr(&bounded);
    let root = ws.path();
    assert!(text.contains(&format!(
        "ERROR: {root}/tests/BUILD.bazel:1:10: no such target '//lib:absent': target 'absent' not declared in package 'lib' defined by {root}/lib/BUILD.bazel and referenced by '//tests:broken'\n"
    )), "{text}");
    assert!(text.ends_with(
        "ERROR: Evaluation of query \"deps(//tests:broken, 1)\" failed: errors were encountered while computing transitive closure\n"
    ), "{text}");
    // --keep_going: the partial result and exit code 3.
    let partial = ws.query(&["--keep_going", "deps(//tests:broken) + //cyc:a"]);
    assert_eq!(partial.status.code(), Some(3), "{}", stderr(&partial));
    assert_eq!(
        String::from_utf8_lossy(&partial.stdout),
        "//cyc:a\n//tests:broken\n"
    );
    let text = stderr(&partial);
    assert!(text.contains("ERROR: Evaluation of query \"deps(//tests:broken)\" failed: errors were encountered while computing transitive closure\n"), "{text}");
    assert!(
        text.ends_with(
            "WARNING: --keep_going specified, ignoring errors. Results may be inaccurate\n"
        ),
        "{text}"
    );
    let skipped = ws.query(&["-k", "//nope:x + //cyc:a"]);
    assert_eq!(skipped.status.code(), Some(3));
    assert!(stderr(&skipped).starts_with("ERROR: Skipping '//nope:x': no such package 'nope'"));
    // Subquery failures name the subexpression.
    let some = ws.query(&["//cyc:a + some(//cyc:a - //cyc:a)"]);
    assert_eq!(some.status.code(), Some(7));
    assert_eq!(
        stderr(&some),
        "ERROR: Evaluation of subquery \"some((//cyc:a - //cyc:a))\" failed (did you want to use --keep_going?): argument set is empty\n"
    );
    let variable = ws.query(&["$undefined"]);
    assert_eq!(variable.status.code(), Some(7));
    assert_eq!(
        stderr(&variable),
        "ERROR: Evaluation of query \"$undefined\" failed: undefined variable 'undefined'\n"
    );
    let regex = ws.query(&["filter('(', //cyc:a)"]);
    assert_eq!(regex.status.code(), Some(7));
    assert_eq!(
        stderr(&regex),
        "ERROR: Evaluation of query \"filter('(', //cyc:a)\" failed: illegal 'filter' pattern regexp '(': Unclosed group near index 1\n(\n"
    );
    let nothing = ws.query(&["//nothing/..."]);
    assert_eq!(nothing.status.code(), Some(7));
    assert_eq!(
        stderr(&nothing),
        "ERROR: no targets found beneath 'nothing'\n"
    );
    // QueryOptionHelper failures are a silent exit 2 in Bazel.
    let none = ws.query(&[]);
    assert_eq!(none.status.code(), Some(2));
    assert_eq!(stderr(&none), "");
    let empty = ws.query(&["//cyc:a - //cyc:a"]);
    assert!(empty.status.success());
    assert_eq!(stderr(&empty), "INFO: Empty results\n");
    let location = ws.query(&["--output=location", "buildfiles(//cyc:a)"]);
    assert_eq!(location.status.code(), Some(2));
    assert_eq!(
        stderr(&location),
        "ERROR: Query expressions involving 'buildfiles' or 'loadfiles' cannot be used with --output=location\n"
    );
    let unsupported = ws.query(&["--output=xml", "//cyc:a"]);
    assert_eq!(unsupported.status.code(), Some(1));
    assert!(stderr(&unsupported).contains("unsupported --output=xml"));
}

#[test]
fn regex_runtime_limits_are_errors_not_mismatches() {
    let ws = Workspace::new();
    ws.write(
        "long/BUILD.bazel",
        &format!("filegroup(name='{}')\n", "a".repeat(40)),
    );
    for function in [
        "filter('^//long:(a|a)*c(?!d)', //long:all)",
        "attr(name, '^(a|a)*c(?!d)', //long:all)",
    ] {
        let output = ws.query(&[function]);
        assert_eq!(output.status.code(), Some(7), "{function}");
        let text = stderr(&output);
        assert!(text.contains("could not be evaluated against"), "{text}");
        assert!(
            text.contains("Max limit for backtracking count exceeded"),
            "{text}"
        );
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn aspects_follow_attributes_only_when_required_providers_are_advertised() {
    // LabelVisitor.visitAspectsIfRequired with AspectDefinition.satisfies.
    let ws = Workspace::fixture("aspects");
    assert_eq!(
        ws.labels(&["deps(//asp:unmatched)"]),
        [
            "//asp:data.txt",
            "//asp:open_tool",
            "//asp:plain",
            "//asp:unmatched"
        ]
    );
    assert_eq!(
        ws.labels(&["deps(//asp:matched)"]),
        [
            "//asp:advertising",
            "//asp:matched",
            "//asp:needs_tool",
            "//asp:open_tool",
            "//asp:plain",
        ]
    );
    // Aspect dependencies are one level below the edge's target.
    assert_eq!(
        ws.labels(&["deps(//asp:matched, 1)"]),
        ["//asp:advertising", "//asp:matched", "//asp:plain"]
    );
    // Aspect attributes are always implicit.
    assert_eq!(
        ws.labels(&["--noimplicit_deps", "deps(//asp:matched)"]),
        ["//asp:advertising", "//asp:matched", "//asp:plain"]
    );
}
