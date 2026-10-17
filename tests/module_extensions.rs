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
    assert_eq!(query.stdout, b"@chosen//:data.txt\n");
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
        "invalid label for module extension found at ",
    );
    reject(
        &f.run(&["build", "//:copy"]),
        "/MODULE.bazel:10:20: no repo visible as '@shared+' here",
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
    assert_eq!(output.stdout, b"@chosen//:data.txt\n");
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
        reject(
            &f.run(&["query", "@chosen//:data.txt"]),
            "trying to mutate a frozen list value",
        );
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
        "parameter 'isolate' is experimental and thus unavailable with the current flags",
    );
    reject(
        &f.run(&[
            "query",
            "--experimental_isolated_extension_usages",
            "@chosen//:data.txt",
        ]),
        "isolated module extensions unsupported",
    );
    assert!(f.roots().is_empty());
}

#[test]
fn repeated_defaults_have_no_native_host_limit() {
    // Bazel bounds no extension input or result. 80 x 1 MiB defaults exceeded
    // the former native 64 MiB weighted host budget (8 bytes per payload byte).
    for tag_defaults in [true, false] {
        let f = Fixture::new();
        let mut module = "module(name='main')\ne=use_extension('//:ext.bzl','files')\nuse_repo(e,chosen='made')\n".to_owned();
        if tag_defaults {
            module.push_str(&"e.item()\n".repeat(80));
            f.file("ext.bzl", "def _impl(ctx):\n    len(ctx.modules[0].tags.item) == 80 or fail('count')\n    fail('CALLBACK_REACHED')\nfiles=module_extension(_impl,tag_classes={'item':tag_class({'payload':attr.string(default='x'*(1024*1024))})})\n");
        } else {
            f.file("ext.bzl", "def _repo(ctx):\n    fail('FETCH_REACHED')\nr=repository_rule(_repo,attrs={'payload':attr.string(default='x'*(1024*1024))})\ndef _impl(ctx):\n    for i in range(80):\n        r(name='repo'+str(i))\n    r(name='made')\nfiles=module_extension(_impl)\n");
        }
        f.file("MODULE.bazel", module);
        let output = f.run(&["query", "@chosen//:data.txt"]);
        assert_eq!(output.status.code(), Some(1), "{}", error(&output));
        let marker = if tag_defaults {
            "CALLBACK_REACHED"
        } else {
            "FETCH_REACHED"
        };
        assert!(error(&output).contains(marker), "{}", error(&output));
        assert!(!error(&output).contains("limit"), "{}", error(&output));
        assert!(f.roots().is_empty());
    }
}

#[test]
fn canonical_label_expansion_has_no_native_host_limit() {
    // 200 labels expanding to 65 KiB canonical names: above the former
    // weighted host budget, accepted like Bazel.
    let symbol = "e".repeat(65_536);
    for tag_labels in [true, false] {
        let f = Fixture::new();
        let mut module = format!(
            "module(name='main')\ne=use_extension('//:ext.bzl','{symbol}')\nuse_repo(e,chosen='made')\n"
        );
        let definition = if tag_labels {
            module.push_str("e.item(refs=['@chosen//:data']*200)\n");
            format!(
                "def _impl(ctx):\n    len(ctx.modules[0].tags.item[0].refs) == 200 or fail('count')\n    fail('CALLBACK_REACHED')\n{symbol}=module_extension(_impl,tag_classes={{'item':tag_class({{'refs':attr.label_list()}})}})\n"
            )
        } else {
            format!(
                "def _repo(ctx):\n    fail('FETCH_REACHED')\nr=repository_rule(_repo,attrs={{'refs':attr.label_list()}})\ndef _impl(ctx):\n    r(name='made',refs=['@sibling//:data']*200)\n    r(name='sibling')\n{symbol}=module_extension(_impl)\n"
            )
        };
        f.file("MODULE.bazel", module);
        f.file("ext.bzl", definition);
        let output = f.run(&["query", "@chosen//:data"]);
        assert_eq!(output.status.code(), Some(1), "{}", error(&output));
        let marker = if tag_labels {
            "CALLBACK_REACHED"
        } else {
            "FETCH_REACHED"
        };
        assert!(error(&output).contains(marker), "{}", error(&output));
        assert!(!error(&output).contains("limit"), "{}", error(&output));
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
        (
            "ctx.extension_metadata(root_module_direct_deps=['a+b'],root_module_direct_dev_deps=[])",
            "in root_module_direct_deps: invalid user-provided repo name 'a+b'",
        ),
        (
            "ctx.read(\"x\",watch=\"yes\")",
            "attempted to watch path under working directory",
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

/// Extension creating `made`, `b`, `dev1`, `extra` and returning `metadata`.
fn metadata_extension(f: &Fixture, metadata: &str) {
    f.file("ext.bzl", format!("load(':rules.bzl','maker')\ndef _impl(ctx):\n    for n in ['made', 'b', 'dev1', 'extra']:\n        maker(name=n, text=n)\n    return {metadata}\nfiles=module_extension(_impl)\n"));
}

fn module_path(f: &Fixture) -> String {
    fs::canonicalize(f.0.path().join("MODULE.bazel"))
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

fn warnings(result: &Output) -> usize {
    error(result).matches("WARNING: ").count()
}

#[test]
fn extension_metadata_use_repo_fixup_warning_is_exact_and_build_succeeds() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='main',version='1.0')\nbazel_dep(name='unrelated',version='1',repo_name=None)\ne = use_extension('//:ext.bzl', 'files')\nuse_repo(e, chosen='made', other='extra')\nd = use_extension('//:ext.bzl', 'files', dev_dependency = True)\nuse_repo(d, 'dev1')\n");
    metadata_extension(
        &f,
        "ctx.extension_metadata(root_module_direct_deps=['made', 'b'], root_module_direct_dev_deps=('dev1',))",
    );
    let result = f.run(&["build", "//:copy"]);
    assert!(result.status.success(), "{}", error(&result));
    let expected = format!(
        "WARNING: {}:3:18: The module extension files defined in @main//:ext.bzl reported incorrect imports of repositories via use_repo():\n\n\
Not imported, but reported as direct dependencies by the extension (may cause the build to fail):\n    b\n\n\
Imported, but reported as indirect dependencies by the extension:\n    extra\n\n\
Fix the use_repo calls by running 'bazel mod tidy'.\n",
        module_path(&f)
    );
    assert!(error(&result).contains(&expected), "{}", error(&result));
    assert_eq!(warnings(&result), 1, "{}", error(&result));
    let (_, path) = String::from_utf8(result.stdout.clone())
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .split_once('\t')
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .unwrap();
    assert_eq!(fs::read(path).unwrap(), b"hello made\n");
    // Every invocation re-evaluates the extension and repeats the warning,
    // while the already-published repository tree is reused unchanged.
    let roots = f.roots();
    let again = f.run(&["build", "//:copy"]);
    assert!(again.status.success(), "{}", error(&again));
    assert!(error(&again).contains(&expected), "{}", error(&again));
    assert_eq!(warnings(&again), 1, "{}", error(&again));
    assert_eq!(f.roots(), roots);

    // Correct imports for "all" (REGULAR), explicit lists, and DEV mode: no warning.
    for (module, metadata) in [
        (
            "use_repo(e, chosen='made', b='b', dev1='dev1', extra='extra')\n",
            "ctx.extension_metadata(root_module_direct_deps='all', root_module_direct_dev_deps=[])",
        ),
        (
            "use_repo(e, chosen='made', b='b', extra='extra')\nuse_repo(d, 'dev1')\n",
            "ctx.extension_metadata(root_module_direct_deps=['made', 'b', 'extra'], root_module_direct_dev_deps=['dev1'], reproducible=True)",
        ),
        (
            "use_repo(d, chosen='made', b='b', dev1='dev1', extra='extra')\n",
            "ctx.extension_metadata(root_module_direct_deps=[], root_module_direct_dev_deps='all')",
        ),
    ] {
        f.file("MODULE.bazel", format!("module(name='main',version='1.0')\ne = use_extension('//:ext.bzl', 'files')\nd = use_extension('//:ext.bzl', 'files', dev_dependency = True)\n{module}"));
        metadata_extension(&f, metadata);
        let result = f.run(&["build", "//:copy"]);
        assert!(result.status.success(), "{metadata}: {}", error(&result));
        assert_eq!(warnings(&result), 0, "{metadata}: {}", error(&result));
    }

    // "all" REGULAR with dev imports: every section, located at the FIRST proxy
    // (here the dev proxy), sorted names, then the import check still runs.
    f.file("MODULE.bazel", "module(name='main',version='1.0')\nd = use_extension('//:ext.bzl', 'files', dev_dependency = True)\nuse_repo(d, 'dev1', 'b')\ne = use_extension(\"@main//:ext.bzl\", 'files')\nuse_repo(e, chosen='made', ghost='ghost')\n");
    metadata_extension(
        &f,
        "ctx.extension_metadata(root_module_direct_deps='all', root_module_direct_dev_deps=[])",
    );
    let result = f.run(&["build", "//:copy"]);
    let expected = format!(
        "WARNING: {}:2:18: The module extension files defined in @main//:ext.bzl reported incorrect imports of repositories via use_repo():\n\n\
Imported, but not created by the extension (will cause the build to fail):\n    ghost\n\n\
Not imported, but reported as direct dependencies by the extension (may cause the build to fail):\n    extra\n\n\
Imported as a dev dependency, but reported as a regular dependency by the extension (may cause the build to fail when used by other modules):\n    b, dev1\n\n\
Fix the use_repo calls by running 'bazel mod tidy'.\n",
        module_path(&f)
    );
    assert!(error(&result).contains(&expected), "{}", error(&result));
    let warning = error(&result).find("WARNING: ").unwrap();
    let import = error(&result)
        .find("did not declare imported repository \"ghost\"")
        .unwrap_or_else(|| panic!("{}", error(&result)));
    assert!(warning < import);
    reject(&result, "ghost");
}

#[test]
fn extension_metadata_errors_fail_root_extension_evaluation() {
    for (module, metadata, marker) in [
        (
            "",
            "ctx.extension_metadata(root_module_direct_deps=['made', 'missing', 'gone'], root_module_direct_dev_deps=[])",
            "root_module_direct_deps contained the following repositories not generated by the extension: missing, gone",
        ),
        (
            "",
            "ctx.extension_metadata(root_module_direct_deps=['made'], root_module_direct_dev_deps=['nope'])",
            "root_module_direct_dev_deps contained the following repositories not generated by the extension: nope",
        ),
        (
            "",
            "ctx.extension_metadata(root_module_direct_deps=[], root_module_direct_dev_deps='all')",
            "root_module_direct_dev_deps must be empty if the root module contains no usages with dev_dependency = True",
        ),
        (
            "d = use_extension('//:ext.bzl', 'files', dev_dependency=True)\n",
            "ctx.extension_metadata(root_module_direct_deps=['made'], root_module_direct_dev_deps=[])",
            "root_module_direct_deps must be empty if the root module contains no usages with dev_dependency = False",
        ),
        (
            "",
            "ctx.extension_metadata(root_module_direct_deps='all', root_module_direct_dev_deps=())",
            "if one of root_module_direct_deps and root_module_direct_dev_deps is \"all\", the other must be an empty list",
        ),
    ] {
        let f = Fixture::new();
        let usage = if module.is_empty() {
            "e = use_extension('//:ext.bzl', 'files')\nuse_repo(e, chosen='made')\n"
        } else {
            "d = use_extension('//:ext.bzl', 'files', dev_dependency=True)\nuse_repo(d, chosen='made')\n"
        };
        f.file("MODULE.bazel", format!("module(name='main')\n{usage}"));
        metadata_extension(&f, metadata);
        let result = f.run(&["build", "//:copy"]);
        reject(&result, marker);
        if !metadata.contains("()") {
            assert!(
                error(&result).contains("error evaluating module extension files in //:ext.bzl"),
                "{}",
                error(&result)
            );
        }
        assert_eq!(warnings(&result), 0, "{}", error(&result));
        assert!(f.roots().is_empty());
    }
}

#[test]
fn skylib_use_all_repos_pattern_from_a_non_root_module_never_warns() {
    let f = Fixture::new();
    f.file("MODULE.bazel", "module(name='main')\nbazel_dep(name='dep',version='1')\nlocal_path_override(module_name='dep',path='dep')\n");
    f.file(
        "BUILD.bazel",
        "genrule(name='copy',srcs=['@dep//:copy.txt'],outs=['copy.txt'],cmd='cat $(SRCS) > $@')\n",
    );
    f.file("dep/MODULE.bazel", "module(name='dep',version='1')\ne=use_extension('//:ext.bzl','files')\nuse_repo(e,'made')\n");
    f.file("dep/BUILD.bazel", "genrule(name='copy',srcs=['@made//:data.txt'],outs=['copy.txt'],cmd='cat $(SRCS) > $@',visibility=['//visibility:public'])\n");
    f.file("dep/rules.bzl", "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.file('data.txt', ctx.attr.text + '\\n')\nmaker=repository_rule(_impl,attrs={'text':attr.string()})\n");
    // bazel_skylib lib/modules.bzl _use_all_repos, verbatim control flow.
    let skylib = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/build_settings/skylib/lib/modules.bzl"),
    )
    .unwrap();
    f.file("dep/modules.bzl", &skylib);
    f.file("dep/ext.bzl", "load(':rules.bzl','maker')\nload(':modules.bzl','modules')\ndef _impl(ctx):\n    maker(name='made', text='root=' + str(ctx.root_module_has_non_dev_dependency))\n    maker(name='unimported', text='x')\n    return modules.use_all_repos(ctx, reproducible=True)\nfiles=module_extension(_impl)\n");
    let result = f.run(&["build", "//:copy"]);
    assert!(result.status.success(), "{}", error(&result));
    assert_eq!(warnings(&result), 0, "{}", error(&result));
    let output = String::from_utf8(result.stdout).unwrap();
    let (_, path) = output.lines().next().unwrap().split_once('\t').unwrap();
    assert_eq!(fs::read(path).unwrap(), b"root=False\n");
    // Without a root usage even invalid explicit lists are never checked
    // (SingleExtensionEvalFunction.java:465-466).
    f.file("dep/ext.bzl", "load(':rules.bzl','maker')\ndef _impl(ctx):\n    maker(name='made', text='explicit')\n    return ctx.extension_metadata(root_module_direct_deps=['missing'], root_module_direct_dev_deps=[])\nfiles=module_extension(_impl)\n");
    let result = f.run(&["build", "//:copy"]);
    assert!(result.status.success(), "{}", error(&result));
    assert_eq!(warnings(&result), 0, "{}", error(&result));
}

#[test]
fn main_mapping_order_follows_bazel_entry_order() {
    // BazelDepGraphValue.getRepositoryMapping: extension imports first
    // (positional, then keywords in call order), then "", the root's repo
    // name and bazel_deps in declaration order.
    let f = Fixture::new();
    f.file(
        "MODULE.bazel",
        "module(name='main',version='1.0',repo_name='self')\ne=use_extension('//:ext.bzl','files')\ne.make(name='made',text='Ada')\ne.make(name='other',text='Bob')\nuse_repo(e,'other',zed='made',alpha='made2')\ne.make(name='made2',text='Cy')\n",
    );
    let repos = bazel_rs::repository::RepositorySet::new(f.0.path().to_owned());
    assert_eq!(
        repos.main_mapping_order(),
        ["other", "zed", "alpha", "", "self"].map(str::to_owned)
    );
}
