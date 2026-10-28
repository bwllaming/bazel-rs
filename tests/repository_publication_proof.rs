//! The completed tiny watched-ignore baseline, with owned execution counters.
//! Pure probes leave public views alone; accepted historical CLI hits must
//! install their exact pointer and external view without executing generators.
#![allow(clippy::disallowed_methods)]
include!("support/temp_root.rs");

use bazel_rs::module_graph::ResolverOptions;
use bazel_rs::registry_session::{RegistrySession, SessionHandle};
use bazel_rs::repository::RepositorySet;
use bazel_rs::repository_publication::Receipt;
use bazel_rs::snapshot::format::{self, Header, Section};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    cache: PathBuf,
    counter: PathBuf,
    extension: bool,
}

impl Fixture {
    fn new(extension: bool, changing: bool) -> Self {
        let temp = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let root = temp.path().join("ws");
        fs::create_dir(&root).unwrap();
        let f = Self {
            root,
            cache: temp.path().join("cache"),
            counter: temp.path().join("executions"),
            _temp: temp,
            extension,
        };
        f.write("MODULE.bazel", &if extension {
            "module(name='publication_baseline')\ne = use_extension('//:repo.bzl', 'gen')\nuse_repo(e, 'constant_repo')\n".to_owned()
        } else {
            "module(name='publication_baseline')\nr = use_repo_rule('//:repo.bzl', 'maker')\nr(name='constant_repo', src='//:.bazelignore')\n".to_owned()
        });
        f.write("BUILD.bazel", "platform(name='platform')\nexports_files(['.bazelignore'])\nfilegroup(name='top', srcs=['@constant_repo//:data'])\n");
        f.write(".bazelignore", "unused-a\n");
        f.write("MODULE.bazel.lock", "{\"lockFileVersion\":28}\n");
        f.write("extension-input", "one");
        f.write("helper.bzl", "CONSTANT = 'constant data\\n'\n");
        let counter = serde_json::to_string(f.counter.to_str().unwrap()).unwrap();
        let data = if changing { "watched" } else { "CONSTANT" };
        f.write("repo.bzl", &format!(r#"load(":helper.bzl", "CONSTANT")
def _make(ctx):
    watched = ctx.read(ctx.attr.src, watch = 'yes')
    ctx.getenv('WATCHED')
    result = ctx.execute(['/bin/sh', '-c', 'echo run >> "$1"', 'counter', {counter}])
    if result.return_code:
        fail(result.stderr)
    ctx.file('BUILD.bazel', "exports_files(['data'])\n")
    ctx.file('data', {data})
maker = repository_rule(implementation = _make, attrs = {{'src': attr.label(allow_single_file = True)}}, environ = ['DECLARED'])
def _ext(ctx):
    ctx.read(Label('//:extension-input'))
    maker(name = 'constant_repo', src = '//:.bazelignore')
gen = module_extension(implementation = _ext)
"#));
        f
    }

    fn write(&self, name: &str, text: &str) {
        fs::write(self.root.join(name), text).unwrap();
    }

    fn query(&self) -> String {
        self.query_full().0
    }

    fn query_full(&self) -> (String, Vec<u8>) {
        let out = Command::new(env!("CARGO_BIN_EXE_bazel-rs"))
            .current_dir(&self.root)
            .env("BAZEL_RS_SNAPSHOT", "on")
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .env("DECLARED", "one")
            .env("WATCHED", "one")
            .args([
                "--ignore_all_rc_files",
                &format!("--cache-dir={}", self.cache.display()),
                &format!("--output_base={}", self.cache.join("output-base").display()),
                "cquery",
                "--experimental-builtin-modules=false",
                "--repository_disable_download",
                "--platforms=//:platform",
                "--host_platform=//:platform",
                "--output=proto",
                "deps(//:top)",
            ])
            .output()
            .unwrap();
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(out.status.success(), "{stderr}");
        (stderr, out.stdout)
    }

    fn prime(&self) -> Receipt {
        let first = self.query();
        if !first.contains("bazel-rs-cquery-snapshot: saved") {
            assert!(self.extension && first.contains("lock"), "{first}");
            assert!(self.query().contains("bazel-rs-cquery-snapshot: saved"));
        }
        assert!(self.query().contains("bazel-rs-cquery-snapshot: hit"));
        let mut snapshots = Vec::new();
        snapshots_in(&self.cache, &mut snapshots);
        for path in snapshots {
            let bytes = fs::read(path).unwrap();
            let header = Header::decode(&bytes, bytes.len() as u64).unwrap();
            let section = header.slot(Section::Inputs).unwrap();
            let inputs = format::decode_inputs(
                &bytes[section.offset as usize..(section.offset + section.len) as usize],
            )
            .unwrap();
            if let Some(receipt) = inputs.recording.selections.into_iter().find(|receipt| {
                if !receipt.canonical.ends_with("+constant_repo") {
                    return false;
                }
                let pointer: serde_json::Value =
                    serde_json::from_slice(&fs::read(self.pointer(receipt)).unwrap()).unwrap();
                pointer["identity"] == receipt.publication
            }) {
                // The guard is explicitly selection-owned, never a generic
                // exemption for all READs of this path.
                assert!(
                    inputs
                        .recording
                        .observations
                        .iter()
                        .any(|(path, observation)| path == &self.pointer(&receipt)
                            && observation.op
                                == bazel_rs::observed_fs::Op::SelectionRead(
                                    receipt.canonical.clone()
                                ))
                );
                return receipt;
            }
        }
        panic!("no persisted typed receipt for constant_repo");
    }

    fn repositories(&self) -> RepositorySet {
        let session = RegistrySession::open(
            &self.root,
            bazel_rs::lockfile::LockfileMode::Update,
            &bazel_rs::registry_session::LockfileTarget::Workspace,
            None,
            Default::default(),
        )
        .unwrap();
        let mut environment: Vec<_> = std::env::vars_os()
            .filter(|(key, _)| key != "DECLARED" && key != "WATCHED")
            .collect();
        environment.extend([
            ("DECLARED".into(), "one".into()),
            ("WATCHED".into(), "one".into()),
        ]);
        RepositorySet::with_options(
            self.root.clone(),
            ResolverOptions {
                registry_session: SessionHandle::new(session),
                ..Default::default()
            },
        )
        .with_source_cache(self.cache.join("repositories"))
        .with_repository_environment(environment)
    }

    fn pointer(&self, receipt: &Receipt) -> PathBuf {
        self.cache
            .join("repositories/direct/specs")
            .join(&receipt.spec)
    }

    fn executions(&self) -> usize {
        fs::read_to_string(&self.counter).unwrap().lines().count()
    }
}

fn snapshots_in(path: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            snapshots_in(&entry.path(), out);
        } else if entry.path().extension().is_some_and(|ext| ext == "snap") {
            out.push(entry.path());
        }
    }
}

fn stamp(path: &Path) -> (Vec<u8>, u64, i64, i64, i64, i64) {
    let meta = fs::metadata(path).unwrap();
    (
        fs::read(path).unwrap(),
        meta.ino(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

#[test]
fn current_spec_and_runtime_proofs_preserve_pointer_and_view_without_reexecution() {
    for extension in [false, true] {
        for changing in [false, true] {
            let f = Fixture::new(extension, changing);
            let a = f.prime();
            f.write(".bazelignore", "unused-b\n");
            let b = f.prime();
            assert_eq!(a.spec, b.spec);
            assert_ne!(a.publication, b.publication);
            assert_eq!(f.executions(), 2);
            let pointer = stamp(&f.pointer(&b));
            let slot = f.cache.join("repositories/external").join(&b.canonical);
            let link = fs::read_link(&slot).unwrap();
            let repositories = f.repositories();
            for (intent, receipt) in [
                ("unused-a\n", &a),
                ("unused-b\n", &b),
                ("unused-a\n", &a),
                ("unused-b\n", &b),
            ] {
                f.write(".bazelignore", intent);
                let proved = repositories
                    .prove_publications(std::slice::from_ref(receipt))
                    .expect("current recipe and live inputs prove candidate");
                assert_eq!(proved[0].receipt(), receipt);
                assert_eq!(stamp(&f.pointer(&b)), pointer);
                assert_eq!(fs::read_link(&slot).unwrap(), link);
                assert_eq!(
                    fs::read_to_string(slot.join("data")).unwrap(),
                    if changing {
                        "unused-b\n"
                    } else {
                        "constant data\n"
                    }
                );
                assert_eq!(f.executions(), 2);
            }
            f.write(".bazelignore", "third-unsaved\n");
            assert!(
                repositories
                    .prove_publications(std::slice::from_ref(&a))
                    .is_none()
            );
            assert!(
                repositories
                    .prove_publications(std::slice::from_ref(&b))
                    .is_none()
            );
            assert_eq!(f.executions(), 2);
        }
    }
}

#[test]
fn accepted_historical_cli_hits_adopt_exact_pointer_and_external_bytes() {
    for extension in [false, true] {
        for changing in [false, true] {
            let f = Fixture::new(extension, changing);
            let a = f.prime();
            let (_, graph_a) = f.query_full();
            f.write(".bazelignore", "unused-b\n");
            let b = f.prime();
            let (_, graph_b) = f.query_full();
            assert_eq!(a.spec, b.spec);
            assert_ne!(a.publication, b.publication);
            let slot = f.cache.join("repositories/external").join(&a.canonical);
            for (intent, receipt, graph) in [
                ("unused-a\n", &a, &graph_a),
                ("unused-b\n", &b, &graph_b),
                ("unused-a\n", &a, &graph_a),
                ("unused-b\n", &b, &graph_b),
            ] {
                f.write(".bazelignore", intent);
                let (stderr, actual) = f.query_full();
                assert!(stderr.contains("bazel-rs-cquery-snapshot: hit"), "{stderr}");
                assert!(
                    !stderr.contains("bazel-rs-cquery-snapshot: saved"),
                    "{stderr}"
                );
                assert_eq!(&actual, graph);
                let pointer: serde_json::Value =
                    serde_json::from_slice(&fs::read(f.pointer(receipt)).unwrap()).unwrap();
                assert_eq!(pointer["identity"], receipt.publication);
                assert_eq!(
                    fs::read_link(&slot).unwrap(),
                    bazel_rs::repository_slots::link_target(&receipt.publication)
                );
                assert_eq!(
                    fs::read_to_string(slot.join("data")).unwrap(),
                    if changing { intent } else { "constant data\n" }
                );
                assert_eq!(f.executions(), 2);
            }
        }
    }
}

#[test]
fn stale_prepared_sets_cannot_certify_changed_current_recipes_or_extension_locks() {
    for extension in [false, true] {
        let f = Fixture::new(extension, false);
        let receipt = f.prime();
        let repositories = f.repositories();
        let probe = || repositories.prove_publications(std::slice::from_ref(&receipt));
        assert!(probe().is_some());
        let source = fs::read_to_string(f.root.join("repo.bzl")).unwrap();
        let module = fs::read_to_string(f.root.join("MODULE.bazel")).unwrap();
        let lock = fs::read(f.root.join("MODULE.bazel.lock")).unwrap();
        let pointer = stamp(&f.pointer(&receipt));
        for (file, changed) in [
            (
                "repo.bzl",
                source.replace("allow_single_file = True", "allow_single_file = False"),
            ),
            (
                "repo.bzl",
                source.replace("attrs =", "local = True, attrs ="),
            ),
            ("helper.bzl", "CONSTANT = 'changed'\n".into()),
            (
                "MODULE.bazel",
                module.replace("constant_repo", "other_repo"),
            ),
            (
                "MODULE.bazel",
                format!("{module}\nbazel_dep(name='unavailable', version='1.0')\n"),
            ),
            (
                "repo.bzl",
                format!("print('must remain private')\n{source}"),
            ),
        ] {
            let original = fs::read(f.root.join(file)).unwrap();
            f.write(file, &changed);
            assert!(
                probe().is_none(),
                "{file}: changed current intent must miss"
            );
            fs::write(f.root.join(file), original).unwrap();
            assert!(probe().is_some(), "restored {file}");
        }
        for name in ["DECLARED", "WATCHED"] {
            let changed = f.repositories().with_repository_environment([
                (
                    "DECLARED".into(),
                    if name == "DECLARED" { "two" } else { "one" }.into(),
                ),
                (
                    "WATCHED".into(),
                    if name == "WATCHED" { "two" } else { "one" }.into(),
                ),
            ]);
            assert!(
                changed
                    .prove_publications(std::slice::from_ref(&receipt))
                    .is_none()
            );
        }
        if extension {
            f.write("extension-input", "third unsaved extension intent");
            assert!(
                probe().is_none(),
                "valid old lock bytes do not certify today's extension inputs"
            );
            f.write("extension-input", "one");
            f.write("MODULE.bazel.lock", r#"{"lockFileVersion":28}"#);
            assert!(
                probe().is_none(),
                "must reopen the lock, not inherit an old session's entry"
            );
            fs::write(f.root.join("MODULE.bazel.lock"), &lock).unwrap();
        } else {
            f.write(
                "MODULE.bazel",
                &module.replace("src='//:.bazelignore'", "src='//:extension-input'"),
            );
            assert!(probe().is_none());
            f.write("MODULE.bazel", &module);
        }
        assert_eq!(stamp(&f.pointer(&receipt)), pointer);
        assert_eq!(f.executions(), 1, "no rejected proof can run a generator");
        assert_eq!(fs::read(f.root.join("MODULE.bazel.lock")).unwrap(), lock);
    }
}

#[test]
fn retained_generated_definition_is_not_a_current_producer_proof() {
    let f = Fixture::new(false, false);
    let producer_count = f._temp.path().join("producer-executions");
    let counter = serde_json::to_string(producer_count.to_str().unwrap()).unwrap();
    f.write("provider.bzl", &format!(r#"
def _provide(ctx):
    ctx.execute(['/bin/sh', '-c', 'echo run >> "$1"', 'counter', {counter}])
    ctx.template('repo.bzl', ctx.attr.source)
    ctx.template('helper.bzl', ctx.attr.helper)
    ctx.file('BUILD.bazel', "exports_files(['repo.bzl', 'helper.bzl'])\n")
provider = repository_rule(implementation = _provide, attrs = {{'source': attr.label(), 'helper': attr.label(), 'revision': attr.string()}})
"#));
    let module = "module(name='publication_baseline')\np = use_repo_rule('//:provider.bzl', 'provider')\np(name='defs', source='//:repo.bzl', helper='//:helper.bzl', revision='A')\nr = use_repo_rule('@defs//:repo.bzl', 'maker')\nr(name='constant_repo', src='//:.bazelignore')\n";
    f.write("MODULE.bazel", module);
    let receipt = f.prime();
    let repositories = f.repositories();
    // This closure is explicitly unsupported even before the producer changes.
    assert!(
        repositories
            .prove_publications(std::slice::from_ref(&receipt))
            .is_none()
    );
    let trees = f.cache.join("repositories/direct/trees");
    let retained: Vec<_> = fs::read_dir(&trees)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(retained.iter().any(|tree| tree.join("repo.bzl").is_file()));
    f.write(
        "MODULE.bazel",
        &module.replace("revision='A'", "revision='B'"),
    );
    assert!(
        repositories
            .prove_publications(std::slice::from_ref(&receipt))
            .is_none()
    );
    assert_eq!(fs::read_to_string(producer_count).unwrap(), "run\n");
    assert_eq!(f.executions(), 1);
    assert_eq!(fs::read_dir(trees).unwrap().count(), retained.len());
    assert!(retained.iter().all(|tree| tree.is_dir()));
}
