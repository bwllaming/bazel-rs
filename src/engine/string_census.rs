//! String census at the end of phase 2 (`BAZEL_RS_STRING_CENSUS=1`;
//! docs/MEMORY.md "String interning").
//!
//! Walks the planned actions, the labels the main planning maps hold and
//! the planned artifacts, and prints per category, on stderr:
//!
//! ```text
//! STRING-CENSUS <category> count=<n> bytes=<b> unique=<u> unique_bytes=<ub> saving=<s>
//! ```
//!
//! `count` is the number of string values (or lists, maps), `bytes` the heap
//! bytes they hold (allocator size classes; an `Arc`-shared string counts
//! once per allocation), `unique`/`unique_bytes` the distinct contents and
//! their length, and `saving` what content interning would save:
//! `bytes - (unique_bytes + 16 * unique)` (an arena copy of each distinct
//! string plus a table entry). Whole-list categories (`*_lists`, `*_maps`)
//! measure sharing one value among equal lists instead.
//! Inline sizes (24 bytes per `String`) are not included.
//!
//! A diagnostic only: the walk costs seconds and memory on large builds.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::{ActionPlanning, Artifact, ConfiguredNodes, Engine, Plan};
use crate::fast_hash::{FxHashMap, FxHashSet};
use crate::label::Label;
use crate::shared_path::SharedPath;

/// `BAZEL_RS_STRING_CENSUS` set to anything but empty or `0`.
pub(super) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    // snapshot-unrecorded: a diagnostics switch printing a report on stderr; no result depends on it.
    *ENABLED.get_or_init(|| crate::diagnostic_switch(std::env::var_os("BAZEL_RS_STRING_CENSUS")))
}

/// jemalloc's size class of an allocation of `n` bytes (0 for none).
fn size_class(n: usize) -> u64 {
    let n = n as u64;
    if n == 0 {
        0
    } else if n <= 8 {
        8
    } else if n <= 128 {
        n.div_ceil(16) * 16
    } else {
        let k = 63 - (n - 1).leading_zeros() as u64; // 2^k < n <= 2^(k+1)
        let step = 1u64 << (k - 1);
        n.div_ceil(step) * step
    }
}

#[derive(Default)]
struct Strings<'a> {
    count: u64,
    bytes: u64,
    unique: FxHashSet<&'a [u8]>,
    unique_bytes: u64,
    allocations: FxHashSet<usize>,
}

impl<'a> Strings<'a> {
    fn content(&mut self, text: &'a [u8]) {
        self.count += 1;
        if self.unique.insert(text) {
            self.unique_bytes += text.len() as u64;
        }
    }

    /// An owned string of capacity `capacity`.
    fn owned(&mut self, text: &'a [u8], capacity: usize) {
        self.bytes += size_class(capacity);
        self.content(text);
    }

    /// A shared allocation (`Arc<str>`, `Arc<Path>`): counted once per
    /// pointer (the two counts of the `ArcInner` included).
    fn shared(&mut self, pointer: usize, text: &'a [u8]) {
        if self.allocations.insert(pointer) {
            self.bytes += size_class(text.len() + 16);
        }
        self.content(text);
    }

    fn string(&mut self, text: &'a String) {
        self.owned(text.as_bytes(), text.capacity());
    }

    /// An interned string: its arena copy counted once.
    fn interned(&mut self, text: &'a crate::interner::IStr) {
        let text: &'static str = text.as_str();
        if self.allocations.insert(text.as_ptr() as usize) {
            self.bytes += text.len() as u64;
        }
        self.content(text.as_bytes());
    }

    fn path_buf(&mut self, path: &'a PathBuf) {
        self.owned(path.as_os_str().as_bytes(), path.capacity());
    }

    fn shared_path(&mut self, path: &'a SharedPath) {
        let path: &'a Path = path.as_path();
        self.shared(
            path.as_os_str().as_bytes().as_ptr() as usize,
            path.as_os_str().as_bytes(),
        );
    }

    fn saving(&self) -> i64 {
        self.bytes as i64 - (self.unique_bytes + 16 * self.unique.len() as u64) as i64
    }
}

/// Whole values (argument lists, environment maps) shared by content.
struct Wholes<'a, T: ?Sized + Eq + std::hash::Hash> {
    count: u64,
    bytes: u64,
    unique: FxHashMap<&'a T, u64>,
    unique_bytes: u64,
}

impl<T: ?Sized + Eq + std::hash::Hash> Default for Wholes<'_, T> {
    fn default() -> Self {
        Self {
            count: 0,
            bytes: 0,
            unique: FxHashMap::default(),
            unique_bytes: 0,
        }
    }
}

impl<'a, T: ?Sized + Eq + std::hash::Hash> Wholes<'a, T> {
    fn add(&mut self, value: &'a T, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
        if self.unique.insert(value, bytes).is_none() {
            self.unique_bytes += bytes;
        }
    }

    fn saving(&self) -> i64 {
        self.bytes as i64 - (self.unique_bytes + 16 * self.unique.len() as u64) as i64
    }
}

fn strings_bytes<'s>(strings: impl Iterator<Item = &'s String>) -> u64 {
    strings.map(|text| size_class(text.capacity())).sum()
}

/// A `BTreeMap<String, String>`'s heap: its strings and ~8 entries per
/// 11-slot leaf node (48 bytes of slots per entry, 11/8 of them).
fn map_bytes(map: &BTreeMap<String, String>) -> u64 {
    strings_bytes(map.keys()) + strings_bytes(map.values()) + map.len() as u64 * 66
}

#[derive(Default)]
struct Census<'a> {
    categories: BTreeMap<&'static str, Strings<'a>>,
    argument_lists: Wholes<'a, [crate::interner::IStr]>,
    environment_maps: Wholes<'a, BTreeMap<String, String>>,
    execution_info_maps: Wholes<'a, BTreeMap<String, String>>,
    execution_requirement_sets: Wholes<'a, BTreeSet<String>>,
    labels: u64,
    seen_artifacts: FxHashSet<usize>,
    seen_files: FxHashSet<usize>,
}

impl<'a> Census<'a> {
    fn category(&mut self, name: &'static str) -> &mut Strings<'a> {
        self.categories.entry(name).or_default()
    }

    fn label(&mut self, label: &'a Label) {
        self.labels += 1;
        self.category("label.repository")
            .interned(&label.repository);
        self.category("label.package").interned(&label.package);
        self.category("label.name").interned(&label.name);
    }

    fn artifact(&mut self, artifact: &'a Artifact) {
        let data: &'a super::ArtifactData = &artifact.0;
        if !self
            .seen_artifacts
            .insert(data as *const super::ArtifactData as usize)
        {
            return;
        }
        self.category("artifact.owner").string(&data.owner);
        if let Some((producer, _)) = &data.producer {
            self.category("artifact.producer_id").string(producer);
        }
        self.category("artifact.paths").shared_path(&data.real);
        self.category("artifact.paths").shared_path(&data.exec);
        let file: &'a crate::starlark_types::FileData = &data.file;
        if self
            .seen_files
            .insert(file as *const crate::starlark_types::FileData as usize)
        {
            self.category("file.path").string(&file.path);
            self.category("file.short_path").string(&file.short_path);
            self.category("file.owner").string(&file.owner);
        }
    }

    fn action(&mut self, planned: &'a crate::scheduler::PlannedAction) {
        let action = &planned.action;
        self.category("action.id").string(&action.id);
        self.category("action.command").string(&action.command);
        if let Some(program) = &action.program {
            self.category("action.program").string(program);
        }
        for argument in &action.arguments {
            self.category("action.arguments").interned(argument);
        }
        let list_bytes =
            size_class(action.arguments.capacity() * size_of::<crate::interner::IStr>());
        self.argument_lists.add(&action.arguments[..], list_bytes);
        for (name, value) in &action.environment {
            self.category("action.environment.name").string(name);
            self.category("action.environment.value").string(value);
        }
        self.environment_maps
            .add(&action.environment, map_bytes(&action.environment));
        for path in &action.outputs {
            self.category("action.outputs").path_buf(path);
        }
        for requirement in &action.execution_info {
            self.category("action.execution_requirements")
                .string(requirement);
        }
        self.execution_requirement_sets.add(
            &action.execution_info,
            strings_bytes(action.execution_info.iter()) + action.execution_info.len() as u64 * 33,
        );
        let extra = &action.extra;
        for (name, value) in &extra.execution_info {
            self.category("action.execution_info.name").string(name);
            self.category("action.execution_info.value").string(value);
        }
        self.execution_info_maps
            .add(&extra.execution_info, map_bytes(&extra.execution_info));
        self.category("action.mnemonic").string(&extra.mnemonic);
        for name in &extra.inherited_environment {
            self.category("action.inherited_environment").string(name);
        }
        for set in [
            &extra.tree_outputs,
            &extra.symlink_outputs,
            &extra.symlink_inputs,
            &extra.tree_inputs,
            &extra.source_directories,
        ] {
            for path in set {
                self.category("action.extra_paths").path_buf(path);
            }
        }
        for path in &extra.constant_metadata {
            self.category("action.extra_paths").path_buf(path);
        }
        for (path, content) in &extra.param_files {
            self.category("action.extra_paths").path_buf(path);
            self.category("action.param_file_contents")
                .owned(content, content.capacity());
        }
        if let Some(owner) = &extra.owner {
            self.category("action.owner").shared_path(owner);
        }
        if let Some(progress) = &extra.progress {
            self.category("action.progress").shared_path(progress);
        }
        if let Some(reason) = &action.remote_unsupported {
            self.category("action.remote_unsupported").string(reason);
        }
        let platform = &action.remote_platform;
        for (name, value) in platform
            .combined
            .iter()
            .chain(platform.exec_properties.iter())
        {
            self.category("action.platform").string(name);
            self.category("action.platform").string(value);
        }
        self.category("action.platform").string(&platform.label);
        self.category("action.platform")
            .string(&platform.remote_execution_properties);
        if let Some(write) = &action.write {
            self.category("action.write_contents")
                .owned(&write.content, write.content.capacity());
        }
        for (source, destination) in &action.inputs {
            self.category("action.inputs").shared_path(source);
            self.category("action.inputs").shared_path(destination);
        }
        for dependency in &planned.dependencies {
            self.category("dependency.action_id")
                .interned(&dependency.action_id);
            self.category("dependency.destination")
                .shared_path(&dependency.destination);
        }
    }

    fn print(&self) {
        let mut rows: Vec<(String, u64, u64, usize, u64, i64)> = self
            .categories
            .iter()
            .map(|(name, strings)| {
                (
                    (*name).to_owned(),
                    strings.count,
                    strings.bytes,
                    strings.unique.len(),
                    strings.unique_bytes,
                    strings.saving(),
                )
            })
            .collect();
        rows.push((
            "action.argument_lists".to_owned(),
            self.argument_lists.count,
            self.argument_lists.bytes,
            self.argument_lists.unique.len(),
            self.argument_lists.unique_bytes,
            self.argument_lists.saving(),
        ));
        for (name, wholes) in [
            ("action.environment_maps", &self.environment_maps),
            ("action.execution_info_maps", &self.execution_info_maps),
        ] {
            rows.push((
                name.to_owned(),
                wholes.count,
                wholes.bytes,
                wholes.unique.len(),
                wholes.unique_bytes,
                wholes.saving(),
            ));
        }
        rows.push((
            "action.execution_requirement_sets".to_owned(),
            self.execution_requirement_sets.count,
            self.execution_requirement_sets.bytes,
            self.execution_requirement_sets.unique.len(),
            self.execution_requirement_sets.unique_bytes,
            self.execution_requirement_sets.saving(),
        ));
        rows.sort_by(|a, b| b.5.cmp(&a.5).then_with(|| a.0.cmp(&b.0)));
        for (name, count, bytes, unique, unique_bytes, saving) in rows {
            eprintln!(
                "STRING-CENSUS {name} count={count} bytes={bytes} unique={unique} unique_bytes={unique_bytes} saving={saving}"
            );
        }
        eprintln!(
            "STRING-CENSUS-SUMMARY labels={} artifacts={} files={}",
            self.labels,
            self.seen_artifacts.len(),
            self.seen_files.len()
        );
    }
}

/// The engine hook (end of phase 2 in `plan_build`): a no-op unless
/// [`enabled`].
pub(super) fn report_if_enabled(
    engine: &Engine,
    state: &ActionPlanning,
    plan: &Plan,
    configured: &ConfiguredNodes,
) {
    if !enabled() {
        return;
    }
    let started = std::time::Instant::now();
    let mut census = Census::default();
    for planned in state.nodes.iter() {
        census.action(planned);
    }
    // Labels: the keys of the main planning maps and the rules' own.
    for (key, rule) in plan {
        census.label(&key.label);
        census.label(&rule.label);
    }
    for key in configured.keys() {
        census.label(&key.label);
    }
    for (key, rule) in engine.configured_rules.iter() {
        census.label(&key.label);
        census.label(&rule.label);
    }
    for package in engine.packages.values() {
        for (label, rule) in &package.rules {
            census.label(label);
            census.label(&rule.label);
        }
    }
    for key in state.dry.keys() {
        census.label(&key.label);
    }
    for key in state.custom_providers.keys() {
        census.label(&key.label);
    }
    for key in state.provider_keys.keys() {
        census.label(&key.label);
    }
    for key in state.runfiles_info.keys() {
        census.label(&key.label);
    }
    for key in state.repositories.keys() {
        census.label(&key.label);
    }
    for key in state.views.keys() {
        census.label(&key.label);
    }
    for key in state.runfiles_nodes.keys() {
        census.label(&key.label);
    }
    for key in state.native_output_groups.keys() {
        census.label(&key.label);
    }
    for key in state.validation.keys() {
        census.label(&key.label);
    }
    for (label, _) in state.target_identities.keys() {
        census.label(label);
    }
    // Artifacts and their Files (once per shared allocation).
    for artifacts in state.dry.values() {
        for artifact in artifacts.iter() {
            census.artifact(artifact);
        }
    }
    census.print();
    eprintln!(
        "STRING-CENSUS-ELAPSED {:.3}s",
        started.elapsed().as_secs_f64()
    );
}
