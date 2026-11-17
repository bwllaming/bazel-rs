//! `native.existing_rule(name)` and `native.existing_rules()` during BUILD
//! evaluation (StarlarkNativeModule.java:320-521 at Bazel 9.3.1).
//!
//! Both return immutable dict-like views (`type()` is `Map`, the Java
//! interface name). `existing_rules()` is a snapshot of the rule targets
//! declared so far, in declaration order (TargetRecorder's
//! SnapshottableBiMap); rules declared later are not visible through it.
//! An `ExistingRuleView` exposes `name`, `kind` and every exportable
//! attribute (`starlarkifyValue`, :714-830): names starting with a letter,
//! with a value representable in Starlark. Computed defaults (`testonly`,
//! `deprecation`, `package_metadata`, a test's `timeout`) and licenses are
//! absent unless set explicitly; lists are tuples, dictionaries immutable,
//! labels relative to the package (`:x`, `//pkg:x`, `@@repo//pkg:x`) and a
//! `select()` value a select whose keys are Label values.
//!
//! The views only read the declaring package's own evaluation state, so a
//! package's result still depends only on its BUILD file and loads (no new
//! Skyframe-like dependency for the analysis snapshot).

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, RwLock};

use allocative::Allocative;
use anyhow::anyhow;
use starlark::environment::{Methods, MethodsBuilder, Module};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::tuple::AllocTuple;
use starlark::values::{
    FrozenHeapName, Heap, NoSerialize, OwnedFrozenValue, ProvidesStaticType, StarlarkPagablePanic,
    StarlarkValue, Value, starlark_value,
};

use crate::custom_rule::{AttrValue, ConfigurablePart, InvocationDeclaration, SharedSchema};
use crate::engine::query_attrs::{self, AttrType, QueryAttribute, Shape};
use crate::label::{Label, PackageId};
use crate::package::{Package, PackageArgs, Rule};

/// A declared rule as `existing_rule` sees it.
pub(crate) struct Entry {
    pub name: String,
    /// Rule.getRuleClass: the Starlark rule's exported name or the native class.
    pub kind: String,
    pub rule: Rule,
    /// A Starlark rule's invocation (attribute values, schema, declaration).
    pub custom: Option<(
        BTreeMap<String, AttrValue>,
        SharedSchema,
        InvocationDeclaration,
    )>,
    /// The package arguments when the entry was recorded.
    pub args: Arc<PackageArgs>,
}

/// The append-only rule table of one package evaluation. A snapshot is a
/// prefix length: names are unique within a package, so a name's index
/// below the length means the rule existed when the snapshot was taken.
#[derive(Default)]
pub(crate) struct Table {
    entries: RwLock<Vec<Arc<Entry>>>,
    index: RwLock<HashMap<String, usize>>,
}

impl Table {
    pub(crate) fn len(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    pub(crate) fn push(&self, entry: Entry) {
        let mut entries = self.entries.write().unwrap();
        self.index
            .write()
            .unwrap()
            .insert(entry.name.clone(), entries.len());
        entries.push(Arc::new(entry));
    }

    fn lookup(&self, name: &str, limit: usize) -> Option<Arc<Entry>> {
        let index = *self.index.read().unwrap().get(name)?;
        (index < limit).then(|| self.entries.read().unwrap()[index].clone())
    }

    fn names(&self, limit: usize) -> Vec<String> {
        self.entries.read().unwrap()[..limit]
            .iter()
            .map(|entry| entry.name.clone())
            .collect()
    }
}

/// Per-evaluation state: the shared table and how many of the package's
/// declared rules (and pending Starlark invocations) it has consumed.
#[derive(Default)]
pub(crate) struct Tracker {
    pub table: Arc<Table>,
    pub synced_rules: std::cell::Cell<usize>,
    pub synced_custom: std::cell::Cell<usize>,
}

#[allow(unsafe_code)]
mod values {
    use super::*;

    /// StarlarkNativeModule.ExistingRuleView.
    #[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct ExistingRuleView {
        pub name: String,
        /// The frozen, ordered `dict` of exported keys and values.
        #[allocative(skip)]
        pub attrs: OwnedFrozenValue,
    }

    /// StarlarkNativeModule.ExistingRulesView.
    #[derive(ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
    pub(crate) struct ExistingRulesView {
        #[allocative(skip)]
        pub table: Arc<Table>,
        pub limit: usize,
    }
}
use values::{ExistingRuleView, ExistingRulesView};

impl fmt::Debug for ExistingRulesView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ExistingRulesView({})", self.limit)
    }
}

starlark_simple_value!(ExistingRuleView);
starlark_simple_value!(ExistingRulesView);

impl fmt::Display for ExistingRuleView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<native.ExistingRuleView for target '{}'>", self.name)
    }
}

impl fmt::Display for ExistingRulesView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<native.ExistingRulesView object>")
    }
}

starlark::methods_static!(RULE_VIEW_METHODS = view_methods);

/// DictLikeView.setIndex is absent: Starlark's assignment error
/// (EvalUtils.setIndex) names the Java type.
fn assign_error() -> starlark::Error {
    starlark::Error::new_other(anyhow!(
        "can only assign an element in a dictionary or a list, not in a 'Map'"
    ))
}

fn not_found(key: Value<'_>) -> starlark::Error {
    starlark::Error::new_other(anyhow!("key {} not found in view", key.to_repr()))
}

/// Views are hashable by identity (StarlarkValue's default Object.hashCode).
fn identity_hash<T>(value: &T, hasher: &mut starlark::collections::StarlarkHasher) {
    std::hash::Hash::hash(&(value as *const T as usize), hasher);
}

impl ExistingRuleView {
    fn dict<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        heap.access_owned_frozen_value(&self.attrs)
    }

    fn lookup<'v>(&self, key: Value<'v>, heap: Heap<'v>) -> Option<Value<'v>> {
        key.unpack_str()?;
        starlark::values::dict::DictRef::from_value(self.dict(heap))?
            .get(key)
            .ok()
            .flatten()
    }

    fn keys<'v>(&self, heap: Heap<'v>) -> Vec<Value<'v>> {
        starlark::values::dict::DictRef::from_value(self.dict(heap))
            .map(|dict| dict.keys().collect())
            .unwrap_or_default()
    }
}

#[starlark_value(type = "Map")]
impl<'v> StarlarkValue<'v> for ExistingRuleView {
    fn get_methods() -> Option<&'static Methods> {
        Some(RULE_VIEW_METHODS.methods())
    }

    fn at(&self, index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        self.lookup(index, heap).ok_or_else(|| not_found(index))
    }

    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other.unpack_str().is_some_and(|key| {
            let frozen = self.attrs.value();
            starlark::values::dict::DictRef::from_value(frozen)
                .is_some_and(|dict| dict.get_str(key).is_some())
        }))
    }

    fn length(&self) -> starlark::Result<i32> {
        Ok(
            starlark::values::dict::DictRef::from_value(self.attrs.value())
                .map_or(0, |dict| dict.len() as i32),
        )
    }

    fn iterate_collect(&self, heap: Heap<'v>) -> starlark::Result<Vec<Value<'v>>> {
        Ok(self.keys(heap))
    }

    fn set_at(&self, _index: Value<'v>, _new_value: Value<'v>) -> starlark::Result<()> {
        Err(assign_error())
    }

    fn write_hash(
        &self,
        hasher: &mut starlark::collections::StarlarkHasher,
    ) -> starlark::Result<()> {
        identity_hash(self, hasher);
        Ok(())
    }
}

impl ExistingRulesView {
    fn view<'v>(&self, key: Value<'v>, heap: Heap<'v>) -> starlark::Result<Option<Value<'v>>> {
        let Some(name) = key.unpack_str() else {
            return Ok(None);
        };
        match self.table.lookup(name, self.limit) {
            Some(entry) => Ok(Some(
                rule_view(&entry, heap).map_err(starlark::Error::new_other)?,
            )),
            None => Ok(None),
        }
    }
}

#[starlark_value(type = "Map")]
impl<'v> StarlarkValue<'v> for ExistingRulesView {
    fn get_methods() -> Option<&'static Methods> {
        Some(RULE_VIEW_METHODS.methods())
    }

    fn at(&self, index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        self.view(index, heap)?.ok_or_else(|| not_found(index))
    }

    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other
            .unpack_str()
            .is_some_and(|name| self.table.lookup(name, self.limit).is_some()))
    }

    fn length(&self) -> starlark::Result<i32> {
        Ok(self.limit as i32)
    }

    fn iterate_collect(&self, heap: Heap<'v>) -> starlark::Result<Vec<Value<'v>>> {
        Ok(self
            .table
            .names(self.limit)
            .iter()
            .map(|name| heap.alloc_str(name).to_value())
            .collect())
    }

    fn set_at(&self, _index: Value<'v>, _new_value: Value<'v>) -> starlark::Result<()> {
        Err(assign_error())
    }

    fn write_hash(
        &self,
        hasher: &mut starlark::collections::StarlarkHasher,
    ) -> starlark::Result<()> {
        identity_hash(self, hasher);
        Ok(())
    }
}

/// The view's value for `key`, or `None` (both view kinds).
fn view_get<'v>(
    this: Value<'v>,
    key: Value<'v>,
    heap: Heap<'v>,
) -> starlark::Result<Option<Value<'v>>> {
    if let Some(view) = ExistingRuleView::from_value(this) {
        return Ok(view.lookup(key, heap));
    }
    if let Some(view) = ExistingRulesView::from_value(this) {
        return view.view(key, heap);
    }
    Err(starlark::Error::new_other(anyhow!(
        "expected a native view"
    )))
}

/// The keys, values or items of a view as a frozen list
/// (`StarlarkList.immutableCopyOf`: mutation fails with "trying to mutate a
/// frozen list value").
#[derive(Clone, Copy)]
enum Listing {
    Keys,
    Values,
    Items,
}

fn listing<'v>(this: Value<'v>, what: Listing, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
    let owned = Module::with_temp_heap(|module| -> anyhow::Result<OwnedFrozenValue> {
        let local = module.heap();
        let mut pairs: Vec<(Value<'_>, Value<'_>)> = Vec::new();
        if let Some(view) = ExistingRuleView::from_value(this) {
            let dict = local.access_owned_frozen_value(&view.attrs);
            if let Some(dict) = starlark::values::dict::DictRef::from_value(dict) {
                pairs.extend(dict.iter());
            }
        } else if let Some(view) = ExistingRulesView::from_value(this) {
            for name in view.table.names(view.limit) {
                let key = local.alloc_str(&name).to_value();
                let value = match what {
                    Listing::Keys => Value::new_none(),
                    _ => match view.table.lookup(&name, view.limit) {
                        Some(entry) => rule_view(&entry, local)?,
                        None => Value::new_none(),
                    },
                };
                pairs.push((key, value));
            }
        } else {
            return Err(anyhow!("expected a native view"));
        }
        let items: Vec<Value<'_>> = pairs
            .into_iter()
            .map(|(key, value)| match what {
                Listing::Keys => key,
                Listing::Values => value,
                Listing::Items => local.alloc(AllocTuple([key, value])),
            })
            .collect();
        module.set("listing", local.alloc(AllocList(items)));
        let frozen = module
            .freeze_named(FrozenHeapName::User(Box::new(
                "native view listing".to_owned(),
            )))
            .map_err(|error| anyhow!("cannot freeze the view listing: {error:?}"))?;
        frozen
            .get("listing")
            .map_err(|error| anyhow!(error.to_string()))
    })
    .map_err(starlark::Error::new_other)?;
    Ok(heap.access_owned_frozen_value(&owned))
}

/// DictLikeView's `get`, `keys`, `values` and `items` (StarlarkNativeModule.java:156-221):
/// `keys`/`values`/`items` return immutable lists.
#[starlark_module]
fn view_methods(builder: &mut MethodsBuilder) {
    fn get<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] key: Value<'v>,
        #[starlark(default = starlark::values::none::NoneType)] default: Value<'v>,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        Ok(view_get(this, key, heap)?.unwrap_or(default))
    }

    fn keys<'v>(this: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        listing(this, Listing::Keys, heap)
    }

    fn values<'v>(this: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        listing(this, Listing::Values, heap)
    }

    fn items<'v>(this: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        listing(this, Listing::Items, heap)
    }
}

/// An `ExistingRulesView` over the first `limit` rules of `table`.
pub(crate) fn rules_view<'v>(heap: Heap<'v>, table: Arc<Table>) -> Value<'v> {
    let limit = table.len();
    heap.alloc_simple(ExistingRulesView { table, limit })
}

/// `existing_rule(name)`: the rule's view, or `None`.
pub(crate) fn rule_lookup<'v>(
    heap: Heap<'v>,
    table: &Table,
    name: &str,
) -> anyhow::Result<Value<'v>> {
    match table.lookup(name, table.len()) {
        Some(entry) => rule_view(&entry, heap),
        None => Ok(Value::new_none()),
    }
}

fn rule_view<'v>(entry: &Entry, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let attrs = Module::with_temp_heap(|module| -> anyhow::Result<_> {
        let local = module.heap();
        let items = exported(entry, local)?;
        module.set("attrs", local.alloc(AllocDict(items)));
        let frozen = module
            .freeze_named(FrozenHeapName::User(Box::new(format!(
                "native.existing_rule({})",
                entry.name
            ))))
            .map_err(|error| anyhow!("cannot freeze the existing rule view: {error:?}"))?;
        frozen
            .get("attrs")
            .map_err(|error| anyhow!(error.to_string()))
    })?;
    Ok(heap.alloc_simple(ExistingRuleView {
        name: entry.name.clone(),
        attrs,
    }))
}

/// Computed defaults (BaseRuleClasses.java:67-108 and the test rules'
/// TIMEOUT_DEFAULT): Rule.getAttr returns the ComputedDefault object, which
/// starlarkifyValue drops, unless the value was set explicitly.
fn computed_default(name: &str) -> bool {
    matches!(
        name,
        "testonly" | "deprecation" | "package_metadata" | "applicable_licenses" | "timeout"
    )
}

/// Whether the rule class fixes `testonly` (test rules and test_suite:
/// a plain `true` default, not the package's computed default).
fn fixed_testonly(entry: &Entry) -> bool {
    match &entry.custom {
        Some((_, _, declaration)) => {
            crate::native_rules::starlark_builtins(declaration.builtin.kind)
                .testonly_default
                .is_some()
        }
        None => entry
            .rule
            .native
            .as_ref()
            .and_then(|native| crate::native_rules::rule_class(&native.rule_class))
            .is_some_and(|class| class.testonly_default.is_some()),
    }
}

/// Rule-class attribute defaults the shared attribute tables do not carry:
/// BazelGenRuleRule.java:47 (`stamp` = TriState.NO) and the Starlark test
/// base rule (StarlarkRuleClassFunctions.java:258-290: `size` "medium",
/// `shard_count` -1, a fixed `testonly` true).
fn class_default(entry: &Entry, name: &str) -> Option<AttrValue> {
    if name == "testonly" && fixed_testonly(entry) {
        return Some(AttrValue::Bool(true));
    }
    match &entry.custom {
        Some((_, _, declaration)) if declaration.builtin.kind.test => match name {
            "size" => Some(AttrValue::String("medium".to_owned())),
            "shard_count" => Some(AttrValue::Int(-1)),
            _ => None,
        },
        Some(_) => None,
        None => match (entry.kind.as_str(), name) {
            ("genrule", "stamp") => Some(AttrValue::Int(0)),
            _ => None,
        },
    }
}

fn exported<'v>(entry: &Entry, heap: Heap<'v>) -> anyhow::Result<Vec<(Value<'v>, Value<'v>)>> {
    let package = Package {
        args: (*entry.args).clone(),
        ..Package::default()
    };
    let custom = entry
        .custom
        .as_ref()
        .map(|(attrs, schema, declaration)| (attrs, schema, declaration));
    let attributes = query_attrs::declared_rule_attributes(&entry.rule, &package, custom);
    let here = entry.rule.label.package_id();
    let mut items = vec![
        (
            heap.alloc_str("name").to_value(),
            heap.alloc_str(&entry.name).to_value(),
        ),
        (
            heap.alloc_str("kind").to_value(),
            heap.alloc_str(&entry.kind).to_value(),
        ),
    ];
    for attribute in &attributes {
        let name = attribute.name.as_str();
        if name == "name"
            || name == "kind"
            || !name.chars().next().is_some_and(char::is_alphabetic)
            || attribute.ty == AttrType::License
        {
            continue;
        }
        if !attribute.explicit
            && computed_default(name)
            && !(name == "testonly" && fixed_testonly(entry))
        {
            continue;
        }
        let class_value = if attribute.explicit {
            None
        } else {
            class_default(entry, name)
        };
        let value = match &attribute.select {
            Some(parts) => select_value(heap, parts, attribute, &here)?,
            None if name == "visibility" => {
                let labels = if entry.rule.explicit_visibility {
                    entry
                        .rule
                        .visibility
                        .declared_labels()
                        .iter()
                        .map(ToString::to_string)
                        .collect()
                } else {
                    Vec::new()
                };
                starlarkify(heap, &AttrValue::Labels(labels), attribute, &here)?
            }
            None if class_value.is_some() => {
                starlarkify(heap, class_value.as_ref().unwrap(), attribute, &here)?
            }
            None => match &attribute.value {
                Some(value) => starlarkify(heap, value, attribute, &here)?,
                None => None,
            },
        };
        if let Some(value) = value {
            items.push((heap.alloc_str(name).to_value(), value));
        }
    }
    Ok(items)
}

/// starlarkifyValue's label rendering: `:name` in the package, `//pkg:name`
/// in the repository, else the unambiguous canonical form.
fn label_text(text: &str, here: &PackageId) -> String {
    let Ok(label) = Label::parse_canonical(text) else {
        return text.to_owned();
    };
    if label.repository == here.repository && label.package == here.package {
        format!(":{}", label.name)
    } else if label.repository == here.repository {
        format!("//{}:{}", label.package, label.name)
    } else {
        format!("@@{}//{}:{}", label.repository, label.package, label.name)
    }
}

/// An output's text: a package-relative output name or a label.
fn output_text(text: &str, here: &PackageId) -> String {
    if text.starts_with("//") || text.starts_with('@') {
        label_text(text, here)
    } else {
        format!(":{}", text.strip_prefix(':').unwrap_or(text))
    }
}

fn strings<'v>(heap: Heap<'v>, values: impl IntoIterator<Item = String>) -> Value<'v> {
    let values: Vec<Value<'v>> = values
        .into_iter()
        .map(|value| heap.alloc_str(&value).to_value())
        .collect();
    heap.alloc(AllocTuple(values))
}

fn starlarkify<'v>(
    heap: Heap<'v>,
    value: &AttrValue,
    attribute: &QueryAttribute,
    here: &PackageId,
) -> anyhow::Result<Option<Value<'v>>> {
    let labels = |texts: &[String]| strings(heap, texts.iter().map(|text| label_text(text, here)));
    let order_independent = matches!(
        attribute.name.as_str(),
        "visibility" | "transitive_configs" | "tags" | "features"
    );
    Ok(Some(match value {
        AttrValue::String(text) => {
            if matches!(attribute.shape, Shape::Label | Shape::Nodep)
                || matches!(attribute.ty, AttrType::Label | AttrType::NodepLabel)
            {
                heap.alloc_str(&label_text(text, here)).to_value()
            } else {
                heap.alloc_str(text).to_value()
            }
        }
        AttrValue::Strings(values) => {
            if matches!(attribute.ty, AttrType::LabelList | AttrType::NodepLabelList) {
                let mut texts: Vec<String> = values.iter().map(|t| label_text(t, here)).collect();
                if order_independent {
                    texts.sort();
                }
                strings(heap, texts)
            } else {
                let mut values = values.clone();
                if order_independent {
                    values.sort();
                }
                strings(heap, values)
            }
        }
        AttrValue::Int(value) => heap.alloc(*value),
        AttrValue::Bool(value) => Value::new_bool(*value),
        AttrValue::Label(None) | AttrValue::Output(None) => return Ok(None),
        AttrValue::Label(Some(text)) => heap.alloc_str(&label_text(text, here)).to_value(),
        AttrValue::Labels(texts) => {
            if order_independent {
                let mut texts: Vec<String> = texts.iter().map(|t| label_text(t, here)).collect();
                texts.sort();
                strings(heap, texts)
            } else {
                labels(texts)
            }
        }
        AttrValue::Output(Some(text)) => heap.alloc_str(&output_text(text, here)).to_value(),
        AttrValue::Outputs(texts) => {
            strings(heap, texts.iter().map(|text| output_text(text, here)))
        }
        AttrValue::Ints(values) => {
            let values: Vec<Value<'v>> = values.iter().map(|value| heap.alloc(*value)).collect();
            heap.alloc(AllocTuple(values))
        }
        AttrValue::StringDict(entries) => heap.alloc(AllocDict(
            entries
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )),
        AttrValue::StringListDict(entries) => {
            heap.alloc(AllocDict(entries.iter().map(|(key, values)| {
                (key.as_str(), strings(heap, values.iter().cloned()))
            })))
        }
        AttrValue::LabelKeyedStringDict(entries) => heap.alloc(AllocDict(
            entries
                .iter()
                .map(|(key, value)| (label_text(key, here), value.as_str())),
        )),
        AttrValue::StringKeyedLabelDict(entries) => heap.alloc(AllocDict(
            entries
                .iter()
                .map(|(key, value)| (key.as_str(), label_text(value, here))),
        )),
        AttrValue::LabelListDict(entries) => heap.alloc(AllocDict(
            entries
                .iter()
                .map(|(key, values)| (key.as_str(), labels(values))),
        )),
    }))
}

/// A select() attribute (starlarkifyValue's SelectorList case): every
/// element becomes a select (a plain element under `//conditions:default`),
/// keyed by Label values; a `None` branch of a type whose default is null
/// stays `None`, other `None` branches take the attribute default.
fn select_value<'v>(
    heap: Heap<'v>,
    parts: &[ConfigurablePart],
    attribute: &QueryAttribute,
    here: &PackageId,
) -> anyhow::Result<Option<Value<'v>>> {
    let mut values = Vec::new();
    let mut selects = Vec::new();
    let mut native_type = None;
    for part in parts {
        let (branches, no_match_error): (Vec<(String, Option<&AttrValue>)>, &str) = match part {
            ConfigurablePart::Plain(value) => {
                (vec![("//conditions:default".to_owned(), Some(value))], "")
            }
            ConfigurablePart::Select {
                branches,
                no_match_error,
            } => (
                branches
                    .iter()
                    .map(|(key, value)| (key.clone(), value.as_ref()))
                    .collect(),
                no_match_error.as_str(),
            ),
        };
        let mut entries: Vec<(Value<'v>, Value<'v>)> = Vec::new();
        for (key, value) in branches {
            let key_text = if query_attrs::is_default_condition(&key) {
                "//conditions:default".to_owned()
            } else {
                key
            };
            let branch = match value.or(attribute.default.as_ref()) {
                Some(value) => starlarkify(heap, value, attribute, here)?,
                None => Some(Value::new_none()),
            };
            let branch = match (value, branch) {
                (None, None) => Some(Value::new_none()),
                (_, branch) => branch,
            };
            let Some(branch) = branch else {
                continue;
            };
            native_type.get_or_insert_with(|| branch.get_type().to_owned());
            let key_value = crate::starlark_types::alloc_label(heap, &key_text)?;
            if let Some(slot) = entries
                .iter_mut()
                .find(|(existing, _)| existing.equals(key_value).unwrap_or(false))
            {
                slot.1 = branch;
            } else {
                entries.push((key_value, branch));
            }
        }
        if entries.is_empty() {
            continue;
        }
        values.push(heap.alloc(AllocDict(entries)));
        selects.push(Some(no_match_error.to_owned()));
    }
    if values.is_empty() {
        return Ok(None);
    }
    Ok(Some(heap.alloc(
        crate::rule_declarations::SelectorListGen {
            values,
            selects,
            native_type: native_type.unwrap_or_else(|| "NoneType".to_owned()),
        },
    )))
}
