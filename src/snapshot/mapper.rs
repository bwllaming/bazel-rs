//! Portable Args `map_each` functions (the analysis snapshot's P1; design:
//! docs/ANALYSIS_SNAPSHOT.md "Args map_each functions").
//!
//! A planned action whose Args expand a tree artifact at execution time
//! keeps its `map_each` callable (crate::command_line::Mapper): Bazel calls
//! it with every file of the tree, whose contents are known only once the
//! tree's producing action ran (StarlarkCustomCommandLine.applyMapEach). A
//! snapshot cannot hold the callable itself, nor its results. It holds a
//! description instead ([`MapperRef`]) from which the hit builds the very
//! same function again before anything executes:
//!
//! * the canonical label of the `.bzl` module whose evaluation created the
//!   function (its code map name), and
//! * either the module's top-level binding of that name (the function
//!   itself, [`Portable::Global`]), or — a nested `def` or `lambda`, as
//!   rules_js's `js_image_layer` passes with `allow_closure = True` — the
//!   function's code by its name and identifier span in that file, with
//!   what its `def` statement evaluated when it ran: the parameter defaults
//!   and the captured variables of the enclosing function
//!   ([`Portable::Def`], starlark::eval::PortableDef). Those values must be
//!   plain data ([`PortableValue`]: None, bool, int, string, list, tuple,
//!   dict, File, another portable function); anything else keeps the plan
//!   unsaved, as before (`map-each-deferred`).
//!
//! Soundness: the module's source files and everything its evaluation
//! read are inputs the snapshot validates (the analysis loaded the module;
//! [`settle`] loads it again inside the recording window), and a `.bzl`
//! module's evaluation is a deterministic function of them (no I/O, no
//! clock, no configuration). Evaluated again from the same sources it
//! compiles the same code at the same span, binds the same globals, and the
//! function instantiated from the description runs that code against them
//! with equal defaults and captured values: the same function, called by
//! the same expansion code at execution as without the snapshot. Planning
//! records a description only after checking, in the saving process, that
//!
//! 1. the function's globals are those of the module the invocation
//!    evaluated for that label (its BUILD-phase `.bzl` cache entry);
//! 2. the description instantiated against that module gives a function
//!    of the same code and module whose description is the same (a round
//!    trip of everything the instantiation takes);
//! 3. the module loads again — once inside a replay scope over the real
//!    repository set, which pins the repository roots and mappings its
//!    loads consult into the snapshot's pinned tables
//!    (crate::snapshot::replay), and once against a pinned set built from
//!    those tables, exactly as a hit loads it — and the description
//!    resolves there to a function with the same description.
//!
//! A hit ([`resolve_request`]) loads each module once against its pinned
//! set and resolves every description; any failure is a miss.

use crate::command_line::Mapper;
use crate::label::Label;
use crate::repository::RepositorySet;
use crate::request::PlannedRequest;
use crate::starlark_types::File;
use serde::{Deserialize, Serialize};
use starlark::environment::{FrozenModule, Module};
use starlark::eval::{PortableDef, PortableParamKind};
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::{AllocList, ListRef};
use starlark::values::tuple::{AllocTuple, TupleRef};
use starlark::values::{FrozenHeapName, FrozenValue, Heap, OwnedFrozenValue, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Portable functions nested in a captured value, at most.
const MAX_DEPTH: usize = 8;

/// A `map_each` function by reference: its defining module and how the
/// function is found or instantiated there.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct MapperRef {
    /// The canonical label of the `.bzl` file (its code map name).
    pub module: String,
    pub function: Portable,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum Portable {
    /// The module's top-level binding of this name is the function.
    Global(String),
    /// A `def` or `lambda` instantiated from the module's compiled code.
    Def(PortableDefData),
}

/// starlark::eval::PortableDef over [`PortableValue`]s (its file is the
/// module's label).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct PortableDefData {
    pub name: String,
    pub begin: u32,
    pub end: u32,
    pub function_name: String,
    pub params: Vec<(String, ParamKind)>,
    pub num_positional_only: u32,
    pub num_positional: u32,
    pub args: Option<u32>,
    pub kwargs: Option<u32>,
    pub captured: Vec<Option<PortableValue>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum ParamKind {
    Required,
    Optional,
    Defaulted(PortableValue),
    Args,
    Kwargs,
}

/// A captured variable's or default's value.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum PortableValue {
    None,
    Bool(bool),
    Int(i32),
    Str(String),
    List(Vec<PortableValue>),
    Tuple(Vec<PortableValue>),
    Dict(Vec<(PortableValue, PortableValue)>),
    File(#[serde(with = "crate::snapshot::codec::file")] File),
    Function(Box<MapperRef>),
}

/// Loads the frozen module of a label.
type LoadModule<'a> = Box<dyn FnMut(&Label) -> Result<FrozenModule, String> + 'a>;

/// The frozen modules descriptions are taken apart against or resolved
/// in, by label (each loaded once).
struct Modules<'a> {
    load: LoadModule<'a>,
    loaded: HashMap<String, Result<FrozenModule, String>>,
}

impl<'a> Modules<'a> {
    fn new(load: impl FnMut(&Label) -> Result<FrozenModule, String> + 'a) -> Self {
        Self {
            load: Box::new(load),
            loaded: HashMap::new(),
        }
    }

    fn get(&mut self, file: &str) -> Result<FrozenModule, String> {
        if let Some(loaded) = self.loaded.get(file) {
            return loaded.clone();
        }
        let loaded = module_label(file).and_then(|label| (self.load)(&label));
        self.loaded.insert(file.to_owned(), loaded.clone());
        loaded
    }
}

/// The canonical `.bzl` label a function's code map names (BUILD-phase
/// loads parse a module under its label's string, crate::loading).
fn module_label(file: &str) -> Result<Label, String> {
    let label = Label::parse_canonical(file)
        .map_err(|_| format!("{file:?} is not the canonical label of a .bzl module"))?;
    if label.to_string() != file || !label.name.ends_with(".bzl") {
        return Err(format!(
            "{file:?} is not the canonical label of a .bzl module"
        ));
    }
    Ok(label)
}

/// Describe the frozen function `value` against `modules` (those it was
/// created by).
fn describe(
    value: FrozenValue,
    modules: &mut Modules<'_>,
    depth: usize,
) -> Result<MapperRef, String> {
    if depth > MAX_DEPTH {
        return Err(format!(
            "functions captured more than {MAX_DEPTH} levels deep"
        ));
    }
    let def = PortableDef::of(value)?;
    let module = modules.get(&def.file)?;
    if !starlark::eval::def_module_is(value, &module) {
        return Err(format!(
            "function {} was not created by the module {} this invocation evaluated",
            def.name, def.file
        ));
    }
    let global = module
        .get_any_visibility(&def.name)
        .ok()
        .and_then(|(binding, _)| binding.value().unpack_frozen())
        .is_some_and(|binding| binding.to_value().ptr_eq(value.to_value()));
    let function = if global {
        Portable::Global(def.name)
    } else {
        let mut params = Vec::with_capacity(def.params.len());
        for (name, kind) in def.params {
            params.push((
                name,
                match kind {
                    PortableParamKind::Required => ParamKind::Required,
                    PortableParamKind::Optional => ParamKind::Optional,
                    PortableParamKind::Defaulted(value) => {
                        ParamKind::Defaulted(describe_value(value.to_value(), modules, depth)?)
                    }
                    PortableParamKind::Args => ParamKind::Args,
                    PortableParamKind::Kwargs => ParamKind::Kwargs,
                },
            ));
        }
        let mut captured = Vec::with_capacity(def.captured.len());
        for value in def.captured {
            captured.push(match value {
                None => None,
                Some(value) => Some(describe_value(value.to_value(), modules, depth)?),
            });
        }
        Portable::Def(PortableDefData {
            name: def.name,
            begin: def.begin,
            end: def.end,
            function_name: def.function_name,
            params,
            num_positional_only: def.num_positional_only,
            num_positional: def.num_positional,
            args: def.args,
            kwargs: def.kwargs,
            captured,
        })
    };
    Ok(MapperRef {
        module: def.file,
        function,
    })
}

fn describe_value(
    value: Value<'_>,
    modules: &mut Modules<'_>,
    depth: usize,
) -> Result<PortableValue, String> {
    if value.is_none() {
        return Ok(PortableValue::None);
    }
    if let Some(boolean) = value.unpack_bool() {
        return Ok(PortableValue::Bool(boolean));
    }
    if let Some(int) = value.unpack_i32() {
        return Ok(PortableValue::Int(int));
    }
    if let Some(text) = value.unpack_str() {
        return Ok(PortableValue::Str(text.to_owned()));
    }
    if let Some(file) = File::from_value(value) {
        return Ok(PortableValue::File(file.clone()));
    }
    let items = |values: &mut dyn Iterator<Item = Value<'_>>,
                 modules: &mut Modules<'_>|
     -> Result<Vec<PortableValue>, String> {
        values
            .map(|value| describe_value(value, modules, depth))
            .collect()
    };
    if let Some(list) = ListRef::from_value(value) {
        return items(&mut list.iter(), modules).map(PortableValue::List);
    }
    if let Some(tuple) = TupleRef::from_value(value) {
        return items(&mut tuple.iter(), modules).map(PortableValue::Tuple);
    }
    if let Some(dict) = DictRef::from_value(value) {
        let mut entries = Vec::with_capacity(dict.len());
        for (key, value) in dict.iter() {
            entries.push((
                describe_value(key, modules, depth)?,
                describe_value(value, modules, depth)?,
            ));
        }
        return Ok(PortableValue::Dict(entries));
    }
    if let Some(frozen) = value.unpack_frozen()
        && PortableDef::of(frozen).is_ok()
    {
        return describe(frozen, modules, depth + 1)
            .map(|function| PortableValue::Function(Box::new(function)));
    }
    Err(format!(
        "a captured or default value of type '{}' has no portable form",
        value.get_type()
    ))
}

/// The function `reference` describes, in `modules`.
fn resolve(reference: &MapperRef, modules: &mut Modules<'_>) -> Result<OwnedFrozenValue, String> {
    let module = modules.get(&reference.module)?;
    match &reference.function {
        Portable::Global(name) => {
            let (value, _) = module
                .get_any_visibility(name)
                .map_err(|error| format!("{}: {error}", reference.module))?;
            let frozen = value.value().unpack_frozen().ok_or("an unfrozen global")?;
            PortableDef::of(frozen)?;
            Ok(value)
        }
        Portable::Def(def) => Module::with_temp_heap(|temp| {
            let heap = temp.heap();
            let mut params = Vec::with_capacity(def.params.len());
            for (name, kind) in &def.params {
                params.push((
                    name.clone(),
                    match kind {
                        ParamKind::Required => PortableParamKind::Required,
                        ParamKind::Optional => PortableParamKind::Optional,
                        ParamKind::Defaulted(value) => {
                            PortableParamKind::Defaulted(alloc_value(value, heap, modules)?)
                        }
                        ParamKind::Args => PortableParamKind::Args,
                        ParamKind::Kwargs => PortableParamKind::Kwargs,
                    },
                ));
            }
            let mut captured = Vec::with_capacity(def.captured.len());
            for value in &def.captured {
                captured.push(match value {
                    None => None,
                    Some(value) => Some(alloc_value(value, heap, modules)?),
                });
            }
            let portable = PortableDef {
                name: def.name.clone(),
                file: reference.module.clone(),
                begin: def.begin,
                end: def.end,
                function_name: def.function_name.clone(),
                params,
                num_positional_only: def.num_positional_only,
                num_positional: def.num_positional,
                args: def.args,
                kwargs: def.kwargs,
                captured,
            };
            let function = portable.instantiate(&module, &temp)?;
            temp.set("map_each", function);
            let frozen = temp
                .freeze_named(FrozenHeapName::User(Box::new(format!(
                    "map_each:{}",
                    reference.module
                ))))
                .map_err(|error| format!("cannot freeze the map_each function: {error:?}"))?;
            frozen
                .get_any_visibility("map_each")
                .map(|(value, _)| value)
                .map_err(|error| error.to_string())
        }),
    }
}

fn alloc_value<'v>(
    value: &PortableValue,
    heap: Heap<'v>,
    modules: &mut Modules<'_>,
) -> Result<Value<'v>, String> {
    Ok(match value {
        PortableValue::None => Value::new_none(),
        PortableValue::Bool(boolean) => Value::new_bool(*boolean),
        PortableValue::Int(int) => heap.alloc(*int),
        PortableValue::Str(text) => heap.alloc_str(text).to_value(),
        PortableValue::File(file) => crate::starlark_types::alloc_file(heap, file.clone()),
        PortableValue::List(items) => {
            let items = items
                .iter()
                .map(|item| alloc_value(item, heap, modules))
                .collect::<Result<Vec<_>, _>>()?;
            heap.alloc(AllocList(items))
        }
        PortableValue::Tuple(items) => {
            let items = items
                .iter()
                .map(|item| alloc_value(item, heap, modules))
                .collect::<Result<Vec<_>, _>>()?;
            heap.alloc(AllocTuple(items))
        }
        PortableValue::Dict(entries) => {
            let mut pairs = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                let key = alloc_value(key, heap, modules)?;
                key.get_hashed()
                    .map_err(|_| format!("an unhashable dict key of type '{}'", key.get_type()))?;
                pairs.push((key, alloc_value(value, heap, modules)?));
            }
            heap.alloc(AllocDict(pairs))
        }
        PortableValue::Function(reference) => {
            let function = resolve(reference, modules)?;
            heap.access_owned_frozen_value(&function)
        }
    })
}

/// Every module label a description loads.
fn module_labels(reference: &MapperRef, labels: &mut Vec<String>) {
    fn value_labels(value: &PortableValue, labels: &mut Vec<String>) {
        match value {
            PortableValue::List(items) | PortableValue::Tuple(items) => {
                items.iter().for_each(|item| value_labels(item, labels));
            }
            PortableValue::Dict(entries) => entries.iter().for_each(|(key, value)| {
                value_labels(key, labels);
                value_labels(value, labels);
            }),
            PortableValue::Function(reference) => module_labels(reference, labels),
            _ => {}
        }
    }
    if !labels.contains(&reference.module) {
        labels.push(reference.module.clone());
    }
    if let Portable::Def(def) = &reference.function {
        for (_, kind) in &def.params {
            if let ParamKind::Defaulted(value) = kind {
                value_labels(value, labels);
            }
        }
        for value in def.captured.iter().flatten() {
            value_labels(value, labels);
        }
    }
}

/// The request's deferred `map_each` mappers with their actions' ids, in
/// plan order.
fn mappers(request: &PlannedRequest) -> Vec<(&str, &Mapper)> {
    let mut mappers = Vec::new();
    for node in &request.nodes {
        if let Some(deferred) = &node.deferred {
            for mapper in deferred.mappers() {
                mappers.push((node.action.id.as_str(), mapper));
            }
        }
    }
    mappers
}

/// A repository set pinned to `tables` (as a hit's replay builds it).
fn pinned_set(
    tables: crate::snapshot::replay::PinnedTables,
    options: crate::module_graph::ResolverOptions,
    environment: crate::repository_environment::Snapshot,
    downloads_enabled: bool,
) -> Arc<RepositorySet> {
    Arc::new(RepositorySet::pinned(
        tables,
        options,
        environment,
        downloads_enabled,
    ))
}

/// What [`settle`] did (EXPLAIN and `PERF-SNAPSHOT map_each`).
#[derive(Debug, Default)]
pub(crate) struct Settled {
    pub mappers: usize,
    pub portable: usize,
    pub modules: usize,
    /// The first unportable mapper's action and reason.
    pub first_reason: Option<(String, String)>,
    pub elapsed: Duration,
}

/// Give every deferred `map_each` of `request` its portable description
/// once the checks of the module doc (1–3) pass, so the request encodes;
/// a mapper that fails any keeps none (the plan's `map-each-deferred`
/// blocker). Runs at the end of planning, inside the recording window,
/// only while the analysis snapshot records for replay (its pinned tables
/// carry what the hit needs); nothing to do without deferred mappers.
pub(crate) fn settle(request: &PlannedRequest, repositories: &Arc<RepositorySet>) -> Settled {
    let started = Instant::now();
    let mappers = mappers(request);
    let mut settled = Settled {
        mappers: mappers.len(),
        ..Settled::default()
    };
    if mappers.is_empty() {
        return settled;
    }
    let capture = crate::snapshot::replay::global();
    let fail = |settled: &mut Settled, id: &str, reason: String| {
        if settled.first_reason.is_none() {
            settled.first_reason = Some((id.to_owned(), reason));
        }
    };
    let Some(capture) = capture.filter(|_| repositories.note_replay_context()) else {
        fail(
            &mut settled,
            mappers[0].0,
            "the analysis snapshot does not record this run".to_owned(),
        );
        settled.elapsed = started.elapsed();
        return settled;
    };
    // 1, 2: described against the modules this invocation evaluated, and
    // instantiated there again to the same code and description.
    let mut analysis = Modules::new(|label: &Label| {
        repositories
            .build_bzl_cache()
            .module(label)
            .ok_or_else(|| format!("{label} is not a module this invocation's BUILD files loaded"))
    });
    let mut candidates: Vec<(&str, &Mapper, Arc<MapperRef>)> = Vec::new();
    let mut described: HashMap<MapperRef, Arc<MapperRef>> = HashMap::new();
    for (id, mapper) in &mappers {
        let checked = (|| {
            let function = mapper.function()?;
            let frozen = function
                .value()
                .unpack_frozen()
                .ok_or("an unfrozen map_each function")?;
            let reference = describe(frozen, &mut analysis, 0)?;
            let again = resolve(&reference, &mut analysis)?;
            let again_frozen = again
                .value()
                .unpack_frozen()
                .ok_or("an unfrozen function")?;
            if !starlark::eval::def_same_code(frozen, again_frozen)
                || describe(again_frozen, &mut analysis, 0)? != reference
            {
                return Err(format!(
                    "the description of {} does not instantiate to the same function",
                    reference.module
                ));
            }
            Ok::<_, String>(reference)
        })();
        match checked {
            Ok(reference) => {
                let shared = described
                    .entry(reference.clone())
                    .or_insert_with(|| Arc::new(reference))
                    .clone();
                candidates.push((id, mapper, shared));
            }
            Err(reason) => fail(&mut settled, id, reason),
        }
    }
    drop(analysis);
    // 3: the modules load again — pinning the lookups, then against the
    // pinned set as a hit loads them — and every description resolves
    // there to itself.
    let mut labels = Vec::new();
    for reference in described.values() {
        module_labels(reference, &mut labels);
    }
    labels.sort();
    settled.modules = labels.len();
    let mut pin_failures: BTreeMap<String, String> = BTreeMap::new();
    for file in &labels {
        let pinned = module_label(file).and_then(|label| {
            let _scope = crate::snapshot::replay::scope();
            crate::loading::load_build_module_private(repositories.clone(), &label)
        });
        if let Err(error) = pinned {
            pin_failures.insert(file.clone(), error);
        }
    }
    let hit_set = match (capture.tables(), capture.context()) {
        (Some(tables), Some(context)) => Some(pinned_set(
            tables,
            context.options.clone(),
            context.environment.fresh(),
            context.downloads_enabled,
        )),
        _ => None,
    };
    let mut hit_modules = Modules::new(|label: &Label| match &hit_set {
        Some(set) => crate::loading::load_build_module_private(set.clone(), label),
        None => Err("the replay capture has no pinned tables".to_owned()),
    });
    let mut verdicts: HashMap<*const MapperRef, Result<(), String>> = HashMap::new();
    for (id, mapper, reference) in candidates {
        let verdict = verdicts
            .entry(Arc::as_ptr(&reference))
            .or_insert_with(|| {
                let mut labels = Vec::new();
                module_labels(&reference, &mut labels);
                if let Some((file, error)) = labels
                    .iter()
                    .find_map(|file| pin_failures.get(file).map(|error| (file, error)))
                {
                    return Err(format!("{file} does not load again: {error}"));
                }
                let resolved = resolve(&reference, &mut hit_modules)
                    .map_err(|error| format!("as a hit loads it: {error}"))?;
                let frozen = resolved
                    .value()
                    .unpack_frozen()
                    .ok_or("an unfrozen function")?;
                if describe(frozen, &mut hit_modules, 0)? != *reference {
                    return Err(format!(
                        "the description of {} resolves to another function as a hit loads it",
                        reference.module
                    ));
                }
                Ok(())
            })
            .clone();
        match verdict {
            Ok(()) => {
                mapper.set_portable(reference);
                settled.portable += 1;
            }
            Err(reason) => fail(&mut settled, id, reason),
        }
    }
    settled.elapsed = started.elapsed();
    settled
}

/// What [`resolve_request`] did.
#[derive(Debug, Default)]
pub(crate) struct Resolved {
    pub mappers: usize,
    pub functions: usize,
    pub modules: usize,
    pub elapsed: Duration,
}

/// Resolve every decoded `map_each` of `request` (a snapshot hit): each
/// defining module loaded once against a repository set pinned to the
/// snapshot's tables with this command's options and environment, each
/// description instantiated there. Any failure is the hit's miss.
pub(crate) fn resolve_request(
    request: &PlannedRequest,
    tables: Option<&crate::snapshot::replay::PinnedTables>,
    replay: &crate::snapshot::hit::ReplayInputs,
) -> Result<Resolved, String> {
    let started = Instant::now();
    let mappers = mappers(request);
    let mut resolved = Resolved {
        mappers: mappers.len(),
        ..Resolved::default()
    };
    if mappers.is_empty() {
        return Ok(resolved);
    }
    let tables = tables.ok_or("map_each functions without pinned tables")?;
    let set = pinned_set(
        tables.clone(),
        replay.options.clone(),
        crate::repository_environment::Snapshot::with_reader(Arc::new(
            crate::repository_environment::FixedReader::new(replay.environment.iter().cloned()),
        )),
        replay.downloads_enabled,
    );
    let mut modules =
        Modules::new(|label: &Label| crate::loading::load_build_module_private(set.clone(), label));
    let mut functions: HashMap<&MapperRef, OwnedFrozenValue> = HashMap::new();
    for (id, mapper) in mappers {
        let reference = mapper
            .portable()
            .ok_or_else(|| format!("action '{id}': a map_each without a description"))?;
        let function = match functions.get(&**reference) {
            Some(function) => function.clone(),
            None => {
                let function = resolve(reference, &mut modules).map_err(|error| {
                    format!("action '{id}': its map_each does not resolve: {error}")
                })?;
                functions.insert(reference, function.clone());
                function
            }
        };
        mapper.resolve(function)?;
    }
    resolved.functions = functions.len();
    resolved.modules = modules.loaded.len();
    resolved.elapsed = started.elapsed();
    Ok(resolved)
}

/// The functions of `request`'s mappers in plan order (the round-trip
/// switch's hand-over: [`fill_decoded`] gives them to the decoded request).
pub(crate) fn functions(request: &PlannedRequest) -> Vec<Option<OwnedFrozenValue>> {
    mappers(request)
        .into_iter()
        .map(|(_, mapper)| mapper.function().ok().cloned())
        .collect()
}

/// Resolve a decoded request's mappers with the planned request's
/// functions ([`functions`], the same plan order).
pub(crate) fn fill_decoded(
    request: &PlannedRequest,
    functions: Vec<Option<OwnedFrozenValue>>,
) -> Result<(), String> {
    let mappers = mappers(request);
    if mappers.len() != functions.len() {
        return Err("internal error: the decoded request has other map_each functions".to_owned());
    }
    for ((_, mapper), function) in mappers.into_iter().zip(functions) {
        if let Some(function) = function {
            mapper.resolve(function)?;
        }
    }
    Ok(())
}

/// EXPLAIN names the first unportable `map_each` and why; `BAZEL_RS_TIMING`
/// prints `PERF-SNAPSHOT map_each settle ...`.
pub(crate) fn report_settled(settled: &Settled) {
    if settled.mappers == 0 {
        return;
    }
    if crate::snapshot::explain()
        && let Some((id, reason)) = &settled.first_reason
    {
        crate::observed_fs::diag(&format!(
            "map_each of action '{id}' is not portable ({} of {} portable): {reason}",
            settled.portable, settled.mappers
        ));
    }
    if crate::diagnostic_switch(std::env::var_os("BAZEL_RS_TIMING")) {
        crate::snapshot::diag::line_fmt(format_args!(
            "PERF-SNAPSHOT map_each settle mappers={} portable={} modules={} ms={:.3}",
            settled.mappers,
            settled.portable,
            settled.modules,
            settled.elapsed.as_secs_f64() * 1000.0
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::Globals;
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};

    const LABEL: &str = "//pkg:defs.bzl";

    const SOURCE: &str = r#"
_TPL = "%s|%s|%s"

def top(f):
    return "top:" + f

def _helper(prefix):
    return lambda f: prefix + "-" + f

def make(prefix, n):
    table = {"k": [1, (True, None)], "n": n}
    shout = _helper(prefix.upper())
    def entry(f, sep = "/", *rest, **named):
        return _TPL % (prefix, f, str(table)) + sep + shout(f)
    return entry

def make_lambda(prefix):
    return lambda f: prefix + f

def make_unportable():
    r = range(3)
    return lambda f: f + str(len(r))
"#;

    fn evaluate(source: &str) -> FrozenModule {
        let ast = AstModule::parse(LABEL, source.to_owned(), &Dialect::Standard).unwrap();
        Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            eval.eval_module(ast, &Globals::standard()).unwrap();
            drop(eval);
            module.freeze().unwrap()
        })
    }

    /// Call `function` of `module` with `arguments` (strings and ints) and
    /// freeze the result.
    fn call(module: &FrozenModule, function: &str, arguments: &[&str]) -> OwnedFrozenValue {
        let function = module.get(function).unwrap();
        Module::with_temp_heap(|temp| {
            let mut eval = Evaluator::new(&temp);
            let arguments: Vec<Value> = arguments
                .iter()
                .map(|argument| match argument.parse::<i32>() {
                    Ok(int) => temp.heap().alloc(int),
                    Err(_) => temp.heap().alloc_str(argument).to_value(),
                })
                .collect();
            let function = temp.heap().access_owned_frozen_value(&function);
            let result = eval.eval_function(function, &arguments, &[]).unwrap();
            drop(eval);
            temp.set("result", result);
            temp.freeze().unwrap().get("result").unwrap()
        })
    }

    fn apply(function: &OwnedFrozenValue, argument: &str) -> String {
        Module::with_temp_heap(|temp| {
            let mut eval = Evaluator::new(&temp);
            let argument = temp.heap().alloc_str(argument).to_value();
            let function = temp.heap().access_owned_frozen_value(function);
            eval.eval_function(function, &[argument], &[])
                .unwrap()
                .unpack_str()
                .unwrap()
                .to_owned()
        })
    }

    fn modules(module: FrozenModule) -> Modules<'static> {
        Modules::new(move |label: &Label| {
            assert_eq!(label.to_string(), LABEL);
            Ok(module.clone())
        })
    }

    fn frozen(value: &OwnedFrozenValue) -> FrozenValue {
        value.value().unpack_frozen().unwrap()
    }

    /// A closure, a lambda and a top-level function, described against the
    /// module that created them, resolve in another evaluation of the same
    /// source to functions with the same results and descriptions.
    #[test]
    fn descriptions_resolve_to_the_same_functions_in_another_evaluation() {
        let first = evaluate(SOURCE);
        let second = evaluate(SOURCE);
        let mut created = modules(first.clone());
        let mut loaded = modules(second.clone());
        let cases = [
            call(&first, "make", &["pre", "-3"]),
            call(&first, "make_lambda", &["pre"]),
            first.get("top").unwrap(),
        ];
        for original in &cases {
            let reference = describe(frozen(original), &mut created, 0).unwrap();
            let resolved = resolve(&reference, &mut loaded).unwrap();
            assert_eq!(apply(original, "x"), apply(&resolved, "x"));
            assert!(!starlark::eval::def_same_code(
                frozen(original),
                frozen(&resolved)
            ));
            assert_eq!(
                describe(frozen(&resolved), &mut loaded, 0).unwrap(),
                reference
            );
            // In the creating module: the same code.
            let again = resolve(&reference, &mut created).unwrap();
            assert!(starlark::eval::def_same_code(
                frozen(original),
                frozen(&again)
            ));
        }
        let closure = describe(frozen(&cases[0]), &mut created, 0).unwrap();
        let Portable::Def(def) = &closure.function else {
            panic!("{closure:?}")
        };
        assert_eq!(def.name, "entry");
        assert_eq!(
            def.params[1],
            (
                "sep".to_owned(),
                ParamKind::Defaulted(PortableValue::Str("/".into()))
            )
        );
        assert_eq!(def.args, Some(2));
        assert_eq!(def.kwargs, Some(3));
        assert!(
            def.captured
                .iter()
                .flatten()
                .any(|value| matches!(value, PortableValue::Function(_)))
        );
        assert_eq!(
            apply(&cases[0], "x"),
            "pre|x|{\"k\": [1, (True, None)], \"n\": -3}/PRE-x"
        );
        assert_eq!(
            describe(frozen(&cases[2]), &mut created, 0)
                .unwrap()
                .function,
            Portable::Global("top".into())
        );
        // The encoding round-trips.
        let bytes = postcard::to_stdvec(&closure).unwrap();
        assert_eq!(postcard::from_bytes::<MapperRef>(&bytes).unwrap(), closure);
    }

    /// No portable form: a captured range; a function of another module
    /// than the one the description names; code that moved.
    #[test]
    fn unportable_functions_and_moved_code_are_refused() {
        let first = evaluate(SOURCE);
        let error = describe(
            frozen(&call(&first, "make_unportable", &[])),
            &mut modules(first.clone()),
            0,
        )
        .unwrap_err();
        assert!(error.contains("'range'"), "{error}");
        let other = evaluate(SOURCE);
        let error = describe(
            frozen(&call(&first, "make_lambda", &["p"])),
            &mut modules(other),
            0,
        )
        .unwrap_err();
        assert!(error.contains("was not created by the module"), "{error}");
        let reference = describe(
            frozen(&call(&first, "make_lambda", &["p"])),
            &mut modules(first.clone()),
            0,
        )
        .unwrap();
        let moved = evaluate(&format!("# moved\n{SOURCE}"));
        let error = resolve(&reference, &mut modules(moved)).unwrap_err();
        assert!(error.contains("compiled no function lambda"), "{error}");
    }
}
