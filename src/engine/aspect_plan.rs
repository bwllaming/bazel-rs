//! N58: the planning of attribute-aspect applications (an aspect-applied
//! dependency of a rule: its aspect collection, each application's inputs,
//! analysis and commit), as one generic recursion over a context
//! ([`AspectCx`]) that answers its lookups and takes its writes:
//!
//! * [`CoordAspects`] — the coordinator's (`&mut Engine`, the planning
//!   state): the sequential engine's, with T3's dry walk at an entry's turn
//!   (`parallel::aspects`);
//! * task B's aspect jobs (`parallel::aspect_jobs`): the same recursion on
//!   an analysis worker over a frozen walk's view, each application
//!   analyzed once (single-flight) and committed at its first dependent's
//!   merge.

use super::*;

/// What an aspect-applied dependency plans: its (merged) view and the
/// applications of its aspects, in application order.
pub(super) type Planned = Result<(DependencyTarget, AspectApplications), String>;

/// The key of an aspect-configured target's one shared `Target` value.
pub(super) type AspectIdentityKey = (Label, Option<ConfigId>, Vec<String>);

/// What an application's analysis depends on besides its key: how its
/// collection reached it (task B's aspect jobs share one analysis per key
/// and context; the sequential walk plans each key once, in the first
/// context in plan order).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(super) struct Context {
    /// The collection's aspects, in order (the propagating `incoming`, the
    /// computed `attr_aspects`).
    pub(super) descriptors: Vec<String>,
    /// What the `target` parameter is built from (merged_dependency of the
    /// requested edge's view: an alias's view and its actual's are the
    /// same here).
    pub(super) base: BaseView,
    /// Whether the base configuration's target platform is resolved.
    pub(super) platform: bool,
}

/// The fields of a dependency view an application's `target` reads, its
/// provider values and runfiles by identity.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(super) struct BaseView {
    label: String,
    files: Vec<crate::starlark_types::File>,
    kind: [bool; 4],
    providers: Vec<(String, u64)>,
    provider_keys: Vec<String>,
    runfiles: usize,
    rule_class: Option<String>,
}

impl BaseView {
    fn of(view: &DependencyTarget) -> Self {
        use std::hash::BuildHasher;
        let hasher = crate::fast_hash::FxBuildHasher::default();
        Self {
            label: view.label.clone(),
            files: view.files.clone(),
            kind: [
                view.is_source,
                view.is_rule,
                view.is_package_group,
                view.is_filegroup,
            ],
            providers: view
                .providers
                .iter()
                .map(|(name, value)| (name.clone(), hasher.hash_one(value.value().identity())))
                .collect(),
            provider_keys: view.provider_keys.clone(),
            runfiles: Arc::as_ptr(&view.runfiles) as usize,
            rule_class: view.rule_class.clone(),
        }
    }
}

/// What the commit of an application needs besides its analysis: its
/// outputs' configuration, its owner and its base target.
pub(super) struct AspectCommit {
    pub(super) output_root: String,
    pub(super) action_owner: String,
    pub(super) action_env: Arc<crate::action_env::ActionEnvironment>,
    pub(super) runfiles: crate::runfiles_tree::RunfilesOptions,
    pub(super) loaded_functions: Arc<crate::analysis_runtime::LoadedFunctions>,
    pub(super) run_under: Option<Arc<analysis::RunUnderPrerequisite>>,
    pub(super) base_key: ConfiguredTargetKey,
    pub(super) base_rule: Rule,
    /// The aspect's identity (the owner of its actions' errors).
    pub(super) aspect_identity: String,
    pub(super) action_execution: Vec<crate::exec_groups::ActionExecution>,
}

impl AspectCommit {
    pub(super) fn outputs(&self) -> analysis::ConfiguredOutputs<'_> {
        analysis::ConfiguredOutputs {
            output_root: &self.output_root,
            action_owner: &self.action_owner,
            action_env: self.action_env.clone(),
            runfiles: self.runfiles,
            loaded_functions: Some(&self.loaded_functions),
            run_under: self.run_under.clone(),
        }
    }
}

/// The lookups and writes of the aspect planning.
pub(super) trait AspectCx: prepare::Lookup + Sized {
    fn configured(&self) -> &ConfiguredNodes;
    /// The plain dependency view (Engine::plan_dependency_view).
    fn view(&mut self, label: &Label, edge_config: ConfigId) -> Result<DependencyTarget, String>;
    /// Engine::aspect_collection_for.
    fn collection_for(
        &mut self,
        aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
        rule: &Rule,
    ) -> Result<crate::aspects::AspectCollection, String>;
    /// Engine::with_computed_attr_aspects.
    fn computed_attr_aspects(
        &mut self,
        collection: crate::aspects::AspectCollection,
        key: &ConfiguredTargetKey,
        rule: &Rule,
    ) -> Result<crate::aspects::AspectCollection, String>;
    /// Phase 1's configurations of an aspect's attributes on a base target.
    fn attribute_configs(
        &self,
        memo: &(ConfiguredTargetKey, String),
    ) -> Option<Arc<BTreeMap<String, TransitionConfigurations>>> {
        self.engine().aspect_attribute_configs.get(memo).cloned()
    }
    /// Phase 1's toolchain contexts of an aspect in a configuration.
    fn aspect_toolchains(&self, memo: &(ConfigId, String)) -> Option<Arc<AspectToolchains>> {
        self.engine().aspect_toolchains.get(memo).cloned()
    }
    /// Walk one aspect-applied dependency (`walk`), or answer it from what
    /// the context knows of it.
    fn walk_dependency(
        &mut self,
        label: &Label,
        edge_config: ConfigId,
        aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
        walk: impl FnOnce(&mut Self, Vec<Arc<crate::aspects::AspectDefinition>>) -> Planned,
    ) -> Planned;
    /// The providers of an application planned before.
    fn memo(
        &self,
        key: &crate::aspects::AspectApplicationKey,
        context: &Context,
    ) -> Option<AspectProviders>;
    /// A collection's application (a memo hit or planned), in order.
    fn collected(&mut self, _key: &crate::aspects::AspectApplicationKey, _context: &Context) {}
    /// A collection meets the application `key` (before its memo).
    fn applying(&mut self, key: &crate::aspects::AspectApplicationKey);
    /// The application `key` was planned: its providers (the ones every
    /// user shares).
    fn applied(
        &mut self,
        key: &crate::aspects::AspectApplicationKey,
        context: &Context,
        providers: AspectProviders,
    ) -> AspectProviders;
    /// The planning of one application starts / ends.
    fn begin_application(&mut self) {}
    fn end_application(&mut self) {}
    /// The one shared `Target` value of an aspect-configured target.
    fn identity(
        &mut self,
        key: AspectIdentityKey,
        variant: &str,
        fill: &mut dyn FnMut() -> Arc<crate::starlark_types::Target>,
    ) -> Arc<crate::starlark_types::Target>;
    /// The planned targets' results (the toolchains' providers).
    fn with_reads<R>(&self, f: impl FnOnce(&dyn PlanReads) -> R) -> R;
    /// The console and aquery owners of an application's actions.
    fn note_owner(
        &mut self,
        action_owner: &str,
        console: crate::shared_path::SharedPath,
        base_key: &ConfiguredTargetKey,
        aspects: &dyn Fn() -> Vec<crate::aquery::Aspect>,
    );
    /// Analyze the application and commit it: its providers.
    fn analyze(
        &mut self,
        key: &crate::aspects::AspectApplicationKey,
        context: &Context,
        application: analysis::AspectApplication,
        commit: AspectCommit,
    ) -> Result<AspectProviders, String>;
}

/// The coordinator's aspect planning (the planning state, written).
pub(super) struct CoordAspects<'s, 'p> {
    pub(super) engine: &'s mut Engine,
    pub(super) state: &'s mut PlanState<'p>,
    pub(super) configured: &'s mut ConfiguredNodes,
}

impl prepare::Lookup for CoordAspects<'_, '_> {
    fn engine(&self) -> &Engine {
        self.engine
    }

    fn target(&mut self, label: &Label) -> Result<Target, String> {
        self.engine.target(label)
    }

    fn no_config(&mut self) -> Result<ConfigId, String> {
        Ok(self.engine.configurations.intern_no_config())
    }

    fn attribute_aspects(
        &mut self,
        rule: &Rule,
        identities: &[String],
    ) -> Result<Vec<Arc<crate::aspects::AspectDefinition>>, String> {
        self.engine.attribute_aspects(rule, identities)
    }
}

impl AspectCx for CoordAspects<'_, '_> {
    fn configured(&self) -> &ConfiguredNodes {
        self.configured
    }

    fn view(&mut self, label: &Label, edge_config: ConfigId) -> Result<DependencyTarget, String> {
        let PlanState {
            dry,
            custom_providers,
            provider_keys,
            runfiles_info,
            artifact_index,
            declared,
            native_output_groups,
            validation,
            target_identities,
            views,
            ..
        } = &mut *self.state;
        self.engine.plan_dependency_view(
            self.configured,
            dry,
            custom_providers,
            provider_keys,
            runfiles_info,
            artifact_index,
            declared,
            native_output_groups,
            validation,
            target_identities,
            views,
            label,
            edge_config,
        )
    }

    fn collection_for(
        &mut self,
        aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
        rule: &Rule,
    ) -> Result<crate::aspects::AspectCollection, String> {
        self.engine.aspect_collection_for(aspects, rule)
    }

    fn computed_attr_aspects(
        &mut self,
        collection: crate::aspects::AspectCollection,
        key: &ConfiguredTargetKey,
        rule: &Rule,
    ) -> Result<crate::aspects::AspectCollection, String> {
        self.engine
            .with_computed_attr_aspects(collection, key, rule)
    }

    fn walk_dependency(
        &mut self,
        label: &Label,
        edge_config: ConfigId,
        aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
        walk: impl FnOnce(&mut Self, Vec<Arc<crate::aspects::AspectDefinition>>) -> Planned,
    ) -> Planned {
        // T3's dry walk (parallel::aspects): a dependency whose applications
        // are all analyzed is walked once per turn, a pending one again only
        // once what it waits for is analyzed.
        if aspects.is_empty() || !parallel::aspects::dry() {
            return walk(self, aspects);
        }
        let walked = match parallel::aspects::enter_walk(label, edge_config, &aspects) {
            parallel::aspects::Enter::Known(planned) => return planned,
            parallel::aspects::Enter::Walk(walked) => walked,
        };
        let planned = walk(self, aspects);
        parallel::aspects::leave_walk(walked, &planned);
        planned
    }

    fn memo(
        &self,
        key: &crate::aspects::AspectApplicationKey,
        _context: &Context,
    ) -> Option<AspectProviders> {
        if let Some(providers) = self.state.aspect_providers.get(key) {
            return Some(providers.clone());
        }
        if parallel::aspects::dry() {
            return parallel::aspects::dry_providers(key);
        }
        None
    }

    fn applying(&mut self, key: &crate::aspects::AspectApplicationKey) {
        self.engine.track_aspect_application(key);
        if order_audit::enabled() && !parallel::aspects::dry() {
            order_audit::note_aspect(!self.state.aspect_providers.contains_key(key));
        }
    }

    fn applied(
        &mut self,
        key: &crate::aspects::AspectApplicationKey,
        _context: &Context,
        providers: AspectProviders,
    ) -> AspectProviders {
        if parallel::aspects::dry() {
            parallel::aspects::note_dry(key, &providers);
            return providers;
        }
        crate::progress::aspect_applied();
        self.state
            .aspect_providers
            .insert(key.clone(), providers.clone());
        providers
    }

    fn identity(
        &mut self,
        key: AspectIdentityKey,
        variant: &str,
        fill: &mut dyn FnMut() -> Arc<crate::starlark_types::Target>,
    ) -> Arc<crate::starlark_types::Target> {
        if let Some(identity) = self.state.aspect_target_identities.get(&key) {
            return identity.clone();
        }
        // Task B: the walk's value of the aspect jobs (the one their
        // analyses saw), first in plan order here.
        let identity = self
            .engine
            .walk_aspect_identity(&key, variant, fill)
            .unwrap_or_else(fill);
        self.state
            .aspect_target_identities
            .insert(key, identity.clone());
        identity
    }

    fn with_reads<R>(&self, f: impl FnOnce(&dyn PlanReads) -> R) -> R {
        f(&MapReads {
            custom_providers: Some(&*self.state.custom_providers),
            ..MapReads::default()
        })
    }

    fn note_owner(
        &mut self,
        action_owner: &str,
        console: crate::shared_path::SharedPath,
        base_key: &ConfiguredTargetKey,
        aspects: &dyn Fn() -> Vec<crate::aquery::Aspect>,
    ) {
        self.engine
            .console_owners
            .insert(action_owner.to_owned(), console);
        self.engine.aquery_owner(action_owner, base_key, aspects);
    }

    fn analyze(
        &mut self,
        key: &crate::aspects::AspectApplicationKey,
        context: &Context,
        application: analysis::AspectApplication,
        commit: AspectCommit,
    ) -> Result<AspectProviders, String> {
        // T3: analyzed on a worker before the turn's prepare, or (its dry
        // walk) queued there (parallel::aspects).
        let outputs = commit.outputs();
        // Task B: a frozen walk's one analysis of it (parallel::aspect_jobs),
        // where it holds; not while the turn's dry walk prefetches (its
        // dependencies' applications are committed only by the replay).
        if !parallel::aspects::dry()
            && let Some((analyzed, printed)) = self.engine.walk_aspect_analysis(
                key,
                context,
                &application,
                &outputs,
                self.state.artifact_index.turn as usize,
                &*self.state.aspect_target_identities,
                &*self.state.aspect_providers,
            )
        {
            return commit_application(self.engine, self.state, &commit, Ok(*analyzed), printed);
        }
        let (analyzed, printed) = match parallel::aspects::analysis(key, application, &outputs) {
            parallel::aspects::Analysis::Here(application) => {
                analysis::analyze_aspect_buffered(&application, &outputs)
            }
            parallel::aspects::Analysis::Done { analyzed, printed } => (Ok(*analyzed), printed),
            parallel::aspects::Analysis::Dry(providers) => return providers,
        };
        commit_application(self.engine, self.state, &commit, analyzed, printed)
    }
}

/// An application's prints and commit (its declared outputs and actions
/// under its own owner), after its analysis: its providers.
pub(super) fn commit_application(
    engine: &mut Engine,
    state: &mut PlanState<'_>,
    commit: &AspectCommit,
    analyzed: Result<analysis::AnalysisResult, String>,
    printed: Vec<String>,
) -> Result<AspectProviders, String> {
    for line in printed {
        engine.note_analysis_print(&line);
    }
    let analyzed = analyzed?;
    let base_key = &commit.base_key;
    if engine.semantic_only() {
        cquery::register_outputs(
            analyzed.actions.iter().flat_map(|action| &action.outputs),
            base_key.config,
            state.artifact_index,
            state.declared,
            |_| false,
        );
        return Ok(analyzed.providers);
    }
    // Register the application's declared outputs and actions under the
    // application's own owner, so they never collide with the base
    // target's or another aspect's (AspectKey as ActionLookupKey).
    let PlanState {
        artifact_index,
        declared,
        producers,
        nodes,
        source_inputs,
        client_environment,
        status_client_environment,
        workspace_status,
        input_groups,
        input_memos,
        listed_tree_actions,
        ..
    } = state;
    let main_ignored = engine.ignored("")?;
    // N63: plan the workspace status system nodes on first need.
    if Engine::workspace_status_needed(&analyzed.actions) {
        engine.ensure_workspace_status(
            workspace_status,
            nodes,
            producers,
            client_environment,
            status_client_environment,
            artifact_index,
        )?;
    }
    let shared = register_action_outputs(
        &analyzed.actions,
        (&commit.action_owner, &base_key.label.to_string()),
        producers,
        artifact_index,
        declared,
        base_key.config,
        |_| false,
    )?;
    let owner = format!("aspect {}", commit.aspect_identity);
    for (mut registered, shared) in analyzed.actions.into_iter().zip(shared) {
        // Another owner's equal action produces its outputs.
        if shared {
            continue;
        }
        if registered.repo_mapping.is_some() {
            return Err(format!(
                "{owner}: py_internal.create_repo_mapping_manifest in an aspect {}",
                crate::language_modules::NOT_IMPLEMENTED
            ));
        }
        // N63: resolve an actions.transform_{version,info}_file
        // registration into its template expansion.
        if registered.build_info.is_some() {
            engine
                .build_info_actions
                .insert(registered.action.id.clone());
        }
        engine.resolve_build_info(workspace_status, &mut registered)?;
        let own = registered
            .inputs
            .iter()
            .map(|file| {
                artifact_index.get(file).ok_or_else(|| {
                    format!("{owner}: input has no declared artifact: {}", file.path)
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut action = registered.action;
        // A spawn's propagated tags are its base rule's (an aspect's
        // RuleContext.getRule), set at registration (keyed,
        // Context::register_action); writes and transforms have none,
        // like a rule's. The base target's exec_properties are not an
        // aspect action's (ConfiguredTargetFactory.createAspect
        // setExecProperties(ImmutableMap.of()), analysis/
        // ConfiguredTargetFactory.java:702: genuine shares two
        // applications' equal spawns whatever their base targets'
        // exec_properties, tests/fixtures/shared_actions aspect_shared),
        // so the key has none; the remote gate stays conservative (they
        // keep the spawn off the remote cache).
        // The action's exec group (of the aspect's ToolchainCollection)
        // decides its execution platform and exec properties
        // (RuleContext.getActionOwner(execGroup)), like a rule's.
        let execution = commit
            .action_execution
            .iter()
            .find(|execution| execution.exec_group == registered.exec_group);
        if let Some(execution) = execution {
            engine
                .action_executions
                .insert(action.id.clone(), execution.clone());
        }
        if action.write.is_none() && action.extra.transform.is_none() {
            engine.set_remote_platform(
                &mut action,
                &commit.base_rule,
                base_key.config,
                execution,
                false,
            );
        }
        // Like a rule's action (PA-1 follow-up B6): its tools' runfiles
        // trees (shared input groups) and its deferred completion,
        // which the aspect commit used to drop.
        engine.commit_with(
            delta::CommitParts {
                artifact_index,
                workspace_status,
                producers,
                nodes,
                source_inputs,
                listed_tree_actions,
                client_environment,
                status_client_environment,
                input_groups,
                input_memos,
            },
            |builder| {
                builder.commit_action(
                    action,
                    own,
                    &registered.runfiles_trees,
                    registered.deferred,
                    &owner,
                    &main_ignored,
                    None,
                )
            },
        )?;
    }
    Ok(analyzed.providers)
}

/// One dependency of an aspect application: the plain configured target
/// view, or the aspect-applied view when `aspects` propagate to it.
pub(super) fn plan_dependency<C: AspectCx>(
    cx: &mut C,
    label: &Label,
    edge_config: ConfigId,
    aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
) -> Planned {
    cx.walk_dependency(label, edge_config, aspects, |cx, aspects| {
        dependency_walk(cx, label, edge_config, aspects)
    })
}

/// [`plan_dependency`].
fn dependency_walk<C: AspectCx>(
    cx: &mut C,
    label: &Label,
    edge_config: ConfigId,
    mut aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
) -> Planned {
    if aspects.is_empty() || incompatible(cx, label, edge_config)? {
        return Ok((cx.view(label, edge_config)?, Vec::new()));
    }
    // The target the aspects apply to: through aliases and to the
    // generating rule of an output file when the aspect asks for it
    // (apply_to_generating_rules).
    let base_label = application_label(cx, label, edge_config);
    let target = cx.target(&base_label)?;
    let target = match target {
        Target::Output(generating, _) => {
            // AspectFunction.java:327-341: an aspect applies to an
            // output-file target only through its generating rule, and
            // only when it asks for that (apply_to_generating_rules; the
            // native ValidateTarget applies to files).
            aspects.retain(|aspect| {
                aspect.declaration.apply_to_generating_rules
                    || aspect.identity == crate::aspects::VALIDATION_ASPECT_IDENTITY
            });
            if aspects.is_empty() {
                return Ok((cx.view(label, edge_config)?, Vec::new()));
            }
            cx.target(&generating.label)?
        }
        target => target,
    };
    let Target::Rule(rule) = target else {
        return Ok((cx.view(label, edge_config)?, Vec::new()));
    };
    if matches!(rule.kind, RuleKind::PackageGroup(_)) || no_config_rule_class(&rule) {
        aspects.clear();
    }
    let collection = if aspects.is_empty() {
        crate::aspects::AspectCollection::default()
    } else {
        cx.collection_for(aspects.to_vec(), &rule)?
    };
    if collection.is_empty() {
        return Ok((cx.view(label, edge_config)?, Vec::new()));
    }
    plan_collection(cx, label, edge_config, &collection)
}

fn incompatible<C: AspectCx>(cx: &mut C, label: &Label, config: ConfigId) -> Result<bool, String> {
    let owner = match cx.target(label)? {
        Target::Output(rule, _) => rule.label.clone(),
        _ => label.clone(),
    };
    Ok(cx
        .configured()
        .get(&ConfiguredTargetKey::new(owner, config))
        .is_some_and(|node| node.incompatible.is_some()))
}

/// The label the aspects of an edge apply to: the alias chain's end in the
/// edge's configuration.
fn application_label<C: AspectCx>(cx: &C, label: &Label, edge_config: ConfigId) -> Label {
    let requested = ConfiguredTargetKey::new(label.clone(), edge_config);
    let configured = cx.configured();
    let mut base_label = cx
        .engine()
        .forwarded_actual(configured, &requested)
        .unwrap_or_else(|| label.clone());
    while let Some(actual) = configured
        .get(&ConfiguredTargetKey::new(base_label.clone(), edge_config))
        .and_then(|node| node.actual.clone())
    {
        base_label = actual;
    }
    base_label
}

/// Plan every aspect application of `collection` on the configured
/// target of `label`/`edge_config` (N58) and return the
/// aspect-configured target's dependency view: the base configured
/// target with each aspect's providers merged in
/// (MergedConfiguredTarget.of — aspect providers shadow the base
/// target's, OutputGroupInfo merges per group).
///
/// Each application runs its aspect's implementation `f(target, ctx)`
/// once, its actions registered under the application's own action
/// owner (`label%aspect`, never the base target's), its aspect-applied
/// dependencies planned recursively (AspectFunction's dependency
/// resolution over the aspect path).
fn plan_collection<C: AspectCx>(
    cx: &mut C,
    label: &Label,
    edge_config: ConfigId,
    collection: &crate::aspects::AspectCollection,
) -> Planned {
    // The base view through the shared dependency machinery.
    let mut base = cx.view(label, edge_config)?;
    if incompatible(cx, label, edge_config)? {
        return Ok((base, Vec::new()));
    }
    // The application target: the alias chain's end in the edge's
    // configuration, with a rule transition's delegation applied
    // (AspectKeyCreator over ConfiguredTargetKey); an output-file edge
    // applies its aspects to the generating rule when they ask for it
    // (apply_to_generating_rules).
    let requested = ConfiguredTargetKey::new(label.clone(), edge_config);
    let mut base_label = application_label(cx, label, edge_config);
    if let Target::Output(generating, _) = cx.target(&base_label)? {
        let applies = collection.used.iter().any(|(aspect, _)| {
            aspect.declaration.apply_to_generating_rules
                || aspect.identity == crate::aspects::VALIDATION_ASPECT_IDENTITY
        });
        if applies {
            base_label = generating.label.clone();
        }
    }
    let base_key = ConfiguredTargetKey::new(
        base_label.clone(),
        cx.engine().delegated_config(&requested, edge_config),
    );
    let platform_key = ConfiguredTargetKey::new(
        cx.engine()
            .configurations
            .get(base_key.config)
            .native
            .target_platform()
            .clone(),
        base_key.config,
    );
    let context = Context {
        descriptors: collection.descriptors(),
        base: BaseView::of(&base),
        platform: matches!(cx.platform(&platform_key), Some(Ok(_))),
    };
    // AspectCollection.createAspectKeys: each aspect over the base
    // target, in application order.
    let mut applications: AspectApplications = Vec::new();
    // T3's dry walk (parallel::aspects): an application waiting for its
    // analysis is pending, with the aspects over it; the others go on.
    let mut pending: Vec<&str> = Vec::new();
    for (aspect, bases) in &collection.used {
        let key = crate::aspects::AspectApplicationKey {
            base: base_key.clone(),
            aspect: aspect.descriptor.clone(),
            bases: bases.clone(),
        };
        cx.applying(&key);
        if let Some(providers) = cx.memo(&key, &context) {
            cx.collected(&key, &context);
            applications.push((key, providers));
            continue;
        }
        if bases.iter().any(|base| pending.contains(&base.as_str())) {
            pending.push(&aspect.descriptor);
            continue;
        }
        cx.begin_application();
        let planned = plan_application(cx, &base_key, &base, aspect, bases, collection, &context);
        cx.end_application();
        match planned {
            Ok(providers) => {
                let providers = cx.applied(&key, &context, providers);
                cx.collected(&key, &context);
                applications.push((key, providers));
            }
            Err(error) if error == parallel::aspects::PENDING => {
                pending.push(&aspect.descriptor);
            }
            Err(error) => return Err(error),
        }
    }
    if !pending.is_empty() {
        return Err(parallel::aspects::PENDING.to_owned());
    }
    base = crate::aspects::merged_dependency(&base, &applications)?;
    // One shared Target value per aspect-configured target (N55's
    // sharing applied to the merged configured target): its providers
    // are the merged set, not the base target's.
    // The value's variant: the requested label (an alias's names it) and
    // the very provider values of the applications merged in (one analyzed
    // in another context holds others), so equal views share one value.
    let requested_text = label.to_string();
    let variant = identity_variant(&format!("{requested_text}\0{edge_config}"), &applications);
    let identity = cx.identity(
        (
            base_label.clone(),
            Some(base_key.config),
            collection.descriptors(),
        ),
        &variant,
        &mut || {
            Arc::new(
                analysis::dependency_target_data(
                    &requested_text,
                    &base.label,
                    &base.files,
                    base.is_source,
                    base.is_rule,
                    base.is_package_group,
                    base.is_filegroup,
                    &base.providers,
                    &base.provider_keys,
                    &base.runfiles,
                )
                .with_rule_class(base.rule_class.as_deref()),
            )
        },
    );
    base.identity = identity;
    Ok((base, applications))
}

/// An aspect-configured `Target` value's variant (task B's table key
/// beside its identity key): the requested label and the identities of the
/// applications' provider values (the merged values are made per merge).
fn identity_variant(requested: &str, applications: &AspectApplications) -> String {
    use std::fmt::Write as _;
    use std::hash::BuildHasher;
    let hasher = crate::fast_hash::FxBuildHasher::default();
    let mut text = requested.to_owned();
    for (key, providers) in applications {
        let _ = write!(text, "\0{}", key.aspect);
        for (name, value) in providers {
            let _ = write!(
                text,
                "\0{name}={:x}",
                hasher.hash_one(value.value().identity())
            );
        }
    }
    text
}

/// The planned dependencies of the label attributes of `owner` (the
/// applied aspect itself, or a base aspect of its path restricted to
/// `names`) in one application, keyed like a rule context's
/// ([`analysis::attribute_dependency_key`], split keys for Starlark
/// transitions), aspect-applied with the path's aspects that propagate
/// over the attribute (Self::analyze_aspect_attribute_edges).
#[allow(clippy::type_complexity)]
fn plan_attribute_dependencies<C: AspectCx>(
    cx: &mut C,
    owner: &crate::aspects::AspectDefinition,
    names: Option<&[String]>,
    attribute_configs: &BTreeMap<String, TransitionConfigurations>,
    path: &[Arc<crate::aspects::AspectDefinition>],
) -> Result<
    (
        BTreeMap<String, DependencyTarget>,
        BTreeMap<String, crate::custom_rule::SplitAttribute>,
    ),
    String,
> {
    let mut aspect_dependencies: BTreeMap<String, DependencyTarget> = BTreeMap::new();
    let mut aspect_split_dependencies = BTreeMap::new();
    let mut pending = false;
    for (name, spec) in &owner.declaration.attrs {
        if names.is_some_and(|names| !names.contains(name)) {
            continue;
        }
        let Some(branches) = attribute_configs.get(name) else {
            continue;
        };
        let propagating = crate::aspects::propagating_over_aspect_attribute(path, owner, name);
        let split = branches.len() != 1 || !branches[0].0.is_empty();
        let mut labels = Vec::new();
        for text in aspect_attribute_labels(spec) {
            let dependency = cx.engine().repositories.parse(
                &text,
                &owner.definition_label.repository,
                &owner.definition_label.package,
            )?;
            labels.push(dependency.clone());
            for (_, attribute_config) in branches.iter() {
                let attribute_config = *attribute_config;
                let (view, _) =
                    match plan_dependency(cx, &dependency, attribute_config, propagating.clone()) {
                        Err(error) if error == parallel::aspects::PENDING => {
                            pending = true;
                            continue;
                        }
                        planned => planned?,
                    };
                aspect_dependencies.insert(
                    if split {
                        analysis::split_dependency_key(
                            &dependency.to_string(),
                            name,
                            attribute_config,
                        )
                    } else {
                        analysis::attribute_dependency_key(&dependency.to_string(), name, spec)
                    },
                    view,
                );
            }
        }
        if split {
            aspect_split_dependencies.insert(
                name.clone(),
                Engine::split_attribute(cx, name, labels, branches)?,
            );
        }
    }
    if pending {
        return Err(parallel::aspects::PENDING.to_owned());
    }
    Ok((aspect_dependencies, aspect_split_dependencies))
}

/// Plan one aspect application (one AspectValue): the aspect's
/// implementation over its `target` parameter and its dependencies (the
/// aspect's own attributes plus the base rule's edges, aspect-applied
/// where the aspect propagates), returning the aspect's providers.
#[allow(clippy::too_many_arguments)]
fn plan_application<C: AspectCx>(
    cx: &mut C,
    base_key: &ConfiguredTargetKey,
    base: &DependencyTarget,
    aspect: &Arc<crate::aspects::AspectDefinition>,
    bases: &[String],
    collection: &crate::aspects::AspectCollection,
    context: &Context,
) -> Result<AspectProviders, String> {
    let top = cx.engine().configurations.top();
    let key = crate::aspects::AspectApplicationKey {
        base: base_key.clone(),
        aspect: aspect.descriptor.clone(),
        bases: bases.to_vec(),
    };
    let base_rule: Rule = match cx.engine().configured_rules.get(base_key) {
        Some(rule) => Rule::clone(rule),
        None => match cx.target(&base_key.label)? {
            Target::Rule(rule) => Rule::clone(&rule),
            _ => {
                return Err(format!(
                    "internal error: aspect {} applied to non-rule {}",
                    aspect.identity, base_key.label
                ));
            }
        },
    };
    // The `target` parameter: the base configured target merged with the
    // directly required aspects' providers (AspectFunction.java:341-346),
    // also what the native ValidateTarget reads its `_validation`
    // outputs from.
    let mut base_aspect_maps = Vec::new();
    for base_aspect in bases {
        // The base aspect's own key carries its directly required
        // aspects (AspectCollection.buildAspectKey over AspectDeps).
        let base_aspect_key = crate::aspects::AspectApplicationKey {
            base: base_key.clone(),
            aspect: base_aspect.clone(),
            bases: collection
                .used
                .iter()
                .find(|(used, _)| used.descriptor == *base_aspect)
                .map(|(_, used_bases)| used_bases.clone())
                .unwrap_or_default(),
        };
        let providers = cx.memo(&base_aspect_key, context).ok_or_else(|| {
            format!(
                "internal error: aspect {base_aspect} of {} was not planned before {}",
                base_key.label, aspect.identity
            )
        })?;
        base_aspect_maps.push((base_aspect_key, providers));
    }
    let merged_target = crate::aspects::merged_dependency(base, &base_aspect_maps)?;
    // The native ValidateTarget (rules/core/ValidateTarget.java:60-77):
    // promotes the (merged) base target's `_validation` set to
    // `_validation_top_level` (the set itself, W1c); not applicable
    // when it is empty.
    if aspect.identity == crate::aspects::VALIDATION_ASPECT_IDENTITY {
        let Some(validations) = merged_target
            .providers
            .get(crate::output_group_info::PROVIDER_ID)
            .and_then(|value| {
                crate::output_group_info::validation(value.value())
                    .map(crate::output_group_info::shared_validation)
            })
        else {
            // ConfiguredAspect.NonApplicableAspect: no providers.
            return Ok(BTreeMap::new());
        };
        let info = crate::output_group_info::freeze_validation_top_level(validations)?;
        return Ok(BTreeMap::from([(
            crate::output_group_info::PROVIDER_ID.to_owned(),
            info,
        )]));
    }
    // A target that does not satisfy the aspect's required providers
    // (an aspect kept only because another one requires it) gets
    // ConfiguredAspect.NonApplicableAspect: no implementation, no
    // dependencies, no providers (AspectFunction.java:332-339).
    if !aspect.applies_to_rule(&base_rule) {
        return Ok(BTreeMap::new());
    }
    // The application's dependencies: the aspect's own attributes, each
    // in its attribute's configuration (phase 1 analyzed them there,
    // Self::aspect_attribute_configurations), and the base rule's edges
    // (aspect-applied where they propagate). The aspect's attributes are
    // a prerequisite collection of their own (AspectContext.
    // getMainAspectPrerequisitesCollection), never the base rule's.
    let attribute_configs = cx
        .attribute_configs(&(base_key.clone(), aspect.identity.clone()))
        .ok_or_else(|| {
            format!(
                "internal analysis order error: aspect {} on {} was not analyzed",
                aspect.identity, base_key.label
            )
        })?;
    let path = cx
        .computed_attr_aspects(collection.clone(), base_key, &base_rule)?
        .path_aspects(&aspect.descriptor);
    // T3's dry walk: what waits for an analysis below is pending; the
    // rest of the application's dependencies are walked on.
    let mut pending = false;
    let (aspect_dependencies, aspect_split_dependencies) =
        match plan_attribute_dependencies(cx, aspect, None, &attribute_configs, &path) {
            Err(error) if error == parallel::aspects::PENDING => {
                pending = true;
                Default::default()
            }
            planned => planned?,
        };
    // The base aspects' attributes `ctx.rule.attr` holds, planned as
    // phase 1 analyzed them (Self::analyze_aspect_attribute_edges).
    let mut base_aspect_attributes = Vec::new();
    for (owner, names) in crate::aspects::base_aspect_attributes(&path, &base_rule) {
        let memo = (base_key.clone(), aspect_attribute_memo_name(&owner, aspect));
        let owner_configs = cx.attribute_configs(&memo).ok_or_else(|| {
                format!(
                    "internal analysis order error: the attributes of aspect {} under {} on {} were not analyzed",
                    owner.identity, aspect.identity, base_key.label
                )
            })?;
        let (dependencies, split_dependencies) =
            match plan_attribute_dependencies(cx, &owner, Some(&names), &owner_configs, &path) {
                Err(error) if error == parallel::aspects::PENDING => {
                    pending = true;
                    continue;
                }
                planned => planned?,
            };
        base_aspect_attributes.push(analysis::BaseAspectAttributes {
            aspect: owner,
            names,
            dependencies,
            split_dependencies,
        });
    }
    let mut dependencies: BTreeMap<String, DependencyTarget> = BTreeMap::new();
    let specialized = cx.computed_attr_aspects(collection.clone(), base_key, &base_rule)?;
    let incoming: Vec<Arc<crate::aspects::AspectDefinition>> = specialized
        .used
        .iter()
        .map(|(used, _)| used.clone())
        .collect();
    let mut edges = cx.engine().rule_edges(&base_rule, base_key.config);
    edges.extend(cx.engine().native_aspect_edges(&base_rule));
    // A native base rule's dependency keys per attribute (native name),
    // in edge order: its `ctx.rule.attr` Targets.
    let mut native_attribute_keys: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for edge in edges {
        let attribute_aspects = base_rule
            .custom
            .as_ref()
            .and_then(|custom| custom.schema.get(&edge.attribute))
            .map(|spec| spec.declaration.aspects.clone())
            .unwrap_or_default();
        let attribute_aspects = cx
            .engine()
            .loaded_attribute_aspects(&base_rule, &attribute_aspects);
        let propagating =
            crate::aspects::propagating_aspects(&attribute_aspects, &incoming, &edge.attribute);
        let edge_configs = Engine::analyzed_edge_configurations_in(cx, base_key, &edge.cfg)?;
        // Aspects never apply in the no-config configuration
        // (AspectFunction.java:296-305: NonApplicableAspect).
        let propagating = if edge.cfg == EdgeCfg::NoConfig {
            Vec::new()
        } else {
            propagating
        };
        let dependency = cx.engine().resolve_rule_src(&base_rule, &edge.label)?;
        for edge_config in edge_configs.ids() {
            let (view, _) = match plan_dependency(cx, &dependency, edge_config, propagating.clone())
            {
                Err(error) if error == parallel::aspects::PENDING => {
                    pending = true;
                    continue;
                }
                planned => planned?,
            };
            let spec = base_rule
                .custom
                .as_ref()
                .and_then(|custom| custom.schema.get(&edge.attribute));
            let map_key = match &edge.cfg {
                EdgeCfg::Exec(_) => analysis::dependency_key(&dependency.to_string(), true),
                EdgeCfg::Starlark(attribute) => {
                    if base_rule
                        .custom
                        .as_ref()
                        .is_some_and(|custom| custom.split_dependencies.contains_key(attribute))
                    {
                        analysis::split_dependency_key(
                            &dependency.to_string(),
                            attribute,
                            edge_config,
                        )
                    } else {
                        analysis::transition_dependency_key(&dependency.to_string(), attribute)
                    }
                }
                EdgeCfg::NoConfig => analysis::metadata_dependency_key(&dependency.to_string()),
                _ => match spec {
                    Some(spec) if analysis::implicit_attribute(&edge.attribute, spec) => {
                        analysis::attribute_dependency_key(
                            &dependency.to_string(),
                            &edge.attribute,
                            spec,
                        )
                    }
                    _ => analysis::dependency_key(&dependency.to_string(), false),
                },
            };
            native_attribute_keys
                .entry(edge.attribute.clone())
                .or_default()
                .push(map_key.clone());
            dependencies.insert(map_key, view);
        }
    }
    // `ctx.rule.toolchains` (AspectFunction.getBaseTargetToolchainContexts,
    // AspectBaseTargetResolvedToolchainContext.load): per resolved type
    // of the base target, the providers of the aspects propagated to
    // its toolchain (`toolchains_aspects`, as phase 1 applied them)
    // and the toolchain's label; described like the aspect's own
    // contexts.
    let rule_toolchains = {
        let mut entries = BTreeMap::new();
        for (implementation, type_labels) in cx.engine().base_target_toolchains(base_key) {
            let propagating = crate::aspects::propagating_to_toolchain(&incoming, &type_labels);
            let mut providers = BTreeMap::new();
            if !propagating.is_empty() {
                let (_, applications) =
                    match plan_dependency(cx, &implementation, base_key.config, propagating) {
                        Err(error) if error == parallel::aspects::PENDING => {
                            pending = true;
                            continue;
                        }
                        planned => planned?,
                    };
                providers = crate::aspects::merge_providers(&BTreeMap::new(), &[], &applications)?
                    .with_groups();
            }
            let entry = crate::toolchain_info::ToolchainAspectsProviders {
                providers,
                label: implementation.clone(),
            }
            .owned()
            .map_err(|error| error.to_string())?;
            entries.entry(type_labels[0].clone()).or_insert(entry);
        }
        let engine = cx.engine();
        let requested = engine
            .toolchain_contexts
            .get(base_key)
            .map(|context| {
                context
                    .requested
                    .iter()
                    .map(|(label, info)| (label.clone(), info.type_label.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Arc::new(crate::toolchain_info::ToolchainContextData {
            target_description: format!(
                "aspect {} applied to {} rule {}",
                aspect.description(),
                base_rule.rule_class_name(),
                crate::rule_declarations::canonical_form(&base_rule.label)
            ),
            requested,
            toolchains: entries,
            repositories: engine.repositories.clone(),
        })
    };
    if pending {
        return Err(parallel::aspects::PENDING.to_owned());
    }
    // Run the implementation.
    let engine = cx.engine();
    let output_root = engine.configurations.output_root(base_key.config);
    let action_env = Arc::new(
        engine
            .configurations
            .get(base_key.config)
            .native
            .action_environment(),
    );
    let action_owner = key.action_owner(top);
    let console = crate::action_console::owner(&crate::action_console::OwnerParts {
        label: &base_key.label.to_string(),
        location: base_rule.location.as_deref(),
        target_kind: &format!("{} rule", base_rule.rule_class_name()),
        aspects: &collection.path_descriptions(&aspect.descriptor),
        tool: engine.configurations.get(base_key.config).native.is_exec,
        configuration: &base_key.config.to_string(),
    });
    cx.note_owner(&action_owner, console, base_key, &|| {
        collection
            .path_aspects(&aspect.descriptor)
            .iter()
            .rev()
            .map(|aspect| aquery::aspect_descriptor(aspect))
            .collect()
    });
    let engine = cx.engine();
    let loaded_functions = engine.loaded_functions.clone();
    let runfiles = engine.configured_runfiles_options(base_key.config);
    let run_under = engine.run_under_dependency_key(base_key.config);
    // The aspect's RuleContext is built in the aspect's configuration,
    // the base configured target's (AspectFunction.java:296-310,
    // ConfiguredTargetFactory.createAspect): `ctx.bin_dir`, `ctx.var`,
    // `ctx.configuration`, `ctx.fragments` and the target platform are
    // the base target's configuration's, never a default one.
    // The aspect's own ToolchainCollection (resolved by phase 1,
    // Self::aspect_toolchain_contexts): ResolvedToolchainContext.load of
    // each context, described `aspect A applied to KIND rule L`
    // (AspectFunction.java:468-485), behind ctx.toolchains,
    // ctx.exec_groups and the action factory's exec groups; the
    // execution table over NO exec_properties (an aspect's RuleContext
    // takes none, ConfiguredTargetFactory.createAspect:702).
    let toolchains = cx
        .aspect_toolchains(&(base_key.config, aspect.identity.clone()))
        .ok_or_else(|| {
            format!(
                "internal analysis order error: the toolchains of aspect {} in the configuration of {} were not resolved",
                aspect.identity, base_key.label
            )
        })?;
    let target_description = format!(
        "aspect {} applied to {} rule {}",
        aspect.identity,
        base_rule.rule_class_name(),
        crate::rule_declarations::canonical_form(&base_rule.label)
    );
    let default_toolchains = cx.with_reads(|reads| {
        cx.engine().load_toolchain_context(
            &base_rule,
            base_key.config,
            toolchains.default.as_ref(),
            &target_description,
            reads,
        )
    })?;
    let mut group_toolchains = Vec::new();
    for group in &toolchains.groups.groups {
        group_toolchains.push((
            group.name.clone(),
            crate::exec_groups::GroupToolchains::Resolved(Arc::new(cx.with_reads(|reads| {
                cx.engine().load_toolchain_context(
                    &base_rule,
                    base_key.config,
                    group.context.as_ref(),
                    &target_description,
                    reads,
                )
            })?)),
        ));
    }
    // RuleContext.fromToolchains (RuleContext.java:914-924): the
    // TemplateVariableInfo (`ctx.var`) of the aspect's resolved
    // toolchains, default context first.
    let mut template_variables = Vec::new();
    for context in std::iter::once(toolchains.default.as_ref())
        .chain(
            toolchains
                .groups
                .groups
                .iter()
                .map(|group| group.context.as_ref()),
        )
        .flatten()
    {
        for (_, implementation) in &context.resolved {
            let provider = cx.with_reads(|reads| {
                reads.provider(
                    &ConfiguredTargetKey::new(implementation.clone(), base_key.config),
                    crate::template_variable_info::PROVIDER_ID,
                )
            });
            if let Some(variables) = provider
                .as_ref()
                .and_then(|provider| crate::template_variable_info::variables(provider.value()))
            {
                template_variables.push(variables);
            }
        }
    }
    let engine = cx.engine();
    // ctx.toolchains (StarlarkRuleContext.java:877-898): under automatic
    // exec groups the requested types of the automatic groups, each
    // resolved in its own group.
    let use_auto = toolchains.groups.use_auto_exec_groups;
    let ctx_toolchains = if use_auto {
        let mut merged = crate::toolchain_info::ToolchainContextData {
            target_description: target_description.clone(),
            requested: Vec::new(),
            toolchains: BTreeMap::new(),
            repositories: engine.repositories.clone(),
        };
        for (name, toolchains) in &group_toolchains {
            if let crate::exec_groups::GroupToolchains::Resolved(data) = toolchains
                && crate::exec_groups::is_automatic(name)
            {
                for entry in &data.requested {
                    if !merged.requested.contains(entry) {
                        merged.requested.push(entry.clone());
                    }
                }
                for (type_label, info) in &data.toolchains {
                    merged
                        .toolchains
                        .entry(type_label.clone())
                        .or_insert_with(|| info.clone());
                }
            }
        }
        merged
    } else {
        default_toolchains
    };
    let mut action_groups = vec![(
        crate::exec_groups::DEFAULT_EXEC_GROUP_NAME.to_owned(),
        Vec::new(),
    )];
    action_groups.extend(
        toolchains
            .groups
            .groups
            .iter()
            .map(|group| (group.name.clone(), group.types.clone())),
    );
    let action_execution = engine
        .execution_table(
            base_key.config,
            &[],
            toolchains.default.as_ref(),
            Some(&toolchains.groups),
            &engine.display_form(&base_rule.label),
        )
        .map_err(|error| located(base_rule.location.as_deref(), error))?;
    let aspect_toolchains = analysis::AspectToolchains {
        toolchains: Arc::new(ctx_toolchains),
        exec_groups: Arc::new(crate::exec_groups::ExecGroupsData {
            target_description,
            groups: group_toolchains,
        }),
        action_exec_groups: crate::exec_groups::ActionExecGroups {
            groups: Some(action_groups),
            use_auto_exec_groups: use_auto,
            subrule: None,
            executions: Arc::from(action_execution.as_slice()),
        },
    };
    let mut analyzed_rule = base_rule.clone();
    let mut native_base = None;
    if analyzed_rule.custom.is_some() || analyzed_rule.native.is_some() {
        let platform_key = ConfiguredTargetKey::new(
            engine
                .configurations
                .get(base_key.config)
                .native
                .target_platform()
                .clone(),
            base_key.config,
        );
        let platform = cx.platform(&platform_key);
        if order_audit::enabled() {
            order_audit::note_platform_read(&platform_key, matches!(platform, Some(Ok(_))));
        }
        let platform = match platform {
            Some(Ok(platform)) => crate::platforms::TargetPlatform::Resolved(platform),
            _ => crate::platforms::TargetPlatform::Deferred,
        };
        let engine = cx.engine();
        let surface = Arc::new(engine.ctx_surface(
            &base_rule,
            base_key.config,
            &output_root,
            platform.clone(),
            template_variables,
        ));
        let cpp = crate::custom_rule::CppConfigurationView::of(
            &engine.configurations.get(base_key.config).native,
        );
        if let Some(custom) = analyzed_rule.custom.as_mut() {
            custom.cpp = cpp;
            custom.target_platform = platform;
            custom.surface = surface;
        } else if let Some(native) = &analyzed_rule.native {
            let args = engine
                .packages
                .get(&analyzed_rule.label.package_id())
                .map(|package| package.args.clone())
                .unwrap_or_default();
            // An explicit `visibility` value, simplified
            // (RuleVisibility.validateAndSimplify).
            let visibility = if analyzed_rule.explicit_visibility {
                analyzed_rule
                    .visibility
                    .declared_labels()
                    .iter()
                    .map(ToString::to_string)
                    .collect()
            } else {
                Vec::new()
            };
            let licenses = match native.attrs.get("licenses") {
                Some(crate::custom_rule::AttrValue::Strings(values)) => Some(values.clone()),
                _ => args.license.clone(),
            };
            native_base = Some(analysis::native_aspect::NativeAspectBase {
                surface,
                target_platform: platform,
                cpp,
                testonly: analyzed_rule.is_test_only(&args),
                deprecation: analyzed_rule.deprecation_warning(&args),
                licenses,
                visibility,
                attribute_keys: native_attribute_keys,
            });
        }
    }
    let repositories = cx.engine().repositories.clone();
    let application = analysis::AspectApplication {
        native: native_base,
        aspect_split_dependencies,
        aspect: aspect.clone(),
        aspect_ids: collection.path_descriptions(&aspect.descriptor),
        aspect_path: collection.path_descriptions(&aspect.descriptor),
        target: merged_target,
        rule: analyzed_rule,
        dependencies,
        aspect_dependencies,
        base_aspect_attributes,
        rule_toolchains,
        toolchains: aspect_toolchains,
        repositories,
    };
    let commit = AspectCommit {
        output_root,
        action_owner,
        action_env,
        runfiles,
        loaded_functions,
        run_under,
        base_key: base_key.clone(),
        base_rule,
        aspect_identity: aspect.identity.clone(),
        action_execution,
    };
    cx.analyze(&key, context, application, commit)
}

impl Engine {
    /// One dependency of an aspect application on the coordinator
    /// ([`plan_dependency`] over [`CoordAspects`]).
    pub(super) fn plan_aspect_dependency(
        &mut self,
        state: &mut PlanState<'_>,
        configured: &mut ConfiguredNodes,
        label: &Label,
        edge_config: ConfigId,
        aspects: Vec<Arc<crate::aspects::AspectDefinition>>,
    ) -> Planned {
        plan_dependency(
            &mut CoordAspects {
                engine: self,
                state,
                configured,
            },
            label,
            edge_config,
            aspects,
        )
    }
}
