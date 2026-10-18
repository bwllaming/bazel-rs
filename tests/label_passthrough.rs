//! Constructor-only coverage, independent of the repo_name getter work.
//! Invalid apparent aliases yield Bazel's invalid Label (N45), whose repository
//! getters fail with Bazel's text.
//! Callback prints are phase markers, never evidence of repository/action reuse.
#![cfg(unix)]
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let fixture = Self(
            tempfile::Builder::new()
                .prefix("label-passthrough-")
                .tempdir()
                .unwrap(),
        );
        for directory in ["home", "tmp"] {
            fs::create_dir(fixture.0.path().join(directory)).unwrap();
        }
        fixture.file("MODULE.bazel", "module(name='root_contract')\n");
        fixture.file("BUILD.bazel", "");
        fixture.file("write.bzl", WRITE);
        fixture
    }

    fn file(&self, relative: &str, bytes: impl AsRef<[u8]>) {
        let path = self.0.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        // Cargo supplies the trusted executable; it need not live under the
        // manifest directory (e.g. an external CARGO_TARGET_DIR).
        let mut command = Command::new("/usr/bin/timeout");
        command
            .args(["--kill-after=5s", "60s", env!("CARGO_BIN_EXE_bazel-rs")])
            .args(["--ignore_all_rc_files", "--cache-dir=cache"])
            .args(args)
            .current_dir(self.0.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("TMPDIR", self.0.path().join("tmp"))
            .env("LC_ALL", "C");
        eprintln!("N2_COMMAND {command:?}");
        let output = command.output().unwrap();
        eprintln!(
            "N2_STATUS {}\nN2_STDOUT_BEGIN\n{}N2_STDOUT_END\nN2_STDERR_BEGIN\n{}N2_STDERR_END",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            errors(&output),
        );
        assert_ne!(output.status.code(), Some(124), "CLI timed out");
        assert_ne!(output.status.code(), Some(137), "CLI forced termination");
        assert!(!errors(&output).contains("panicked"));
        output
    }

    fn macro_body(&self, imports: &str, body: &str) {
        self.file(
            "probe.bzl",
            format!("load(':write.bzl', 'write')\n{imports}\ndef probe():\n{body}\n"),
        );
        self.file("BUILD.bazel", "load(':probe.bzl','probe')\nprobe()\ngenrule(name='copy',srcs=[':probe'],outs=['copy.txt'],cmd='/bin/cat $(SRCS) > $@')\n");
    }

    fn external(&self) {
        self.file("MODULE.bazel", "module(name='root_contract')\nbazel_dep(name='dep',version='1.0',repo_name='shared')\nbazel_dep(name='other',version='1.0',repo_name='other_root')\nlocal_path_override(module_name='dep',path='dep')\nlocal_path_override(module_name='other',path='other')\n");
        self.file("dep/MODULE.bazel", "module(name='dep',version='1.0')\nbazel_dep(name='other',version='1.0',repo_name='shared')\n");
        self.file("other/MODULE.bazel", "module(name='other',version='1.0')\n");
        self.file("other/BUILD.bazel", "");
        self.file("dep/BUILD.bazel", "");
        self.file("root_helper.bzl", HELPER);
        self.file("dep/helper.bzl", HELPER);
        self.file(
            "root_export.bzl",
            "VALUE=Label('//:same')\nKEYS={VALUE:'root'}\n",
        );
        self.file(
            "dep/export.bzl",
            "VALUE=Label('//:same')\nSHARED=Label('@shared//:same')\nKEYS={VALUE:'external'}\n",
        );
    }

    fn build(&self, targets: &[&str], expected: &[(&str, &[u8])]) -> Built {
        let mut args = vec!["build"];
        args.extend_from_slice(targets);
        let output = self.run(&args);
        assert!(output.status.success(), "{}", errors(&output));
        let mut paths = BTreeMap::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let (label, path) = line.split_once('\t').expect("canonical label/path row");
            assert_eq!(
                bazel_rs::label::Label::parse_canonical(label)
                    .unwrap()
                    .to_string(),
                label
            );
            assert!(
                paths
                    .insert(label.to_owned(), PathBuf::from(path))
                    .is_none()
            );
        }
        assert_eq!(paths.len(), expected.len());
        for (label, bytes) in expected {
            let path = paths.get(*label).unwrap();
            let actual = fs::read(path).unwrap();
            let raw_mode = fs::metadata(path).unwrap().permissions().mode();
            eprintln!("N2_ARTIFACT {label} raw_mode={raw_mode:#o} bytes={actual:?}");
            assert_eq!(actual, *bytes, "{label}");
            let expected_mode = if path.starts_with(self.0.path().join("cache/actions")) {
                // Completed action publication is read-only, including writes.
                0o100555
            } else {
                assert!(path.starts_with(self.0.path().join("cache/repositories/direct/trees")));
                // These repository source fixtures use executable=False.
                0o100644
            };
            // Include the regular-file type and special bits; do not mask an
            // unexpected executable, writable, setuid/setgid, or sticky bit.
            assert_eq!(raw_mode, expected_mode, "{label}");
        }
        Built {
            paths,
            stderr: errors(&output),
        }
    }

    fn cold_warm_rule(&self, expected: &[u8]) {
        let expected = [("//:probe", expected), ("//:copy", expected)];
        let cold = self.build(&["//:probe", "//:copy"], &expected);
        cold.actions(2, 0);
        let warm = self.build(&["//:probe", "//:copy"], &expected);
        warm.actions(0, 2);
        assert_eq!(cold.paths, warm.paths);
    }

    fn failure(&self, expression: &str, diagnostic: &str, evaluate_argument: bool) {
        let (preamble, expression) = if evaluate_argument {
            (
                format!("def argument():\n    print('ARG_EVALUATED')\n    return {expression}\n"),
                "Label(argument())".to_owned(),
            )
        } else {
            (String::new(), expression.to_owned())
        };
        self.file("probe.bzl", format!("{preamble}\ndef probe():\n    print('BEFORE_BAD_CALL')\n    value={expression}\n    print('AFTER_BAD_CALL')\n    native.filegroup(name='probe')\n"));
        self.file("BUILD.bazel", "load(':probe.bzl','probe')\nprobe()\n");
        let output = self.run(&["query", "//:probe"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let text = errors(&output);
        let lines: Vec<_> = text.lines().collect();
        let before = lines
            .iter()
            .position(|line| *line == "BEFORE_BAD_CALL")
            .unwrap();
        assert!(!lines.contains(&"AFTER_BAD_CALL"));
        // Exact diagnostic text, not a source excerpt containing the same words.
        let error = lines
            .iter()
            .position(|line| *line == format!("error: {diagnostic}"))
            .unwrap_or_else(|| panic!("missing exact diagnostic {diagnostic:?}: {text}"));
        assert!(before < error);
        if evaluate_argument {
            let argument = lines
                .iter()
                .position(|line| *line == "ARG_EVALUATED")
                .unwrap();
            assert!(before < argument && argument < error);
        }
        assert!(!self.0.path().join("cache/actions").exists());
    }
}

struct Built {
    paths: BTreeMap<String, PathBuf>,
    stderr: String,
}

impl Built {
    fn actions(&self, executed: usize, hits: usize) {
        // The summary's execution-mode note is strategy-dependent (N59):
        // these fixtures run under the default (sandboxed) strategy.
        assert!(
            self.stderr.lines().any(|line| line.starts_with(&format!(
                "bazel-rs: {executed} actions executed, {hits} cache hits (experimental local execution; "
            ))),
            "{}",
            self.stderr
        );
    }
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn inventory(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
    fn visit(root: &Path, at: &Path, result: &mut BTreeMap<PathBuf, (Vec<u8>, u32)>) {
        if let Ok(entries) = fs::read_dir(at) {
            for entry in entries {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                assert!(!kind.is_symlink());
                if kind.is_dir() {
                    visit(root, &entry.path(), result);
                } else {
                    assert!(!entry.file_name().to_string_lossy().starts_with('.'));
                    result.insert(
                        entry.path().strip_prefix(root).unwrap().to_owned(),
                        (
                            fs::read(entry.path()).unwrap(),
                            entry.metadata().unwrap().permissions().mode(),
                        ),
                    );
                }
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

const WRITE: &str = r#"
def _impl(ctx):
    output=ctx.actions.declare_file(ctx.label.name+'.txt')
    ctx.actions.write(output,ctx.attr.text)
    return DefaultInfo(files=depset([output]))
write=rule(implementation=_impl,attrs={'text':attr.string()})
"#;
const HELPER: &str = r#"
def copy(value):
    return Label(value)
def local():
    return Label('//:same')
def shared():
    return Label('@shared//:same')
"#;

#[test]
fn root_and_package_labels_keep_identity_and_local_string_context() {
    let f = Fixture::new();
    f.file("pkg/BUILD.bazel", "");
    f.file("pkg/helper.bzl", "VALUE=Label(':same')\ndef copy(value):\n    return Label(value)\ndef local():\n    return Label(':same')\n");
    f.macro_body("load('//pkg:helper.bzl','VALUE','copy','local')", "    root=Label('//:same')\n    copied=copy(root)\n    keys={root:'root'}\n    keys[copied]='copy'\n    if len(keys)!=1 or keys[root]!='copy' or copied!=root or Label(VALUE)!=VALUE:\n        fail('KEY_IDENTITY')\n    write(name='probe',text='|'.join([str(copied),str(local()),str(Label(VALUE))])+'\\n')");
    f.cold_warm_rule(b"@@//:same|@@//pkg:same|@@//pkg:same\n");
}

#[test]
fn opposite_repository_helpers_preserve_labels_but_strings_resolve_locally() {
    let f = Fixture::new();
    f.external();
    f.macro_body("load(':root_helper.bzl',root_copy='copy',root_local='local',root_shared='shared')\nload('@shared//:helper.bzl',dep_copy='copy',dep_local='local',dep_shared='shared')\nload(':root_export.bzl',ROOT='VALUE')\nload('@shared//:export.bzl',EXTERNAL='VALUE',OTHER='SHARED')", "    values=[root_copy(EXTERNAL),dep_copy(ROOT),root_copy(OTHER),dep_copy(root_shared()),root_local(),dep_local(),root_shared(),dep_shared()]\n    write(name='probe',text='|'.join([str(v) for v in values])+'\\n')");
    f.cold_warm_rule(b"@@dep+//:same|@@//:same|@@other+//:same|@@dep+//:same|@@//:same|@@dep+//:same|@@dep+//:same|@@other+//:same\n");
}

#[test]
fn frozen_exports_two_readers_and_reconstructed_dictionary_keys() {
    let f = Fixture::new();
    f.external();
    for reader in ["a", "b"] {
        f.file(&format!("{reader}.bzl"), "load('@shared//:export.bzl','VALUE','KEYS')\ndef read():\n    copied=Label(VALUE)\n    rebuilt=Label('@@'+copied.workspace_name+'//:'+copied.name)\n    if copied!=rebuilt or KEYS[rebuilt]!='external' or hash(str(copied))!=hash(str(VALUE)):\n        fail('FROZEN_KEY')\n    return [copied,KEYS[copied],str(copied)]\n");
    }
    f.macro_body("load(':a.bzl',a='read')\nload(':b.bzl',b='read')", "    x=a()\n    y=b()\n    if x!=y or {x[0]:'key'}[y[0]]!='key':\n        fail('TWO_READERS')\n    write(name='probe',text=x[2]+'|'+x[1]+'\\n')");
    f.cold_warm_rule(b"@@dep+//:same|external\n");
}

#[test]
fn generated_repository_label_export_passes_through_root_helper() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='root_contract')\nmake=use_repo_rule('//:repo.bzl','make')\nmake(name='made')\n");
    f.file("repo.bzl", "def _impl(ctx):\n    ctx.file('BUILD.bazel',\"exports_files(['export.bzl'])\\n\",executable=False)\n    ctx.file('export.bzl',\"VALUE=Label('//:same')\\ndef copy(value):\\n    return Label(value)\\n\",executable=False)\nmake=repository_rule(_impl)\n");
    f.file("root_helper.bzl", HELPER);
    f.macro_body("load('@made//:export.bzl','VALUE',generated_copy='copy')\nload(':root_helper.bzl',root_copy='copy')", "    a=root_copy(VALUE)\n    b=generated_copy(Label('//:root'))\n    if a!=VALUE or {VALUE:1}[a]!=1:\n        fail('GENERATED_COPY')\n    write(name='probe',text=str(a)+'|'+a.workspace_name+'|'+str(b)+'\\n')");
    f.cold_warm_rule(b"@@+make+made//:same|+make+made|@@//:root\n");
}

#[test]
fn analysis_file_owner_and_target_labels_feed_real_output_consumer() {
    let f = Fixture::new();
    f.file("input.txt", "source\n");
    f.file("analysis.bzl", "def _impl(ctx):\n    output=ctx.actions.declare_file(ctx.label.name+'.txt')\n    originals=[ctx.file.src.owner,ctx.attr.src.label,ctx.label,output.owner]\n    copies=[Label(value) for value in originals]\n    if originals!=copies or [type(value) for value in copies]!=['Label']*4:\n        fail('ANALYSIS_LABELS')\n    ctx.actions.write(output,'|'.join([str(value) for value in copies])+'\\n')\n    return DefaultInfo(files=depset([output]))\nprobe=rule(implementation=_impl,attrs={'src':attr.label(allow_single_file=True)})\n");
    f.file("BUILD.bazel", "load(':analysis.bzl','probe')\nprobe(name='probe',src='input.txt')\ngenrule(name='copy',srcs=[':probe'],outs=['copy.txt'],cmd='/bin/cat $(SRCS) > $@')\n");
    f.cold_warm_rule(b"@@//:input.txt|@@//:input.txt|@@//:probe|@@//:probe\n");
}

#[test]
fn wrong_types_fail_in_typed_binder_after_argument_evaluation() {
    // BuiltinFunction.checkParamValue (BuiltinFunction.java:404-410) with
    // Label's allowedTypes String, Label.
    for (expression, actual) in [
        ("None", "NoneType"),
        ("True", "bool"),
        ("1", "int"),
        ("[]", "list"),
        ("{}", "dict"),
        ("struct(type='Label')", "struct"),
    ] {
        let f = Fixture::new();
        f.failure(
            expression,
            &format!(
                "in call to Label(), parameter 'input' got value of type '{actual}', want 'string or Label'"
            ),
            true,
        );
    }
}

#[test]
fn missing_extra_and_both_keyword_spellings_are_rejected() {
    // Bazel 9.3 BuiltinFunction binding texts (BuiltinFunction.java:320-490);
    // Label's `input` is positional-only.
    for (expression, diagnostic) in [
        (
            "Label()",
            "Label() missing 1 required positional argument: input",
        ),
        (
            "Label('//:a','//:b')",
            "Label() accepts no more than 1 positional argument but got 2",
        ),
        (
            "Label(input='//:a')",
            "Label() got named argument for positional-only parameter 'input'",
        ),
        (
            "Label(label_string='//:a')",
            "Label() got unexpected keyword argument 'label_string'",
        ),
        (
            "Label(input=Label('//:a'))",
            "Label() got named argument for positional-only parameter 'input'",
        ),
        (
            "Label('//:a',input='//:b')",
            "Label() got named argument for positional-only parameter 'input'",
        ),
        (
            "Label('//:a',label_string='//:b')",
            "Label() got unexpected keyword argument 'label_string'",
        ),
    ] {
        Fixture::new().failure(expression, diagnostic, false);
    }
}

#[test]
fn unknown_apparent_repository_yields_invalid_label_like_bazel() {
    // Label.parseWithPackageContext keeps an invalid repository
    // (`[unknown repo 'R' requested from @@]`); passthrough keeps it and only
    // the repository getters fail (Label.checkRepoVisibilityForStarlark).
    let f = Fixture::new();
    f.file("probe.bzl", "def probe():\n    print('BEFORE_UNKNOWN_CONSTRUCTOR')\n    original=Label('@not_visible//:missing')\n    print('BEFORE_PASSTHROUGH')\n    copied=Label(original)\n    print('AFTER_PASSTHROUGH ' + str(copied) + ' ' + copied.name + ' ' + str(copied == original))\n    native.filegroup(name='probe')\n");
    f.file("BUILD.bazel", "load(':probe.bzl','probe')\nprobe()\n");
    let output = f.run(&["query", "//:probe"]);
    assert_eq!(output.status.code(), Some(0), "{}", errors(&output));
    assert_eq!(output.stdout, b"//:probe\n");
    let text = errors(&output);
    let lines: Vec<_> = text.lines().collect();
    assert!(lines.contains(&"BEFORE_UNKNOWN_CONSTRUCTOR"));
    assert!(lines.contains(&"BEFORE_PASSTHROUGH"));
    assert!(
        lines.contains(
            &"AFTER_PASSTHROUGH @@[unknown repo 'not_visible' requested from @@]//:missing missing True"
        ),
        "{text}"
    );

    let g = Fixture::new();
    g.file("probe.bzl", "def probe():\n    copied=Label(Label('@not_visible//:missing'))\n    print(copied.workspace_name)\n    native.filegroup(name='probe')\n");
    g.file("BUILD.bazel", "load(':probe.bzl','probe')\nprobe()\n");
    let output = g.run(&["query", "//:probe"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let text = errors(&output);
    assert!(
        text.contains("'workspace_name' is not allowed on invalid Label @@[unknown repo 'not_visible' requested from @@]//:missing"),
        "{text}"
    );
    assert!(!g.0.path().join("cache/actions").exists());
}

#[test]
fn label_syntax_errors_use_bazel_texts() {
    // LabelValidator.validateTargetName (cmdline/LabelValidator.java:146-212);
    // non-ASCII target names are valid (Label('//pkg:café') succeeds).
    for (expression, diagnostic) in [
        (
            "Label('//pkg:../item')",
            "invalid target name '../item': target names may not contain up-level references '..'",
        ),
        (
            "Label('//pkg:a:b')",
            "invalid target name 'a:b': target names may not contain ':'",
        ),
    ] {
        Fixture::new().failure(expression, diagnostic, false);
    }
}

#[test]
fn extension_and_repository_producers_publish_stable_canonical_payload() {
    let f = Fixture::new();
    f.file(
        "MODULE.bazel",
        "module(name='root_contract')\ne=use_extension('//:ext.bzl','files')\nuse_repo(e,'made')\n",
    );
    f.file("export.bzl", "VALUE=Label('//:sentinel')\n");
    f.file("repo.bzl", "load(':export.bzl','VALUE')\ndef _repo(ctx):\n    print('N2_REPOSITORY_CALLBACK')\n    copied=Label(VALUE)\n    ctx.file('BUILD.bazel',\"exports_files(['data'])\\n\",executable=False)\n    ctx.file('data',ctx.attr.text+'|'+str(copied)+'\\n',executable=False)\nmake=repository_rule(_repo,attrs={'text':attr.string()})\n");
    f.file("ext.bzl", "load(':export.bzl','VALUE')\nload(':repo.bzl','make')\ndef _ext(ctx):\n    print('N2_EXTENSION_CALLBACK')\n    copied=Label(VALUE)\n    if {VALUE:'value'}[copied]!='value':\n        fail('EXTENSION_KEY')\n    make(name='made',text=str(copied))\nfiles=module_extension(_ext)\n");
    f.file("BUILD.bazel", "genrule(name='copy',srcs=['@made//:data'],outs=['copy.txt'],cmd='/bin/cat $(SRCS) > $@')\n");
    let bytes = b"@@//:sentinel|@@//:sentinel\n";
    let expected = [
        ("@@+files+made//:data", bytes.as_slice()),
        ("//:copy", bytes.as_slice()),
    ];
    let cold = f.build(&["@made//:data", "//:copy"], &expected);
    cold.actions(1, 0);
    let store = f.0.path().join("cache/repositories/direct");
    let before = inventory(&store);
    assert!(before.keys().any(|p| p.starts_with("trees")));
    assert!(before.keys().any(|p| p.starts_with("manifests")));
    let warm = f.build(&["@made//:data", "//:copy"], &expected);
    warm.actions(0, 1);
    assert_eq!(cold.paths, warm.paths);
    assert_eq!(before, inventory(&store));
    // Do not assert callback-marker absence on warm loads: native callbacks may
    // repeat even when the published tree and the consumer action are reused.
}
