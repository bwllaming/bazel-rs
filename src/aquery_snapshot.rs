//! The payload of an aquery snapshot (docs/AQUERY.md "Snapshot"): what a
//! repeated `aquery` of the same universe and options prints from without
//! analyzing again. The snapshot itself (key, recorded inputs, validation,
//! repository replay, saved diagnostics, two history slots) is the
//! configured-graph snapshot's (crate::cquery_state); this module holds what
//! aquery adds to the configured graph:
//!
//! * the dump's configurations;
//! * every planned action of the universe **described** (its
//!   [`ActionRecord`]s, computed at the save by the very code a cold aquery
//!   prints from, `Engine::aquery_describe_snapshot`): a hit runs no
//!   analysis, no Starlark and no description, only the expression's
//!   selection over the stored configured graph and the formatters;
//! * for a record whose inputs are its planned action's
//!   ([`ActionRecord::listed_from`]), that action's input listing as
//!   [`Segment`]s: its own inputs and references into a table of the shared
//!   input groups (crate::scheduler::InputGroup, each stored once: a
//!   1,655-file Python runtime listed by 70k PyCompile actions), expanded and
//!   deduplicated exactly like crate::aquery::listed_inputs when the record
//!   is written.
//!
//! Records are stored by configured target (graph node), in **blocks** of
//! consecutive nodes, each block and each chunk of the group table deflated
//! on its own (crate::snapshot::format::compress_plan): a hit inflates only
//! the blocks of the nodes the expression selected and the groups their
//! listings reference. The block index, the group chunk index, the
//! configurations and the graph are the PLAN section; the blobs are the
//! AQUERY section.
use crate::aquery::{ActionRecord, Configuration};
use crate::engine::cquery::Graph;
use crate::scheduler::{InputGroup, PlannedAction};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

/// Planned actions plus TestRunner records per block (a block never splits
/// a configured target's records).
pub const BLOCK: usize = 256;
/// Shared input groups per stored chunk of the group table.
const GROUP_CHUNK: usize = 32;

/// A shared input group as a listing reads it: its inputs' and its
/// generated inputs' destinations.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GroupListing {
    pub inputs: Vec<String>,
    pub dependencies: Vec<String>,
}

/// A run of a listing, in order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Segment {
    Own(Vec<String>),
    /// The inputs of the shared group at this index of the table.
    GroupInputs(u32),
    /// The generated inputs of the shared group at this index.
    GroupDependencies(u32),
}

/// A planned action's inputs as crate::aquery::listed_inputs lists them
/// (before deduplication), and its input order (Meta::input_order).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub segments: Vec<Segment>,
    pub input_order: Option<Vec<String>>,
}

/// One stored record: listed from `listing` when it was listed from its
/// planned action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredRecord {
    pub record: ActionRecord,
    pub listing: Option<Listing>,
}

/// The records of one configured target (graph node), in the order the
/// dump lists them: its planned actions' in plan order, then its TestRunner
/// actions'.
pub type Unit = (u32, Vec<StoredRecord>);

/// A deflated blob in the AQUERY section.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blob {
    pub offset: u64,
    pub len: u64,
}

/// A block of units: the graph nodes `first..=last` (ascending).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockRef {
    pub first: u32,
    pub last: u32,
    pub blob: Blob,
}

/// The PLAN section of an aquery snapshot (owned, decoded).
#[derive(Debug, PartialEq, Deserialize)]
pub struct Plan {
    pub graph: Graph,
    pub configurations: Vec<Configuration>,
    pub group_count: u32,
    pub groups: Vec<Blob>,
    pub blocks: Vec<BlockRef>,
}

/// [`Plan`] borrowed for encoding (the same fields in the same order).
#[derive(Serialize)]
pub struct PlanRef<'a> {
    pub graph: &'a Graph,
    pub configurations: &'a [Configuration],
    pub group_count: u32,
    pub groups: &'a [Blob],
    pub blocks: &'a [BlockRef],
}

/// The described records of an analysis, encoded
/// (`Engine::aquery_describe_snapshot`).
#[derive(Debug, Default)]
pub struct Records {
    pub group_count: u32,
    pub groups: Vec<Blob>,
    pub blocks: Vec<BlockRef>,
    /// The AQUERY section: the blobs.
    pub bytes: Vec<u8>,
}

impl Records {
    fn push(&mut self, deflated: &[u8]) -> Blob {
        let blob = Blob {
            offset: self.bytes.len() as u64,
            len: deflated.len() as u64,
        };
        self.bytes.extend_from_slice(deflated);
        blob
    }

    /// Append the group table's chunks.
    pub fn push_groups(&mut self, table: &GroupTable) -> Result<(), String> {
        self.group_count = u32::try_from(table.table.len()).map_err(|e| e.to_string())?;
        for chunk in table.table.chunks(GROUP_CHUNK) {
            let raw = postcard::to_stdvec(chunk).map_err(|e| e.to_string())?;
            let blob = self.push(&crate::snapshot::format::compress_plan(&raw));
            self.groups.push(blob);
        }
        Ok(())
    }

    /// Append one encoded block ([`encode_block`]) of the nodes
    /// `first..=last`.
    pub fn push_block(&mut self, first: u32, last: u32, deflated: &[u8]) {
        let blob = self.push(deflated);
        self.blocks.push(BlockRef { first, last, blob });
    }
}

/// One block's units, encoded ([`TabledBlock`]) and deflated.
pub fn encode_block(units: &[Unit]) -> Result<Vec<u8>, String> {
    let mut block = BlockEncoder::default();
    for (node, records) in units {
        block.start(*node);
        for stored in records {
            block.push(&stored.record, stored.listing.as_ref());
        }
    }
    block.finish()
}

/// A block being encoded record by record (`Engine::aquery_describe_snapshot`
/// drops each described record once it is in the block: a block's memory is
/// its distinct strings and their indices, not every record's strings).
#[derive(Default)]
pub struct BlockEncoder {
    table: Strings,
    units: Vec<(u32, Vec<TabledRecord>)>,
}

impl BlockEncoder {
    /// Start the records of the graph node `node` (ascending).
    pub fn start(&mut self, node: u32) {
        self.units.push((node, Vec::new()));
    }

    /// One record of the current node, with its listing.
    pub fn push(&mut self, record: &ActionRecord, listing: Option<&Listing>) {
        let tabled = TabledRecord::of(record, listing, &mut self.table);
        if let Some((_, records)) = self.units.last_mut() {
            records.push(tabled);
        }
    }

    /// The encoded, deflated block.
    pub fn finish(self) -> Result<Vec<u8>, String> {
        let block = TabledBlockRef {
            strings: self.table.strings.iter().map(|text| &**text).collect(),
            units: self.units,
        };
        let raw = postcard::to_stdvec(&block).map_err(|e| e.to_string())?;
        Ok(crate::snapshot::format::compress_plan(&raw))
    }
}

/// [`encode_block`]'s units.
fn decode_block(records: &[u8], blob_ref: &Blob) -> Result<Vec<Unit>, String> {
    let block: TabledBlock = inflate(records, blob_ref)?;
    let strings = &block.strings;
    block
        .units
        .into_iter()
        .map(|(node, records)| {
            Ok((
                node,
                records
                    .into_iter()
                    .map(|record| record.decode(strings))
                    .collect::<Result<_, String>>()?,
            ))
        })
        .collect()
}

/// A block as stored: its distinct strings once (an action lists the paths,
/// flags and environment of its siblings: deflate's 32 KiB window would see
/// few of the repeats), and its units with every string an index into them.
#[derive(Deserialize)]
struct TabledBlock {
    strings: Vec<String>,
    units: Vec<(u32, Vec<TabledRecord>)>,
}

/// [`TabledBlock`] borrowing the strings of the units it encodes.
#[derive(Serialize)]
struct TabledBlockRef<'a> {
    strings: Vec<&'a str>,
    units: Vec<(u32, Vec<TabledRecord>)>,
}

/// A block's string table being built: each distinct string once.
#[derive(Default)]
struct Strings {
    index: HashMap<std::rc::Rc<str>, u32>,
    strings: Vec<std::rc::Rc<str>>,
}

impl Strings {
    fn of(&mut self, text: &str) -> u32 {
        if let Some(&index) = self.index.get(text) {
            return index;
        }
        let index = self.strings.len() as u32;
        let shared: std::rc::Rc<str> = std::rc::Rc::from(text);
        self.strings.push(std::rc::Rc::clone(&shared));
        self.index.insert(shared, index);
        index
    }

    fn all(&mut self, texts: &[String]) -> Vec<u32> {
        texts.iter().map(|text| self.of(text)).collect()
    }

    fn pairs(&mut self, pairs: &[(String, String)]) -> Vec<(u32, u32)> {
        pairs
            .iter()
            .map(|(key, value)| (self.of(key), self.of(value)))
            .collect()
    }
}

fn string(strings: &[String], index: u32) -> Result<String, String> {
    strings
        .get(index as usize)
        .cloned()
        .ok_or_else(|| "invalid aquery snapshot string index".to_owned())
}

fn strings_of(strings: &[String], indices: Vec<u32>) -> Result<Vec<String>, String> {
    indices
        .into_iter()
        .map(|index| string(strings, index))
        .collect()
}

fn pairs_of(strings: &[String], pairs: Vec<(u32, u32)>) -> Result<Vec<(String, String)>, String> {
    pairs
        .into_iter()
        .map(|(key, value)| Ok((string(strings, key)?, string(strings, value)?)))
        .collect()
}

#[derive(Serialize, Deserialize)]
enum TabledSegment {
    Own(Vec<u32>),
    GroupInputs(u32),
    GroupDependencies(u32),
}

/// A [`StoredRecord`] with its strings in the block's table: every field of
/// [`ActionRecord`] (destructured exhaustively: a new field must be stored)
/// but `listed_from`, which the stored listing stands for.
#[derive(Serialize, Deserialize)]
pub struct TabledRecord {
    target: u32,
    display_target: u32,
    rule_class: Option<u32>,
    configuration: u64,
    aspects: Vec<(u32, Vec<(u32, u32)>)>,
    mnemonic: u32,
    description: u32,
    action_key: u32,
    discovers_inputs: bool,
    arguments: Option<Vec<u32>>,
    environment: Vec<(u32, u32)>,
    inputs: Vec<u32>,
    outputs: Vec<(u32, bool)>,
    execution_info: Vec<(u32, u32)>,
    execution_platform: Option<u32>,
    display_execution_platform: Option<u32>,
    file_write: Option<(bool, u32)>,
    template: Option<(u32, Vec<(u32, u32)>)>,
    unresolved_symlink: Option<u32>,
    param_file: Option<Vec<u32>>,
    pretty: Option<u32>,
    runfiles_support: bool,
    text_environment: Option<Vec<(u32, u32)>>,
    input_order: Option<Vec<u32>>,
    expansion_error: Option<u32>,
    listing: Option<(Vec<TabledSegment>, Option<Vec<u32>>)>,
}

impl TabledRecord {
    fn of(record: &ActionRecord, listing: Option<&Listing>, t: &mut Strings) -> Self {
        let ActionRecord {
            target,
            display_target,
            rule_class,
            configuration,
            aspects,
            mnemonic,
            description,
            action_key,
            discovers_inputs,
            arguments,
            environment,
            inputs,
            outputs,
            execution_info,
            execution_platform,
            display_execution_platform,
            file_write,
            template,
            unresolved_symlink,
            param_file,
            pretty,
            runfiles_support,
            text_environment,
            input_order,
            expansion_error,
            listed_from: _,
        } = record;
        Self {
            target: t.of(target),
            display_target: t.of(display_target),
            rule_class: rule_class.as_deref().map(|text| t.of(text)),
            configuration: *configuration as u64,
            aspects: aspects
                .iter()
                .map(|aspect| (t.of(&aspect.name), t.pairs(&aspect.parameters)))
                .collect(),
            mnemonic: t.of(mnemonic),
            description: t.of(description),
            action_key: t.of(action_key),
            discovers_inputs: *discovers_inputs,
            arguments: arguments.as_deref().map(|items| t.all(items)),
            environment: t.pairs(environment),
            inputs: t.all(inputs),
            outputs: outputs
                .iter()
                .map(|(path, tree)| (t.of(path), *tree))
                .collect(),
            execution_info: t.pairs(execution_info),
            execution_platform: execution_platform.as_deref().map(|text| t.of(text)),
            display_execution_platform: display_execution_platform
                .as_deref()
                .map(|text| t.of(text)),
            file_write: file_write
                .as_ref()
                .map(|(executable, contents)| (*executable, t.of(contents))),
            template: template
                .as_ref()
                .map(|(text, substitutions)| (t.of(text), t.pairs(substitutions))),
            unresolved_symlink: unresolved_symlink.as_deref().map(|text| t.of(text)),
            param_file: param_file.as_deref().map(|items| t.all(items)),
            pretty: pretty.as_deref().map(|text| t.of(text)),
            runfiles_support: *runfiles_support,
            text_environment: text_environment.as_deref().map(|pairs| t.pairs(pairs)),
            input_order: input_order.as_deref().map(|items| t.all(items)),
            expansion_error: expansion_error.as_deref().map(|text| t.of(text)),
            listing: listing.map(|listing| {
                (
                    listing
                        .segments
                        .iter()
                        .map(|segment| match segment {
                            Segment::Own(paths) => TabledSegment::Own(t.all(paths)),
                            Segment::GroupInputs(index) => TabledSegment::GroupInputs(*index),
                            Segment::GroupDependencies(index) => {
                                TabledSegment::GroupDependencies(*index)
                            }
                        })
                        .collect(),
                    listing.input_order.as_deref().map(|items| t.all(items)),
                )
            }),
        }
    }

    fn decode(self, s: &[String]) -> Result<StoredRecord, String> {
        let one = |index: u32| string(s, index);
        let maybe = |index: Option<u32>| index.map(one).transpose();
        let record = ActionRecord {
            target: one(self.target)?,
            display_target: one(self.display_target)?,
            rule_class: maybe(self.rule_class)?,
            configuration: usize::try_from(self.configuration).map_err(|e| e.to_string())?,
            aspects: self
                .aspects
                .into_iter()
                .map(|(name, parameters)| {
                    Ok(crate::aquery::Aspect {
                        name: one(name)?,
                        parameters: pairs_of(s, parameters)?,
                    })
                })
                .collect::<Result<_, String>>()?,
            mnemonic: one(self.mnemonic)?,
            description: one(self.description)?,
            action_key: one(self.action_key)?,
            discovers_inputs: self.discovers_inputs,
            arguments: self
                .arguments
                .map(|items| strings_of(s, items))
                .transpose()?,
            environment: pairs_of(s, self.environment)?,
            inputs: strings_of(s, self.inputs)?,
            outputs: self
                .outputs
                .into_iter()
                .map(|(path, tree)| Ok((one(path)?, tree)))
                .collect::<Result<_, String>>()?,
            execution_info: pairs_of(s, self.execution_info)?,
            execution_platform: maybe(self.execution_platform)?,
            display_execution_platform: maybe(self.display_execution_platform)?,
            file_write: self
                .file_write
                .map(|(executable, contents)| Ok::<_, String>((executable, one(contents)?)))
                .transpose()?,
            template: self
                .template
                .map(|(text, substitutions)| {
                    Ok::<_, String>((one(text)?, pairs_of(s, substitutions)?))
                })
                .transpose()?,
            unresolved_symlink: maybe(self.unresolved_symlink)?,
            param_file: self
                .param_file
                .map(|items| strings_of(s, items))
                .transpose()?,
            pretty: maybe(self.pretty)?,
            runfiles_support: self.runfiles_support,
            text_environment: self
                .text_environment
                .map(|pairs| pairs_of(s, pairs))
                .transpose()?,
            input_order: self
                .input_order
                .map(|items| strings_of(s, items))
                .transpose()?,
            expansion_error: maybe(self.expansion_error)?,
            listed_from: None,
        };
        let listing = self
            .listing
            .map(|(segments, input_order)| {
                Ok::<_, String>(Listing {
                    segments: segments
                        .into_iter()
                        .map(|segment| {
                            Ok(match segment {
                                TabledSegment::Own(paths) => Segment::Own(strings_of(s, paths)?),
                                TabledSegment::GroupInputs(index) => Segment::GroupInputs(index),
                                TabledSegment::GroupDependencies(index) => {
                                    Segment::GroupDependencies(index)
                                }
                            })
                        })
                        .collect::<Result<_, String>>()?,
                    input_order: input_order.map(|items| strings_of(s, items)).transpose()?,
                })
            })
            .transpose()?;
        Ok(StoredRecord { record, listing })
    }
}

/// The shared input groups the stored listings reference, each once (by
/// value: equal groups of distinct allocations are one entry, so the
/// payload does not depend on which allocations a parallel analysis
/// shared), in first-seen order.
#[derive(Default)]
pub struct GroupTable {
    /// Group allocations by address (identities only, never dereferenced).
    by_pointer: HashMap<usize, u32>,
    by_value: HashMap<GroupListing, u32>,
    table: Vec<GroupListing>,
}

impl GroupTable {
    /// Note the shared runs (the groups that are not runfiles trees: a
    /// runfiles tree is listed as its root) of `node`.
    pub fn note(&mut self, node: &PlannedAction) {
        for group in node.groups.iter().filter(|group| !group.runfiles) {
            let pointer = Arc::as_ptr(group) as usize;
            if self.by_pointer.contains_key(&pointer) {
                continue;
            }
            let listing = GroupListing {
                inputs: group
                    .inputs
                    .iter()
                    .map(|(_, destination)| destination.to_string_lossy().into_owned())
                    .collect(),
                dependencies: group
                    .dependencies
                    .iter()
                    .map(|dependency| dependency.destination.to_string_lossy().into_owned())
                    .collect(),
            };
            let next = self.table.len() as u32;
            let index = *self.by_value.entry(listing.clone()).or_insert(next);
            if index == next {
                self.table.push(listing);
            }
            self.by_pointer.insert(pointer, index);
        }
    }

    fn index(&self, group: &Arc<InputGroup>) -> Option<u32> {
        self.by_pointer.get(&(Arc::as_ptr(group) as usize)).copied()
    }

    /// The listing of `node` (every one of its shared runs noted), as
    /// crate::aquery::listed_inputs lists it before deduplication:
    /// PlannedAction::runs_listed's inputs (the shared runs at
    /// `groups_at`, else after the own inputs), generated inputs (the
    /// runs' between the own ones before and after `groups_at`), embedded
    /// inputs, then the runfiles trees' roots.
    pub fn listing(&self, node: &PlannedAction) -> Option<Listing> {
        let path = |path: &std::path::Path| path.to_string_lossy().into_owned();
        let action = &node.action;
        let inputs: Vec<String> = action
            .inputs
            .iter()
            .map(|(_, destination)| path(destination))
            .collect();
        let runs: Vec<&Arc<InputGroup>> =
            node.groups.iter().filter(|group| !group.runfiles).collect();
        let mut segments = Vec::new();
        let mut own: Vec<String> = Vec::new();
        let flush = |own: &mut Vec<String>, segments: &mut Vec<Segment>| {
            if !own.is_empty() {
                segments.push(Segment::Own(std::mem::take(own)));
            }
        };
        if runs.is_empty() {
            own.extend(inputs);
            own.extend(
                node.dependencies
                    .iter()
                    .map(|dependency| path(&dependency.destination)),
            );
            own.extend(action.embedded_inputs.iter().map(|(p, _)| path(p)));
            own.extend(node.groups.iter().map(|group| path(&group.root)));
            flush(&mut own, &mut segments);
        } else {
            let indices: Vec<u32> = runs
                .iter()
                .map(|group| self.index(group))
                .collect::<Option<_>>()?;
            // Inputs (scheduler::list_groups of the runs).
            let position = match node.groups_at {
                Some(at) => (at.inputs as usize).min(inputs.len()),
                None => inputs.len(),
            };
            let mut inputs = inputs;
            let after = inputs.split_off(position);
            own.extend(inputs);
            flush(&mut own, &mut segments);
            segments.extend(indices.iter().map(|&index| Segment::GroupInputs(index)));
            own.extend(after);
            // Generated inputs (PlannedAction::own_dependencies around the
            // runs' own).
            let split = match node.groups_at {
                Some(at) => (at.dependencies as usize).min(node.dependencies.len()),
                None => node.dependencies.len(),
            };
            let (before, after) = node.dependencies.split_at(split);
            own.extend(
                before
                    .iter()
                    .map(|dependency| path(&dependency.destination)),
            );
            flush(&mut own, &mut segments);
            segments.extend(
                indices
                    .iter()
                    .map(|&index| Segment::GroupDependencies(index)),
            );
            own.extend(after.iter().map(|dependency| path(&dependency.destination)));
            own.extend(action.embedded_inputs.iter().map(|(p, _)| path(p)));
            own.extend(
                node.groups
                    .iter()
                    .filter(|group| group.runfiles)
                    .map(|group| path(&group.root)),
            );
            flush(&mut own, &mut segments);
        }
        Some(Listing {
            segments,
            input_order: action
                .extra
                .aquery
                .as_ref()
                .and_then(|meta| meta.input_order_paths()),
        })
    }

    /// [`expand`] over this table (the parity check of a stored listing).
    pub fn expand(&self, listing: &Listing) -> Option<Vec<String>> {
        expand(listing, |index| self.table.get(index as usize))
    }
}

/// A listing's inputs, deduplicated (crate::aquery::listed_inputs).
fn expand<'a>(
    listing: &'a Listing,
    group: impl Fn(u32) -> Option<&'a GroupListing>,
) -> Option<Vec<String>> {
    let mut all: Vec<String> = Vec::new();
    for segment in &listing.segments {
        match segment {
            Segment::Own(paths) => all.extend(paths.iter().cloned()),
            Segment::GroupInputs(index) => all.extend(group(*index)?.inputs.iter().cloned()),
            Segment::GroupDependencies(index) => {
                all.extend(group(*index)?.dependencies.iter().cloned())
            }
        }
    }
    Some(crate::aquery::dedup(all))
}

/// The listings of a hit's selected records (crate::aquery::Listings::Stored)
/// and the groups they reference.
#[derive(Debug, Default)]
pub struct StoredListings {
    listings: Vec<Listing>,
    groups: HashMap<u32, GroupListing>,
}

impl StoredListings {
    /// The inputs and input order of the listing at `index`.
    pub fn listed(&self, index: usize) -> Option<(Vec<String>, Option<Vec<String>>)> {
        let listing = self.listings.get(index)?;
        let inputs = expand(listing, |group| self.groups.get(&group))?;
        Some((inputs, listing.input_order.clone()))
    }
}

/// What an aquery snapshot hit read: the PLAN and the AQUERY section.
#[derive(Debug, PartialEq)]
pub struct Hit {
    pub plan: Plan,
    pub records: Vec<u8>,
}

fn blob<'a>(records: &'a [u8], blob: &Blob) -> Result<&'a [u8], String> {
    let start = usize::try_from(blob.offset).map_err(|e| e.to_string())?;
    let len = usize::try_from(blob.len).map_err(|e| e.to_string())?;
    start
        .checked_add(len)
        .and_then(|end| records.get(start..end))
        .ok_or_else(|| "aquery snapshot blob outside its section".to_owned())
}

fn inflate<T: serde::de::DeserializeOwned>(records: &[u8], blob_ref: &Blob) -> Result<T, String> {
    let raw = crate::snapshot::format::decompress_plan(blob(records, blob_ref)?.to_vec())?;
    postcard::from_bytes(&raw).map_err(|e| format!("aquery snapshot decode: {e}"))
}

impl Hit {
    /// Check the indices of the PLAN section (the block ranges ascending and
    /// inside the graph, every blob inside the AQUERY section).
    pub fn check(&self) -> Result<(), String> {
        let nodes = self.plan.graph.nodes.len();
        let mut previous: Option<u32> = None;
        for block in &self.plan.blocks {
            if block.first > block.last
                || block.last as usize >= nodes
                || previous.is_some_and(|previous| block.first <= previous)
            {
                return Err("invalid aquery snapshot block index".into());
            }
            previous = Some(block.last);
            blob(&self.records, &block.blob)?;
        }
        if self.plan.groups.len() != (self.plan.group_count as usize).div_ceil(GROUP_CHUNK) {
            return Err("invalid aquery snapshot group index".into());
        }
        for group in &self.plan.groups {
            blob(&self.records, group)?;
        }
        Ok(())
    }

    /// The records of the `selected` graph nodes, per node in the dump's
    /// order (nodes ascending), and their listings: only the blocks holding
    /// a selected node and the group chunks their listings reference are
    /// inflated (on up to `threads` threads).
    pub fn load(
        &self,
        selected: &BTreeSet<u32>,
        threads: usize,
    ) -> Result<(Vec<(u32, ActionRecord)>, StoredListings), String> {
        let blocks: Vec<&BlockRef> = self
            .plan
            .blocks
            .iter()
            .filter(|block| selected.range(block.first..=block.last).next().is_some())
            .collect();
        let decoded = on_threads(&blocks, threads, &|block: &&BlockRef| {
            let units: Vec<Unit> = decode_block(&self.records, &block.blob)?;
            Ok(units
                .into_iter()
                .filter(|(node, _)| selected.contains(node))
                .collect::<Vec<Unit>>())
        })?;
        let configurations = self.plan.configurations.len();
        let mut actions = Vec::new();
        let mut listings = StoredListings::default();
        let mut previous: Option<u32> = None;
        for (node, records) in decoded.into_iter().flatten() {
            if previous.is_some_and(|previous| node <= previous)
                || node as usize >= self.plan.graph.nodes.len()
            {
                return Err("invalid aquery snapshot block".into());
            }
            previous = Some(node);
            for StoredRecord {
                mut record,
                listing,
            } in records
            {
                if record.configuration >= configurations {
                    return Err("invalid aquery snapshot configuration index".into());
                }
                if let Some(listing) = listing {
                    record.listed_from = Some(listings.listings.len());
                    listings.listings.push(listing);
                }
                actions.push((node, record));
            }
        }
        let mut chunks: BTreeSet<usize> = BTreeSet::new();
        for listing in &listings.listings {
            for segment in &listing.segments {
                if let Segment::GroupInputs(index) | Segment::GroupDependencies(index) = segment {
                    if *index >= self.plan.group_count {
                        return Err("invalid aquery snapshot group reference".into());
                    }
                    chunks.insert(*index as usize / GROUP_CHUNK);
                }
            }
        }
        let chunks: Vec<usize> = chunks.into_iter().collect();
        let groups = on_threads(&chunks, threads, &|chunk: &usize| {
            let groups: Vec<GroupListing> = inflate(&self.records, &self.plan.groups[*chunk])?;
            let expected = (self.plan.group_count as usize - chunk * GROUP_CHUNK).min(GROUP_CHUNK);
            if groups.len() != expected {
                return Err("invalid aquery snapshot group chunk".to_owned());
            }
            Ok(groups)
        })?;
        for (chunk, groups) in chunks.iter().zip(groups) {
            for (offset, group) in groups.into_iter().enumerate() {
                listings
                    .groups
                    .insert((chunk * GROUP_CHUNK + offset) as u32, group);
            }
        }
        Ok((actions, listings))
    }
}

/// `work` of every item on up to `threads` threads (at most 32), results
/// in order; the first error in order is the result.
pub fn on_threads<I: Sync, T: Send>(
    items: &[I],
    threads: usize,
    work: &(dyn Fn(&I) -> Result<T, String> + Sync),
) -> Result<Vec<T>, String> {
    let threads = threads.clamp(1, 32).min(items.len());
    if threads <= 1 {
        return items.iter().map(work).collect();
    }
    let results: Vec<std::sync::Mutex<Option<Result<T, String>>>> =
        items.iter().map(|_| std::sync::Mutex::new(None)).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let run = || {
        loop {
            let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(item) = items.get(index) else {
                break;
            };
            let result = work(item);
            *results[index]
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
        }
    };
    std::thread::scope(|scope| {
        for _ in 1..threads {
            // The describing threads' stack (deep depset fingerprints) like
            // Engine::aquery_describe's.
            std::thread::Builder::new()
                .name("aquery-snapshot".to_owned())
                .stack_size(16 << 20)
                .spawn_scoped(scope, run)
                .expect("spawn an aquery snapshot thread");
        }
        run();
    });
    results
        .into_iter()
        .map(|result| {
            result
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .unwrap_or_else(|| Err("an aquery snapshot decoder panicked".to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{Dependency, GroupsAt};
    use std::path::PathBuf;

    fn dependency(destination: &str) -> Dependency {
        Dependency {
            action_id: "//pkg:gen#0".into(),
            output_index: 0,
            destination: PathBuf::from(destination).into(),
        }
    }

    fn group(root: &str, runfiles: bool, inputs: &[&str], generated: &[&str]) -> Arc<InputGroup> {
        Arc::new(InputGroup {
            root: root.into(),
            runfiles,
            sources: inputs
                .iter()
                .map(|path| PathBuf::from(format!("/src/{path}")).into())
                .collect(),
            inputs: inputs
                .iter()
                .map(|path| {
                    (
                        PathBuf::from(format!("/src/{path}")).into(),
                        PathBuf::from(path).into(),
                    )
                })
                .collect(),
            dependencies: generated.iter().map(|path| dependency(path)).collect(),
            tree_inputs: Vec::new(),
            symlink_inputs: Vec::new(),
            constant_metadata: Vec::new(),
            empty_files: Vec::new(),
            valid: true,
        })
    }

    /// A stored listing expands to crate::aquery::listed_inputs of its
    /// planned action: shared runs at `groups_at` or after the own inputs,
    /// runfiles trees as their roots, duplicates across runs and own
    /// inputs dropped (first occurrence kept), two equal groups of distinct
    /// allocations one table entry.
    #[test]
    fn stored_listings_expand_like_listed_inputs() {
        let toolchain = group("tc", false, &["tc/a.h", "pkg/x.h", "tc/b.h"], &["gen/tc.h"]);
        let toolchain_copy = group("tc", false, &["tc/a.h", "pkg/x.h", "tc/b.h"], &["gen/tc.h"]);
        let runtime = group("rt", false, &["rt/lib.py"], &[]);
        let tree = group("bin/tool.runfiles", true, &["bin/tool.runfiles/x"], &[]);
        let own = || {
            let action = crate::action::Action {
                id: "//pkg:t#0".into(),
                inputs: vec![
                    ("/src/pkg/x.h".into(), "pkg/x.h".into()),
                    ("/src/pkg/a.cc".into(), "pkg/a.cc".into()),
                    ("/src/tc/b.h".into(), "tc/b.h".into()),
                ],
                ..Default::default()
            };
            PlannedAction::new(
                action,
                vec![
                    dependency("gen/own.h"),
                    dependency("gen/tc.h"),
                    dependency("gen/last.h"),
                ],
            )
        };
        let mut cases = Vec::new();
        for groups in [
            vec![],
            vec![Arc::clone(&tree)],
            vec![Arc::clone(&toolchain)],
            vec![
                Arc::clone(&toolchain_copy),
                Arc::clone(&runtime),
                Arc::clone(&tree),
            ],
            vec![Arc::clone(&runtime), Arc::clone(&toolchain)],
        ] {
            for at in [
                None,
                Some(GroupsAt {
                    inputs: 0,
                    dependencies: 0,
                }),
                Some(GroupsAt {
                    inputs: 1,
                    dependencies: 2,
                }),
                Some(GroupsAt {
                    inputs: 9,
                    dependencies: 9,
                }),
            ] {
                let mut node = own();
                node.groups = groups.clone();
                node.groups_at = at;
                cases.push(node);
            }
        }
        let mut table = GroupTable::default();
        for node in &cases {
            table.note(node);
        }
        // toolchain and its copy are one entry; the tree is not a run.
        assert_eq!(table.table.len(), 2);
        for node in &cases {
            let listing = table.listing(node).unwrap();
            assert_eq!(
                table.expand(&listing).unwrap(),
                crate::aquery::listed_inputs(node),
                "{:?} {:?}",
                node.groups_at,
                node.groups.len()
            );
        }
    }

    /// A block stores every field of a record and its listing.
    #[test]
    fn blocks_round_trip_every_field() {
        let pair = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        let record = ActionRecord {
            target: "//pkg:t".into(),
            display_target: "//pkg:t".into(),
            rule_class: Some("genrule".into()),
            configuration: 1,
            aspects: vec![crate::aquery::Aspect {
                name: "//:a.bzl%a".into(),
                parameters: vec![pair("p", "v")],
            }],
            mnemonic: "Genrule".into(),
            description: "Executing genrule //pkg:t".into(),
            action_key: "0123".into(),
            discovers_inputs: true,
            arguments: Some(vec!["/bin/bash".into(), "-c".into(), "/bin/bash".into()]),
            environment: vec![pair("PATH", "/bin")],
            inputs: vec!["pkg/in".into()],
            outputs: vec![("bazel-out/out".into(), true)],
            execution_info: vec![pair("no-remote", "")],
            execution_platform: Some("@@platforms//host:host".into()),
            display_execution_platform: Some("@platforms//host".into()),
            file_write: Some((true, "contents".into())),
            template: Some(("tmpl".into(), vec![pair("{X}", "x")])),
            unresolved_symlink: Some("../x".into()),
            param_file: Some(vec!["--flag".into()]),
            pretty: Some("runfiles for //pkg:t".into()),
            runfiles_support: true,
            text_environment: Some(vec![pair("PATH", "/usr/bin")]),
            input_order: Some(vec!["pkg/in".into()]),
            expansion_error: Some("error".into()),
            listed_from: None,
        };
        let listing = Listing {
            segments: vec![
                Segment::Own(vec!["pkg/in".into(), "/bin/bash".into()]),
                Segment::GroupInputs(3),
                Segment::GroupDependencies(4),
            ],
            input_order: Some(vec!["pkg/first".into()]),
        };
        let units: Vec<Unit> = vec![
            (
                2,
                vec![
                    StoredRecord {
                        record: record.clone(),
                        listing: Some(listing),
                    },
                    StoredRecord {
                        record: ActionRecord::default(),
                        listing: None,
                    },
                ],
            ),
            (
                7,
                vec![StoredRecord {
                    record,
                    listing: None,
                }],
            ),
        ];
        let mut records = Records::default();
        records.push_block(2, 7, &encode_block(&units).unwrap());
        assert_eq!(
            decode_block(&records.bytes, &records.blocks[0].blob).unwrap(),
            units
        );
    }

    /// A hit loads the selected nodes' records only, with their listings,
    /// from the blocks and group chunks they need.
    #[test]
    fn hits_load_the_selected_records_and_their_listings() {
        let node = |groups: Vec<Arc<InputGroup>>| {
            let mut node = PlannedAction::new(
                crate::action::Action {
                    id: "//pkg:t#0".into(),
                    inputs: vec![("/src/pkg/a".into(), "pkg/a".into())],
                    ..Default::default()
                },
                Vec::new(),
            );
            node.groups = groups;
            node
        };
        let groups: Vec<Arc<InputGroup>> = (0..40)
            .map(|index| group("g", false, &[&format!("g/{index}")], &[]))
            .collect();
        let nodes: Vec<PlannedAction> = groups
            .iter()
            .map(|group| node(vec![Arc::clone(group)]))
            .collect();
        let mut table = GroupTable::default();
        nodes.iter().for_each(|node| table.note(node));
        let mut records = Records::default();
        records.push_groups(&table).unwrap();
        // Units of nodes 0..40, two per block.
        for pair in (0..40u32).collect::<Vec<_>>().chunks(2) {
            let units: Vec<Unit> = pair
                .iter()
                .map(|&id| {
                    let record = ActionRecord {
                        target: format!("//pkg:{id}"),
                        ..Default::default()
                    };
                    (
                        id,
                        vec![StoredRecord {
                            record,
                            listing: table.listing(&nodes[id as usize]),
                        }],
                    )
                })
                .collect();
            records.push_block(pair[0], pair[1], &encode_block(&units).unwrap());
        }
        let graph_node = || crate::engine::cquery::Node {
            target: crate::cquery_proto::Target::Rule(crate::cquery_proto::Rule {
                name: "//pkg:x".into(),
                ..Default::default()
            }),
            configuration: None,
            edges: Vec::new(),
            incompatible: false,
            attribute_errors: None,
            providers: crate::engine::cquery::NO_PROVIDERS,
            files: Box::default(),
        };
        let graph = Graph {
            nodes: (0..40).map(|_| graph_node()).collect(),
            configurations: Vec::new(),
            top_checksum: String::new(),
            patterns: Default::default(),
            repository_mapping: Default::default(),
            provider_sets: Vec::new(),
            build_options: Vec::new(),
            fact_errors: Default::default(),
            files_recorded: false,
            build_blocks: Default::default(),
            build_recorded: false,
            transitions: Default::default(),
            transitions_recorded: false,
            no_config_configurations: Default::default(),
            fragments: Default::default(),
            fragments_recorded: false,
        };
        let hit = Hit {
            plan: Plan {
                graph,
                configurations: vec![Configuration {
                    mnemonic: "k8-fastbuild".into(),
                    platform_name: "k8".into(),
                    checksum: "abc".into(),
                    is_tool: false,
                }],
                group_count: records.group_count,
                groups: records.groups.clone(),
                blocks: records.blocks.clone(),
            },
            records: records.bytes.clone(),
        };
        hit.check().unwrap();
        let selected: BTreeSet<u32> = [3, 4, 35].into_iter().collect();
        let (actions, listings) = hit.load(&selected, 4).unwrap();
        let targets: Vec<(u32, &str)> = actions
            .iter()
            .map(|(node, record)| (*node, record.target.as_str()))
            .collect();
        assert_eq!(targets, [(3, "//pkg:3"), (4, "//pkg:4"), (35, "//pkg:35")]);
        // Both group chunks (35 is in the second chunk of 32); node 3's
        // records need the first only.
        assert_eq!(listings.groups.len(), 40);
        let (_, first) = hit.load(&[3].into_iter().collect(), 1).unwrap();
        assert_eq!(first.groups.len(), 32);
        for (index, (node, record)) in actions.iter().enumerate() {
            assert_eq!(record.listed_from, Some(index));
            let (inputs, _) = listings.listed(index).unwrap();
            assert_eq!(inputs, ["pkg/a".to_owned(), format!("g/{node}")]);
        }
    }
}
