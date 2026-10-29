//! Aspects applied to native rule targets (docs/ASPECTS.md "Native base
//! rules"): `ctx.rule` over a native rule class's attributes
//! (StarlarkRuleContext.java:288-319 builds it from `rule.getAttributes()`
//! for every rule, StarlarkAttributesCollection.Builder.addAttribute), the
//! aspect's surface without a Starlark CustomInvocation, and the `License`
//! value of the `licenses` attribute.
use super::*;
use crate::native_rules::{NativeType, PrivateAttrKind};
use starlark::values::dict::AllocDict;
use std::fmt;

/// The configured facts of a native base rule an aspect application needs
/// besides the rule itself (crate::engine computes them per application).
#[derive(Clone, Debug)]
pub(crate) struct NativeAspectBase {
    /// The base target configuration's `ctx` facts (crate::ctx_surface).
    pub surface: Arc<crate::ctx_surface::CtxSurface>,
    pub target_platform: crate::platforms::TargetPlatform,
    pub cpp: crate::custom_rule::CppConfigurationView,
    /// Rule.isTestOnly: explicit, class or package default.
    pub testonly: bool,
    /// Rule.getDeprecationWarning: explicit or the package default.
    pub deprecation: Option<String>,
    /// The `licenses` strings: explicit, else the package's `licenses()`.
    pub licenses: Option<Vec<String>>,
    /// The explicit `visibility` labels (canonical), empty when unset.
    pub visibility: Vec<String>,
    /// The application's dependency keys per attribute (native name), in
    /// edge order (crate::engine's dependency map of the application).
    pub attribute_keys: BTreeMap<String, Vec<String>>,
}

// Only derive-generated markers require this exception. The value owns plain
// Rust data and freezes unchanged.
#[allow(unsafe_code)]
mod owned {
    use starlark::values::{NoSerialize, ProvidesStaticType, StarlarkPagablePanic};

    /// packages/License.java: the canonical license types (LicenseType enum
    /// order, lower case) then `exception=LABEL`s (sorted).
    #[derive(
        Clone,
        Debug,
        PartialEq,
        Eq,
        ProvidesStaticType,
        NoSerialize,
        allocative::Allocative,
        StarlarkPagablePanic,
    )]
    pub(crate) struct License {
        pub(super) types: Vec<&'static str>,
        pub(super) exceptions: Vec<String>,
    }
}
pub(crate) use owned::License;
starlark::starlark_simple_value!(License);

/// License.LicenseType (packages/License.java:60-69) in enum order.
const LICENSE_TYPES: [&str; 8] = [
    "by_exception_only",
    "restricted",
    "restricted_if_statically_linked",
    "reciprocal",
    "notice",
    "permissive",
    "unencumbered",
    "none",
];

impl License {
    /// License.parseLicense (packages/License.java:115-145) of already
    /// validated strings; `None` is License.NO_LICENSE.
    pub(crate) fn parse(strings: Option<&[String]>) -> Self {
        let Some(strings) = strings else {
            return Self {
                types: vec!["none"],
                exceptions: Vec::new(),
            };
        };
        let mut exceptions: Vec<String> = Vec::new();
        let mut types = Vec::new();
        for text in strings {
            if let Some(label) = text.strip_prefix("exception=") {
                let canonical = crate::label::Label::parse_canonical(label)
                    .map(|label| crate::rule_declarations::canonical_form(&label))
                    .unwrap_or_else(|_| label.to_owned());
                exceptions.push(canonical);
            } else if let Some(known) = LICENSE_TYPES
                .iter()
                .find(|known| known.eq_ignore_ascii_case(text))
            {
                types.push(*known);
            }
        }
        types.sort_by_key(|name| LICENSE_TYPES.iter().position(|known| known == name));
        types.dedup();
        exceptions.sort();
        exceptions.dedup();
        Self { types, exceptions }
    }
}

/// License.toString (packages/License.java:171-178).
impl fmt::Display for License {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}]", self.types.join(", "))?;
        if !self.exceptions.is_empty() {
            write!(f, " with exceptions [{}]", self.exceptions.join(", "))?;
        }
        Ok(())
    }
}

#[starlark::values::starlark_value(type = "License")]
impl<'v> starlark::values::StarlarkValue<'v> for License {
    /// License.repr (packages/License.java:209-221): a list of strings.
    fn collect_repr(&self, collector: &mut String) {
        collector.push('[');
        let items = self
            .types
            .iter()
            .map(|name| (*name).to_owned())
            .chain(
                self.exceptions
                    .iter()
                    .map(|label| format!("exception={label}")),
            )
            .collect::<Vec<_>>();
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                collector.push_str(", ");
            }
            collector.push_str(&serde_json::to_string(item).unwrap_or_default());
        }
        collector.push(']');
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(License::from_value(other).is_some_and(|other| other == self))
    }
}

/// The aspect's RuleSurface over a native base rule: the base target
/// configuration's facts, no Starlark schema.
pub(super) fn rule_surface(
    rule: &Rule,
    base: &NativeAspectBase,
    repositories: &RepositorySet,
    dependencies: &BTreeMap<String, DependencyTarget>,
    output_root: &str,
) -> anyhow::Result<crate::ctx_surface::RuleSurface> {
    let label = crate::rule_declarations::canonical_form(&rule.label);
    let class = rule.rule_class_name().to_owned();
    // LocationExpander.buildLocationMap (analysis/LocationExpander.java:
    // 393-485): the rule's outputs, `srcs` files to build and `tools`
    // executables (else files to build).
    let mut locations = crate::location_expander::LocationMap::default();
    for name in &rule.outs {
        let output = rule.label.with_name(name).map_err(anyhow::Error::msg)?;
        locations.add(
            &output.to_string(),
            [File::new(crate::starlark_types::FileData {
                is_directory: false,
                is_symlink: false,
                path: output
                    .generated_exec_path(output_root)
                    .to_string_lossy()
                    .into_owned(),
                short_path: output.generated_short_path(),
                owner: rule.label.to_string(),
                is_source: false,
            })],
        );
    }
    for name in ["srcs", "tools"] {
        for key in base.attribute_keys.get(name).into_iter().flatten() {
            let Some(target) = dependencies.get(key) else {
                continue;
            };
            let files = match target.runfiles.files_to_run(&target.files).executable() {
                Some(file) if name != "srcs" => vec![file.clone()],
                _ => target.files.clone(),
            };
            locations.add(&target.label, files);
        }
    }
    Ok(crate::ctx_surface::RuleSurface {
        config: base.surface.clone(),
        schema: Default::default(),
        builtin_kind: Default::default(),
        rule_class: class.clone(),
        label: rule.label.clone(),
        declared_fragments: Vec::new(),
        target_description: format!("{class} rule {label}"),
        has_alwayslink_attribute: false,
        explicit_resource_strip_prefix: false,
        split_transition_attribute: false,
        skylark_testable: false,
        mapping: repositories
            .shared_mapping(&rule.label.repository)
            .unwrap_or_default(),
        locations,
        data_locations: Default::default(),
        errors: Default::default(),
    })
}

/// `ctx.rule.attr`/`files`/`file`/`executable` of a native base rule
/// (StarlarkRuleContext.java:300-303 over `rule.getAttributes()`;
/// StarlarkAttributesCollection.Builder.addAttribute, :383-446): every
/// attribute under its public name except TRISTATE ones and
/// `exec_group_compatible_with` (shouldIgnore, :237-262); dependency
/// attributes as Targets with a `ctx.rule.files` entry, `nodep` labels and
/// outputs as Labels, other values converted directly
/// (Attribute.valueToStarlark). No native attribute is a single-file or
/// executable label, so `file` and `executable` are empty. The base
/// aspects' attributes follow, the first of a name winning.
pub(super) fn attribute_structs<'v>(
    rule: &Rule,
    base: &NativeAspectBase,
    dependencies: &BTreeMap<String, DependencyTarget>,
    base_aspects: Vec<AspectAttributeMaps<'v>>,
    heap: Heap<'v>,
) -> anyhow::Result<[Value<'v>; 4]> {
    let native = rule
        .native
        .as_ref()
        .ok_or_else(|| anyhow!("{} is not a rule", rule.label))?;
    let class = crate::native_rules::rule_class(&native.rule_class).ok_or_else(|| {
        anyhow!(
            "{}: aspects over native rule class {} are not modeled",
            rule.label,
            native.rule_class
        )
    })?;
    let mut attrs: BTreeMap<String, Value<'v>> = BTreeMap::new();
    let mut files: BTreeMap<String, Value<'v>> = BTreeMap::new();
    let mut single_files: BTreeMap<String, Value<'v>> = BTreeMap::new();
    let mut executables: BTreeMap<String, Value<'v>> = BTreeMap::new();
    // The Targets of one attribute's prerequisites, in edge order.
    let prerequisites = |name: &str| -> anyhow::Result<Vec<&DependencyTarget>> {
        base.attribute_keys
            .get(name)
            .into_iter()
            .flatten()
            .map(|key| {
                dependencies
                    .get(key)
                    .ok_or_else(|| anyhow!("attribute {name}: dependency {key} was not resolved"))
            })
            .collect()
    };
    let labels = |values: &[String]| -> anyhow::Result<Value<'v>> {
        Ok(heap.alloc(
            values
                .iter()
                .map(|label| crate::starlark_types::alloc_label(heap, &canonical(label)))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ))
    };
    attrs.insert("name".to_owned(), heap.alloc(rule.label.name.as_str()));
    for attribute in &class.attrs {
        let name = attribute.name;
        if name == "exec_group_compatible_with" {
            continue;
        }
        let explicit = native.attrs.get(name);
        let value = match (name, attribute.ty) {
            (_, NativeType::TriState) => continue,
            ("testonly", _) => heap.alloc(base.testonly),
            ("deprecation", _) => base
                .deprecation
                .as_deref()
                .map(|text| heap.alloc(text))
                .unwrap_or_else(Value::new_none),
            ("tags", _) => heap.alloc(native.tags.clone()),
            ("visibility", _) => labels(&base.visibility)?,
            ("generator_name" | "generator_function" | "generator_location", _)
                if !matches!(explicit, Some(AttrValue::String(_))) =>
            {
                let computed = rule.generator.as_ref().map(|generator| match name {
                    "generator_name" => Some(generator.name.as_str()),
                    "generator_function" => Some(generator.function.as_str()),
                    _ => generator.location.as_deref(),
                });
                match computed {
                    None => heap.alloc(""),
                    Some(Some(text)) => heap.alloc(text),
                    Some(None) => Value::new_none(),
                }
            }
            (_, NativeType::License) => heap.alloc(License::parse(match explicit {
                Some(AttrValue::Strings(values)) => Some(values.as_slice()),
                _ => base.licenses.as_deref(),
            })),
            (_, NativeType::String) => match explicit {
                Some(AttrValue::String(text)) => heap.alloc(text.as_str()),
                _ => heap.alloc(string_default(&native.rule_class, name)),
            },
            (_, NativeType::StringList) => heap.alloc(native.strings(name)),
            (_, NativeType::StringDict) => match explicit {
                Some(AttrValue::StringDict(entries)) => {
                    crate::attribute_values::alloc_string_dict(heap, entries)?
                }
                _ => crate::attribute_values::alloc_string_dict(heap, &[])?,
            },
            (_, NativeType::Bool) => heap.alloc(match explicit {
                Some(AttrValue::Bool(value)) => *value,
                // GenQueryRule.java:50: `strict` defaults to True.
                _ => native.rule_class == "genquery" && name == "strict",
            }),
            (_, NativeType::Int) => heap.alloc(match explicit {
                Some(AttrValue::Int(value)) => *value,
                _ => 0,
            }),
            (_, NativeType::NodepLabel) => match explicit {
                Some(AttrValue::Label(Some(label))) => {
                    crate::starlark_types::alloc_label(heap, &canonical(label))?
                }
                _ => Value::new_none(),
            },
            (_, NativeType::NodepLabelList) => labels(&native.labels(name))?,
            (_, NativeType::OutputList) => heap.alloc(
                native
                    .strings(name)
                    .iter()
                    .map(|output| output_label(heap, rule, output))
                    .collect::<anyhow::Result<Vec<_>>>()?,
            ),
            (_, NativeType::Label) => {
                let targets = prerequisites(name)?;
                files.insert(
                    name.to_owned(),
                    prerequisite_files(heap, targets.iter().map(|t| t.files.as_slice())),
                );
                targets
                    .first()
                    .map(|target| alloc_dependency(heap, target))
                    .unwrap_or_else(Value::new_none)
            }
            (_, NativeType::LabelList) => {
                let targets = prerequisites(name)?;
                files.insert(
                    name.to_owned(),
                    prerequisite_files(heap, targets.iter().map(|t| t.files.as_slice())),
                );
                heap.alloc(
                    targets
                        .iter()
                        .map(|target| alloc_dependency(heap, target))
                        .collect::<Vec<_>>(),
                )
            }
            (_, NativeType::LabelKeyedStringDict) => {
                // convertAttributeValue (:361-369): prerequisite Target to
                // its string value, in prerequisite order.
                let targets = prerequisites(name)?;
                files.insert(
                    name.to_owned(),
                    prerequisite_files(heap, targets.iter().map(|t| t.files.as_slice())),
                );
                let entries = match explicit {
                    Some(AttrValue::LabelKeyedStringDict(entries)) => entries.as_slice(),
                    _ => &[],
                };
                let items = targets
                    .iter()
                    .zip(entries)
                    .map(|(target, (_, value))| {
                        (alloc_dependency(heap, target), heap.alloc(value.as_str()))
                    })
                    .collect::<Vec<_>>();
                heap.alloc(AllocDict(items))
            }
            (_, NativeType::LabelListDict) => bail!(
                "{}: native attribute '{name}' of type {} has no ctx.rule.attr value",
                rule.label,
                attribute.ty.name()
            ),
        };
        attrs.insert(name.to_owned(), value);
    }
    for private in crate::native_rules::aspect_private_attributes(&native.rule_class) {
        let public = private.public_name();
        let value = match private.kind {
            PrivateAttrKind::EmptyTargets => {
                files.insert(public.clone(), heap.alloc(Vec::<Value<'v>>::new()));
                heap.alloc(Vec::<Value<'v>>::new())
            }
            PrivateAttrKind::IsExecutable => heap.alloc(matches!(
                native.attrs.get("executable"),
                Some(AttrValue::Bool(true))
            )),
            PrivateAttrKind::Label { .. } => {
                let targets = prerequisites(private.native)?;
                files.insert(
                    public.clone(),
                    prerequisite_files(heap, targets.iter().map(|t| t.files.as_slice())),
                );
                targets
                    .first()
                    .map(|target| alloc_dependency(heap, target))
                    .unwrap_or_else(Value::new_none)
            }
            PrivateAttrKind::Labels { .. } => {
                let targets = prerequisites(private.native)?;
                files.insert(
                    public.clone(),
                    prerequisite_files(heap, targets.iter().map(|t| t.files.as_slice())),
                );
                heap.alloc(
                    targets
                        .iter()
                        .map(|target| alloc_dependency(heap, target))
                        .collect::<Vec<_>>(),
                )
            }
        };
        attrs.insert(public, value);
    }
    for aspect in base_aspects {
        for (name, value) in aspect.attrs {
            if attrs.contains_key(&name) {
                continue;
            }
            attrs.insert(name.clone(), value);
            if let Some(value) = aspect.files.get(&name) {
                files.insert(name.clone(), *value);
            }
            if let Some(value) = aspect.single_files.get(&name) {
                single_files.insert(name.clone(), *value);
            }
            if let Some(value) = aspect.executables.get(&name) {
                executables.insert(name.clone(), *value);
            }
        }
    }
    Ok([
        heap.alloc(AllocStruct(attrs)),
        heap.alloc(AllocStruct(files)),
        heap.alloc(AllocStruct(single_files)),
        heap.alloc(AllocStruct(executables)),
    ])
}

/// A retained native label in Label's canonical text.
fn canonical(label: &str) -> String {
    crate::label::Label::parse_canonical(label)
        .map(|label| label.to_string())
        .unwrap_or_else(|_| label.to_owned())
}

/// The non-empty default of a native string attribute (else `""`):
/// PlatformRule.java's `missing_toolchain_error`, the `scope` of
/// config_feature_flag/label_flag/label_setting (`universal`).
fn string_default(class: &str, name: &str) -> &'static str {
    match (class, name) {
        ("platform", "missing_toolchain_error") => {
            crate::platforms::DEFAULT_MISSING_TOOLCHAIN_ERROR
        }
        ("config_feature_flag" | "label_flag" | "label_setting", "scope") => "universal",
        _ => "",
    }
}
