//! Real CLI: a rules_go-shaped `go_sdk` extension reads go.mod files from the
//! root module and a local_path_override module through module_ctx.path/read,
//! returns extension_metadata, and its generated repository feeds a genrule.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);

const ROOT_GO_MOD: &str = "module example.com/app\n\ngo 1.27.1\n";
const DEP_GO_MOD: &str = "module example.com/gorules\n\ngo 1.25.0\n";
const EXPECTED: &[u8] =
    b"main:download:1.27.1:True\nmain:from_file:1.27.1\ngorules:from_file:1.25.0\n";

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("module-path-cli-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        );
        for directory in ["home", "tmp", "xdg", "dep"] {
            fs::create_dir(f.0.path().join(directory)).unwrap();
        }
        f.file("MODULE.bazel", "module(name='main')\nbazel_dep(name='gorules',version='0.63.0',repo_name='io_bazel_rules_go')\nlocal_path_override(module_name='gorules',path='dep')\ngo_sdk=use_extension('@io_bazel_rules_go//:ext.bzl','go_sdk')\ngo_sdk.download(version='1.27.1')\ngo_sdk.from_file(go_mod='//:go.mod')\nuse_repo(go_sdk,'go_toolchains')\n");
        f.file("BUILD.bazel", "genrule(name='copy', srcs=['@go_toolchains//:data.txt'], outs=['copy.txt'], cmd='cat $(SRCS) > $@')\n");
        f.file("go.mod", ROOT_GO_MOD);
        f.file("dep/MODULE.bazel", "module(name='gorules',version='0.63.0')\ngo_sdk=use_extension('//:ext.bzl','go_sdk')\ngo_sdk.from_file(go_mod='//:go.mod')\n");
        f.file("dep/BUILD.bazel", "");
        f.file("dep/go.mod", DEP_GO_MOD);
        f.file("dep/rules.bzl", "def _impl(ctx):\n    ctx.file('BUILD.bazel', \"exports_files(['data.txt'])\\n\")\n    ctx.file('data.txt', ctx.attr.text, executable=False)\nsdk_repo=repository_rule(_impl, attrs={'text':attr.string()})\n");
        f.file("dep/ext.bzl", r#"load(':rules.bzl', 'sdk_repo')
def _version(ctx, label):
    p = ctx.path(label)
    type(p) == 'path' or fail(type(p))
    for line in ctx.read(p).split('\n'):
        if line.startswith('go '):
            return line[3:].strip()
    fail('no go directive in %s' % p)
def _impl(ctx):
    seen = []
    for mod in ctx.modules:
        for tag in mod.tags.download:
            fields = struct(**{k: getattr(tag, k) for k in dir(tag)})
            seen.append(mod.name + ':download:' + fields.version + ':' + str(ctx.facts.get('versions') == None))
        for tag in mod.tags.from_file:
            seen.append(mod.name + ':from_file:' + _version(ctx, tag.go_mod))
    sdk_repo(name='go_toolchains', text='\n'.join(seen) + '\n')
    return ctx.extension_metadata(reproducible=True, facts={'versions': seen, 'schema': 1})
go_sdk = module_extension(_impl, tag_classes={'download': tag_class({'version': attr.string()}), 'from_file': tag_class({'go_mod': attr.label()})})
"#);
        f
    }
    fn file(&self, path: &str, contents: impl AsRef<[u8]>) {
        fs::write(self.0.path().join(path), contents).unwrap();
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new("/usr/bin/timeout")
            .args([
                "--kill-after=2s",
                "30s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
            ])
            .args(args)
            .current_dir(self.0.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("TMPDIR", self.0.path().join("tmp"))
            .env("XDG_CONFIG_HOME", self.0.path().join("xdg"))
            .env("LANG", "C")
            .output()
            .unwrap()
    }
    fn build(&self) -> Vec<u8> {
        let result = self.run(&["build", "//:copy"]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let output = String::from_utf8(result.stdout).unwrap();
        let (_, path) = output.lines().next().unwrap().split_once('\t').unwrap();
        fs::read(path).unwrap()
    }
    fn roots(&self) -> Vec<PathBuf> {
        let mut roots = fs::read_dir(self.0.path().join("cache/repositories/direct/trees"))
            .map(|entries| {
                entries
                    .map(|e| e.unwrap().path())
                    .filter(|p| !p.file_name().unwrap().to_string_lossy().starts_with('.'))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        roots.sort();
        roots
    }
}

#[test]
fn go_sdk_shaped_extension_reads_go_mod_labels_and_rekeys_on_source_bytes() {
    let f = Fixture::new();
    assert_eq!(f.build(), EXPECTED);
    let first = f.roots();
    assert_eq!(first.len(), 1);
    assert_eq!(fs::read(first[0].join("data.txt")).unwrap(), EXPECTED);
    // Unchanged inputs: identical generated-repository identity.
    assert_eq!(f.build(), EXPECTED);
    assert_eq!(f.roots(), first);
    // Same parsed version, different go.mod bytes: the read_source digest
    // alone rekeys the extension even though every declaration is identical.
    f.file("go.mod", format!("{ROOT_GO_MOD}// comment\n"));
    assert_eq!(f.build(), EXPECTED);
    let second = f.roots();
    assert_eq!(second.len(), 2);
    assert!(second.contains(&first[0]));
    // A changed dependency go.mod changes both identity and consumer bytes.
    f.file("dep/go.mod", DEP_GO_MOD.replace("1.25.0", "1.26.2"));
    assert_eq!(
        f.build(),
        b"main:download:1.27.1:True\nmain:from_file:1.27.1\ngorules:from_file:1.26.2\n"
    );
    let third = f.roots();
    assert_eq!(third.len(), 3);
    // Restoring the original bytes restores the original identity.
    f.file("go.mod", ROOT_GO_MOD);
    f.file("dep/go.mod", DEP_GO_MOD);
    assert_eq!(f.build(), EXPECTED);
    assert_eq!(f.roots(), third);
    assert_eq!(fs::read_dir(f.0.path().join("tmp")).unwrap().count(), 0);
}

#[test]
fn source_label_failures_are_explicit_and_publish_nothing() {
    for (body, marker) in [
        (
            "ctx.path(Label('//nopkg:go.mod'))",
            "Unable to load package for @@gorules+//nopkg:go.mod: ",
        ),
        (
            "ctx.read(Label('//:go.mod'), watch='sometimes')",
            "bad value for 'watch' parameter; want 'yes', 'no', or 'auto', got sometimes",
        ),
        (
            "ctx.read(Label('//:missing.mod'))",
            "missing.mod (No such file or directory)",
        ),
        (
            "ctx.file('/tmp/opencode/n43-never-written', '')",
            "Cannot write outside of the repository directory for path /tmp/opencode/n43-never-written",
        ),
        (
            "return struct()",
            "expected module extension @@gorules+//:ext.bzl%go_sdk to return None or extension_metadata, got struct",
        ),
    ] {
        let f = Fixture::new();
        f.file("dep/ext.bzl", format!("load(':rules.bzl', 'sdk_repo')\ndef _impl(ctx):\n    sdk_repo(name='go_toolchains', text='never')\n    {body}\ngo_sdk = module_extension(_impl, tag_classes={{'download': tag_class({{'version': attr.string()}}), 'from_file': tag_class({{'go_mod': attr.label()}})}})\n"));
        let result = f.run(&["build", "//:copy"]);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success(), "{body}");
        assert_ne!(result.status.code(), Some(124));
        assert!(!stderr.contains("panicked"), "{stderr}");
        assert!(stderr.contains(marker), "{marker}\n{stderr}");
        assert!(f.roots().is_empty());
    }
}
