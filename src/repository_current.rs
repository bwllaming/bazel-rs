//! Lookup-only current loading results. Only normal successful materialization
//! mints certificates; candidate metadata cannot demand or execute repositories.
use super::*;
use crate::observed_fs::Recording;
use crate::repository_publication::{Origin, Prepared};
use crate::repository_store::{AdoptionGuard, LocalExecution};
use crate::snapshot::replay::ReplayRecord;
use crate::snapshot::validate::{self, Environment};

#[derive(Debug)]
pub(super) struct LocalResult {
    invocation: u64,
    demanded: bool,
    canonical: String,
    pub(super) origin: Origin,
    pub(super) spec: String,
    definition: String,
    pub(super) root: PathBuf,
    pub(super) execution: LocalExecution,
    ordinary: bool,
    silent: bool,
}

impl LocalResult {
    pub(super) fn validate(&self, store: &Path) -> Result<(), String> {
        if !self.demanded || !self.ordinary || !self.silent {
            return Err("current local result is mutable or printed output".into());
        }
        if !crate::repository_slots::immutable_slot(store, &self.canonical)
            || !crate::repository_slots::selected_view(
                store,
                &self.canonical,
                &self.execution.identity,
            )
        {
            return Err("current local view changed or became mutable".into());
        }
        crate::repository_store::verify_local_publication(&self.root, &self.execution)
            .map_err(|error| format!("current local publication failed integrity: {error:?}"))
    }
}

impl RepositorySet {
    /// Enable the explicit cquery experiment before any current option/pattern
    /// loading. Ordinary/default commands do not collect this ledger.
    pub fn enable_current_loading(&self) {
        self.state
            .current_loading
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_current_local(
        &self,
        canonical: &str,
        origin: Origin,
        spec: &str,
        definition: &str,
        root: &Path,
        execution: &LocalExecution,
        silent: bool,
    ) {
        if !self
            .state
            .current_loading
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let ordinary = self.source_cache_path().ok().is_some_and(|store| {
            crate::repository_store::slot_class(store, canonical, root).is_some_and(
                |(id, class)| {
                    id == execution.identity
                        && class == crate::repository_slots::SlotClass::Ordinary
                },
            )
        });
        let result = LocalResult {
            invocation: self.state.runtime_id,
            demanded: crate::repository_demand::evaluating(
                self.state.runtime_id,
                &format!("repository:{canonical}"),
            ),
            canonical: canonical.to_owned(),
            origin,
            spec: spec.to_owned(),
            definition: definition.to_owned(),
            root: root.to_path_buf(),
            execution: execution.clone(),
            ordinary,
            silent,
        };
        self.state
            .current_locals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(canonical.to_owned(), Arc::new(result));
    }
}

/// Sealed only after ordinary current loading succeeds. No materialize/ensure
/// method is exposed. Its repository-set reference is private and used solely
/// for completed-result lookup, current proof configuration and consistency.
pub struct CurrentRefresh {
    repositories: Arc<RepositorySet>,
    locals: BTreeMap<String, Arc<LocalResult>>,
    observations: Recording,
    eligible: bool,
}

impl CurrentRefresh {
    pub(crate) fn seal(repositories: Arc<RepositorySet>, silent: bool) -> Self {
        let locals = repositories
            .state
            .current_locals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let recording = crate::observed_fs::current_recording();
        let eligible = repositories
            .state
            .current_loading
            .load(std::sync::atomic::Ordering::Relaxed)
            && crate::observed_fs::recording()
            && silent
            && recording
                .as_ref()
                .is_some_and(|recording| recording.taints.is_empty())
            && repositories.state.options.registry_session.mode()
                != crate::lockfile::LockfileMode::Refresh;
        Self {
            repositories,
            locals,
            observations: recording.unwrap_or_default(),
            eligible,
        }
    }

    pub(crate) fn eligible(&self) -> bool {
        self.eligible
    }

    fn proof_set(&self) -> Result<RepositorySet, String> {
        let source = &self.repositories.state;
        let mut set = RepositorySet::configured_environment(
            source.root.clone(),
            source.options.clone(),
            source.source_cache.clone(),
            Arc::new(source.environment.fresh()),
            source.host.clone(),
            false,
        );
        let state = Arc::get_mut(&mut set.state).expect("new proof set");
        state.replay = None;
        state.proof_locals = self.locals.clone();
        Ok(set)
    }

    fn check(
        &self,
        records: &[ReplayRecord],
        inputs: &Recording,
        env: &Environment<'_>,
    ) -> Result<(), String> {
        if !self.eligible || records.len() != self.locals.len() {
            return Err(
                "current loading does not exactly cover the candidate's local repositories".into(),
            );
        }
        let store = self.repositories.source_cache_path()?;
        let mut seen = BTreeSet::new();
        for record in records {
            let local = self
                .locals
                .get(&record.canonical)
                .ok_or("candidate local repository was not demanded by current loading")?;
            if !seen.insert(&record.canonical)
                || local.invocation != self.repositories.state.runtime_id
                || local.definition != record.identity
                || local.root != record.root
                || local.execution != record.execution
                || !record.output.is_empty()
                || !crate::repository_slots::immutable_slot(store, &record.canonical)
                || !crate::repository_slots::selected_view(
                    store,
                    &record.canonical,
                    &local.execution.identity,
                )
            {
                return Err(format!(
                    "current local result differs for @@{}",
                    record.canonical
                ));
            }
            local.validate(store)?;
        }
        // Lookup of ready roots only: never a demand. Retained loading caches
        // cannot be left pointing at a different publication after adoption.
        for (canonical, root) in self.repositories.materialized_roots() {
            if self.locals.contains_key(&canonical) {
                continue;
            }
            if crate::repository_store::store_tree_of(store, &root)
                .is_some_and(|tree| tree.kind == "direct")
                && !inputs
                    .selections
                    .iter()
                    .any(|receipt| receipt.canonical == canonical && receipt.root(store) == root)
            {
                return Err(format!(
                    "candidate conflicts with current loaded repository @@{canonical}"
                ));
            }
        }
        validate::validate_for_hit(&self.observations, env)?;
        Ok(())
    }

    pub(crate) fn prepare(
        &self,
        records: &[ReplayRecord],
        inputs: &mut Recording,
        env: &Environment<'_>,
    ) -> Result<Option<Prepared>, String> {
        self.check(records, inputs, env)?;
        if inputs.selections.is_empty() {
            validate::validate_for_hit(inputs, env)?;
            Ok(None)
        } else {
            Prepared::prepare(&|| self.proof_set(), inputs, env).map(Some)
        }
    }

    pub(crate) fn accept(
        &self,
        records: &[ReplayRecord],
        inputs: &Recording,
        prepared: Option<Prepared>,
        env: &Environment<'_>,
    ) -> Result<(), String> {
        let store = self.repositories.source_cache_path()?;
        let names: Vec<_> = self.locals.keys().cloned().collect();
        // Identity locks protect each actual current local publication too.
        let mut trees: Vec<_> = self
            .locals
            .values()
            .map(|local| crate::observed_fs::RepositoryRecord {
                kind: "direct",
                name: local.spec.clone(),
                root: local.root.clone(),
                manifest: Some(
                    store
                        .join("direct/manifests")
                        .join(format!("{}.json", local.execution.identity)),
                ),
                entries: Vec::new(),
            })
            .collect();
        trees.extend(self.observations.repositories.iter().cloned());
        if let Some(prepared) = prepared {
            prepared.commit_current(&|| self.proof_set(), inputs, env, &names, &trees, || {
                self.check(records, inputs, env)
            })?;
        } else {
            let _guards = AdoptionGuard::acquire_current(store, &[], trees.iter(), &names)?;
            self.check(records, inputs, env)?;
            validate::validate_for_hit(inputs, env)?;
        }
        Ok(())
    }
}
