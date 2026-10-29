//! Non-local publication receipts and guarded configured-graph adoption. A
//! receipt is a hint, never current intent or permission to bind a repository.
//! Whole-candidate acceptance re-proves intent under publisher/binder guards.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The preparation path that computed the specification. The version refers
/// to the specification recipe, not the publication manifest format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Origin {
    DirectV2,
    ExtensionV3,
}

/// The selected pointer's scope. Workspace names retain raw path bytes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Scope {
    Shared,
    Workspace(Vec<u8>),
}

/// Persistable evidence of a successful live selection. Deserialization does
/// not authorize reuse: current preparation, runtime probes, integrity and
/// pointer preference must be proved again before adoption can be considered.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Receipt {
    pub canonical: String,
    pub origin: Origin,
    pub spec: String,
    pub publication: String,
    pub scope: Scope,
    pub manifest_digest: [u8; 32],
}

/// A read-only store probe succeeded. This deliberately has no bind/publish
/// method and is not serializable. It does not prove the specification is
/// current; only RepositorySet's isolated preparation can establish that.
#[derive(Debug)]
pub struct Candidate {
    pub(crate) receipt: Receipt,
}

impl Candidate {
    pub fn receipt(&self) -> &Receipt {
        &self.receipt
    }
}

impl Receipt {
    pub(crate) fn pointer(&self, store: &Path) -> PathBuf {
        let name = match &self.scope {
            Scope::Shared => self.spec.clone(),
            Scope::Workspace(bytes) => crate::repository_store::workspace_pointer(
                &self.spec,
                &crate::host_os::latin1_text(bytes),
            ),
        };
        store.join("direct/specs").join(name)
    }

    pub(crate) fn root(&self, store: &Path) -> PathBuf {
        store.join("direct/trees").join(&self.publication)
    }

    fn owns(&self, store: &Path, path: &Path, op: &crate::observed_fs::Op) -> bool {
        matches!(op, crate::observed_fs::Op::SelectionRead(owner) | crate::observed_fs::Op::SelectionAbsent(owner) if owner == &self.canonical)
            && (path == self.pointer(store) || path == store.join("direct/specs").join(&self.spec))
    }
}

pub(crate) type Factory<'a> = &'a dyn Fn() -> Result<crate::repository::RepositorySet, String>;

/// Partial commit is a command error, never another candidate's miss.
pub(crate) const COMMIT_ERROR: &str = "repository publication adoption failed: ";

/// A strict pointer match alone cannot leave an ordinary view at another
/// publication (for example a different spec last bound this canonical slot).
/// Established mutable-slot snapshots retain their ordinary validation path.
pub(crate) fn needs_view_adoption(recording: &crate::observed_fs::Recording) -> bool {
    recording.selections.iter().any(|receipt| {
        let Some(root) = recording
            .repositories
            .iter()
            .find(|tree| tree.kind == "direct" && tree.name == receipt.spec)
            .map(|tree| &tree.root)
        else {
            return true;
        };
        let Some(store) = root.parent().and_then(Path::parent).and_then(Path::parent) else {
            return true;
        };
        let slot = crate::repository_slots::slot_path(store, &receipt.canonical);
        if std::fs::symlink_metadata(&slot).is_ok_and(|metadata| metadata.is_dir()) {
            return crate::repository_slots::marker(store, &receipt.canonical).as_deref()
                != Some(receipt.publication.as_str());
        }
        std::fs::read_link(slot).ok()
            != Some(crate::repository_slots::link_target(&receipt.publication))
    })
}

/// Candidate-local proof inputs, not authority to mutate public selection. The
/// constructor validates receipt/REPO ownership; commit repeats all checks with
/// every relevant publication and slot locked, after graph decoding finishes.
pub(crate) struct Prepared {
    store: PathBuf,
    receipts: Vec<Receipt>,
    proof_inputs: crate::observed_fs::Recording,
}

fn current(
    factory: Factory<'_>,
    receipts: &[Receipt],
) -> Result<(PathBuf, crate::observed_fs::Recording), String> {
    let (result, inputs) = crate::observed_fs::record_private(|| {
        let repositories = factory()?;
        repositories
            .prove_publications_current(receipts)
            .ok_or("repository publication has no current-intent proof")?;
        Ok::<_, String>(repositories.source_cache_path()?.to_path_buf())
    });
    if !inputs.taints.is_empty() {
        return Err("unstable current publication proof".into());
    }
    Ok((result?, inputs))
}

impl Prepared {
    pub(crate) fn prepare(
        factory: Factory<'_>,
        recording: &mut crate::observed_fs::Recording,
        environment: &crate::snapshot::validate::Environment<'_>,
    ) -> Result<Self, String> {
        if recording.selections.is_empty() {
            return Err("no typed publication selections".into());
        }
        let (store, proof_inputs) = current(factory, &recording.selections)?;
        let mut seen = std::collections::BTreeSet::new();
        for receipt in &recording.selections {
            if !seen.insert(&receipt.canonical)
                || !recording.repositories.iter().any(|tree| {
                    tree.kind == "direct"
                        && tree.name == receipt.spec
                        && tree.root == receipt.root(&store)
                        && tree.manifest.as_ref()
                            == Some(
                                &store
                                    .join("direct/manifests")
                                    .join(format!("{}.json", receipt.publication)),
                            )
                })
                || !recording.observations.iter().any(|(path, observation)| {
                    path == &receipt.pointer(&store)
                        && observation.op
                            == crate::observed_fs::Op::SelectionRead(receipt.canonical.clone())
                })
            {
                return Err(
                    "publication receipt lacks an operation-owned selection and repository record"
                        .into(),
                );
            }
        }
        // Only explicitly owned operations are replaced. An ordinary READ of
        // precisely the same path remains, as do every unrelated operation.
        recording.observations.retain(|(path, observation)| {
            !recording
                .selections
                .iter()
                .any(|receipt| receipt.owns(&store, path, &observation.op))
        });
        crate::snapshot::validate::validate_for_hit(recording, environment)?;
        crate::snapshot::validate::validate_for_hit(&proof_inputs, environment)?;
        Ok(Self {
            store,
            receipts: recording.selections.clone(),
            proof_inputs,
        })
    }

    /// Called only after the ordinary candidate's complete payload and indices
    /// have been checked. Nothing has been changed publicly before this point.
    pub(crate) fn commit(
        self,
        factory: Factory<'_>,
        recording: &crate::observed_fs::Recording,
        environment: &crate::snapshot::validate::Environment<'_>,
    ) -> Result<(), String> {
        let guard = crate::repository_store::AdoptionGuard::acquire(
            &self.store,
            &self.receipts,
            recording
                .repositories
                .iter()
                .chain(&self.proof_inputs.repositories),
        )?;
        let (store, latest) = current(factory, &self.receipts)?;
        if store != self.store {
            return Err("publication store changed during acceptance".into());
        }
        if latest.repositories.iter().any(|latest| {
            !recording
                .repositories
                .iter()
                .chain(&self.proof_inputs.repositories)
                .any(|held| held.root == latest.root && held.manifest == latest.manifest)
        }) {
            return Err("current producer closure changed while acquiring adoption locks".into());
        }
        for inputs in [recording, &self.proof_inputs, &latest] {
            crate::snapshot::validate::validate_for_hit(inputs, environment)?;
        }
        guard
            .install()
            .map_err(|error| format!("{COMMIT_ERROR}{error}"))
    }
}

#[cfg(test)]
pub(crate) mod tests;
