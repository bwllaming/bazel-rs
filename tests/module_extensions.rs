//! Real native CLI contracts for bounded ordinary module-extension execution.
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("module-extension-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        f.file("MODULE.bazel", "module(name='main',version='1.0')\ne=use_extension('//:ext.bzl','files')\ne.make(name='made',text='Ada')\nuse_repo(e,chosen='made')\n");
        f.file("BUILD.bazel", "genrule(name='copy',srcs=['@chosen//:data.txt'],outs=['copy.txt'],cmd='cat $(SRCS) > $@')\n");
        f.file("rules.bzl", "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.file('data.txt', 'hello ' + ctx.attr.text + '\\n', executable=False)\nmaker=repository_rule(_impl,attrs={'text':attr.string()})\n");
        f.file("ext.bzl", "load(':rules.bzl','maker')\ndef _impl(ctx):\n    for m in ctx.modules:\n        for t in m.tags.make:\n            maker(name=t.name,text=t.text)\nfiles=module_extension(_impl,tag_classes={'make':tag_class(attrs={'name':attr.string(mandatory=True),'text':attr.string(default='world')})})\n");
        f
    }
    fn file(&self, name: &str, content: impl AsRef<[u8]>) {
        let path = self.0.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new("timeout")
            .args([
                "30s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
            ])
            .args(args)
            .current_dir(self.0.path())
            .output()
            .unwrap()
    }
    fn build(&self, target: &str) -> Vec<u8> {
        let result = self.run(&["build", target]);
        assert!(result.status.success(), "{}", error(&result));
        let output = String::from_utf8(result.stdout).unwrap();
        let (_, path) = output.lines().next().unwrap().split_once('\t').unwrap();
        fs::read(path).unwrap()
    }
    fn roots(&self) -> Vec<PathBuf> {
        fs::read_dir(self.0.path().join("cache/repositories/direct/trees"))
            .map(|entries| {
                entries
                    .map(|e| e.unwrap().path())
                    .filter(|p| !p.file_name().unwrap().to_string_lossy().starts_with('.'))
                    .collect()
            })
            .unwrap_or_default()
    }
    fn plain_usage(&self) {
        self.file("MODULE.bazel", "module(name='main',version='1.0')\ne=use_extension('//:ext.bzl','files')\nuse_repo(e,chosen='made')\n");
    }
}
fn error(result: &Output) -> String {
    String::from_utf8_lossy(&result.stderr).into_owned()
}
fn reject(result: &Output, marker: &str) {
    assert!(!result.status.success());
    assert_ne!(
        result.status.code(),
        Some(124),
        "extension request timed out"
    );
    assert!(!error(result).contains("panicked"), "{}", error(result));
    assert!(error(result).contains(marker), "{}", error(result));
    assert!(result.stdout.is_empty());
}

#[test]
fn computed_repo_real_query_and_build() {
    let f = Fixture::new();
    f.plain_usage();
    f.file("ext.bzl", "load(':rules.bzl','maker')\ndef _impl(ctx):\n    maker(name='ma'+'de',text='Ada')\nfiles=module_extension(_impl)\n");
    let query = f.run(&["query", "@chosen//:data.txt"]);
    assert!(query.status.success(), "{}", error(&query));
    assert_eq!(query.stdout, b"@@+files+made//:data.txt\n");
    assert_eq!(f.build("//:copy"), b"hello Ada\n");
    assert_eq!(f.build("@@+files+made//:data.txt"), b"hello Ada\n");
    assert_eq!(f.roots().len(), 1);
}

#[test]
fn whole_callback_failure_and_bad_return_publish_nothing() {
    for (tail, marker) in [
        ("fail('AFTER_DECLARATION')", "AFTER_DECLARATION"),
        ("return 42", "return"),
    ] {
        let f = Fixture::new();
        f.plain_usage();
        f.file("ext.bzl", format!("load(':rules.bzl','maker')\ndef _impl(ctx):\n    maker(name='made',text='Ada')\n    {tail}\nfiles=module_extension(_impl)\n"));
        reject(&f.run(&["query", "@chosen//:data.txt"]), marker);
        assert!(f.roots().is_empty());
    }
}

#[test]
fn unused_repo_is_validated_but_its_callback_is_not_executed() {
    let f = Fixture::new();
    f.plain_usage();
    f.file("rules.bzl", "def _impl(ctx):\n    if ctx.original_name == 'unused':\n        fail('UNUSED_FETCH')\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.file('data.txt',ctx.attr.text)\nmaker=repository_rule(_impl,attrs={'text':attr.string(mandatory=True)})\n");
    let ext = "load(':rules.bzl','maker')\ndef _impl(ctx):\n    maker(name='made',text='actual')\n    maker(name='unused',text='ignored')\nfiles=module_extension(_impl)\n";
    f.file("ext.bzl", ext);
    assert_eq!(f.build("//:copy"), b"actual");
    assert_eq!(f.roots().len(), 1);
    let before = f.roots();
    f.file(
        "ext.bzl",
        ext.replace("name='unused',text='ignored'", "name='unused'"),
    );
    reject(&f.run(&["build", "//:copy"]), "mandatory");
    assert_eq!(f.roots(), before);
}

#[test]
fn missing_import_invalidates_the_entire_successful_callback() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='main')\ne=use_extension('//:ext.bzl','files')\ne.make(name='made',text='Ada')\nuse_repo(e,chosen='made',unused='missing')\n");
    reject(&f.run(&["query", "@chosen//:data.txt"]), "missing");
    assert!(f.roots().is_empty());
}

#[test]
fn repository_call_arguments_are_snapshots_not_end_of_callback_values() {
    let f = Fixture::new();
    f.plain_usage();
    f.file("rules.bzl", "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.file('data.txt',','.join(ctx.attr.words))\nmaker=repository_rule(_impl,attrs={'words':attr.string_list()})\n");
    f.file("ext.bzl", "load(':rules.bzl','maker')\ndef _impl(ctx):\n    words=['before']\n    maker(name='made',words=words)\n    words.append('after')\nfiles=module_extension(_impl)\n");
    assert_eq!(f.build("//:copy"), b"before");
}

#[test]
fn canonical_aliases_merge_ordered_tags_and_only_bfs_participants_are_exposed() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='main',version='1.0+stamp')\nbazel_dep(name='z',version='2.0')\nbazel_dep(name='a',version='5')\nbazel_dep(name='shared',version='1')\nlocal_path_override(module_name='z',path='z')\nlocal_path_override(module_name='a',path='a')\nlocal_path_override(module_name='shared',path='shared')\nlocal_path_override(module_name='deep',path='deep')\ne=use_extension('@shared//:ext.bzl','files')\nother=use_extension('@@shared//:ext.bzl','files')\ne.note(value='first')\nother.note(value='second')\ne.note(value='third')\ndev=use_extension('@shared//:ext.bzl','files',dev_dependency=True)\ndev.note(value='dev')\nuse_repo(e,chosen='made')\n");
    f.file("z/MODULE.bazel", "module(name='z',version='2.0+stamp')\nbazel_dep(name='shared',version='1')\nbazel_dep(name='deep',version='3.0')\ne=use_extension('@shared//:ext.bzl','files')\ne.note(value='z')\ndev=use_extension('@shared//:ext.bzl','files',dev_dependency=True)\ndev.note(value='ignored')\n");
    f.file("a/MODULE.bazel", "module(name='a',version='5')\nbazel_dep(name='shared',version='1')\ne=use_extension('@shared//:ext.bzl','files')\n");
    f.file("deep/MODULE.bazel", "module(name='deep',version='3.0')\nbazel_dep(name='shared',version='1')\ne=use_extension('@shared//:ext.bzl','files')\ne.note(value='deep')\n");
    f.file("shared/MODULE.bazel", "module(name='shared',version='1')\n");
    f.file("shared/BUILD.bazel", "");
    f.file("shared/rules.bzl", "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.file('data.txt',ctx.attr.text)\nmaker=repository_rule(_impl,attrs={'text':attr.string()})\n");
    f.file("shared/ext.bzl", "load(':rules.bzl','maker')\ndef _impl(ctx):\n    parts=[]\n    for m in ctx.modules:\n        tags=[t.value+':'+str(ctx.is_dev_dependency(t)) for t in m.tags.note]\n        parts.append(m.name+':'+m.version+':'+str(m.is_root)+':'+','.join(tags))\n    maker(name='made',text='|'.join(parts)+';root='+str(ctx.root_module_has_non_dev_dependency))\nfiles=module_extension(_impl,tag_classes={'note':tag_class({'value':attr.string()})})\n");
    let expected = "main:1.0:True:first:False,second:False,third:False,dev:True|z:2.0:False:z:False|a:5:False:|deep:3.0:False:deep:False;root=True";
    assert_eq!(f.build("//:copy"), expected.as_bytes());
    // MODULE extension labels normalize @@ spellings back to apparent names;
    // unlike BUILD labels they cannot bypass visibility with a canonical name.
    let original = fs::read_to_string(f.0.path().join("MODULE.bazel")).unwrap();
    f.file(
        "MODULE.bazel",
        original.replace("@@shared//", "@@shared+//"),
    );
    reject(
        &f.run(&["build", "//:copy"]),
        "unknown apparent repository @shared+",
    );
    // The ignore flag also suppresses root overrides in the pinned MODULE
    // contract. Test its tag filtering without relying on local overrides.
    let isolated = Fixture::new();
    isolated.file("MODULE.bazel", "module(name='main')\ne=use_extension('//:ext.bzl','files')\ne.make(name='made',text='normal')\ndev=use_extension('//:ext.bzl','files',dev_dependency=True)\ndev.make(name='made',text='would-duplicate')\nuse_repo(e,chosen='made')\n");
    let ignored = isolated.run(&["build", "--ignore_dev_dependency=true", "//:copy"]);
    assert!(ignored.status.success(), "{}", error(&ignored));
    let line = String::from_utf8(ignored.stdout).unwrap();
    let (_, path) = line.lines().next().unwrap().split_once('\t').unwrap();
    assert_eq!(fs::read(path).unwrap(), b"hello normal\n");
}

#[test]
fn label_ownership_defaults_and_forward_unimported_siblings() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='main')\nbazel_dep(name='host',version='1',repo_name='sibling')\nlocal_path_override(module_name='host',path='host')\ne=use_extension('//pkg:ext.bzl','files')\ne.input(supplied='//:input')\nuse_repo(e,chosen='made')\n");
    f.file("host/MODULE.bazel", "module(name='host',version='1')\n");
    f.file("host/BUILD.bazel", "exports_files(['data.txt'])\n");
    f.file("host/data.txt", "HOST\n");
    f.file("input", "ROOT\n");
    f.file("helper/BUILD.bazel", "");
    f.file("helper/input", "HELPER\n");
    f.file("helper/tags.bzl", "def inputs():\n    return tag_class({'supplied':attr.label(),'fallback':attr.label(default=':input')})\n");
    f.file("pkg/BUILD.bazel", "");
    f.file("pkg/local", "PUBLIC\n");
    f.file("defs/BUILD.bazel", "");
    f.file("defs/rules.bzl", "def _impl(ctx):\n    if ctx.original_name == 'sibling':\n        ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n        ctx.file('data.txt','GENERATED\\n')\n    else:\n        ctx.file('BUILD.bazel', \"exports_files(['raw','explicit','supplied','fallback','relative'])\\n\")\n        ctx.template('raw',ctx.attr.raw)\n        ctx.template('explicit',ctx.attr.explicit)\n        ctx.template('supplied',ctx.attr.supplied)\n        ctx.template('fallback',ctx.attr.fallback)\n        ctx.template('relative',ctx.attr.relative)\nmaker=repository_rule(_impl,attrs={'raw':attr.label(),'explicit':attr.label(),'supplied':attr.label(),'fallback':attr.label(),'relative':attr.label()})\n");
    f.file("pkg/ext.bzl", "load('//defs:rules.bzl','maker')\nload('//helper:tags.bzl','inputs')\ndef _impl(ctx):\n    tag=ctx.modules[0].tags.input[0]\n    maker(name='made',raw='@sibling//:data.txt',explicit=Label('@sibling//:data.txt'),supplied=tag.supplied,fallback=tag.fallback,relative=':local')\n    maker(name='sibling')\nfiles=module_extension(_impl,tag_classes={'input':inputs()})\n");
    f.file("BUILD.bazel", "genrule(name='copy',srcs=['@chosen//:raw','@chosen//:explicit','@chosen//:supplied','@chosen//:fallback','@chosen//:relative'],outs=['copy.txt'],cmd='cat $(SRCS) > $@')\n");
    assert_eq!(
        f.build("//:copy"),
        b"GENERATED\nHOST\nROOT\nHELPER\nPUBLIC\n"
    );
    assert_eq!(f.build("@@+files+sibling//:data.txt"), b"GENERATED\n");
}

#[test]
fn private_extension_symbols_are_distinct_requested_identities() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='main')\ne=use_extension('//:ext.bzl','_private')\nuse_repo(e,chosen='made')\n");
    f.file("ext.bzl", "load(':rules.bzl','maker')\ndef _impl(ctx):\n    maker(name='made',text='private')\n_private=module_extension(_impl)\nfiles=_private\n");
    let output = f.run(&["query", "@chosen//:data.txt"]);
    assert!(output.status.success(), "{}", error(&output));
    assert_eq!(output.stdout, b"@@+_private+made//:data.txt\n");
    assert_eq!(f.build("//:copy"), b"hello private\n");
}

#[test]
fn cross_extension_preparation_cycles_fail_without_hanging() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='main')\na=use_extension('//:a.bzl','a')\nb=use_extension('//:b.bzl','b')\nuse_repo(a,'a_repo')\nuse_repo(b,'b_repo')\n");
    f.file("a.bzl", "load('@b_repo//:helper.bzl','value')\ndef _impl(ctx):\n    pass\na=module_extension(_impl)\n");
    f.file("b.bzl", "load('@a_repo//:helper.bzl','value')\ndef _impl(ctx):\n    pass\nb=module_extension(_impl)\n");
    reject(&f.run(&["query", "@a_repo//:data.txt"]), "cycle");
    assert!(f.roots().is_empty());
}

#[test]
fn direct_and_ordinary_preparation_share_the_cycle_guard() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='main')\ne=use_extension('//:ext.bzl','files')\nuse_repo(e,chosen='made')\nseed=use_repo_rule('@chosen//:rules.bzl','seed')\nseed(name='seed')\n");
    f.file("ext.bzl", "load('@seed//:helper.bzl','helper')\ndef _impl(ctx):\n    pass\nfiles=module_extension(_impl)\n");
    let output = f.run(&["query", "@chosen//:data.txt"]);
    reject(&output, "cycle");
    assert!(error(&output).contains("prepare:ordinary:") && error(&output).contains("+seed+seed"));
    assert!(f.roots().is_empty());
}

#[test]
fn projected_module_and_tag_lists_are_immutable_on_the_real_surface() {
    for mutation in [
        "ctx.modules.append(ctx.modules[0])",
        "ctx.modules[0].tags.input.append(ctx.modules[0].tags.input[0])",
        "ctx.modules[0].tags.input[0].words.append('bad')",
    ] {
        let f = Fixture::new();
        f.file("MODULE.bazel", "module(name='main')\ne=use_extension('//:ext.bzl','files')\ne.input(words=['before'])\nuse_repo(e,chosen='made')\n");
        f.file("ext.bzl", format!("load(':rules.bzl','maker')\ndef _impl(ctx):\n    maker(name='made',text='never published')\n    {mutation}\nfiles=module_extension(_impl,tag_classes={{'input':tag_class({{'words':attr.string_list()}})}})\n"));
        reject(&f.run(&["query", "@chosen//:data.txt"]), "Immutable");
        assert!(f.roots().is_empty());
    }
}

#[test]
fn unused_unknown_routes_and_isolated_demands_do_not_fabricate_repositories() {
    let f = Fixture::new();
    f.file("ext.bzl", "fail('UNUSED_EXTENSION_LOADED')\n");
    let local = f.run(&["query", "//:copy"]);
    assert!(local.status.success(), "{}", error(&local));
    let unknown = f.run(&["query", "@@+files_extra+made//:data.txt"]);
    reject(&unknown, "unknown canonical repository");
    assert!(!error(&unknown).contains("UNUSED_EXTENSION_LOADED"));
    f.file("MODULE.bazel", "module(name='main')\ne=use_extension('//:ext.bzl','files',isolate=True)\nuse_repo(e,chosen='made')\n");
    reject(
        &f.run(&["query", "@chosen//:data.txt"]),
        "isolated module extensions unsupported",
    );
    assert!(f.roots().is_empty());
}

#[test]
fn repeated_defaults_fail_gracefully_before_host_allocation_exhaustion() {
    for tag_defaults in [true, false] {
        let f = Fixture::new();
        let mut module = "module(name='main')\ne=use_extension('//:ext.bzl','files')\nuse_repo(e,chosen='made')\n".to_owned();
        if tag_defaults {
            module.push_str(&"e.item()\n".repeat(600));
            f.file("ext.bzl", "def _impl(ctx):\n    fail('CALLBACK_REACHED')\nfiles=module_extension(_impl,tag_classes={'item':tag_class({'payload':attr.string(default='x'*(1024*1024))})})\n");
        } else {
            f.file("ext.bzl", "def _repo(ctx):\n    fail('FETCH_REACHED')\nr=repository_rule(_repo,attrs={'payload':attr.string(default='x'*(1024*1024))})\ndef _impl(ctx):\n    for i in range(600):\n        r(name='repo'+str(i))\n    r(name='made')\nfiles=module_extension(_impl)\n");
        }
        f.file("MODULE.bazel", module);
        let output = Command::new("prlimit")
            .args([
                "--as=536870912",
                "--core=0",
                "--",
                "timeout",
                "30s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "query",
                "@chosen//:data.txt",
            ])
            .current_dir(f.0.path())
            .output()
            .unwrap();
        // An abort/OOM kill is not an acceptable unsupported-profile error.
        assert_eq!(output.status.code(), Some(1), "{}", error(&output));
        assert!(
            error(&output).contains("host") && error(&output).contains("limit"),
            "{}",
            error(&output)
        );
        assert!(
            !error(&output).contains("CALLBACK_REACHED")
                && !error(&output).contains("FETCH_REACHED")
        );
        assert!(f.roots().is_empty());
    }
}

#[test]
fn canonical_label_expansion_is_charged_before_binding_lists() {
    let symbol = "e".repeat(65_536);
    for tag_labels in [true, false] {
        let f = Fixture::new();
        let mut module = format!(
            "module(name='main')\ne=use_extension('//:ext.bzl','{symbol}')\nuse_repo(e,chosen='made')\n"
        );
        let definition = if tag_labels {
            module.push_str("e.item(refs=['@chosen//:data']*20000)\n");
            format!(
                "def _impl(ctx):\n    fail('CALLBACK_REACHED')\n{symbol}=module_extension(_impl,tag_classes={{'item':tag_class({{'refs':attr.label_list()}})}})\n"
            )
        } else {
            format!(
                "def _repo(ctx):\n    fail('FETCH_REACHED')\nr=repository_rule(_repo,attrs={{'refs':attr.label_list()}})\ndef _impl(ctx):\n    r(name='made',refs=['@sibling//:data']*20000)\n    r(name='sibling')\n{symbol}=module_extension(_impl)\n"
            )
        };
        f.file("MODULE.bazel", module);
        f.file("ext.bzl", definition);
        let output = Command::new("prlimit")
            .args([
                "--as=536870912",
                "--core=0",
                "--",
                "timeout",
                "30s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
                "query",
                "@chosen//:data",
            ])
            .current_dir(f.0.path())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{}", error(&output));
        assert!(
            error(&output).contains("host") && error(&output).contains("limit"),
            "{}",
            error(&output)
        );
        assert!(
            !error(&output).contains("CALLBACK_REACHED")
                && !error(&output).contains("FETCH_REACHED")
        );
        assert!(f.roots().is_empty());
    }
}

#[test]
fn fresh_runtime_observes_tags_callback_sources_and_binary_templates() {
    let f = Fixture::new();
    assert_eq!(f.build("//:copy"), b"hello Ada\n");
    let module = fs::read_to_string(f.0.path().join("MODULE.bazel")).unwrap();
    f.file("MODULE.bazel", module.replace("text='Ada'", "text='Bob'"));
    assert_eq!(f.build("//:copy"), b"hello Bob\n");
    f.file("rules.bzl", "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.template('data.txt',Label('//:template'))\nmaker=repository_rule(_impl,attrs={'text':attr.string()})\n");
    f.file("template", b"binary\xff\0\r\n");
    assert_eq!(f.build("//:copy"), b"binary\xff\0\r\n");
    f.file("template", b"updated\xfe");
    assert_eq!(f.build("//:copy"), b"updated\xfe");
}

#[test]
fn unsupported_context_operations_and_invalid_names_never_publish() {
    for (operation, marker) in [
        ("ctx.extension_metadata()", "extension_metadata"),
        ("ctx.os.environ", "repository_os.environ is unsupported"),
        (
            "ctx.read(\"x\",watch=\"yes\")",
            "unsupported module I/O profile: watch=yes on extension-owned files",
        ),
        ("maker(name='a+b',text='bad')", "invalid repository name"),
    ] {
        let f = Fixture::new();
        f.plain_usage();
        f.file("ext.bzl", format!("load(':rules.bzl','maker')\ndef _impl(ctx):\n    {operation}\n    maker(name='made',text='never')\nfiles=module_extension(_impl)\n"));
        if operation.starts_with("ctx.read(") {
            // Match the legacy validator's read-watch fixture byte for byte.
            f.file("MODULE.bazel", "module(name=\"main\", version=\"1.0\")\ne=use_extension(\"//:ext.bzl\", \"files\")\nuse_repo(e, chosen=\"made\")\n");
            f.file("BUILD.bazel", "");
            f.file("ext.bzl", "load(\":rules.bzl\", \"maker\")\ndef _impl(ctx):\n    maker(name=\"made\")\n    ctx.read(\"x\",watch=\"yes\")\n    fail(\"EXTENSION_AFTER_RECORD_SENTINEL\")\nfiles=module_extension(_impl, tag_classes={}, doc=None, environ=[], os_dependent=False, arch_dependent=False, facts_version=0)\n");
            f.file("rules.bzl", "def _impl(ctx):\n    fail(\"UNUSED_REPOSITORY_CALLBACK_SENTINEL\")\nmaker = repository_rule(implementation = _impl, attrs = {})\n");
        }
        let target = if operation.starts_with("ctx.read(") {
            "@chosen//:data"
        } else {
            "@chosen//:data.txt"
        };
        let output = f.run(&["query", target]);
        if operation.starts_with("ctx.read(") {
            eprintln!(
                "READ_WATCH_FIXTURE_BEGIN\n{}READ_WATCH_FIXTURE_END\nREAD_WATCH_STDERR_BEGIN\n{}READ_WATCH_STDERR_END",
                fs::read_to_string(f.0.path().join("ext.bzl")).unwrap(),
                error(&output)
            );
            assert_eq!(output.status.code(), Some(1), "{}", error(&output));
            assert_eq!(
                error(&output)
                    .lines()
                    .filter(|line| line.starts_with("error: "))
                    .collect::<Vec<_>>(),
                [format!("error: {marker}")]
            );
            assert!(!error(&output).contains("EXTENSION_AFTER_RECORD_SENTINEL"));
            assert!(!error(&output).contains("UNUSED_REPOSITORY_CALLBACK_SENTINEL"));
            assert!(
                error(&output)
                    .lines()
                    .any(|line| line == " --> //:ext.bzl:4:5")
            );
            assert!(
                error(&output)
                    .lines()
                    .any(|line| line == "4 |     ctx.read(\"x\",watch=\"yes\")")
            );
            assert!(!f.0.path().join("cache").exists());
            for name in ["MODULE.bazel", "BUILD.bazel", "rules.bzl"] {
                eprintln!(
                    "READ_WATCH_INPUT_BEGIN {name}\n{}READ_WATCH_INPUT_END {name}",
                    fs::read_to_string(f.0.path().join(name)).unwrap()
                );
            }
        }
        reject(&output, marker);
        assert!(f.roots().is_empty());
    }
}
