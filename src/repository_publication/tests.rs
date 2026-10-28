use super::*;
use crate::observed_fs::{self, Observation, Op, Recording, StatKey};
use crate::repository::RepositorySet;
use crate::snapshot::validate::Environment;
use std::fs;

pub(crate) struct Fixture {
    pub temp: tempfile::TempDir,
    pub root: PathBuf,
    pub store: PathBuf,
    pub a: Recording,
}

impl Fixture {
    pub fn new() -> Self {
        let temp = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let root = temp.path().join("ws");
        let store = temp.path().join("store");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("MODULE.bazel"), "module(name='test')\nr=use_repo_rule('//:repo.bzl','maker')\nr(name='r',src='//:input')\n").unwrap();
        fs::write(root.join("BUILD.bazel"), "").unwrap();
        fs::write(root.join("repo.bzl"), "def impl(ctx):\n    text=ctx.read(ctx.attr.src, watch='yes')\n    ctx.file('BUILD.bazel', \"exports_files(['data'])\")\n    ctx.file('data', text)\nmaker=repository_rule(implementation=impl,attrs={'src':attr.label()})\n").unwrap();
        let mut fixture = Self {
            temp,
            root,
            store,
            a: Recording::default(),
        };
        fixture.a = fixture.publish("A");
        assert_eq!(fixture.a.selections.len(), 1);
        assert!(fixture.a.taints.is_empty(), "{:?}", fixture.a.taints);
        fixture.publish("B");
        fs::write(fixture.root.join("input"), "A").unwrap();
        fixture
    }

    pub fn repositories(&self) -> Result<RepositorySet, String> {
        Ok(RepositorySet::new(self.root.clone()).with_source_cache(self.store.clone()))
    }

    fn publish(&self, input: &str) -> Recording {
        fs::write(self.root.join("input"), input).unwrap();
        let window = observed_fs::tests_support::record_this_thread();
        let repositories = self.repositories().unwrap();
        repositories.validate_root_module().unwrap();
        let label = repositories.parse("@r//:data", "", "").unwrap();
        repositories.root(&label.repository).unwrap();
        window.recorder.take()
    }

    pub fn receipt(&self) -> &Receipt {
        &self.a.selections[0]
    }
    pub fn state(&self) -> (Vec<u8>, StatKey, PathBuf) {
        let pointer = self.receipt().pointer(&self.store);
        (
            fs::read(&pointer).unwrap(),
            StatKey::of(&fs::metadata(pointer).unwrap()),
            fs::read_link(crate::repository_slots::slot_path(
                &self.store,
                &self.receipt().canonical,
            ))
            .unwrap(),
        )
    }

    fn prepare(&self) -> (Prepared, Recording) {
        let mut recording = self.a.clone();
        let prepared = Prepared::prepare(
            &|| self.repositories(),
            &mut recording,
            &Environment::default(),
        )
        .unwrap();
        (prepared, recording)
    }
}

#[test]
fn private_proof_records_current_inputs_without_touching_the_calling_window() {
    let f = Fixture::new();
    let state = f.state();
    let window = observed_fs::tests_support::record_this_thread();
    let (prepared, _) = f.prepare();
    assert!(
        prepared
            .proof_inputs
            .observations
            .iter()
            .any(|(p, _)| p == &f.root.join("MODULE.bazel"))
    );
    assert!(
        prepared
            .proof_inputs
            .observations
            .iter()
            .any(|(p, _)| p == &f.root.join("repo.bzl"))
    );
    assert!(
        prepared
            .proof_inputs
            .observations
            .iter()
            .any(|(p, _)| p == &f.root.join("input"))
    );
    assert!(window.recorder.snapshot().observations.is_empty());
    assert!(window.recorder.snapshot().selections.is_empty());
    assert_eq!(f.state(), state);
}

#[test]
fn same_path_ordinary_read_is_not_owned_by_selection() {
    let f = Fixture::new();
    let state = f.state();
    let mut recording = f.a.clone();
    let (path, selection) = recording
        .observations
        .iter()
        .find(|(_, obs)| matches!(obs.op, Op::SelectionRead(_)))
        .unwrap()
        .clone();
    recording.observations.push((
        path,
        Observation {
            op: Op::Read,
            outcome: selection.outcome,
        },
    ));
    assert!(
        Prepared::prepare(
            &|| f.repositories(),
            &mut recording,
            &Environment::default()
        )
        .is_err()
    );
    assert_eq!(f.state(), state);
}

#[test]
fn changed_input_or_mutable_marker_between_prepare_and_commit_does_not_adopt() {
    let f = Fixture::new();
    let state = f.state();
    let (prepared, recording) = f.prepare();
    fs::write(f.root.join("input"), "third unsaved intent").unwrap();
    assert!(
        prepared
            .commit(&|| f.repositories(), &recording, &Environment::default())
            .is_err()
    );
    assert_eq!(f.state(), state);
    fs::write(f.root.join("input"), "A").unwrap();
    let (prepared, recording) = f.prepare();
    let writer =
        crate::repository_slots::SlotWriter::acquire(&f.store, &f.receipt().canonical).unwrap();
    writer.mark(&f.receipt().publication).unwrap();
    drop(writer);
    assert!(
        prepared
            .commit(&|| f.repositories(), &recording, &Environment::default())
            .is_err()
    );
    assert_eq!(f.state(), state);
}

#[test]
fn busy_publisher_and_evicted_publication_are_precommit_misses() {
    let f = Fixture::new();
    let state = f.state();
    let (prepared, recording) = f.prepare();
    let writer =
        crate::repository_slots::SlotWriter::acquire(&f.store, &f.receipt().canonical).unwrap();
    assert!(
        prepared
            .commit(&|| f.repositories(), &recording, &Environment::default())
            .is_err()
    );
    assert_eq!(f.state(), state);
    drop(writer);
    let (prepared, recording) = f.prepare();
    fs::remove_dir_all(f.receipt().root(&f.store)).unwrap();
    assert!(
        prepared
            .commit(&|| f.repositories(), &recording, &Environment::default())
            .is_err()
    );
    assert_eq!(f.state(), state);
}

#[test]
fn newly_reusable_current_winner_rejects_the_prepared_historical_candidate() {
    let f = Fixture::new();
    let (prepared, recording) = f.prepare();
    let path = f
        .store
        .join("direct/manifests")
        .join(format!("{}.json", f.receipt().publication));
    let manifest: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let recorded: Vec<u8> = serde_json::from_value(manifest["recorded"].clone()).unwrap();
    // A legal different publication of this spec and these runtime probes
    // (a producer can have nondeterministic output) wins the current pointer.
    crate::repository_store::materialize_keyed(
        &f.store,
        &f.receipt().canonical,
        &f.receipt().spec,
        b"another observed execution",
        |_, _| Ok(false),
        |stage| {
            fs::write(stage.join("data"), "different winner").unwrap();
            Ok(crate::repository_store::Executed {
                inputs: b"runtime".to_vec(),
                recorded,
            })
        },
    )
    .unwrap();
    let current = f.state();
    let error = prepared
        .commit(&|| f.repositories(), &recording, &Environment::default())
        .unwrap_err();
    assert!(!error.starts_with(COMMIT_ERROR), "{error}");
    assert_eq!(f.state(), current);
}

#[test]
fn malformed_missing_pointer_and_tampered_manifest_never_adopt() {
    for mutation in ["malformed", "missing", "manifest"] {
        let f = Fixture::new();
        let (prepared, recording) = f.prepare();
        let pointer = f.receipt().pointer(&f.store);
        let link = f.state().2;
        match mutation {
            "malformed" => fs::write(&pointer, b"bad pointer").unwrap(),
            "missing" => fs::remove_file(&pointer).unwrap(),
            _ => fs::write(
                f.store
                    .join("direct/manifests")
                    .join(format!("{}.json", f.receipt().publication)),
                b"bad manifest",
            )
            .unwrap(),
        }
        let bytes = fs::read(&pointer).ok();
        assert!(
            prepared
                .commit(&|| f.repositories(), &recording, &Environment::default())
                .is_err()
        );
        assert_eq!(fs::read(&pointer).ok(), bytes);
        assert_eq!(
            fs::read_link(crate::repository_slots::slot_path(
                &f.store,
                &f.receipt().canonical
            ))
            .unwrap(),
            link
        );
    }
}

#[test]
fn adoption_excludes_normal_binders_and_partial_commit_is_terminal_then_recoverable() {
    let f = Fixture::new();
    let before = f.state();
    let (prepared, recording) = f.prepare();
    crate::repository_store::FAIL_ADOPTION_BIND.with(|flag| flag.set(true));
    let error = prepared
        .commit(&|| f.repositories(), &recording, &Environment::default())
        .unwrap_err();
    assert!(error.starts_with(COMMIT_ERROR), "{error}");
    assert_ne!(f.state().0, before.0);
    assert_eq!(f.state().2, before.2);
    assert!(
        needs_view_adoption(&f.a),
        "partial pointer/view commit must not use strict fast path"
    );
    let (prepared, recording) = f.prepare();
    prepared
        .commit(&|| f.repositories(), &recording, &Environment::default())
        .unwrap();
    assert!(!needs_view_adoption(&f.a));
    let accepted = f.state();
    let guard = crate::repository_store::AdoptionGuard::acquire(&f.store, &f.a.selections).unwrap();
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                crate::repository_slots::bind(&f.store, &f.receipt().canonical, &"b".repeat(64))
            })
            .join()
            .unwrap()
            .unwrap();
    });
    assert_eq!(f.state(), accepted);
    drop(guard);
}
