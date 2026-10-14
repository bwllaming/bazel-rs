//! Bounded resolved-Label accessor contracts, not full Bazel Label API parity.
//! NATIVE_BOUNDARY negatives preserve eager rejection: no invalid Label is
//! constructed and no getter is reached. All fixtures are offline/local.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let f = Self(
            tempfile::Builder::new()
                .prefix("label-repo-name-cli-")
                .tempdir()
                .unwrap(),
        );
        for directory in ["home", "tmp"] {
            fs::create_dir(f.0.path().join(directory)).unwrap();
        }
        f.file("MODULE.bazel", ROOT_MODULE);
        f.file("dep/MODULE.bazel", "module(name='dep',version='1',repo_name='self_dep')\nbazel_dep(name='other',version='1',repo_name='shared')\n");
        f.file("other/MODULE.bazel", "module(name='other',version='1')\n");
        f.file("dep/BUILD.bazel", "exports_files(['dep.txt'])\n");
        f.file("other/BUILD.bazel", "exports_files(['other.txt'])\n");
        f.file("dep/dep.txt", "DEP\n");
        f.file("other/other.txt", "OTHER\n");
        f.file("root.txt", "ROOT\n");
        f
    }

    fn file(&self, name: &str, contents: impl AsRef<[u8]>) {
        let path = self.0.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        // Cargo supplies this worktree's freshly built executable, never a
        // reference binary or a release binary from another checkout.
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_bazel-rs"));
        assert!(binary.is_file());
        let output = Command::new("/usr/bin/timeout")
            .args(["--kill-after=5s", "60s"])
            .arg(binary)
            .args(["--ignore_all_rc_files", "--cache-dir=cache"])
            .args(args)
            .current_dir(self.0.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.path().join("home"))
            .env("TMPDIR", self.0.path().join("tmp"))
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            output.status.code().is_some(),
            "signal: {}",
            errors(&output)
        );
        assert!(
            !matches!(output.status.code(), Some(124 | 137)),
            "timeout: {}",
            errors(&output)
        );
        assert!(!errors(&output).contains("panicked"), "{}", errors(&output));
        output
    }

    fn probe(&self, source: &str) {
        let (loads, body): (Vec<_>, Vec<_>) =
            source.lines().partition(|line| line.starts_with("load("));
        self.file("defs.bzl", format!("{}\n{SURFACE}\n{}\ndef probe(name):\n    native.filegroup(name=name,srcs=['root.txt'])\n", loads.join("\n"), body.join("\n")));
        self.file(
            "BUILD.bazel",
            "load(':defs.bzl','probe')\nprobe(name='probe')\n",
        );
    }

    fn query(&self) -> Output {
        let output = self.run(&["query", "//:probe"]);
        assert!(output.status.success(), "{}", errors(&output));
        assert_eq!(output.stdout, b"//:probe\n");
        output
    }

    fn reject(&self, marker: &str) -> String {
        let output = self.run(&["query", "//:probe"]);
        let diagnostic = errors(&output);
        assert_eq!(output.status.code(), Some(1), "{diagnostic}");
        assert!(output.stdout.is_empty(), "partial success");
        assert!(diagnostic.contains(marker), "{marker:?}: {diagnostic}");
        for unexpected in ["Variable `Label` not found", "Variable `print` not found"] {
            assert!(!diagnostic.contains(unexpected), "{diagnostic}");
        }
        for (marker, expected) in [("BEFORE_OPERATION", 1), ("AFTER_OPERATION", 0)] {
            assert_eq!(
                diagnostic.lines().filter(|line| *line == marker).count(),
                expected,
                "{diagnostic}"
            );
        }
        assert!(!self.0.path().join("cache/actions").exists());
        diagnostic
    }

    fn build(&self, target: &str, expected_label: &str, bytes: &[u8]) -> (PathBuf, String) {
        let output = self.run(&["build", target]);
        assert!(output.status.success(), "{}", errors(&output));
        let stdout = String::from_utf8_lossy(&output.stdout);
        let rows: Vec<_> = stdout.lines().collect();
        assert_eq!(rows.len(), 1, "{stdout}");
        let (label, path) = rows[0].split_once('\t').unwrap();
        assert_eq!(label, expected_label);
        let path = PathBuf::from(path);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        (path, errors(&output))
    }

    fn extension(&self) {
        self.file("MODULE.bazel", format!("{ROOT_MODULE}\ne=use_extension('@shared//:ext.bzl','observe')\ne.input(id='root-local',value='//:root.txt')\ne.input(id='root-shared',value='@shared//:dep.txt')\ne.input(id='root-self',value='@root_self//:root.txt')\ne.input(id='root-default')\nuse_repo(e,made_alias='made')\n"));
        self.file("dep/MODULE.bazel", "module(name='dep',version='1',repo_name='self_dep')\nbazel_dep(name='other',version='1',repo_name='shared')\ne=use_extension('//:ext.bzl','observe')\ne.input(id='dep-local',value='//:dep.txt')\ne.input(id='dep-shared',value='@shared//:other.txt')\ne.input(id='dep-self',value='@self_dep//:dep.txt')\ne.input(id='dep-default')\n");
        self.file("dep/tags.bzl", "CAPTURED = Label('@shared//:other.txt')\ninput = tag_class(attrs={'id':attr.string(), 'value':attr.label(default=CAPTURED)})\n");
        self.file("dep/ext.bzl", EXTENSION);
        self.file("BUILD.bazel", "genrule(name='copy',srcs=['@made_alias//:data.txt'],outs=['copy.txt'],cmd='cat $(SRCS) > $@')\n");
    }
}

fn errors(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// Snapshot only this fixture's private publication paths and bytes. Callback
// diagnostics are not a proxy for publication reuse in the native profile.
fn inventory(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, at: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            assert!(!kind.is_symlink(), "unexpected private-cache symlink");
            if kind.is_dir() {
                visit(root, &entry.path(), out);
            } else {
                assert!(kind.is_file());
                out.insert(
                    entry.path().strip_prefix(root).unwrap().to_owned(),
                    fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

const ROOT_MODULE: &str = "module(name='root_contract',repo_name='root_self')\nbazel_dep(name='dep',version='1',repo_name='shared')\nbazel_dep(name='other',version='1',repo_name='other_root')\nlocal_path_override(module_name='dep',path='dep')\nlocal_path_override(module_name='other',path='other')\n";

// Check membership, not equality with the genuine reference's wider dir API.
const SURFACE: &str = r#"
def check(label, expected):
    if type(label) != "Label" or type(label.repo_name) != "string":
        fail("getter type")
    if label.repo_name != expected or label.workspace_name != expected:
        fail("canonical repository", label.repo_name, expected)
    if getattr(label, "repo_name", "DEFAULT") != expected:
        fail("getattr including empty main")
    if not hasattr(label, "repo_name") or "repo_name" not in dir(label):
        fail("accessor discovery")
    if getattr(label, "missing_field", "DEFAULT") != "DEFAULT" or hasattr(label, "missing_field"):
        fail("unknown field fallback")
    return label.repo_name
"#;

const EXTENSION: &str = r#"
load(':tags.bzl', 'input', 'CAPTURED')
def _repo(ctx):
    print('REPO_CALLBACK')
    ctx.file('BUILD.bazel', "exports_files(['data.txt', 'generated.bzl'])\n")
    ctx.file('data.txt', ctx.attr.text)
    ctx.file('generated.bzl', "VALUE = Label('//:data.txt')\n")
maker = repository_rule(_repo, attrs={'text':attr.string()})
def _observe(ctx):
    print('EXTENSION_CALLBACK')
    lexical = Label('//:dep.txt')
    if lexical.repo_name != 'dep+' or CAPTURED.repo_name != 'other+':
        fail('definition scope')
    rows = ['lexical=' + lexical.repo_name]
    for mod in ctx.modules:
        if hasattr(mod, 'repo_name') or getattr(mod, 'repo_name', 'ABSENT') != 'ABSENT':
            fail('participant repo_name must remain absent')
        for tag in mod.tags.input:
            label = tag.value
            rebuilt = Label('@@' + label.repo_name + '//' + label.package + ':' + label.name)
            if label != rebuilt or {label: 'identity'}[rebuilt] != 'identity':
                fail('Label equality/hash changed')
            if label.repo_name != label.workspace_name:
                fail('legacy accessor changed')
            rows.append(tag.id + '=' + label.repo_name)
    maker(name='made', text='\n'.join(rows) + '\n')
observe = module_extension(_observe, tag_classes={'input':input})
"#;

const TAG_BYTES: &[u8] = b"lexical=dep+\nroot-local=\nroot-shared=dep+\nroot-self=\nroot-default=other+\ndep-local=dep+\ndep-shared=other+\ndep-self=dep+\ndep-default=other+\n";

#[test]
fn root_aliases_main_self_alias_and_existing_identity() {
    let f = Fixture::new();
    f.probe(
        r#"
def verify():
    root = Label('//:root.txt')
    for spelling in ['//:root.txt', '@@//:root.txt', '@//:root.txt', '@root_self//:root.txt']:
        label = Label(spelling)
        check(label, '')
        if label != root or {root: 7}[label] != 7 or str(label) != '@@//:root.txt':
            fail('main identity')
    dep = Label('@shared//:dep.txt')
    check(dep, 'dep+')
    check(Label('@other_root//:other.txt'), 'other+')
    canonical = Label('@@' + dep.repo_name + '//:dep.txt')
    if canonical != dep or {dep: 9}[canonical] != 9 or str(dep) != '@@dep+//:dep.txt':
        fail('external identity')
verify()
print('ROOT_GETTERS_OK')
"#,
    );
    assert!(errors(&f.query()).contains("ROOT_GETTERS_OK"));
}

#[test]
fn external_lexical_helper_uses_its_owner_mapping_not_the_reader() {
    let f = Fixture::new();
    f.file("dep/helper.bzl", "VALUE = Label('@shared//:other.txt')\ndef labels():\n    return [Label('//:dep.txt'), Label('@self_dep//:dep.txt'), Label('@shared//:other.txt'), Label('@@//:root.txt')]\n");
    f.probe(
        r#"
load('@shared//:helper.bzl', 'labels', 'VALUE')
def verify():
    values = labels()
    for i in range(4):
        check(values[i], ['dep+', 'dep+', 'other+', ''][i])
    check(VALUE, 'other+')
    check(Label('@shared//:dep.txt'), 'dep+')
    if values[0] != values[1] or values[2] != VALUE:
        fail('frozen lexical identity')
verify()
print('LEXICAL_GETTERS_OK')
"#,
    );
    assert!(errors(&f.query()).contains("LEXICAL_GETTERS_OK"));
}

#[test]
fn canonical_nonexistent_repository_needs_no_target_lookup() {
    let f = Fixture::new();
    f.probe(
        r#"
label = Label('@@unfetched+segments+name//no/package:no_target')
check(label, 'unfetched+segments+name')
print('NONEXISTENT_CANONICAL_GETTER_OK')
"#,
    );
    assert!(errors(&f.query()).contains("NONEXISTENT_CANONICAL_GETTER_OK"));
}

#[test]
fn real_extension_tag_owner_defaults_participant_boundary_and_cold_warm_bytes() {
    let f = Fixture::new();
    f.extension();
    let (cold_path, cold) = f.build("//:copy", "//:copy", TAG_BYTES);
    assert!(cold.contains("1 actions executed, 0 cache hits"), "{cold}");
    let store = f.0.path().join("cache/repositories/direct");
    let cold_publication = inventory(&store);
    let manifests: Vec<_> = cold_publication
        .iter()
        .filter(|(path, _)| path.starts_with("manifests"))
        .collect();
    assert_eq!(manifests.len(), 1);
    let (manifest_path, manifest_bytes) = manifests[0];
    let manifest: serde_json::Value = serde_json::from_slice(manifest_bytes).unwrap();
    let identity = manifest["identity"].as_str().unwrap();
    assert_eq!(
        manifest_path.file_stem().unwrap().to_str().unwrap(),
        identity
    );
    assert_eq!(
        cold_publication[&Path::new("trees").join(identity).join("data.txt")],
        TAG_BYTES
    );
    let (warm_path, warm) = f.build("//:copy", "//:copy", TAG_BYTES);
    assert_eq!(
        cold_path, warm_path,
        "unchanged logical values changed action identity"
    );
    assert!(warm.contains("0 actions executed, 1 cache hits"), "{warm}");
    assert_eq!(
        inventory(&store),
        cold_publication,
        "publication identity, manifest, or file inventory changed"
    );
    // NATIVE_PROFILE: fresh CLI processes run callbacks even when publication
    // and consumer action identities are reused. Not Bazel callback parity.
    for diagnostic in [&cold, &warm] {
        for marker in ["EXTENSION_CALLBACK", "REPO_CALLBACK"] {
            assert_eq!(
                diagnostic.lines().filter(|line| *line == marker).count(),
                1,
                "{diagnostic}"
            );
        }
    }
}

#[test]
fn generated_repository_full_canonical_name_survives_loading_and_aliases() {
    let f = Fixture::new();
    f.extension();
    f.probe(
        r#"
load('@made_alias//:generated.bzl', 'VALUE')
check(VALUE, 'dep++observe+made')
check(Label('@made_alias//:data.txt'), 'dep++observe+made')
check(Label('@@dep++observe+made//:data.txt'), 'dep++observe+made')
if_not_equal = {VALUE: 'same'}[Label('@made_alias//:data.txt')]
def verify():
    if if_not_equal != 'same' or str(VALUE) != '@@dep++observe+made//:data.txt':
        fail('generated identity')
verify()
print('GENERATED_GETTER_OK')
"#,
    );
    assert!(errors(&f.query()).contains("GENERATED_GETTER_OK"));
    // Real repository bytes are consumed through both apparent and canonical routes.
    let (alias, _) = f.build(
        "@made_alias//:data.txt",
        "@@dep++observe+made//:data.txt",
        TAG_BYTES,
    );
    let (canonical, _) = f.build(
        "@@dep++observe+made//:data.txt",
        "@@dep++observe+made//:data.txt",
        TAG_BYTES,
    );
    assert_eq!(alias, canonical);
}

#[test]
fn constructed_label_assignment_and_missing_property_fail_at_access() {
    for (operation, diagnostic) in [
        ("label.repo_name = 'changed'", "not supported"),
        ("label.workspace_name = 'changed'", "not supported"),
        ("value = label.missing_field", "missing_field"),
    ] {
        let f = Fixture::new();
        f.probe(&format!("label = Label('//:root.txt')\ncheck(label, '')\nprint('BEFORE_OPERATION')\n{operation}\nprint('AFTER_OPERATION')\n"));
        f.reject(diagnostic);
    }
}

#[test]
fn native_boundary_unknown_apparent_alias_rejects_in_constructor() {
    for external in [false, true] {
        let f = Fixture::new();
        // Deliberately no getter expression: unlike Bazel's deferred invalid
        // Label, native never reaches the post-construction marker.
        let source = "print('BEFORE_OPERATION')\nlabel = Label('@not_visible//:missing')\nprint('AFTER_OPERATION')\n";
        if external {
            f.file("dep/invalid.bzl", format!("{source}\nVALUE = 1\n"));
            f.probe("load('@shared//:invalid.bzl', 'VALUE')\n");
        } else {
            f.probe(source);
        }
        let diagnostic = f.reject("unknown apparent repository @not_visible");
        let owner = if external { "dep+" } else { "" };
        assert!(
            diagnostic.contains(&format!("from canonical repository {owner:?}")),
            "{diagnostic}"
        );
        assert!(
            !diagnostic.contains("not allowed on invalid Label"),
            "{diagnostic}"
        );
    }
}

#[test]
fn native_boundary_external_empty_apparent_alias_rejects_in_constructor() {
    let f = Fixture::new();
    f.file("dep/invalid.bzl", "print('BEFORE_OPERATION')\nlabel = Label('@//:root.txt')\nprint('AFTER_OPERATION')\nVALUE = 1\n");
    f.probe("load('@shared//:invalid.bzl', 'VALUE')\n");
    let diagnostic = f.reject("unknown apparent repository @ from canonical repository \"dep+\"");
    assert!(
        !diagnostic.contains("not allowed on invalid Label"),
        "{diagnostic}"
    );
}
