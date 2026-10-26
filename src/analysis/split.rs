//! Immutable configured split views. Only metadata/map keys are duplicated;
//! all Target/provider/runfiles values use the engine's shared dependency views.
use super::*;
use crate::custom_rule::{CustomInvocation, SplitAttribute};
use starlark::values::dict::AllocDict;

type Pairs<'a> = Vec<(String, &'a DependencyTarget)>;

fn pairs<'a>(
    metadata: &SplitAttribute,
    dependencies: &'a BTreeMap<String, DependencyTarget>,
) -> anyhow::Result<Pairs<'a>> {
    metadata
        .prerequisites
        .iter()
        .map(|p| {
            dependencies
                .get(&p.map_key)
                .map(|target| (p.label.to_string(), target))
                .ok_or_else(|| anyhow!("split dependency {} was not resolved", p.map_key))
        })
        .collect()
}

fn value<'v>(
    heap: Heap<'v>,
    attribute: &AttrValue,
    pairs: &[(String, &DependencyTarget)],
    split_view: bool,
) -> anyhow::Result<Value<'v>> {
    let list = || {
        heap.alloc(
            pairs
                .iter()
                .map(|(_, target)| alloc_dependency(heap, target))
                .collect::<Vec<_>>(),
        )
    };
    let last = |label: &str| {
        pairs
            .iter()
            .rev()
            .find(|(name, _)| name == label)
            .map(|(_, target)| alloc_dependency(heap, target))
            .ok_or_else(|| anyhow!("split prerequisite {label} was not resolved"))
    };
    Ok(match attribute {
        AttrValue::Label(_) if split_view => {
            ensure!(
                pairs.len() == 1,
                "split label attribute has {} prerequisites",
                pairs.len()
            );
            alloc_dependency(heap, pairs[0].1)
        }
        AttrValue::Label(_) | AttrValue::Labels(_) => list(),
        AttrValue::LabelKeyedStringDict(_) if split_view => list(),
        AttrValue::LabelKeyedStringDict(entries) => heap.alloc(AllocDict(
            pairs
                .iter()
                .map(|(label, target)| {
                    let text = entries
                        .iter()
                        .find(|(name, _)| name == label)
                        .map(|(_, text)| text)
                        .ok_or_else(|| anyhow!("split label {label} has no dictionary value"))?;
                    Ok((alloc_dependency(heap, target), heap.alloc(text.as_str())))
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
        )),
        AttrValue::StringKeyedLabelDict(entries) => heap.alloc(AllocDict(
            entries
                .iter()
                .map(|(key, label)| Ok((heap.alloc(key.as_str()), last(label)?)))
                .collect::<anyhow::Result<Vec<_>>>()?,
        )),
        AttrValue::LabelListDict(entries) => heap.alloc(AllocDict(
            entries
                .iter()
                .map(|(key, labels)| {
                    Ok((
                        heap.alloc(key.as_str()),
                        heap.alloc(
                            labels
                                .iter()
                                .map(|label| last(label))
                                .collect::<anyhow::Result<Vec<_>>>()?,
                        ),
                    ))
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
        )),
        _ => bail!("split transition on a non-dependency attribute"),
    })
}

pub(super) struct Values<'v> {
    pub attr: Value<'v>,
    pub files: Value<'v>,
    pub file: Option<Value<'v>>,
    pub executable: Option<Value<'v>>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn allocate<'v>(
    name: &str,
    attribute: &AttrValue,
    spec: &AttrSpec,
    metadata: &SplitAttribute,
    dependencies: &BTreeMap<String, DependencyTarget>,
    context: &Context,
    prefix: &str,
    consumer: Consumer<'_>,
    heap: Heap<'v>,
) -> anyhow::Result<Values<'v>> {
    let pairs = pairs(metadata, dependencies)?;
    for (label, target) in &pairs {
        checked_dependency(label, name, spec, target, prefix, consumer)?;
    }
    if spec.allow_single_file || spec.declaration.executable {
        ensure!(
            pairs.len() <= 1,
            "{prefix}attribute {name} produces more than one prerequisite ({}); singular file/executable access is not valid for a split",
            pairs.len()
        );
    }
    let file = spec.allow_single_file.then(|| {
        pairs
            .first()
            .and_then(|(_, t)| t.files.first())
            .map(|f| alloc_file(heap, f.clone()))
            .unwrap_or_else(Value::new_none)
    });
    let executable = if spec.declaration.executable {
        Some(match pairs.first() {
            Some((_, target)) => {
                let runfiles = target.runfiles.files_to_run(&target.files);
                let file = runfiles
                    .executable()
                    .ok_or_else(|| {
                        anyhow!(
                            "{prefix}{} does not refer to a valid executable target",
                            target.label
                        )
                    })?
                    .clone();
                context
                    .runfiles
                    .executables
                    .borrow_mut()
                    .entry(file.clone())
                    .or_insert(runfiles);
                alloc_file(heap, file)
            }
            None => Value::new_none(),
        })
    } else {
        None
    };
    Ok(Values {
        attr: value(heap, attribute, &pairs, false)?,
        files: prerequisite_files(heap, pairs.iter().map(|(_, t)| t.files.as_slice())),
        file,
        executable,
    })
}

pub(super) fn split_attr<'v>(
    invocation: &CustomInvocation,
    dependencies: &BTreeMap<String, DependencyTarget>,
    heap: Heap<'v>,
) -> anyhow::Result<Value<'v>> {
    let mut attributes = Vec::new();
    for (name, spec) in invocation.schema.iter() {
        if !starlark_transition(spec) {
            continue;
        }
        let attribute = invocation.attrs.get(name).unwrap_or(&spec.default);
        let mut groups: Vec<(Option<String>, Pairs<'_>)> = Vec::new();
        if let Some(metadata) = invocation.split_dependencies.get(name) {
            for p in &metadata.prerequisites {
                let target = dependencies
                    .get(&p.map_key)
                    .ok_or_else(|| anyhow!("split dependency {} was not resolved", p.map_key))?;
                for key in &p.transition_keys {
                    let key = (!key.is_empty()).then(|| key.clone());
                    let index = match groups.iter().position(|(seen, _)| *seen == key) {
                        Some(index) => index,
                        None => {
                            groups.push((key, Vec::new()));
                            groups.len() - 1
                        }
                    };
                    groups[index].1.push((p.label.to_string(), target));
                }
            }
        } else {
            let mut targets = Vec::new();
            for label in dependency_labels(attribute) {
                let key = attribute_dependency_key(label, name, spec);
                let target = dependencies
                    .get(&key)
                    .ok_or_else(|| anyhow!("split dependency {key} was not resolved"))?;
                targets.push((label.to_owned(), target));
            }
            if !targets.is_empty() {
                groups.push((None, targets));
            }
        }
        ensure!(
            groups.len() <= 1 || groups.iter().all(|(key, _)| key.is_some()),
            "split transition cannot mix the empty patch key with named keys"
        );
        let dict = heap.alloc(AllocDict(
            groups
                .iter()
                .map(|(key, targets)| {
                    Ok((
                        key.as_ref()
                            .map(|k| heap.alloc(k.as_str()))
                            .unwrap_or_else(Value::new_none),
                        value(heap, attribute, targets, true)?,
                    ))
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
        ));
        attributes.push((name.as_str(), dict));
    }
    Ok(heap.alloc(AllocStruct(attributes)))
}
