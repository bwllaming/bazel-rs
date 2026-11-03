"""Cargo integration tests (tests/*.rs) as Bazel rust_tests (docs/SELF_BUILD.md)."""

load("@crates//:defs.bzl", "aliases", "all_crate_deps")
load("@rules_rust//rust:defs.bzl", "rust_test")

# Cargo compiles `env!("CARGO_BIN_EXE_bazel-rs")` to the binary's absolute
# path. Bazel knows only a runfiles path at compile time, and the tests run
# the binary from other working directories, so a Bazel copy of the test
# finds it from TEST_SRCDIR at run time instead.
_BINARY_LOOKUP = """
// Appended by //:self_build.bzl: the Bazel-built binary in the runfiles.
#[allow(dead_code)]
fn __self_build_bazel_rs() -> &'static str {
    static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let root = std::env::var("TEST_SRCDIR").expect("TEST_SRCDIR (bazel test)");
        format!("{root}/_main/bazel-rs")
    })
}
"""

def copy_test_support(name):
    """Copies tests/support/*.rs to self_build/tests/support/, beside the test copies.

    Args:
        name: Name of the genrule.
    """
    srcs = native.glob(["tests/support/*.rs"])
    native.genrule(
        name = name,
        srcs = srcs,
        outs = ["self_build/" + src for src in srcs],
        cmd = "for f in $(SRCS); do cp $$f $(RULEDIR)/self_build/$$f; done",
    )

def bazel_rs_integration_test(name, data = [], size = "medium", args = []):
    """A rust_test of tests/<name>.rs that runs the Bazel-built //:bazel-rs.

    Args:
        name: The test file's name without `.rs`.
        data: Extra runtime files.
        size: The test size.
        args: Test arguments (e.g. `--skip=<test>`).
    """
    src = "self_build/tests/%s.rs" % name
    native.genrule(
        name = name + "_src",
        srcs = ["tests/%s.rs" % name],
        outs = [src],
        cmd = "sed 's/env!(\"CARGO_BIN_EXE_bazel-rs\")/crate::__self_build_bazel_rs()/g' $< > $@ && " +
              "cat >> $@ <<'EOF'\n" + _BINARY_LOOKUP + "EOF",
    )
    rust_test(
        name = name,
        size = size,
        srcs = [src],
        aliases = aliases(normal_dev = True, proc_macro_dev = True),
        args = args,
        compile_data = [":test_support"],
        crate_root = src,
        data = [":bazel-rs"] + data,
        edition = "2024",
        # .cargo/config.toml [env]: native LABEL<TAB>PATH result lines.
        env = {"BAZEL_RS_RESULT_LINES": "1"},
        lint_config = ":lints",
        proc_macro_deps = all_crate_deps(proc_macro_dev = True),
        rustc_env_files = [":cargo_pkg_version"],
        deps = [":bazel_rs"] + all_crate_deps(normal = True, normal_dev = True),
    )
