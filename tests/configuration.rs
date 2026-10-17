//! CLI coverage for the native build configuration and configured attribute
//! resolution (docs/CONFIGURATION.md): modelled options, `config_setting`
//! `values`/`define_values`, select() specialization, no-match and ambiguity
//! errors, `None` branches, concatenation, attribute order and invalid
//! conditions. Expected texts follow the pinned Bazel 9.3 sources cited in
//! src/configured_attributes.rs and src/config_setting.rs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        let base = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let ws = Self(
            tempfile::Builder::new()
                .prefix("configuration-")
                .tempdir_in(base)
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='configuration')\n");
        ws.file("defs.bzl", DEFS);
        for (name, content) in files {
            ws.file(name, content);
        }
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, content: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new("timeout")
            .args(["--kill-after=5s", "60s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        assert_ne!(output.status.code(), Some(137), "CLI killed: {args:?}");
        output
    }

    /// Built output contents of a single-target build.
    fn built(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(output.status.success(), "{args:?}: {}", errors(&output));
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let lines = stdout.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "{stdout}");
        fs::read_to_string(lines[0].split_once('\t').unwrap().1).unwrap()
    }

    fn rejected(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: {}",
            errors(&output)
        );
        assert!(output.stdout.is_empty(), "partial output on failure");
        let diagnostic = errors(&output);
        assert!(!diagnostic.contains("panicked"), "{diagnostic}");
        assert!(!diagnostic.contains("actions executed"), "{diagnostic}");
        diagnostic
    }
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `show` writes its configured attribute values; `never` must not be analyzed.
const DEFS: &str = r#"
def _show_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    lines = []
    for name in ["mode", "flags", "count", "env", "strip", "stamp", "debug", "dep"]:
        value = getattr(ctx.attr, name)
        if name == "dep":
            value = str(value.label) if value else None
        lines.append(name + "=" + repr(value))
    lines.append("files=" + repr([f.short_path for f in ctx.files.srcs]))
    lines.append("conditions=" + repr([str(t.label) for t in ctx.attr._config_dependencies]))
    ctx.actions.write(out, "\n".join(lines) + "\n")
    return [DefaultInfo(files = depset([out]))]

show = rule(implementation = _show_impl, attrs = {
    "mode": attr.string(default = "unset"),
    "flags": attr.string_list(default = ["dflt"]),
    "count": attr.int(),
    "env": attr.string_dict(),
    "strip": attr.bool(),
    "stamp": attr.bool(),
    "debug": attr.bool(),
    "dep": attr.label(allow_files = True),
    "srcs": attr.label_list(allow_files = True),
})

def _never_impl(ctx):
    fail("analysis must not reach " + ctx.label.name)

never = rule(implementation = _never_impl)

def _ordered_impl(ctx):
    return []

# attrs dict order (zeta before alpha) is the rule-class attribute order.
ordered = rule(implementation = _ordered_impl, attrs = {
    "zeta": attr.string(),
    "alpha": attr.string(),
    "needed": attr.string_list(mandatory = True),
})
"#;

const SETTINGS: &str = r#"
config_setting(name = "opt", values = {"compilation_mode": "opt"})
config_setting(name = "dbg", values = {"compilation_mode": "dbg"})
config_setting(name = "fastbuild", values = {"compilation_mode": "fastbuild"})
config_setting(name = "stamped", values = {"stamp": "1"})
config_setting(name = "opt_stamped", values = {"compilation_mode": "opt", "stamp": "true"})
config_setting(name = "strip_always", values = {"strip": "always"})
config_setting(name = "pic", values = {"force_pic": "yes"})
config_setting(name = "release", define_values = {"release": "1"})
"#;

#[test]
fn values_match_modelled_options_across_configurations() {
    let ws = Workspace::new(&[(
        "BUILD.bazel",
        &format!(
            "load(':defs.bzl', 'show')\n{SETTINGS}\nshow(name = 'm', mode = select({{':opt': 'opt', ':dbg': 'dbg', ':fastbuild': 'fastbuild'}}), stamp = select({{':stamped': True, '//conditions:default': False}}), strip = select({{':strip_always': True, '//conditions:default': False}}), debug = select({{':pic': True, '//conditions:default': False}}), flags = select({{':release': ['release'], '//conditions:default': []}}))\n"
        ),
    )]);
    for (args, mode, stamp, strip, pic, flags) in [
        (&[][..], "fastbuild", "False", "False", "False", "[]"),
        (&["-c", "dbg"][..], "dbg", "False", "False", "False", "[]"),
        (
            &["-c", "opt", "--stamp"][..],
            "opt",
            "True",
            "False",
            "False",
            "[]",
        ),
        (
            &["--strip=always"][..],
            "fastbuild",
            "False",
            "True",
            "False",
            "[]",
        ),
        (
            &[
                "--compilation_mode=OPT",
                "--force_pic",
                "--define=release=1",
            ][..],
            "opt",
            "False",
            "False",
            "True",
            "[\"release\"]",
        ),
        (
            &["--define", "release=1", "--define=release=0", "--nostamp"][..],
            "fastbuild",
            "False",
            "False",
            "False",
            "[]",
        ),
    ] {
        let mut command = vec!["build"];
        command.extend_from_slice(args);
        command.push("//:m");
        let text = ws.built(&command);
        assert!(
            text.contains(&format!("mode=\"{mode}\"\n")),
            "{args:?}: {text}"
        );
        assert!(
            text.contains(&format!("stamp={stamp}\n")),
            "{args:?}: {text}"
        );
        assert!(
            text.contains(&format!("strip={strip}\n")),
            "{args:?}: {text}"
        );
        assert!(text.contains(&format!("debug={pic}\n")), "{args:?}: {text}");
        assert!(
            text.contains(&format!("flags={flags}\n")),
            "{args:?}: {text}"
        );
    }
    // `$config_dependencies` is never resolved as a prerequisite
    // (DependencyResolutionHelpers.java:408-416): ctx.attr sees [].
    let text = ws.built(&["build", "//:m"]);
    assert!(text.contains("conditions=[]\n"), "{text}");
    // Configuration options are parsed like Bazel's options parser.
    for (args, message) in [
        (
            &["-c", "fast"][..],
            "bazel-rs: While parsing option -c fast: Not a valid compilation mode: 'fast' (should be fastbuild, dbg or opt)",
        ),
        (
            &["--stamp=maybe"][..],
            "bazel-rs: While parsing option --stamp=maybe: 'maybe' is not a boolean",
        ),
        // Errors with an invalid argument are prefixed `ARG :: `
        // (runtime/BlazeOptionHandler.java:104, 628-637).
        (
            &["--nostamp=0"][..],
            "bazel-rs: --nostamp=0 :: Unexpected value after boolean option: --nostamp=0",
        ),
        (
            &["--nocompilation_mode"][..],
            "bazel-rs: --nocompilation_mode :: Illegal use of 'no' prefix on non-boolean option: --nocompilation_mode",
        ),
        (
            &["-c=opt"][..],
            "bazel-rs: -c=opt :: Invalid options syntax: -c=opt",
        ),
        (
            &["--define=novalue"][..],
            "bazel-rs: While parsing option --define=novalue: Variable definitions must be in the form of a 'name=value' assignment",
        ),
        (
            &["--cpu=k8"][..],
            "bazel-rs: unsupported option \"--cpu=k8\"; options are never silently ignored",
        ),
    ] {
        let mut command = vec!["build"];
        command.extend_from_slice(args);
        command.push("//:m");
        let diagnostic = ws.rejected(&command);
        assert_eq!(diagnostic.trim_end(), message, "{args:?}");
    }
    // Other platforms are resolved as platform targets (PlatformFunction /
    // PlatformLookupUtil, docs/PLATFORMS.md): a missing target reports its
    // loading exception (paired capture U/configuration_values).
    for args in [&["--platforms=//:p"][..], &["--host_platform", "//:p"][..]] {
        let mut command = vec!["build"];
        command.extend_from_slice(args);
        command.push("//:m");
        assert_eq!(
            ws.rejected(&command),
            format!(
                "bazel-rs: com.google.devtools.build.lib.packages.NoSuchTargetException: no such target '//:p': target 'p' not declared in package '' defined by {}/BUILD.bazel (did you mean m?)\n",
                ws.path().display()
            ),
            "{args:?}"
        );
    }
    // The default host platform spelled explicitly is the default configuration.
    for platform in [
        "--platforms=@bazel_tools//tools:host_platform",
        "--host_platform=@bazel_tools//tools:host_platform",
        "--host_platform=@@bazel_tools//tools:host_platform",
    ] {
        let text = ws.built(&["build", platform, "//:m"]);
        assert!(text.contains("mode=\"fastbuild\"\n"), "{platform}: {text}");
    }
    // `--stamp` takes no separate value: `true` is a target pattern.
    let diagnostic = ws.rejected(&["build", "--stamp", "true", "//:m"]);
    assert_eq!(
        diagnostic,
        format!(
            "bazel-rs: no such target '//:true': target 'true' not declared in package '' defined by {}/BUILD.bazel\n",
            ws.path().display()
        )
    );
    // query is configuration-independent but accepts the CoreOptions
    // (runtime/commands/QueryCommand.java:59-67).
    let query = ws.run(&["query", "-c", "opt", "--stamp", "--define=a=1", "//:m"]);
    assert!(query.status.success(), "{}", errors(&query));
    assert_eq!(String::from_utf8_lossy(&query.stdout), "//:m\n");
    let query = ws.run(&["query", "-c", "fast", "//:m"]);
    assert_eq!(
        errors(&query).trim_end(),
        "bazel-rs: While parsing option -c fast: Not a valid compilation mode: 'fast' (should be fastbuild, dbg or opt)"
    );
    let query = ws.run(&["query", "--strip=always", "//:m"]);
    assert!(errors(&query).contains("unsupported option \"--strip=always\""));
}

#[test]
fn output_root_follows_compilation_mode_and_default_identity_is_unchanged() {
    let ws = Workspace::new(&[(
        "BUILD.bazel",
        "genrule(name = 'g', outs = ['g.txt'], cmd = 'echo $@ > $@')\n",
    )]);
    let default = ws.built(&["build", "//:g"]);
    assert_eq!(default, "bazel-out/k8-fastbuild/bin/g.txt\n");
    // Options without an output-path effect keep the action identity.
    let output = ws.run(&["build", "--stamp", "--define=a=1", "//:g"]);
    assert!(
        errors(&output).contains("0 actions executed, 1 cache hits"),
        "{}",
        errors(&output)
    );
    assert_eq!(
        ws.built(&["build", "-c", "dbg", "//:g"]),
        "bazel-out/k8-dbg/bin/g.txt\n"
    );
    assert_eq!(
        ws.built(&["build", "-c", "opt", "//:g"]),
        "bazel-out/k8-opt/bin/g.txt\n"
    );
    let output = ws.run(&["build", "//:g"]);
    assert!(
        errors(&output).contains("0 actions executed, 1 cache hits"),
        "{}",
        errors(&output)
    );
    // Workspace platform mappings are read by Bazel; they are not modelled.
    ws.file("platform_mappings", "");
    let diagnostic = ws.rejected(&["build", "//:g"]);
    assert!(
        diagnostic.contains("platform mappings are not implemented"),
        "{diagnostic}"
    );
}

#[test]
fn rules_go_go_config_shaped_selects() {
    // go/private/BUILD.bazel config_settings and the go_config select shapes
    // of rules_go (go/private/rules/go_config? / go/config/BUILD.bazel).
    let ws = Workspace::new(&[
        (
            "go/private/BUILD.bazel",
            "config_setting(name = 'stamp', values = {'stamp': 'true'}, visibility = ['//:__pkg__'])\nconfig_setting(name = 'is_strip_always', values = {'strip': 'always'}, visibility = ['//:__pkg__'])\nconfig_setting(name = 'is_strip_sometimes_fastbuild', values = {'strip': 'sometimes', 'compilation_mode': 'fastbuild'}, visibility = ['//:__pkg__'])\nconfig_setting(name = 'is_compilation_mode_dbg', values = {'compilation_mode': 'dbg'})\n",
        ),
        (
            "BUILD.bazel",
            "load(':defs.bzl', 'show')\nshow(name = 'go_config', strip = select({'//go/private:is_strip_always': True, '//go/private:is_strip_sometimes_fastbuild': True, '//conditions:default': False}), stamp = select({'//go/private:stamp': True, '//conditions:default': False}), debug = select({'//go/private:is_compilation_mode_dbg': True, '//conditions:default': False}))\n",
        ),
    ]);
    for (args, strip, stamp, debug) in [
        (&[][..], "True", "False", "False"),
        (&["-c", "dbg"][..], "False", "False", "True"),
        (&["-c", "opt", "--stamp"][..], "False", "True", "False"),
        (
            &["--strip=always", "-c", "opt"][..],
            "True",
            "False",
            "False",
        ),
        (&["--strip=never"][..], "False", "False", "False"),
    ] {
        let mut command = vec!["build"];
        command.extend_from_slice(args);
        command.push("//:go_config");
        let text = ws.built(&command);
        assert!(
            text.contains(&format!("strip={strip}\nstamp={stamp}\ndebug={debug}\n")),
            "{args:?}: {text}"
        );
    }
}

#[test]
fn specialization_and_ambiguity() {
    let ws = Workspace::new(&[(
        "BUILD.bazel",
        &format!(
            "load(':defs.bzl', 'show')\n{SETTINGS}\nshow(name = 'special', mode = select({{':opt': 'opt', ':opt_stamped': 'opt+stamp'}}))\nshow(name = 'same', mode = select({{':opt': 'x', ':stamped': 'x'}}))\nshow(name = 'ambiguous', mode = select({{':opt': 'a', ':stamped': 'b'}}))\n"
        ),
    )]);
    assert!(
        ws.built(&["build", "-c", "opt", "//:special"])
            .contains("mode=\"opt\"\n")
    );
    assert!(
        ws.built(&["build", "-c", "opt", "--stamp", "//:special"])
            .contains("mode=\"opt+stamp\"\n")
    );
    // Unrelated matches resolving to equal values are allowed.
    assert!(
        ws.built(&["build", "-c", "opt", "--stamp", "//:same"])
            .contains("mode=\"x\"\n")
    );
    let diagnostic = ws.rejected(&["build", "-c", "opt", "--stamp", "//:ambiguous"]);
    assert!(
        diagnostic.contains(&format!(
            "{}/BUILD.bazel:14:5: Illegal ambiguous match on configurable attribute \"mode\" in //:ambiguous:\n//:opt\n//:stamped\nMultiple matches are not allowed unless one is unambiguously more specialized or they resolve to the same value. See https://bazel.build/reference/be/functions#select.",
            ws.path().display()
        )),
        "{diagnostic}"
    );
}

fn config_identifier(diagnostic: &str) -> String {
    let marker = "has configuration identifier ";
    let start = diagnostic.find(marker).expect("configuration identifier") + marker.len();
    let id = &diagnostic[start..start + 7];
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit()), "{diagnostic}");
    id.to_owned()
}

#[test]
fn no_match_errors_with_and_without_custom_message() {
    let ws = Workspace::new(&[(
        "BUILD.bazel",
        &format!(
            "load(':defs.bzl', 'show')\n{SETTINGS}\nshow(name = 'plain', mode = select({{':opt': 'o', ':dbg': 'd'}}))\nshow(name = 'custom', mode = select({{':opt': 'o'}}, no_match_error = 'requires -c opt'))\n"
        ),
    )]);
    let diagnostic = ws.rejected(&["build", "//:plain"]);
    let id = config_identifier(&diagnostic);
    assert_eq!(
        diagnostic,
        format!(
            "bazel-rs: {}/BUILD.bazel:12:5: configurable attribute \"mode\" in //:plain doesn't match this configuration. Would a default condition help?\n\nConditions checked:\n //:opt\n //:dbg\n\nTo see a condition's definition, run: bazel query --output=build <condition label>.\n\nThis instance of //:plain has configuration identifier {id}. To inspect its configuration, run: bazel config {id}.\n\nFor more help, see https://bazel.build/docs/configurable-attributes#faq-select-choose-condition.\n\n\n",
            ws.path().display()
        )
    );
    let diagnostic = ws.rejected(&["build", "//:custom"]);
    let custom_id = config_identifier(&diagnostic);
    assert_eq!(custom_id, id, "same configuration, same identifier");
    assert!(
        diagnostic.contains(&format!(
            "configurable attribute \"mode\" in //:custom doesn't match this configuration: requires -c opt\n\nThis instance of //:custom has configuration identifier {id}."
        )),
        "{diagnostic}"
    );
    // The identifier is configuration-specific.
    let diagnostic = ws.rejected(&["build", "--stamp", "//:custom"]);
    assert_ne!(config_identifier(&diagnostic), id);
    assert!(
        ws.built(&["build", "-c", "opt", "//:custom"])
            .contains("mode=\"o\"\n")
    );
}

#[test]
fn none_branches_use_attribute_defaults() {
    let ws = Workspace::new(&[(
        "BUILD.bazel",
        &format!(
            "load(':defs.bzl', 'show', 'ordered')\n{SETTINGS}\nshow(name = 'none', flags = select({{':opt': None, '//conditions:default': ['x']}}), mode = select({{':opt': None, '//conditions:default': 'set'}}), dep = select({{':opt': None, '//conditions:default': 'in.txt'}}))\ngenrule(name = 'gen', outs = ['gen.txt'], cmd = select({{':opt': None, '//conditions:default': 'echo ok > $@'}}))\nordered(name = 'mandatory', needed = select({{':opt': None, '//conditions:default': ['x']}}))\n"
        ),
    )]);
    ws.file("in.txt", "in\n");
    let text = ws.built(&["build", "//:none"]);
    assert!(text.contains("mode=\"set\"\nflags=[\"x\"]\n"), "{text}");
    assert!(text.contains("dep=\"@@//:in.txt\"\n"), "{text}");
    let text = ws.built(&["build", "-c", "opt", "//:none"]);
    assert!(
        text.contains("mode=\"unset\"\nflags=[\"dflt\"]\n"),
        "{text}"
    );
    assert!(text.contains("dep=None\n"), "{text}");
    assert_eq!(ws.built(&["build", "//:gen"]), "ok\n");
    // A chosen None is not an explicitly specified cmd (GenRuleBase.java:262-284).
    let diagnostic = ws.rejected(&["build", "-c", "opt", "//:gen"]);
    assert!(
        diagnostic
            .contains("in cmd attribute of genrule rule //:gen: missing value for `cmd` attribute"),
        "{diagnostic}"
    );
    let diagnostic = ws.rejected(&["build", "-c", "opt", "//:mandatory"]);
    assert!(
        diagnostic.contains(
            "BUILD.bazel:14:8: Mandatory attribute 'needed' resolved to 'None' after evaluating 'select' expression"
        ),
        "{diagnostic}"
    );
    assert_eq!(ws.run(&["build", "//:mandatory"]).status.code(), Some(0));
}

#[test]
fn concatenation_forms_follow_type_concat() {
    let ws = Workspace::new(&[(
        "BUILD.bazel",
        &format!(
            "load(':defs.bzl', 'show')\n{SETTINGS}\nshow(name = 'cat', flags = ['a'] + select({{':opt': ['o'], '//conditions:default': ['d']}}) + select({{':stamped': ['s'], '//conditions:default': []}}) + ['z'], mode = 'm-' + select({{':opt': 'opt', '//conditions:default': 'other'}}), count = select({{':opt': 2147483647, '//conditions:default': 1}}) + select({{':opt': 2147483647, '//conditions:default': 2}}), env = select({{':opt': {{'a': '3', 'c': '4'}}, '//conditions:default': {{}}}}) | {{'a': '1', 'b': '2'}} | select({{':opt': {{'a': '5'}}, '//conditions:default': {{'d': '6'}}}}))\n"
        ),
    )]);
    let text = ws.built(&["build", "//:cat"]);
    assert!(text.contains("mode=\"m-other\"\nflags=[\"a\", \"d\", \"z\"]\ncount=3\nenv={\"a\": \"1\", \"b\": \"2\", \"d\": \"6\"}\n"), "{text}");
    let text = ws.built(&["build", "-c", "opt", "--stamp", "//:cat"]);
    assert!(
        // Dict union keeps a key's first position with its last value.
        text.contains("mode=\"m-opt\"\nflags=[\"a\", \"o\", \"s\", \"z\"]\ncount=-2\nenv={\"a\": \"5\", \"c\": \"4\", \"b\": \"2\"}\n"),
        "{text}"
    );
    // Types whose Type.concat is null reject concatenation at loading.
    ws.file(
        "BUILD.bazel",
        "load(':defs.bzl', 'show')\nshow(name = 'bad', strip = select({'//conditions:default': True}) + select({'//conditions:default': False}))\n",
    );
    let output = ws.run(&["query", "//:bad"]);
    assert!(
        errors(&output).contains("type 'boolean' doesn't support select concatenation"),
        "{}",
        errors(&output)
    );
}

#[test]
fn attribute_validation_order_and_unchosen_branches() {
    let ws = Workspace::new(&[(
        "BUILD.bazel",
        &format!(
            "load(':defs.bzl', 'show', 'never', 'ordered')\n{SETTINGS}\nordered(name = 'two', alpha = select({{':dbg': 'a'}}), zeta = select({{':dbg': 'z'}}), needed = [])\nnever(name = 'broken')\nshow(name = 'lazy', srcs = select({{':opt': [':broken'], '//conditions:default': ['in.txt']}}))\nfilegroup(name = 'files', srcs = select({{':opt': [':broken'], '//conditions:default': ['in.txt']}}))\ngenrule(name = 'cmd', srcs = select({{':opt': ['other.txt'], '//conditions:default': ['in.txt']}}), outs = ['cmd.txt'], cmd = select({{':opt': 'cat $(location other.txt) > $@', '//conditions:default': 'cat $(location in.txt) $(SRCS) > $@'}}))\n"
        ),
    )]);
    ws.file("in.txt", "in\n");
    ws.file("other.txt", "other\n");
    // validateAttributes walks the rule-class attribute order (attrs dict
    // order, zeta before alpha), the first failure winning.
    let diagnostic = ws.rejected(&["build", "//:two"]);
    assert!(
        diagnostic.contains("configurable attribute \"zeta\" in //:two"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("\"alpha\""), "{diagnostic}");
    // The unchosen branch's broken target is never analyzed.
    let text = ws.built(&["build", "//:lazy"]);
    assert!(text.contains("files=[\"in.txt\"]\n"), "{text}");
    let diagnostic = ws.rejected(&["build", "-c", "opt", "//:lazy"]);
    assert!(
        diagnostic.contains("analysis must not reach broken"),
        "{diagnostic}"
    );
    let output = ws.run(&["build", "//:files"]);
    assert!(output.status.success(), "{}", errors(&output));
    assert!(
        ws.rejected(&["build", "-c", "opt", "//:files"])
            .contains("analysis must not reach broken")
    );
    // genrule srcs, $(location) and $(SRCS) use the resolved values.
    assert_eq!(ws.built(&["build", "//:cmd"]), "in\nin\n");
    assert_eq!(ws.built(&["build", "-c", "opt", "//:cmd"]), "other\n");
}

#[test]
fn invalid_conditions_and_condition_visibility() {
    let ws = Workspace::new(&[
        (
            "BUILD.bazel",
            "load(':defs.bzl', 'show')\nfilegroup(name = 'fg', srcs = [])\nshow(name = 'key', mode = select({':fg': 'x', '//conditions:default': 'y'}))\nshow(name = 'source', mode = select({':in.txt': 'x', '//conditions:default': 'y'}))\nshow(name = 'public', mode = select({'//c:public_by_default': 'x', '//conditions:default': 'y'}))\nshow(name = 'private', mode = select({'//c:private': 'x', '//conditions:default': 'y'}))\nshow(name = 'unknown', mode = select({'//c:unknown': 'x', '//conditions:default': 'y'}))\nshow(name = 'flags', mode = select({'//c:flags': 'x', '//conditions:default': 'y'}))\nshow(name = 'empty', mode = select({'//c:empty': 'x', '//conditions:default': 'y'}))\n",
        ),
        (
            "c/BUILD.bazel",
            "package(default_visibility = ['//visibility:private'])\nconfig_setting(name = 'public_by_default', values = {'compilation_mode': 'opt'})\nconfig_setting(name = 'private', values = {'compilation_mode': 'opt'}, visibility = ['//visibility:private'])\nconfig_setting(name = 'unknown', values = {'no_such_option': '1'}, visibility = ['//visibility:public'])\nconfig_setting(name = 'flags', flag_values = {':x': '1'}, visibility = ['//visibility:public'])\nconfig_setting(name = 'empty', visibility = ['//visibility:public'])\n",
        ),
    ]);
    ws.file("in.txt", "in\n");
    let diagnostic = ws.rejected(&["build", "//:key"]);
    assert_eq!(
        diagnostic,
        format!(
            "bazel-rs: {}/BUILD.bazel:3:5: //:fg is not a valid select() condition for //:key.\nTo inspect the select(), run: bazel query --output=build //:key.\nFor more help, see https://bazel.build/reference/be/functions#select.\n\n\n",
            ws.path().display()
        )
    );
    assert!(
        ws.rejected(&["build", "//:source"])
            .contains("//:in.txt is not a valid select() condition for //:source.")
    );
    // DEFAULT_PUBLIC: a config_setting without explicit visibility is public,
    // whatever the package default (Rule.java:694-710).
    assert!(
        ws.built(&["build", "-c", "opt", "//:public"])
            .contains("mode=\"x\"\n")
    );
    // CommonPrerequisiteValidator's visibility text (:380-405) at the consumer.
    assert_eq!(
        ws.rejected(&["build", "//:private"]),
        format!(
            "bazel-rs: {}/BUILD.bazel:6:5: in show rule //:private: Visibility error:\ntarget '//c:private' is not visible from\ntarget '//:private'\nRecommendation: modify the visibility declaration if you think the dependency is legitimate. For more info see https://bazel.build/concepts/visibility\n",
            ws.path().display()
        )
    );
    // config_setting errors carry its location (c/BUILD.bazel:4:15).
    assert!(ws.rejected(&["build", "//:unknown"]).contains(&format!(
        "{}/c/BUILD.bazel:4:15: in values attribute of config_setting rule //c:unknown: error while parsing configuration settings: unknown option: 'no_such_option'",
        ws.path().display()
    )));
    // A flag_values key without a target is an assumed input file that does
    // not exist (RuleContext.java:1869-1880; paired capture row T9/flag_values).
    assert_eq!(
        ws.rejected(&["build", "//:flags"]),
        format!(
            "bazel-rs: {ws}/c/BUILD.bazel:5:15: in flag_values attribute of config_setting rule //c:flags: rule '//c:x' does not exist\n{ws}/BUILD.bazel:8:5: errors encountered resolving select() keys for //:flags\n",
            ws = ws.path().display()
        )
    );
    assert!(ws.rejected(&["build", "//:empty"]).contains(&format!(
        "{}/c/BUILD.bazel:6:15: in config_setting rule //c:empty: Either values, flag_values or constraint_values must be specified and non-empty",
        ws.path().display()
    )));
    // A config_setting builds on its own (no files).
    let output = ws.run(&["build", "//c:public_by_default"]);
    assert!(output.status.success(), "{}", errors(&output));
    assert!(output.stdout.is_empty());
}

#[test]
fn gap_order_and_select_branch_inputs() {
    let ws = Workspace::new(&[(
        "BUILD.bazel",
        &format!(
            "load(':defs.bzl', 'show', 'tooled', 'edged')\n{SETTINGS}\nconfig_setting(name = 'bad', values = {{'no_such_option': '1'}})\ntooled(name = 'tooled', mode = select({{':bad': 'x', '//conditions:default': 'y'}}), deps = ['//missing:dep'])\nedged(name = 'edge_unset', tools = select({{':opt': [':in.txt'], '//conditions:default': []}}))\nedged(name = 'edge_order', tools = [':in.txt'], mode = select({{':dbg': 'd'}}))\nshow(name = 'inputs', srcs = select({{':opt': ['only_in_select.txt'], '//conditions:default': []}}))\n"
        ),
    )]);
    ws.file(
        "defs.bzl",
        &format!(
            "{DEFS}\ntooled = rule(implementation = _ordered_impl, toolchains = ['//:toolchain_type'], attrs = {{'mode': attr.string(), 'deps': attr.label_list(allow_files = True)}})\nedged = rule(implementation = _ordered_impl, attrs = {{'mode': attr.string(), 'tools': attr.label_list(cfg = 'exec', allow_files = True)}})\n"
        ),
    );
    ws.file("in.txt", "in\n");
    ws.file("only_in_select.txt", "selected\n");
    // select() conditions are analyzed before toolchain resolution, which
    // precedes dependencies (the missing package) (PrerequisiteProducer).
    let diagnostic = ws.rejected(&["build", "//:tooled"]);
    assert!(
        diagnostic.contains(
            "in values attribute of config_setting rule //:bad: error while parsing configuration settings: unknown option: 'no_such_option'"
        ),
        "{diagnostic}"
    );
    ws.file(
        "BUILD.bazel",
        &format!(
            "load(':defs.bzl', 'show', 'tooled', 'edged')\n{SETTINGS}\ntooled(name = 'tooled', mode = select({{':dbg': 'x'}}), deps = ['//missing:dep'])\nedged(name = 'edge_unset', tools = select({{':opt': [':in.txt'], '//conditions:default': []}}))\nedged(name = 'edge_order', tools = [':in.txt'], mode = select({{':dbg': 'd'}}))\nshow(name = 'inputs', srcs = select({{':opt': ['only_in_select.txt'], '//conditions:default': []}}))\nplatform(name = 'p')\n"
        ),
    );
    // Toolchain resolution (N17, docs/TOOLCHAINS.md) precedes the
    // dependencies: the missing type target is reported, not the missing
    // //missing:dep package. The default host platform needs the built-in
    // profile, so an explicit platform is used.
    let diagnostic = ws.rejected(&["build", "--host_platform=//:p", "//:tooled"]);
    assert!(
        diagnostic.contains(
            "While resolving toolchains for target //:tooled ("
        ) && diagnostic.contains(
            "): com.google.devtools.build.lib.packages.NoSuchTargetException: no such target '//:toolchain_type': target 'toolchain_type' not declared in package '' defined by "
        ),
        "{diagnostic}"
    );
    let diagnostic = ws.rejected(&["build", "//:tooled"]);
    assert!(
        diagnostic.contains("needs the built-in @bazel_tools repository"),
        "{diagnostic}"
    );
    // Exec edges come from resolved values only, after attribute
    // validation: an unselected exec edge needs no execution platform; a
    // selected one needs the parent's execution platform (the host platform;
    // the default one needs the built-in profile, docs/CONFIGURATION.md).
    let output = ws.run(&["build", "//:edge_unset"]);
    assert!(output.status.success(), "{}", errors(&output));
    let diagnostic = ws.rejected(&["build", "-c", "opt", "//:edge_unset"]);
    assert!(
        diagnostic.contains("While resolving toolchains for target //:edge_unset (")
            && diagnostic.contains("needs the built-in @bazel_tools repository"),
        "{diagnostic}"
    );
    let output = ws.run(&[
        "build",
        "-c",
        "opt",
        "--host_platform=//:p",
        "//:edge_unset",
    ]);
    assert!(output.status.success(), "{}", errors(&output));
    let diagnostic = ws.rejected(&["build", "//:edge_order"]);
    assert!(
        diagnostic.contains("configurable attribute \"mode\" in //:edge_order doesn't match"),
        "{diagnostic}"
    );
    let output = ws.run(&[
        "build",
        "-c",
        "dbg",
        "--host_platform=//:p",
        "//:edge_order",
    ]);
    assert!(output.status.success(), "{}", errors(&output));
    // Labels only named in a select() branch are declared input files
    // (Package.java:893-920).
    let text = ws.built(&["build", "-c", "opt", "//:inputs"]);
    assert!(text.contains("files=[\"only_in_select.txt\"]\n"), "{text}");
    let query = ws.run(&["query", "//:only_in_select.txt"]);
    assert!(query.status.success(), "{}", errors(&query));
}

#[test]
fn unconditional_none_missing_inputs_and_missing_conditions() {
    let ws = Workspace::new(&[(
        "BUILD.bazel",
        &format!(
            "load(':defs.bzl', 'show', 'ordered')\n{SETTINGS}\nshow(name = 'default_none', mode = select({{'//conditions:default': None}}), flags = select({{'//conditions:default': None}}), dep = select({{'//conditions:default': None}}))\nordered(name = 'mandatory_none', needed = select({{'//conditions:default': None}}))\nshow(name = 'missing_target', srcs = select({{':opt': [':nothere'], '//conditions:default': ['in.txt']}}))\ngenrule(name = 'consume', srcs = select({{':opt': [':nothere'], '//conditions:default': ['in.txt']}}), outs = ['consume.txt'], cmd = 'cat $(SRCS) > $@')\nfilegroup(name = 'files', srcs = select({{':opt': [':nothere'], '//conditions:default': ['in.txt']}}))\nshow(name = 'missing_key_target', mode = select({{':no_such_setting': 'x', '//conditions:default': 'y'}}))\nshow(name = 'missing_key_pkg', mode = select({{'//nope:cs': 'x', '//conditions:default': 'y'}}))\nshow(name = 'dict_order', env = select({{':opt': {{'a': '1', 'b': '2'}}, ':stamped': {{'b': '2', 'a': '1'}}}}))\ngenrule(name = 'no_cmd', outs = ['no_cmd.txt'])\n"
        ),
    )]);
    ws.file("in.txt", "in\n");
    // select({"//conditions:default": None}) is the type default at loading
    // (BuildType.java:165-211): "" / [] for strings and lists, so a mandatory
    // attribute is satisfied; a null label stays unset.
    let text = ws.built(&["build", "//:default_none"]);
    assert!(
        text.contains("mode=\"\"\nflags=[]\n") && text.contains("dep=None\n"),
        "{text}"
    );
    let output = ws.run(&["build", "//:mandatory_none"]);
    assert!(output.status.success(), "{}", errors(&output));
    // A missing file named in the chosen branch is still an (assumed) input
    // file target; it fails only where an action or the request needs it
    // (ArtifactFunction "missing input file").
    let text = ws.built(&["build", "-c", "opt", "//:missing_target"]);
    assert!(text.contains("files=[\"nothere\"]\n"), "{text}");
    assert_eq!(
        ws.rejected(&["build", "-c", "opt", "//:consume"]),
        format!(
            "bazel-rs: {ws}/BUILD.bazel:15:8: Executing genrule //:consume failed: missing input file '//:nothere'\n{ws}/BUILD.bazel:15:8: Executing genrule //:consume failed: 1 input file(s) do not exist\n",
            ws = ws.path().display()
        )
    );
    assert_eq!(
        ws.rejected(&["build", "-c", "opt", "//:files"]),
        format!(
            "bazel-rs: {ws}/BUILD.bazel:16:10: //:files: missing input file '//:nothere'\n{ws}/BUILD.bazel:16:10: 1 input file(s) do not exist\n",
            ws = ws.path().display()
        )
    );
    // A missing select key is an input file target, hence not a condition.
    assert!(
        ws.rejected(&["build", "//:missing_key_target"]).contains(&format!(
            "{}/BUILD.bazel:17:5: //:no_such_setting is not a valid select() condition for //:missing_key_target.\n",
            ws.path().display()
        ))
    );
    // The consumer adds `errors encountered resolving select() keys`
    // (ConfigConditionsProducer.java:121-129, 200-211; N16a capture T11).
    assert_eq!(
        ws.rejected(&["build", "//:missing_key_pkg"]),
        format!(
            "bazel-rs: no such package 'nope': BUILD file not found in any of the following directories. Add a BUILD file to a directory to mark it as a package.\n - nope\n{}/BUILD.bazel:18:5: errors encountered resolving select() keys for //:missing_key_pkg\n",
            ws.path().display()
        )
    );
    // Equal maps in another order are the same value (Java Map.equals).
    let text = ws.built(&["build", "-c", "opt", "--stamp", "//:dict_order"]);
    assert!(
        text.contains("env={\"a\": \"1\", \"b\": \"2\"}\n"),
        "{text}"
    );
    // genrule errors carry the rule location.
    assert!(ws.rejected(&["build", "//:no_cmd"]).contains(&format!(
        "{}/BUILD.bazel:20:8: in cmd attribute of genrule rule //:no_cmd: missing value for `cmd` attribute",
        ws.path().display()
    )));
}

#[test]
fn concatenated_selects_reject_duplicate_labels_at_analysis() {
    // RuleContext.Builder.checkAttributesForDuplicateLabels
    // (analysis/RuleContext.java:1553-1568): a resolved LABEL_LIST built from
    // several elements must not repeat a label; each element alone is checked
    // at loading (RuleClass.checkForDuplicateLabels).
    let ws = Workspace::new(&[
        ("a.txt", "a\n"),
        ("b.txt", "b\n"),
        (
            "BUILD.bazel",
            "load(':defs.bzl', 'show')\nconfig_setting(name = 'opt', values = {'compilation_mode': 'opt'})\nfilegroup(name = 'fg', srcs = ['a.txt'] + select({':opt': ['a.txt'], '//conditions:default': []}))\ngenrule(name = 'g', srcs = ['a.txt'] + select({':opt': ['b.txt', 'a.txt'], '//conditions:default': ['b.txt']}), outs = ['g.txt'], cmd = 'cat $(SRCS) > $@')\nshow(name = 's', srcs = ['a.txt', 'b.txt'] + select({':opt': ['b.txt', 'a.txt'], '//conditions:default': []}))\n",
        ),
    ]);
    // Without the duplicate the targets build.
    for target in ["//:fg", "//:s"] {
        let output = ws.run(&["build", target]);
        assert!(output.status.success(), "{target}: {}", errors(&output));
    }
    assert_eq!(ws.built(&["build", "//:g"]), "a\nb\n");
    let ws_path = ws.path().display().to_string();
    for (target, expected) in [
        (
            "//:fg",
            format!(
                "bazel-rs: {ws_path}/BUILD.bazel:3:10: in srcs attribute of filegroup rule //:fg: Label '//:a.txt' is duplicated\n"
            ),
        ),
        (
            "//:g",
            format!(
                "bazel-rs: {ws_path}/BUILD.bazel:4:8: in srcs attribute of genrule rule //:g: Label '//:a.txt' is duplicated\n"
            ),
        ),
        // Every duplicate is reported (native order: first duplicate first;
        // Bazel's is a HashSet's).
        (
            "//:s",
            format!(
                "bazel-rs: {ws_path}/BUILD.bazel:5:5: in srcs attribute of show rule //:s: Label '//:b.txt' is duplicated\n{ws_path}/BUILD.bazel:5:5: in srcs attribute of show rule //:s: Label '//:a.txt' is duplicated\n"
            ),
        ),
    ] {
        assert_eq!(
            ws.rejected(&["build", "-c", "opt", target]),
            expected,
            "{target}"
        );
    }
}
