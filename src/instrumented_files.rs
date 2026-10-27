//! The analysis-time coverage API (docs/COVERAGE.md):
//! `coverage_common.instrumented_files_info` and the `InstrumentedFilesInfo`
//! provider.
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Paths below are relative to `src/main/java/com/google/devtools/build/lib/`.
//!
//! CoverageCommon.instrumentedFilesInfo (analysis/test/CoverageCommon.java:47-157)
//! validates its arguments and then InstrumentedFilesCollector.collect
//! (analysis/test/InstrumentedFilesCollector.java:94-148) returns the shared
//! `InstrumentedFilesInfo.EMPTY` (analysis/test/InstrumentedFilesInfo.java:34-41)
//! whenever coverage is disabled; in a coverage configuration it collects
//! the dependency attributes' providers and the instrumented local sources
//! ([`Builder`]), with a BaselineCoverageAction. Native rules add the
//! provider through the same collector (Filegroup.java:63-108,
//! GenRuleBase.java:234-239; crate::engine), an output file target answers
//! with its generating rule's provider or EMPTY
//! (OutputFileConfiguredTarget.java:100-107).
use std::fmt;
use std::hash::Hash;
use std::sync::{Arc, OnceLock};

use allocative::Allocative;
use starlark::collections::StarlarkHasher;
use starlark::environment::{GlobalsBuilder, Methods, MethodsBuilder};
use starlark::eval::{Arguments, Evaluator};
use starlark::values::dict::DictRef;
use starlark::values::{
    Heap, NoSerialize, OwnedFrozenValue, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue,
    Value, starlark_value,
};
use starlark::{starlark_module, starlark_simple_value};

use crate::language_modules::{Param, failure, param, type_error};
use crate::starlark_types::{Depset, DepsetNode, File, Item, ItemType, Order};

/// The provider key of InstrumentedFilesInfo (InstrumentedFilesProvider,
/// a BuiltinProvider named `InstrumentedFilesInfo`).
pub(crate) const PROVIDER_ID: &str = "builtin:InstrumentedFilesInfo";

// Only the upstream derives generate unsafe marker implementations; both
// values own no Starlark heap data.
#[allow(unsafe_code)]
mod owned {
    use super::*;

    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct Constructor;

    /// `InstrumentedFilesInfo.EMPTY` (`data` is `None`: coverage disabled,
    /// or nothing collected), or a collected instance of a coverage
    /// configuration.
    #[derive(Clone, Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(super) struct Info {
        #[allocative(skip)]
        pub(super) data: Option<Arc<Collected>>,
    }
}
use owned::{Constructor, Info};

starlark_simple_value!(Constructor);
starlark_simple_value!(Info);

/// The fields of a collected InstrumentedFilesInfo
/// (analysis/test/InstrumentedFilesInfo.java:43-96).
#[derive(Clone, Debug)]
pub(crate) struct Collected {
    pub instrumented: Depset,
    pub metadata: Depset,
    pub baseline: Depset,
    pub support: Depset,
    /// `buildKeepingLast` of the environments, in insertion order.
    pub environment: Vec<(String, String)>,
    pub reported_to_actual: Depset,
}

/// InstrumentedFilesCollector.InstrumentedFilesInfoBuilder
/// (analysis/test/InstrumentedFilesCollector.java:223-317): stable-order
/// sets of the dependencies' sets and the target's own items.
#[derive(Default)]
pub(crate) struct Builder {
    instrumented: Vec<Arc<DepsetNode>>,
    local: Vec<File>,
    metadata: (Vec<File>, Vec<Arc<DepsetNode>>),
    baseline: (Vec<File>, Vec<Arc<DepsetNode>>),
    support: Vec<Arc<DepsetNode>>,
    environment: Vec<(String, String)>,
    reported_to_actual: Option<Depset>,
}

impl Builder {
    /// addFromDependency (:262-271).
    pub(crate) fn add_from(&mut self, info: Option<&Collected>) {
        let Some(info) = info else { return };
        self.instrumented.push(info.instrumented.node.clone());
        self.metadata.1.push(info.metadata.node.clone());
        self.baseline.1.push(info.baseline.node.clone());
        self.support.push(info.support.node.clone());
        self.environment.extend(info.environment.iter().cloned());
    }

    pub(crate) fn add_support(&mut self, support: Depset) {
        self.support.push(support.node);
    }

    pub(crate) fn add_environment(&mut self, environment: Vec<(String, String)>) {
        self.environment.extend(environment);
    }

    pub(crate) fn set_local_sources(&mut self, local: Vec<File>) {
        let mut seen = std::collections::BTreeSet::new();
        self.local = local
            .into_iter()
            .filter(|file| seen.insert(file.clone()))
            .collect();
    }

    pub(crate) fn local_sources(&self) -> &[File] {
        &self.local
    }

    pub(crate) fn add_baseline(&mut self, files: Vec<File>) {
        self.baseline.0.extend(files);
    }

    pub(crate) fn add_metadata(&mut self, files: Vec<File>) {
        self.metadata.0.extend(files);
    }

    pub(crate) fn set_reported_to_actual(&mut self, set: Depset) {
        self.reported_to_actual = Some(set);
    }

    /// build (:290-316).
    pub(crate) fn build(self) -> Collected {
        let files = |direct: Vec<File>, children: Vec<Arc<DepsetNode>>| {
            Depset::compose_framework(
                direct.into_iter().map(Item::File).collect(),
                children,
                Order::Default,
                Some(ItemType::File),
            )
            .unwrap_or_else(|_| Depset::from_files(Vec::new()))
        };
        let mut instrumented = self.instrumented;
        if !self.local.is_empty() {
            instrumented.push(Depset::from_files(self.local).node);
        }
        // ImmutableMap.Builder.buildKeepingLast: the first position of each
        // key, the last value.
        let mut environment: Vec<(String, String)> = Vec::new();
        for (key, value) in self.environment {
            match environment
                .iter_mut()
                .find(|(existing, _)| *existing == key)
            {
                Some(entry) => entry.1 = value,
                None => environment.push((key, value)),
            }
        }
        Collected {
            instrumented: files(Vec::new(), instrumented),
            metadata: files(self.metadata.0, self.metadata.1),
            baseline: files(self.baseline.0, self.baseline.1),
            support: files(Vec::new(), self.support),
            environment,
            reported_to_actual: self
                .reported_to_actual
                .unwrap_or_else(|| Depset::from_files(Vec::new())),
        }
    }
}

/// TestingBootstrap binds `InstrumentedFilesInfo`
/// (starlarkbuildapi/test/TestingBootstrap.java:46-52).
pub(crate) fn install(builder: &mut GlobalsBuilder) {
    builder.set("InstrumentedFilesInfo", Constructor);
}

pub(crate) fn is_constructor(value: Value<'_>) -> bool {
    Constructor::from_value(value).is_some()
}

pub(crate) fn is_instance(value: Value<'_>) -> bool {
    Info::from_value(value).is_some()
}

/// The collected fields of an InstrumentedFilesInfo value (`None` for
/// EMPTY or another value).
pub(crate) fn collected(value: Value<'_>) -> Option<Arc<Collected>> {
    Info::from_value(value).and_then(|info| info.data.clone())
}

/// The frozen `InstrumentedFilesInfo.EMPTY` of native rule and output-file
/// targets.
pub(crate) fn empty_frozen() -> Result<OwnedFrozenValue, String> {
    static EMPTY: OnceLock<Result<OwnedFrozenValue, String>> = OnceLock::new();
    EMPTY.get_or_init(|| freeze(None)).clone()
}

/// A frozen InstrumentedFilesInfo of collected fields (a native rule's in a
/// coverage configuration).
pub(crate) fn frozen(collected: Collected) -> Result<OwnedFrozenValue, String> {
    freeze(Some(Arc::new(collected)))
}

fn freeze(data: Option<Arc<Collected>>) -> Result<OwnedFrozenValue, String> {
    starlark::environment::Module::with_temp_heap(|module| {
        module.set("__info", module.heap().alloc(Info { data }));
        module
            .freeze()
            .map_err(|error| format!("cannot freeze InstrumentedFilesInfo: {error:?}"))?
            .get("__info")
            .map_err(|error| error.to_string())
    })
}

/// Allocate a collected InstrumentedFilesInfo on `heap`.
pub(crate) fn alloc<'v>(heap: Heap<'v>, collected: Collected) -> Value<'v> {
    heap.alloc(Info {
        data: Some(Arc::new(collected)),
    })
}

/// BuiltinProvider.repr (packages/BuiltinProvider.java:93-96).
impl fmt::Display for Constructor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<function InstrumentedFilesInfo>")
    }
}

#[starlark_value(type = "Provider")]
impl<'v> StarlarkValue<'v> for Constructor {
    // A BuiltinProvider without @StarlarkConstructor is not callable.
    fn invoke(
        &self,
        _me: Value<'v>,
        _args: &Arguments<'v, '_>,
        _eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        Err(failure("'Provider' object is not callable".to_owned()))
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(is_constructor(other))
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        PROVIDER_ID.hash(hasher);
        Ok(())
    }
}

/// The empty, untyped stable-order depset of the EMPTY instance's fields
/// (`Depset.of(Artifact.class, emptySet)`).
fn empty_depset<'v>(heap: Heap<'v>) -> Value<'v> {
    heap.alloc(
        Depset::compose(
            Vec::new(),
            Vec::new(),
            crate::starlark_types::Order::Default,
            None,
        )
        .expect("an empty depset is valid"),
    )
}

/// NativeInfo.repr (packages/NativeInfo.java:120-138): the non-method
/// fields sorted by name.
impl fmt::Display for Info {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.data {
            None => {
                f.write_str("struct(instrumented_files = depset([]), metadata_files = depset([]))")
            }
            Some(data) => write!(
                f,
                "struct(instrumented_files = {}, metadata_files = {})",
                data.instrumented, data.metadata
            ),
        }
    }
}

starlark::methods_static!(INFO_METHODS = info_methods);

/// InstrumentedFilesInfoApi (starlarkbuildapi/test/InstrumentedFilesInfoApi.java:47-79).
#[starlark_module]
fn info_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn instrumented_files<'v>(this: &Info, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        Ok(match &this.data {
            Some(data) => heap.alloc(data.instrumented.clone()),
            None => empty_depset(heap),
        })
    }

    #[starlark(attribute)]
    fn metadata_files<'v>(this: &Info, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        Ok(match &this.data {
            Some(data) => heap.alloc(data.metadata.clone()),
            None => empty_depset(heap),
        })
    }

    /// InstrumentedFilesInfo.getCoverageEnvironmentForStarlark (:106-111):
    /// BuiltinRestriction.failIfCalledOutsideDefaultAllowlist.
    fn get_coverage_environment<'v>(
        this: &Info,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        crate::subrule_fragments::private_api(eval)?;
        let entries: Vec<(&str, &str)> = this
            .data
            .iter()
            .flat_map(|data| data.environment.iter())
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        Ok(eval
            .heap()
            .alloc(starlark::values::dict::AllocDict(entries)))
    }

    /// getCoverageSupportFilesForStarlark (:98-103), restricted likewise.
    fn get_coverage_support_files<'v>(
        this: &Info,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        crate::subrule_fragments::private_api(eval)?;
        Ok(match &this.data {
            Some(data) => eval.heap().alloc(data.support.clone()),
            None => empty_depset(eval.heap()),
        })
    }
}

#[starlark_value(type = "InstrumentedFilesInfo")]
impl<'v> StarlarkValue<'v> for Info {
    fn get_methods() -> Option<&'static Methods> {
        Some(INFO_METHODS.methods())
    }

    // NativeInfo.equals/hashCode (packages/NativeInfo.java:97-117): the
    // provider and the fields (collected instances by identity).
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(
            Info::from_value(other).is_some_and(|other| match (&self.data, &other.data) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }),
        )
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> starlark::Result<()> {
        PROVIDER_ID.hash(hasher);
        Ok(())
    }
}

const F: &str = "instrumented_files_info";

/// The parameters of CoverageCommonApi.instrumentedFilesInfo
/// (starlarkbuildapi/test/CoverageCommonApi.java:39-133), in order.
const PARAMS: [Param; 9] = [
    param("ctx", true, true, true),
    param("source_attributes", false, true, false),
    param("dependency_attributes", false, true, false),
    param("coverage_support_files", false, true, false),
    param("coverage_environment", false, true, false),
    param("extensions", false, true, false),
    param("metadata_files", false, true, false),
    param("reported_to_actual_sources", false, true, false),
    param("baseline_coverage_files", false, true, false),
];

fn is_sequence(value: Value<'_>) -> bool {
    matches!(value.get_type(), "list" | "tuple")
}

fn sequence_items<'v>(value: Value<'v>) -> Vec<Value<'v>> {
    crate::custom_rule::sequence_ref(value)
        .map(|items| items.to_vec())
        .unwrap_or_default()
}

/// Sequence.cast (net/starlark/java/eval/Sequence.java:132-149).
fn cast_sequence(value: Value<'_>, want: &str, what: &str) -> starlark::Result<()> {
    for (index, item) in sequence_items(value).into_iter().enumerate() {
        let ok = match want {
            "string" => item.unpack_str().is_some(),
            _ => File::from_value(item).is_some(),
        };
        if !ok {
            return Err(failure(format!(
                "at index {index} of {what}, got element of type {}, want {want}",
                item.get_type()
            )));
        }
    }
    Ok(())
}

/// Depset.cast (collect/nestedset/Depset.java:262-272) of a depset value.
fn cast_depset(value: Value<'_>, want: ItemType, what: &str) -> starlark::Result<bool> {
    let depset = Depset::from_value(value).expect("type-checked depset");
    if let Some(kind) = depset.node.element_type
        && !depset.node.empty
        && kind != want
    {
        return Err(failure(format!(
            "for '{what}', got a depset of '{}', expected a depset of '{}'",
            kind.name(),
            want.name()
        )));
    }
    Ok(!depset.node.empty)
}

/// CoverageCommon.instrumentedFilesInfo (analysis/test/CoverageCommon.java:47-111).
pub(crate) fn instrumented_files_info<'v>(
    style: crate::language_modules::Style,
    args: &Arguments<'v, '_>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Value<'v>> {
    let heap = eval.heap();
    // The caller's binding style (N33: built-in method vs def-style texts).
    let bound = crate::language_modules::bind_styled(style, F, &PARAMS, args, heap)?;
    let ctx = bound[0].expect("required");
    // The @Param types, in declaration order (Starlark's argument checks).
    if ctx.get_type() != "ctx" {
        return Err(type_error(F, "ctx", ctx, "ctx"));
    }
    let wants: [(usize, &str); 8] = [
        (1, "sequence"),
        (2, "sequence"),
        (3, "sequence or depset"),
        (4, "dict"),
        (5, "sequence or NoneType"),
        (6, "sequence"),
        (7, "depset or NoneType"),
        (8, "sequence or NoneType"),
    ];
    for (index, want) in wants {
        let Some(value) = bound[index] else { continue };
        let ok = match index {
            1 | 2 | 6 => is_sequence(value),
            3 => is_sequence(value) || Depset::from_value(value).is_some(),
            4 => DictRef::from_value(value).is_some(),
            5 | 8 => value.is_none() || is_sequence(value),
            _ => value.is_none() || Depset::from_value(value).is_some(),
        };
        if !ok {
            return Err(type_error(F, PARAMS[index].name, value, want));
        }
    }
    let present = |index: usize| bound[index].filter(|value| !value.is_none());
    // extensions, reported_to_actual_sources, baseline_coverage_files,
    // coverage_environment, coverage_support_files (CoverageCommon.java:62-102).
    if let Some(extensions) = present(5) {
        cast_sequence(extensions, "string", "extensions")?;
    }
    let mut restricted = false;
    if let Some(reported) = present(7) {
        restricted |= cast_depset(reported, ItemType::Tuple, "reported_to_actual_sources")?;
    }
    if let Some(baseline) = present(8) {
        cast_sequence(baseline, "File", "baseline_coverage_files")?;
    }
    if let Some(environment) = bound[4] {
        let dict = DictRef::from_value(environment).expect("type-checked dict");
        for (key, value) in dict.iter() {
            if key.unpack_str().is_none() || value.unpack_str().is_none() {
                return Err(failure(format!(
                    "got dict<{}, {}> for 'coverage_environment', want dict<string, string>",
                    key.get_type(),
                    value.get_type()
                )));
            }
        }
        restricted |= !dict.is_empty();
    }
    if let Some(support) = bound[3] {
        if Depset::from_value(support).is_some() {
            restricted |= cast_depset(support, ItemType::File, "coverage_support_files")?;
        } else {
            for (index, item) in sequence_items(support).into_iter().enumerate() {
                if Depset::from_value(item).is_some() {
                    restricted |= cast_depset(item, ItemType::File, "coverage_support_files")?;
                } else if File::from_value(item).is_some() {
                    restricted = true;
                } else {
                    return Err(failure(format!(
                        "at index {index} of coverage_support_files, got element of type {}, want one of depset, File or FilesToRunProvider",
                        item.get_type()
                    )));
                }
            }
        }
    }
    // BuiltinRestriction.failIfCalledOutsideDefaultAllowlist (:103-107).
    if restricted {
        crate::subrule_fragments::private_api(eval)?;
    }
    if let Some(sources) = bound[1] {
        cast_sequence(sources, "string", "source_attributes")?;
    }
    if let Some(dependencies) = bound[2] {
        cast_sequence(dependencies, "string", "dependency_attributes")?;
    }
    if let Some(metadata) = bound[6] {
        cast_sequence(metadata, "File", "metadata_files")?;
    }
    // InstrumentedFilesCollector.collect (:94-148): EMPTY without coverage.
    let surface = crate::analysis_ctx::current_surface(ctx).map_err(starlark::Error::new_other)?;
    let Some(instrumenting) = surface.config.instrumenting.clone() else {
        return Ok(heap.alloc(Info { data: None }));
    };
    let strings = |value: Option<Value<'v>>| -> Vec<String> {
        value
            .map(sequence_items)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|item| item.unpack_str().map(str::to_owned))
            .collect()
    };
    let mut builder = Builder::default();
    if let Some(support) = bound[3] {
        builder.add_support(support_files(support));
    }
    for name in strings(bound[2]) {
        for target in attribute_targets(ctx, &name, heap)? {
            builder.add_from(target_collected(&target).as_deref());
        }
    }
    if let Some(environment) = bound[4] {
        let dict = DictRef::from_value(environment).expect("type-checked dict");
        builder.add_environment(
            dict.iter()
                .filter_map(|(key, value)| {
                    Some((key.unpack_str()?.to_owned(), value.unpack_str()?.to_owned()))
                })
                .collect(),
        );
    }
    // shouldIncludeLocalSources (:176-186) and the instrumented file types
    // (CoverageCommon.createInstrumentedFilesInfo :118-141: no `extensions`
    // is ANY_FILE, an empty list NO_FILE, else FileType.of = a suffix).
    let extensions = present(5).map(|value| strings(Some(value)));
    let label = surface.label.to_string();
    let is_test = surface.rule_class.ends_with("_test");
    let mut local = Vec::new();
    if instrumenting.includes(&label, is_test) {
        for name in strings(bound[1]) {
            for target in attribute_targets(ctx, &name, heap)? {
                for file in &target.files {
                    let basename = file.path.rsplit('/').next().unwrap_or(&file.path);
                    let typed = extensions.as_ref().is_none_or(|extensions| {
                        extensions
                            .iter()
                            .any(|extension| basename.ends_with(extension.as_str()))
                    });
                    if (file.is_source || instrumenting.generated_files) && typed {
                        local.push(file.clone());
                    }
                }
            }
        }
    }
    builder.set_local_sources(local);
    // build (:290-305): the target's own baseline coverage, the given
    // files or a BaselineCoverageAction.
    if !builder.local_sources().is_empty() {
        match present(8) {
            Some(baseline) => builder.add_baseline(
                sequence_items(baseline)
                    .into_iter()
                    .filter_map(|item| File::from_value(item).cloned())
                    .collect(),
            ),
            None => {
                let file = register_baseline_coverage(eval, builder.local_sources())
                    .map_err(starlark::Error::new_other)?;
                builder.add_baseline(vec![file]);
            }
        }
    }
    if let Some(metadata) = bound[6] {
        builder.add_metadata(
            sequence_items(metadata)
                .into_iter()
                .filter_map(|item| File::from_value(item).cloned())
                .collect(),
        );
    }
    if let Some(reported) = present(7) {
        builder.set_reported_to_actual(
            Depset::from_value(reported)
                .expect("type-checked depset")
                .clone(),
        );
    }
    Ok(alloc(eval.heap(), builder.build()))
}

/// The coverage support files argument (a sequence of Files and depsets,
/// or a depset) as one stable-order set.
fn support_files(value: Value<'_>) -> Depset {
    let mut direct = Vec::new();
    let mut children = Vec::new();
    let items = match Depset::from_value(value) {
        Some(depset) => {
            children.push(depset.node.clone());
            Vec::new()
        }
        None => sequence_items(value),
    };
    for item in items {
        if let Some(depset) = Depset::from_value(item) {
            children.push(depset.node.clone());
        } else if let Some(file) = File::from_value(item) {
            direct.push(Item::File(file.clone()));
        }
    }
    Depset::compose_framework(direct, children, Order::Default, Some(ItemType::File))
        .unwrap_or_else(|_| Depset::from_files(Vec::new()))
}

/// RuleContext.getPrerequisites(name) through `ctx.attr`: the Target values
/// of a label-typed attribute (none for a missing or non-label attribute).
fn attribute_targets<'v>(
    ctx: Value<'v>,
    name: &str,
    heap: Heap<'v>,
) -> starlark::Result<Vec<Arc<crate::starlark_types::Target>>> {
    let Some(attr) = ctx.get_attr("attr", heap)? else {
        return Ok(Vec::new());
    };
    let Some(value) = attr.get_attr(name, heap)? else {
        return Ok(Vec::new());
    };
    let mut targets = Vec::new();
    let mut add = |item: Value<'v>| {
        if let Some(target) = crate::starlark_types::TargetValue::from_value(item) {
            targets.push(target.0.clone());
        }
    };
    if let Some(dict) = DictRef::from_value(value) {
        for (key, entry) in dict.iter() {
            add(key);
            add(entry);
            for item in sequence_items(entry) {
                add(item);
            }
        }
    } else if is_sequence(value) {
        for item in sequence_items(value) {
            add(item);
        }
    } else {
        add(value);
    }
    Ok(targets)
}

/// The collected InstrumentedFilesInfo of a dependency Target (`None`:
/// EMPTY or none).
pub(crate) fn target_collected(target: &crate::starlark_types::Target) -> Option<Arc<Collected>> {
    target
        .providers
        .get(PROVIDER_ID)
        .and_then(|info| collected(info.value()))
}

/// BaselineCoverageAction.create (analysis/test/BaselineCoverageAction.java:
/// 84-95): `<testlogs>/<package>/<name>/baseline_coverage.dat`, one
/// `SF:`/`end_of_record` pair per instrumented local source.
fn register_baseline_coverage(
    eval: &Evaluator<'_, '_, '_>,
    sources: &[File],
) -> anyhow::Result<File> {
    let context = crate::analysis_runtime::context(eval)?;
    let root = context.testlogs_root();
    let path = format!(
        "{}/{}/baseline_coverage.dat",
        context.label.package_exec_path().display(),
        context.label.name
    );
    let path = path.trim_start_matches('/').to_owned();
    let file = context.declare_shareable(&root, &path)?;
    let mut content = String::new();
    for source in sources {
        content.push_str(&format!("SF:{}\nend_of_record\n", source.path));
    }
    context.register(
        vec![file.clone()],
        Vec::new(),
        String::new(),
        Some(crate::action::WriteAction {
            content: content.into_bytes(),
            executable: false,
        }),
        None,
        Vec::new(),
        std::collections::BTreeMap::new(),
        false,
        false,
    )?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    #[test]
    fn empty_instance_is_shared_and_frozen() {
        let a = super::empty_frozen().unwrap();
        let b = super::empty_frozen().unwrap();
        assert!(a.value().ptr_eq(b.value()));
        assert_eq!(a.value().get_type(), "InstrumentedFilesInfo");
        assert_eq!(
            a.value().to_repr(),
            "struct(instrumented_files = depset([]), metadata_files = depset([]))"
        );
    }
}
