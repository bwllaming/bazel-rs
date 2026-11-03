//! Affected targets between two revisions (`bazel-rs affected`,
//! docs/AFFECTED_TARGETS.md): a native replacement for target-determinator.
//!
//! Each revision is planned (`test --nobuild PATTERNS`, through the analysis
//! snapshot and incremental analysis like any request), and every requested
//! target gets a *target digest*: a Merkle digest over what building and
//! testing it would do — the actions it owns and the producers of its
//! reported outputs and (for a test) of its test's inputs, each action
//! digested from its planned identity (command, environment, mnemonic,
//! execution info, inputs and outputs by path; no owner) with its source
//! inputs' contents and its generated inputs' producers' digests, and the
//! test's properties and run settings. A target is affected when its digest
//! changed or it is new. The digests are written by the planning process
//! when [`DIGESTS_VAR`] names a file ([`write_if_requested`]); [`run`]
//! orchestrates the two revisions in one git worktree (so the second
//! revision's analysis is an incremental one from the first's snapshot) and
//! prints the affected labels, one per line.

use crate::label::Label;
use crate::request::{PlannedRequest, Produced, RequestCommand};
use crate::scheduler::PlannedAction;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};

mod value_hash;

/// Names the file the planning process writes the target digests to.
pub const DIGESTS_VAR: &str = "BAZEL_RS_TARGET_DIGESTS";

/// Set (by `bazel-rs affected --include_manual`) in a planning process:
/// the target pattern phase's wildcards keep `manual` rules (and `test`'s
/// FILTER_TESTS keeps `manual` tests), as `query` selects them. A snapshot
/// key input (crate::snapshot::key).
pub const INCLUDE_MANUAL_VAR: &str = "BAZEL_RS_AFFECTED_INCLUDE_MANUAL";

/// Set (by `bazel-rs affected`) in a planning process: its console drops
/// the DEBUG, WARNING and INFO notes (crate::ui::set_hidden_kinds) — the
/// `print()` output and warnings of the analysis and the build summary,
/// which are no output of `affected` (target-determinator's cquery shows
/// none of its events either). Errors and the `bazel-rs` progress notes
/// stay. Console-only: no snapshot key input.
pub const QUIET_EVENTS_VAR: &str = "BAZEL_RS_AFFECTED_QUIET_EVENTS";

/// The event kinds a planning process hides ([`QUIET_EVENTS_VAR`]).
pub const QUIET_EVENT_KINDS: &[&str] = &["DEBUG", "WARNING", "INFO"];

/// Whether this process's wildcards keep `manual` rules
/// ([`INCLUDE_MANUAL_VAR`]).
pub fn include_manual() -> bool {
    static INCLUDE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *INCLUDE.get_or_init(|| std::env::var_os(INCLUDE_MANUAL_VAR).is_some_and(|value| value == "1"))
}

/// The first line of a digests file.
const HEADER: &str = "bazel-rs-target-digests-v1";

/// The digest of a target whose analysis failed (`--keep_going`) or that is
/// incompatible: never equal to a computed one.
const FAILED: &str = "failed";
const INCOMPATIBLE: &str = "incompatible";

type Digest32 = [u8; 32];

fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn hex(digest: &Digest32) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The marker of a repository store tree in a path
/// (`<cache>/repositories/<kind>/trees/<64 hex digits>/...`).
const TREES: &[u8] = b"/trees/";
const TREE_ID_LEN: usize = 64;
/// The marker of the cache's external repository links.
const EXTERNAL: &[u8] = b"/repositories/external/";

/// `bytes` with every occurrence of the workspace root replaced by a fixed
/// token (the same commit planned in two directories digests alike) and
/// every repository store tree identity by another: a tree's identity
/// covers its specification and its extension group (docs/WORKTREES.md),
/// so an unrelated change of the group renames every tree of it while the
/// files — whose contents the digests hold — stay the same. The path before
/// a tree identity (the cache directory: `--cache-dir`, or the default
/// `<workspace>/bazel-rs-out`) is dropped with it, so digests written with
/// one cache location (`--write_digests`) compare with another's.
fn normalized(bytes: &[u8], root: &[u8]) -> Vec<u8> {
    // Both patterns begin with `/` (the root is absolute): copy the runs
    // between slashes whole.
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(offset) = bytes[at..].iter().position(|byte| *byte == b'/') {
        let slash = at + offset;
        out.extend_from_slice(&bytes[at..slash]);
        let rest = &bytes[slash..];
        if !root.is_empty() && rest.starts_with(root) {
            out.extend_from_slice(b"<workspace>");
            at = slash + root.len();
        } else if rest.starts_with(EXTERNAL) {
            // The cache's link tree of external repositories
            // (`<cache>/repositories/external/<name>/...`): from its start.
            let start = out
                .iter()
                .rposition(|byte| !is_path_byte(*byte))
                .map_or(0, |at| at + 1);
            out.truncate(start);
            out.extend_from_slice(b"<external>/");
            at = slash + EXTERNAL.len();
        } else if rest.starts_with(TREES)
            && rest.len() >= TREES.len() + TREE_ID_LEN
            && rest[TREES.len()..TREES.len() + TREE_ID_LEN]
                .iter()
                .all(u8::is_ascii_hexdigit)
        {
            // Back to where the path began (a delimiter, or the start).
            let start = out
                .iter()
                .rposition(|byte| !is_path_byte(*byte))
                .map_or(0, |at| at + 1);
            out.truncate(start);
            out.extend_from_slice(b"<tree>");
            at = slash + TREES.len() + TREE_ID_LEN;
        } else {
            out.push(b'/');
            at = slash + 1;
        }
    }
    out.extend_from_slice(&bytes[at..]);
    out
}

/// Hashes `value`'s serialized data with every string and byte string
/// normalized ([`normalized`], [`value_hash`]).
fn normalized_value<T: serde::Serialize>(hash: &mut Sha256, value: &T, root: &[u8]) {
    let mut scratch = Sha256::new();
    let hashed = crate::snapshot::codec::map_each::with_digest_identity(|| {
        value.serialize(&mut value_hash::Hasher {
            hash: &mut scratch,
            root,
        })
    });
    match hashed {
        Ok(()) => hash.update(scratch.finalize()),
        // A serializer error (none expected): the compact serialization,
        // normalized as bytes (exact, but root-length dependent).
        Err(_) => {
            VALUE_FALLBACKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match postcard::to_stdvec(value) {
                Ok(bytes) => frame(hash, &normalized(&bytes, root)),
                Err(error) => frame(hash, error.to_string().as_bytes()),
            }
        }
    }
}

/// [`normalized_value`]s serialized compactly instead (`BAZEL_RS_TIMING`).
static VALUE_FALLBACKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A byte of an absolute path as plans write it (the `<workspace>` token
/// included).
fn is_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"/._-+@~<>,%#".contains(&byte)
}

/// target-determinator's spelling of a label (gazelle's `Label.String`):
/// `//pkg` for `//pkg:pkg`.
fn display(label: &str) -> std::borrow::Cow<'_, str> {
    if let Some((package, name)) = label.rsplit_once(':')
        && package.rsplit('/').next() == Some(name)
        && !name.is_empty()
    {
        return std::borrow::Cow::Borrowed(package);
    }
    std::borrow::Cow::Borrowed(label)
}

/// The digest of a source file: its user-executable bit and contents
/// (through the digest memo); a directory digests its sorted listing
/// recursively, a missing file a marker.
static DIRECTORY_ENTRIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn source_digest(path: &Path) -> Digest32 {
    let mut hash = Sha256::new();
    match std::fs::metadata(path) {
        Err(_) => frame(&mut hash, b"missing"),
        Ok(metadata) if metadata.is_dir() => {
            frame(&mut hash, b"dir");
            let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
                .map(|entries| {
                    entries
                        .filter_map(|entry| entry.ok().map(|e| e.path()))
                        .collect()
                })
                .unwrap_or_default();
            entries.sort();
            DIRECTORY_ENTRIES.fetch_add(entries.len(), std::sync::atomic::Ordering::Relaxed);
            for entry in entries {
                frame(
                    &mut hash,
                    entry.file_name().unwrap_or_default().as_encoded_bytes(),
                );
                hash.update(source_digest(&entry));
            }
        }
        Ok(metadata) => {
            use std::os::unix::fs::PermissionsExt;
            frame(&mut hash, b"file");
            frame(
                &mut hash,
                &[u8::from(metadata.permissions().mode() & 0o100 != 0)],
            );
            match crate::action::source_file_digest(path, metadata) {
                Ok(digest) => frame(&mut hash, &digest),
                Err(error) => frame(&mut hash, error.as_bytes()),
            }
        }
    }
    hash.finalize().into()
}

/// The digests of `paths`, on scoped threads.
fn source_digests(paths: Vec<PathBuf>) -> HashMap<PathBuf, Digest32> {
    let threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, 16);
    let chunk = paths.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = paths
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    part.iter()
                        .map(|path| (path.clone(), source_digest(path)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    })
}

/// `f` over `items` on scoped threads, in order.
fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, 16);
    let chunk = items.len().div_ceil(threads).max(1);
    let f = &f;
    std::thread::scope(|scope| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|part| scope.spawn(move || part.iter().map(f).collect::<Vec<R>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    })
}

/// The per-action digests of a planned request: each action's *local*
/// digest (everything but its producers' digests) on threads, then the
/// Merkle digests over the producers in post-order.
struct ActionDigests<'a> {
    nodes: &'a [PlannedAction],
    by_id: HashMap<&'a str, usize>,
    sources: HashMap<PathBuf, Digest32>,
    local: Vec<Digest32>,
    memo: Vec<Option<Digest32>>,
    root: Vec<u8>,
}

/// What a node's or group's local digest needs.
struct Local<'b> {
    sources: &'b HashMap<PathBuf, Digest32>,
    root: &'b [u8],
}

impl Local<'_> {
    fn path(&self, hash: &mut Sha256, path: &Path) {
        frame(
            hash,
            &normalized(path.as_os_str().as_encoded_bytes(), self.root),
        );
    }

    fn source(&self, hash: &mut Sha256, path: &Path) {
        match self.sources.get(path) {
            Some(digest) => hash.update(digest),
            None => hash.update(source_digest(path)),
        }
    }

    /// A generated input without its producer (that enters the Merkle
    /// digest): which output, where.
    fn dependency(&self, hash: &mut Sha256, dependency: &crate::scheduler::Dependency) {
        frame(hash, &dependency.output_index.to_le_bytes());
        self.path(hash, &dependency.destination);
    }

    /// A shared input group (once per group: a tool's runfiles tree is
    /// held by thousands of actions).
    fn group(&self, group: &crate::scheduler::InputGroup) -> Digest32 {
        let mut hash = Sha256::new();
        frame(&mut hash, b"group-v1");
        self.path(&mut hash, &group.root);
        frame(&mut hash, &(group.inputs.len() as u64).to_le_bytes());
        for (source, destination) in &group.inputs {
            self.path(&mut hash, source);
            self.path(&mut hash, destination);
            self.source(&mut hash, source);
        }
        frame(&mut hash, &(group.dependencies.len() as u64).to_le_bytes());
        for dependency in &group.dependencies {
            self.dependency(&mut hash, dependency);
        }
        for list in [
            &group.tree_inputs,
            &group.symlink_inputs,
            &group.constant_metadata,
            &group.empty_files,
        ] {
            frame(&mut hash, &(list.len() as u64).to_le_bytes());
            for entry in list {
                self.path(&mut hash, entry);
            }
        }
        hash.finalize().into()
    }

    fn node(&self, node: &PlannedAction, groups: &HashMap<usize, Digest32>) -> Digest32 {
        let mut hash = Sha256::new();
        frame(&mut hash, b"action-v1");
        // The planned identity without the owner: the id and the producers'
        // ids are cleared (the producers enter the Merkle digest); the
        // shared input groups (not serialized with the node) by digest.
        let mut own = PlannedAction {
            action: node.action.clone(),
            dependencies: node.dependencies.clone(),
            deferred: node.deferred.clone(),
            groups: Vec::new(),
        };
        own.action.id.clear();
        for dependency in &mut own.dependencies {
            dependency.action_id.clear();
        }
        normalized_value(&mut hash, &own, self.root);
        for (source, _) in &node.action.inputs {
            self.source(&mut hash, source);
        }
        for dependency in &node.dependencies {
            self.dependency(&mut hash, dependency);
        }
        for group in &node.groups {
            match groups.get(&(std::sync::Arc::as_ptr(group) as usize)) {
                Some(digest) => hash.update(digest),
                None => hash.update(self.group(group)),
            }
        }
        hash.finalize().into()
    }
}

impl<'a> ActionDigests<'a> {
    fn new(nodes: &'a [PlannedAction], root: &Path, extra_sources: &[&Path]) -> Self {
        let by_id = nodes
            .iter()
            .enumerate()
            .map(|(at, node)| (node.action.id.as_str(), at))
            .collect();
        // Each shared input group once (by address), not once per holder.
        let mut seen_groups = std::collections::HashSet::new();
        let mut distinct = Vec::new();
        for node in nodes {
            for group in &node.groups {
                if seen_groups.insert(std::sync::Arc::as_ptr(group) as usize) {
                    distinct.push(group);
                }
            }
        }
        let mut paths: Vec<PathBuf> = nodes
            .iter()
            .flat_map(|node| node.action.inputs.iter())
            .chain(distinct.iter().flat_map(|group| group.inputs.iter()))
            .map(|(source, _)| source.to_path_buf())
            .chain(extra_sources.iter().map(|path| path.to_path_buf()))
            .collect();
        paths.sort_unstable();
        paths.dedup();
        let sources = source_digests(paths);
        let root = root.as_os_str().as_encoded_bytes().to_vec();
        let local = Local {
            sources: &sources,
            root: &root,
        };
        let groups: HashMap<usize, Digest32> = distinct
            .iter()
            .map(|group| std::sync::Arc::as_ptr(group) as usize)
            .zip(par_map(&distinct, |group| local.group(group)))
            .collect();
        let local = par_map(nodes, |node| local.node(node, &groups));
        ActionDigests {
            nodes,
            by_id,
            local,
            memo: vec![None; nodes.len()],
            sources,
            root,
        }
    }

    /// The digest of the action `id` (`None`: not planned): its local
    /// digest and its generated inputs' producers' digests.
    fn of(&mut self, id: &str) -> Option<Digest32> {
        let at = *self.by_id.get(id)?;
        // Iterative post-order over the generated inputs' producers.
        let mut stack = vec![(at, false)];
        while let Some((at, expanded)) = stack.pop() {
            if self.memo[at].is_some() {
                continue;
            }
            let node = &self.nodes[at];
            if !expanded {
                stack.push((at, true));
                for dependency in node.all_dependencies() {
                    if let Some(&producer) = self.by_id.get(dependency.action_id.as_str())
                        && self.memo[producer].is_none()
                    {
                        stack.push((producer, false));
                    }
                }
                continue;
            }
            let mut hash = Sha256::new();
            frame(&mut hash, b"merkle-v1");
            hash.update(self.local[at]);
            for dependency in node.all_dependencies() {
                match self
                    .by_id
                    .get(dependency.action_id.as_str())
                    .and_then(|&producer| self.memo[producer])
                {
                    Some(digest) => hash.update(digest),
                    // Not planned (a cycle, or not in a reachable plan): the id.
                    None => frame(&mut hash, dependency.action_id.as_bytes()),
                }
            }
            self.memo[at] = Some(hash.finalize().into());
        }
        self.memo[at]
    }

    fn produced(&mut self, hash: &mut Sha256, produced: &Produced) {
        frame(
            hash,
            &normalized(produced.real.as_os_str().as_encoded_bytes(), &self.root),
        );
        match &produced.producer {
            Some((id, index)) => {
                frame(hash, &index.to_le_bytes());
                match self.of(id) {
                    Some(digest) => hash.update(digest),
                    None => frame(hash, id.as_bytes()),
                }
            }
            None => match self.sources.get(&produced.real) {
                Some(digest) => hash.update(digest),
                None => hash.update(source_digest(&produced.real)),
            },
        }
    }
}

/// The configured target owning action `id` (crate::snapshot::incremental's
/// action owners; a target name holding `#` or `%` is not told apart):
/// `//pkg:name` for `//pkg:name#3`, `//pkg:name#runfiles_tree` and the native rule
/// action `//pkg:name`, `//pkg:name[cfg]` for `//pkg:name[cfg]#0`, and the
/// target an aspect applied to for `//pkg:name%//:a.bzl%asp#0`.
fn owner_key(id: &str) -> &str {
    // The label's name begins after its first `:`; the owner ends at the
    // first `#` (an action index: `#3`, `#runfiles_manifest`, ...) or `%`
    // (an aspect) after it.
    match id.find(':') {
        Some(colon) => match id[colon..].find(['#', '%']) {
            Some(end) => &id[..colon + end],
            None => id,
        },
        None => id,
    }
}

/// The target digests of `request` (planned in workspace `root`), by label.
pub fn target_digests(request: &PlannedRequest, root: &Path) -> BTreeMap<Label, String> {
    let mut labels: Vec<Label> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut add = |label: &Label, labels: &mut Vec<Label>| {
        seen.entry(label.to_string()).or_insert_with(|| {
            labels.push(label.clone());
            labels.len() - 1
        });
    };
    for (label, _) in &request.requested {
        add(label, &mut labels);
    }
    for (label, _) in &request.results {
        add(label, &mut labels);
    }
    for (label, _) in &request.run_plans {
        add(label, &mut labels);
    }
    let tests: HashMap<&Label, &crate::request::PlannedTest> = match &request.command {
        RequestCommand::Test { build, tests } => {
            for label in build {
                add(label, &mut labels);
            }
            tests.iter().map(|test| (&test.label, test)).collect()
        }
        _ => HashMap::new(),
    };
    drop(seen);
    // The actions of every configured target (with its aspects' actions).
    let mut by_owner: HashMap<&str, Vec<&str>> = HashMap::new();
    for node in &request.nodes {
        let id = node.action.id.as_str();
        by_owner.entry(owner_key(id)).or_default().push(id);
    }
    for ids in by_owner.values_mut() {
        ids.sort_unstable();
    }
    // Reported sources (a filegroup's files) and run plan sources, hashed
    // with the actions' inputs.
    let extra: Vec<&Path> = request
        .requested
        .iter()
        .map(|(_, produced)| produced)
        .chain(request.run_plans.iter().flat_map(|(_, run)| {
            std::iter::once(&run.executable).chain(
                run.entries
                    .iter()
                    .flatten()
                    .filter_map(|(_, entry)| entry.as_ref()),
            )
        }))
        .filter(|produced| produced.producer.is_none())
        .map(|produced| produced.real.as_path())
        .collect();
    let started = std::time::Instant::now();
    let mut digests = ActionDigests::new(&request.nodes, root, &extra);
    let sources_ms = started.elapsed().as_millis();
    let mut by_label: BTreeMap<Label, Vec<&Produced>> = BTreeMap::new();
    for (label, produced) in &request.requested {
        by_label.entry(label.clone()).or_default().push(produced);
    }
    let runs: HashMap<&Label, &crate::request::PlannedRun> = request
        .run_plans
        .iter()
        .map(|(label, run)| (label, run))
        .collect();
    let mut out = BTreeMap::new();
    for label in &labels {
        let mut hash = Sha256::new();
        frame(&mut hash, b"target-v1");
        if let Some(outputs) = by_label.get(label) {
            frame(&mut hash, b"outputs");
            for produced in outputs {
                digests.produced(&mut hash, produced);
            }
        }
        // The actions the target owns, and those of the targets producing
        // its reported outputs (an alias's actual, a filegroup's members):
        // their hidden top-level artifacts (runfiles trees, validations)
        // are built with it.
        let me = label.to_string();
        let mut owners: Vec<&str> = by_label
            .get(label)
            .into_iter()
            .flatten()
            .filter_map(|produced| produced.producer.as_ref().map(|(id, _)| owner_key(id)))
            .filter(|owner| *owner != me)
            .collect();
        owners.sort_unstable();
        owners.dedup();
        frame(&mut hash, b"owned");
        for owner in std::iter::once(me.as_str()).chain(owners) {
            frame(
                &mut hash,
                if owner == me {
                    b"self"
                } else {
                    owner.as_bytes()
                },
            );
            for id in by_owner.get(owner).into_iter().flatten() {
                frame(&mut hash, &id.as_bytes()[owner.len()..]);
                match digests.of(id) {
                    Some(digest) => hash.update(digest),
                    None => frame(&mut hash, id.as_bytes()),
                }
            }
        }
        if let Some(run) = runs.get(label) {
            frame(&mut hash, b"run");
            digests.produced(&mut hash, &run.executable);
            frame(&mut hash, run.executable_short_path.as_bytes());
            if let Some((tree, _)) = &run.tree {
                digests.produced(&mut hash, tree);
            }
            for (path, entry) in run.entries.iter().flatten() {
                frame(&mut hash, path.as_bytes());
                if let Some(entry) = entry {
                    digests.produced(&mut hash, entry);
                }
            }
            let mut completion: Vec<&String> = run.completion.iter().collect();
            completion.sort();
            for id in completion {
                match digests.of(id) {
                    Some(digest) => hash.update(digest),
                    None => frame(&mut hash, id.as_bytes()),
                }
            }
            normalized_value(&mut hash, &run.settings, &digests.root);
            if let Some(configuration) = request.configurations.get(run.configuration)
                && let Ok(bytes) = postcard::to_stdvec(configuration)
            {
                frame(&mut hash, &bytes);
            }
        }
        if let Some(test) = tests.get(label)
            && let Ok(bytes) = postcard::to_stdvec(&test.info)
        {
            // Not its location: a line moved is no change.
            frame(&mut hash, b"test");
            frame(&mut hash, &bytes);
        }
        let digest: Digest32 = hash.finalize().into();
        out.insert(label.clone(), hex(&digest));
    }
    if std::env::var_os("BAZEL_RS_TIMING").is_some() {
        eprintln!(
            "PERF-AFFECTED sources={} directory_entries={} value_fallbacks={} local_ms={sources_ms} merkle_ms={}",
            digests.sources.len(),
            DIRECTORY_ENTRIES.load(std::sync::atomic::Ordering::Relaxed),
            VALUE_FALLBACKS.load(std::sync::atomic::Ordering::Relaxed),
            started.elapsed().as_millis() - sources_ms
        );
    }
    // Diagnostics (hidden): every action's local and Merkle digest.
    if let Some(path) = std::env::var_os("BAZEL_RS_AFFECTED_ACTIONS") {
        let mut text = String::new();
        for (at, node) in request.nodes.iter().enumerate() {
            let merkle = digests.of(&node.action.id).map(|digest| hex(&digest));
            text.push_str(&format!(
                "{}\t{}\t{}\n",
                node.action.id,
                hex(&digests.local[at]),
                merkle.unwrap_or_default()
            ));
        }
        let _ = std::fs::write(&path, text);
        if let Some(filter) = std::env::var_os("BAZEL_RS_AFFECTED_ACTIONS_JSON") {
            let filter = filter.to_string_lossy().into_owned();
            let mut json = String::new();
            for node in &request.nodes {
                if node.action.id.contains(&filter) {
                    json.push_str(&format!(
                        "{}\n{}\n",
                        node.action.id,
                        serde_json::to_string(&node.deferred).unwrap_or_default()
                    ));
                }
            }
            let mut json_path = path.clone();
            json_path.push(".json");
            let _ = std::fs::write(json_path, json);
        }
    }
    for label in &request.skipped {
        out.insert(label.clone(), INCOMPATIBLE.to_owned());
    }
    if let Some(plan) = &request.keep_going {
        for (label, _) in &plan.failed {
            out.insert(label.clone(), FAILED.to_owned());
        }
    }
    out
}

/// Writes the target digests of `request` to the file [`DIGESTS_VAR`]
/// names, if it is set (before `finalize`: the workspace status nodes are
/// still placeholders, so stamping changes no digest).
pub fn write_if_requested(request: &PlannedRequest) -> Result<(), String> {
    let Some(path) = std::env::var_os(DIGESTS_VAR) else {
        return Ok(());
    };
    let started = std::time::Instant::now();
    let root = std::env::current_dir()
        .ok()
        .and_then(|cwd| crate::workspace::workspace_root(&cwd).ok())
        .unwrap_or_default();
    let digests = target_digests(request, &root);
    let mut text = String::new();
    text.push_str(HEADER);
    text.push('\n');
    for (label, digest) in &digests {
        text.push_str(&format!("{label}\t{digest}\n"));
    }
    std::fs::write(&path, text).map_err(|error| {
        format!(
            "{DIGESTS_VAR}: cannot write {}: {error}",
            Path::new(&path).display()
        )
    })?;
    if std::env::var_os("BAZEL_RS_TIMING").is_some() {
        eprintln!(
            "PERF-AFFECTED targets={} actions={} digest_ms={}",
            digests.len(),
            request.nodes.len(),
            started.elapsed().as_millis()
        );
    }
    Ok(())
}

/// Names the scope input of a planning process (crate::engine's
/// `affected_scope`, docs/AFFECTED_TARGETS.md §8); the process writes the
/// scope it computed to `<digests file>.scope`.
pub const SCOPE_VAR: &str = "BAZEL_RS_AFFECTED_SCOPE";

/// The first line of a scope input file.
pub const SCOPE_HEADER: &str = "bazel-rs-affected-scope-v1";

/// A digests file: the digests by label, and its `#name<TAB>value`
/// metadata lines (`--write_digests`).
struct DigestsFile {
    digests: BTreeMap<String, String>,
    meta: BTreeMap<String, String>,
}

/// Reads a digests file.
fn read_digests_file(path: &Path) -> Result<DigestsFile, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return Err(format!("{}: not a target digests file", path.display()));
    }
    let mut file = DigestsFile {
        digests: BTreeMap::new(),
        meta: BTreeMap::new(),
    };
    for (label, digest) in lines.filter_map(|line| line.split_once('\t')) {
        match label.strip_prefix('#') {
            Some(name) => file.meta.insert(name.to_owned(), digest.to_owned()),
            None => file.digests.insert(label.to_owned(), digest.to_owned()),
        };
    }
    Ok(file)
}

fn read_digests(path: &Path) -> Result<BTreeMap<String, String>, String> {
    read_digests_file(path).map(|file| file.digests)
}

/// Why `label` is affected (`None`: it is not).
fn reason(base: &BTreeMap<String, String>, label: &str, digest: &str) -> Option<&'static str> {
    if digest == INCOMPATIBLE {
        return None;
    }
    match base.get(label) {
        None => Some("new"),
        Some(_) if digest == FAILED => Some("analysis failed"),
        Some(before) if before != digest => Some("changed"),
        Some(_) => None,
    }
}

/// `bazel-rs affected` options.
#[derive(Debug, Default)]
struct Options {
    base: Option<String>,
    head: Option<String>,
    worktree: Option<PathBuf>,
    command: String,
    keep: Option<PathBuf>,
    explain: bool,
    include_incompatible: bool,
    /// `--include_manual`: wildcards select `manual` rules too
    /// ([`INCLUDE_MANUAL_VAR`]).
    include_manual: bool,
    /// `--base_failure=all`: a base that fails to plan makes every head
    /// target affected (target-determinator's default) instead of failing.
    base_failure_all: bool,
    /// `--scope=auto` (the default): analyse only what a local change can
    /// reach; `--scope=full`: both revisions entirely.
    scope: bool,
    base_digests: Option<PathBuf>,
    write_digests: Option<PathBuf>,
    /// `--[no]concurrent_revisions`: plan both revisions at once (about
    /// twice the peak memory). Unset: [`auto_concurrent`] decides.
    concurrent: Option<bool>,
    forwarded: Vec<String>,
    patterns: Vec<String>,
    /// The working copy a planning in place runs in: its analysis snapshot
    /// is saved for the commands after `affected` ([`SAVE_VAR`]).
    in_place: Option<PathBuf>,
}

/// Set in a planning process that runs in the working copy (the head
/// revision planned in place): its analysis snapshot is saved, under a key
/// of its own, for the `build` and `test` of its targets that derive their
/// plan from it (crate::snapshot::derive). Plannings in a worktree save
/// none (nothing after `affected` runs there).
pub const SAVE_VAR: &str = "BAZEL_RS_AFFECTED_SAVE";

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        command: "test".to_owned(),
        scope: true,
        ..Options::default()
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| -> Result<String, String> {
            match arg.strip_prefix(&format!("--{name}=")) {
                Some(value) => Ok(value.to_owned()),
                None => args
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("--{name} requires a value")),
            }
        };
        let name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
        match name {
            "--base" => options.base = Some(value("base")?),
            "--head" => options.head = Some(value("head")?),
            "--worktree" => options.worktree = Some(PathBuf::from(value("worktree")?)),
            "--affected_command" => options.command = value("affected_command")?,
            "--keep_digests" => options.keep = Some(PathBuf::from(value("keep_digests")?)),
            "--base_digests" => {
                options.base_digests = Some(PathBuf::from(value("base_digests")?));
            }
            "--write_digests" => {
                options.write_digests = Some(PathBuf::from(value("write_digests")?));
            }
            "--scope" => {
                options.scope = match value("scope")?.as_str() {
                    "auto" => true,
                    "full" => false,
                    other => return Err(format!("--scope must be auto or full, not {other}")),
                }
            }
            "--base_failure" => {
                options.base_failure_all = match value("base_failure")?.as_str() {
                    "error" => false,
                    "all" => true,
                    other => {
                        return Err(format!("--base_failure must be error or all, not {other}"));
                    }
                }
            }
            "--explain_affected" => options.explain = true,
            "--concurrent_revisions" => {
                options.concurrent = match arg.split_once('=').map(|(_, value)| value) {
                    None | Some("true" | "yes" | "1") => Some(true),
                    Some("false" | "no" | "0") => Some(false),
                    Some("auto") => None,
                    Some(other) => {
                        return Err(format!(
                            "--concurrent_revisions must be true, false or auto, not {other}"
                        ));
                    }
                }
            }
            "--noconcurrent_revisions" => options.concurrent = Some(false),
            "--include_incompatible" => options.include_incompatible = true,
            "--include_manual" => options.include_manual = true,
            "--noinclude_manual" => options.include_manual = false,
            "--" => {
                options.patterns.extend(args.by_ref().cloned());
            }
            _ if arg.starts_with('-') => options.forwarded.push(arg.clone()),
            _ => options.patterns.push(arg.clone()),
        }
    }
    if options.command != "test" && options.command != "build" {
        return Err("--affected_command must be test or build".to_owned());
    }
    if options.patterns.is_empty() {
        options.patterns.push("//...".to_owned());
    }
    Ok(options)
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = crate::spawn_lock::output(
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args),
    )
    .map_err(|error| format!("cannot run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Before the head is planned in place: the working copy's hidden lockfile
/// (module extension results, crate::registry_session), when it has none,
/// starts as the base planning's in the worktree, without the results that
/// name the worktree. A planning that writes its hidden lockfile is not
/// saved as an analysis snapshot (the write changes what it read), so a
/// cold cache's head planning would otherwise never be the snapshot the
/// `build` and `test` after `affected` derive their plan from. Its
/// extensions are validated against their recorded inputs as always: a
/// result that does not hold is evaluated again (and the head not saved).
fn seed_hidden_lockfile(cache: &Path, base: &Path, workspace: &Path) {
    let target = crate::registry_session::hidden_lockfile_path(cache, workspace);
    if target.exists() {
        return;
    }
    let source = crate::registry_session::hidden_lockfile_path(cache, base);
    let Ok(lockfile) = crate::lockfile::read(&source, crate::lockfile::LockfileMode::Update) else {
        return;
    };
    if !source.exists() {
        return;
    }
    let lockfile = crate::registry_session::without_mentions(
        lockfile,
        &crate::worktree::WorkspaceMentions::except_cache(base, cache),
    );
    if let Some(parent) = target.parent()
        && std::fs::create_dir_all(parent).is_ok()
    {
        let temporary = target.with_extension(format!("seed-{}", std::process::id()));
        if std::fs::write(&temporary, lockfile.serialize()).is_ok() {
            let _ = std::fs::rename(&temporary, &target);
        }
    }
}

/// Checks out `commit` in the worktree `dir` of the repository at `top`
/// (created or repaired as needed), clean.
fn checkout(top: &Path, dir: &Path, commit: &str) -> Result<(), String> {
    let usable = dir.join(".git").exists()
        && git(dir, &["rev-parse", "--git-common-dir"]).is_ok_and(|common| {
            let common = dir.join(common);
            let ours =
                PathBuf::from(git(top, &["rev-parse", "--git-common-dir"]).unwrap_or_default());
            let ours = top.join(ours);
            common.canonicalize().ok() == ours.canonicalize().ok()
        });
    if usable {
        git(dir, &["checkout", "--quiet", "--force", "--detach", commit])?;
        git(dir, &["clean", "--quiet", "-ffdx"])?;
    } else {
        if dir.exists() {
            std::fs::remove_dir_all(dir)
                .map_err(|error| format!("cannot remove {}: {error}", dir.display()))?;
        }
        let _ = git(top, &["worktree", "prune"]);
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        let dir_text = dir.to_string_lossy().into_owned();
        git(
            top,
            &[
                "worktree", "add", "--quiet", "--force", "--detach", &dir_text, commit,
            ],
        )?;
    }
    let _ = git(
        dir,
        &["submodule", "--quiet", "update", "--init", "--recursive"],
    );
    Ok(())
}

/// The scope a planning process used (`<digests>.scope`).
#[derive(Debug)]
enum Scope {
    /// No scope: everything planned.
    None,
    /// The in-scope selected targets (planned alone when restricted).
    Scoped(std::collections::BTreeSet<String>),
    /// A root reached a change: everything planned.
    Full(String),
}

/// One revision's planning result.
struct Planned {
    digests: BTreeMap<String, String>,
    scope: Scope,
}

/// The scope request of a planning process: the input file and whether
/// the selected targets are restricted to the scope (else only reported).
struct ScopeRequest<'a> {
    spec: &'a Path,
}

/// A planning process started by [`start_plan`].
struct Running {
    child: std::process::Child,
    started: std::time::Instant,
    workspace: PathBuf,
    out: PathBuf,
    scoped: bool,
}

/// Starts planning `options.patterns` in `workspace`.
fn start_plan(
    binary: &Path,
    startup: &[String],
    options: &Options,
    workspace: &Path,
    out: &Path,
    scope: Option<&ScopeRequest<'_>>,
) -> Result<Running, String> {
    let _ = std::fs::remove_file(out);
    let _ = std::fs::remove_file(format!("{}.scope", out.display()));
    eprintln!(
        "bazel-rs affected: planning {}{} in {}",
        options.patterns.join(" "),
        if scope.is_some() { " (scoped)" } else { "" },
        workspace.display()
    );
    let mut command = std::process::Command::new(binary);
    command
        .args(startup)
        .arg(&options.command)
        .arg("--nobuild")
        .args(&options.forwarded)
        .arg("--")
        .args(&options.patterns)
        .current_dir(workspace)
        .env(DIGESTS_VAR, out)
        .env(QUIET_EVENTS_VAR, "1")
        .env_remove(SCOPE_VAR)
        .env_remove(SAVE_VAR)
        .env_remove(INCLUDE_MANUAL_VAR)
        // Our stdout is the affected labels: the planning's goes to stderr.
        .stdout(std::process::Stdio::from(
            std::io::stderr()
                .as_fd()
                .try_clone_to_owned()
                .map_err(|error| error.to_string())?,
        ));
    if let Some(scope) = scope {
        command.env(SCOPE_VAR, scope.spec);
    }
    if options.in_place.as_deref() == Some(workspace) {
        command.env(SAVE_VAR, "1");
    }
    if options.include_manual {
        command.env(INCLUDE_MANUAL_VAR, "1");
    }
    let child = crate::spawn_lock::spawn(&mut command)
        .map_err(|error| format!("cannot run {}: {error}", binary.display()))?;
    Ok(Running {
        child,
        started: std::time::Instant::now(),
        workspace: workspace.to_path_buf(),
        out: out.to_path_buf(),
        scoped: scope.is_some(),
    })
}

impl Running {
    /// Waits for the planning and returns its digests and scope.
    fn finish(mut self) -> Result<Planned, String> {
        let status = self
            .child
            .wait()
            .map_err(|error| format!("cannot wait for the planning: {error}"))?;
        eprintln!(
            "bazel-rs affected: planned {} in {:.1} s (exit {})",
            self.workspace.display(),
            self.started.elapsed().as_secs_f64(),
            status.code().unwrap_or(-1)
        );
        // A planning process exits 0 once its analysis succeeded and its
        // digests are written (`test --nobuild` too: crate's main, under
        // DIGESTS_VAR, instead of Bazel's `Unable to run tests` exit 1).
        // Any other exit is a failure, whatever it wrote: a partial
        // analysis (`--keep_going`, a skipped pattern) must never decide
        // the answer.
        if !status.success() {
            return Err(format!(
                "planning in {} failed (exit {}; its output is above)",
                self.workspace.display(),
                status
                    .code()
                    .map_or_else(|| status.to_string(), |code| code.to_string())
            ));
        }
        if !self.out.exists() {
            return Err(format!(
                "planning in {} wrote no digests (exit 0)",
                self.workspace.display()
            ));
        }
        let scope = if self.scoped {
            let scope_out = PathBuf::from(format!("{}.scope", self.out.display()));
            let text = std::fs::read_to_string(&scope_out).map_err(|error| {
                format!(
                    "planning in {} wrote no scope: {error}",
                    self.workspace.display()
                )
            })?;
            let mut lines = text.lines();
            match lines.next() {
                Some("scoped") => Scope::Scoped(lines.map(str::to_owned).collect()),
                Some(line) => Scope::Full(line.strip_prefix("full\t").unwrap_or(line).to_owned()),
                None => Scope::Full("empty scope file".to_owned()),
            }
        } else {
            Scope::None
        };
        Ok(Planned {
            digests: read_digests(&self.out)?,
            scope,
        })
    }
}

/// Plans `options.patterns` in `workspace` and returns its digests.
fn plan(
    binary: &Path,
    startup: &[String],
    options: &Options,
    workspace: &Path,
    out: &Path,
    scope: Option<&ScopeRequest<'_>>,
) -> Result<Planned, String> {
    start_plan(binary, startup, options, workspace, out, scope)?.finish()
}

/// The identity of what a digests file holds besides its commit: this
/// binary, the command, its options and patterns, and whether wildcards
/// selected `manual` rules (`--base_digests` must match it).
fn digests_key(binary: &Path, startup: &[String], options: &Options) -> Result<String, String> {
    let bytes = std::fs::read(binary)
        .map_err(|error| format!("cannot read {}: {error}", binary.display()))?;
    Ok(digests_key_of(
        &Sha256::digest(&bytes).into(),
        startup,
        options,
    ))
}

fn digests_key_of(binary: &Digest32, startup: &[String], options: &Options) -> String {
    let mut hash = Sha256::new();
    frame(&mut hash, HEADER.as_bytes());
    frame(&mut hash, binary);
    if options.include_manual {
        // Before the arguments (none of them is a bare marker like this).
        frame(&mut hash, b"#include_manual");
    }
    for arg in startup
        .iter()
        // The cache location changes no digest.
        .filter(|arg| !arg.starts_with("--cache-dir="))
        .chain([&options.command])
        .chain(&options.forwarded)
        .chain([&"--".to_owned()])
        .chain(&options.patterns)
    {
        frame(&mut hash, arg.as_bytes());
    }
    hex(&hash.finalize().into())
}

/// Writes the head's digests for a later `--base_digests`.
fn write_digests_file(
    path: &Path,
    commit: &str,
    key: &str,
    digests: &BTreeMap<String, String>,
) -> Result<(), String> {
    let mut text = format!("{HEADER}\n#commit\t{commit}\n#key\t{key}\n");
    for (label, digest) in digests {
        text.push_str(&format!("{label}\t{digest}\n"));
    }
    std::fs::write(path, text).map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// The memory a host must have available (MemAvailable) for `affected` to
/// plan both revisions of a local change at once by default: two scoped
/// plannings of ~/code peak at about 4.5 GB each, a full one at about
/// 9.2 GB (docs/AFFECTED_TARGETS.md §8.5). `BAZEL_RS_AFFECTED_CONCURRENT_MB`
/// overrides it (0: always, when not asked otherwise).
const AUTO_CONCURRENT_MB: u64 = 20 * 1024;

/// MemAvailable of `/proc/meminfo` text, in MB.
fn available_mb(meminfo: &str) -> Option<u64> {
    meminfo.lines().find_map(|line| {
        let rest = line.strip_prefix("MemAvailable:")?;
        let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
        Some(kb / 1024)
    })
}

/// Whether to plan both revisions at once when neither
/// `--concurrent_revisions` nor `--noconcurrent_revisions` was given: when
/// the host has [`AUTO_CONCURRENT_MB`] available.
fn auto_concurrent() -> bool {
    let needed = std::env::var("BAZEL_RS_AFFECTED_CONCURRENT_MB")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(AUTO_CONCURRENT_MB);
    let available = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .as_deref()
        .and_then(available_mb);
    match available {
        Some(available) if available >= needed => true,
        Some(available) => {
            eprintln!(
                "bazel-rs affected: planning the revisions one after the other ({available} MB available, {needed} MB for both at once)"
            );
            false
        }
        None => needed == 0,
    }
}

use std::os::fd::AsFd;

/// `bazel-rs [STARTUP...] affected --base=REV [--head=REV] [OPTIONS...] [PATTERNS...]`.
/// Returns the exit code.
pub fn run(binary: &Path, startup: &[String], args: &[String]) -> Result<i32, String> {
    let mut options = parse(args)?;
    let base = options
        .base
        .clone()
        .ok_or("bazel-rs affected: --base=REV is required")?;
    let started = std::time::Instant::now();
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let root = crate::workspace::workspace_root(&cwd)?;
    let top = PathBuf::from(git(&root, &["rev-parse", "--show-toplevel"])?);
    let below = root
        .canonicalize()
        .ok()
        .and_then(|root| {
            top.canonicalize()
                .ok()
                .and_then(|top| root.strip_prefix(top).ok().map(Path::to_path_buf))
        })
        .unwrap_or_default();
    let base_commit = git(
        &root,
        &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )?;
    let head_commit = match &options.head {
        Some(head) => Some(git(
            &root,
            &["rev-parse", "--verify", &format!("{head}^{{commit}}")],
        )?),
        None => {
            // Without --head: HEAD when the working copy is clean; the
            // working copy itself (planned in place, never scoped) otherwise.
            let dirty = git(&top, &["status", "--porcelain", "--untracked-files=no"])?;
            if dirty.is_empty() {
                Some(git(&top, &["rev-parse", "HEAD"])?)
            } else {
                None
            }
        }
    };
    // The head is planned in place when the working copy is HEAD exactly:
    // no change to a tracked file and no untracked file (ignored files are
    // the working copy's, which the commands after `affected` see too).
    // `BAZEL_RS_AFFECTED_HEAD_IN_PLACE=0` checks HEAD out in the worktree.
    let head_in_place = options.head.is_none()
        && head_commit.is_some()
        && !matches!(
            std::env::var_os("BAZEL_RS_AFFECTED_HEAD_IN_PLACE").as_deref(),
            Some(value) if value == "0"
        )
        && git(&top, &["status", "--porcelain", "--untracked-files=normal"])?.is_empty();
    options.in_place = Some(root.clone());
    // The worktree: given; else in an explicit cache directory (outside the
    // checkout: genuine Bazel's `//...` does not skip `.git`, and would load
    // a worktree's packages there); else next to the repository's git
    // directory.
    let explicit_cache = startup
        .iter()
        .find_map(|arg| arg.strip_prefix("--cache-dir="))
        .map(PathBuf::from);
    let shared_cache = explicit_cache.clone();
    let worktree = match (&options.worktree, explicit_cache) {
        (Some(dir), _) if dir.is_absolute() => dir.clone(),
        (Some(dir), _) => cwd.join(dir),
        (None, Some(cache)) => {
            let common = PathBuf::from(git(&top, &["rev-parse", "--git-common-dir"])?);
            let common = if common.is_absolute() {
                common
            } else {
                top.join(common)
            };
            let common = common.canonicalize().unwrap_or(common);
            let id = hex(&Sha256::digest(common.as_os_str().as_encoded_bytes()).into());
            cache
                .join("affected-worktrees")
                .join(&id[..16])
                .join("checkout")
        }
        (None, None) => {
            let common = PathBuf::from(git(&top, &["rev-parse", "--git-common-dir"])?);
            let common = if common.is_absolute() {
                common
            } else {
                top.join(common)
            };
            common.join("bazel-rs-affected")
        }
    };
    let scratch = std::env::temp_dir().join(format!("bazel-rs-affected-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).map_err(|error| error.to_string())?;
    let keep = options.keep.clone().unwrap_or_else(|| scratch.clone());
    std::fs::create_dir_all(&keep).map_err(|error| error.to_string())?;
    let key = if options.base_digests.is_some() || options.write_digests.is_some() {
        Some(digests_key(binary, startup, &options)?)
    } else {
        None
    };
    let result = (|| -> Result<i32, String> {
        // The changes: a local change is analysed in its scope only.
        let spec_path = keep.join("scope.spec");
        let scoped = match &head_commit {
            Some(head) if options.scope => {
                let classified = crate::affected_changes::classify(
                    &top,
                    &below,
                    &base_commit,
                    head,
                    &options.command,
                    &options.forwarded,
                )?;
                if classified.global.is_empty() {
                    let (files, packages, bzl, roots) = classified.counts;
                    eprintln!(
                        "bazel-rs affected: local change ({files} file(s), {packages} package(s) with rule changes, {bzl} .bzl file(s); {roots} configuration root(s)): scoped analysis"
                    );
                    std::fs::write(
                        &spec_path,
                        format!("{SCOPE_HEADER}\nmode\trestrict\n{}", classified.spec),
                    )
                    .map_err(|error| format!("cannot write {}: {error}", spec_path.display()))?;
                    true
                } else {
                    for reason in classified.global.iter().take(10) {
                        eprintln!("bazel-rs affected: global change: {reason}");
                    }
                    if classified.global.len() > 10 {
                        eprintln!(
                            "bazel-rs affected: ... and {} more global change(s)",
                            classified.global.len() - 10
                        );
                    }
                    eprintln!("bazel-rs affected: full analysis of both revisions");
                    false
                }
            }
            Some(_) => false,
            None => {
                eprintln!(
                    "bazel-rs affected: the working copy has changes: full analysis, planned in place"
                );
                false
            }
        };
        // The same scope, reported but not restricted: for the head of
        // `--write_digests` (its every digest is kept) and for a revision
        // whose scoped planning failed (planned entirely instead).
        let report_path = keep.join("scope-report.spec");
        if scoped {
            let spec = std::fs::read_to_string(&spec_path).map_err(|error| error.to_string())?;
            std::fs::write(
                &report_path,
                spec.replace("mode\trestrict\n", "mode\treport\n"),
            )
            .map_err(|error| error.to_string())?;
        }
        let restrict = ScopeRequest { spec: &spec_path };
        let report = ScopeRequest { spec: &report_path };
        // A scoped (restricted) planning that failed: once more (a transient
        // failure, e.g. a registry's 502), then entirely, its scope reported
        // (the analysis of a subset must never decide the answer by failing).
        let entire_after_retry = |workspace: &Path,
                                  out: &Path,
                                  name: &str,
                                  error: String|
         -> Result<(Planned, bool), String> {
            eprintln!("bazel-rs affected: {name}: scoped planning failed ({error}): once more");
            if let Ok(planned) = plan(binary, startup, &options, workspace, out, Some(&restrict)) {
                return Ok((planned, false));
            }
            eprintln!(
                "bazel-rs affected: {name}: scoped planning failed again: planning it entirely"
            );
            plan(binary, startup, &options, workspace, out, Some(&report))
                .map(|planned| (planned, true))
        };
        // Plans a revision; a scoped (restricted) planning that fails is
        // repeated entirely (its scope reported): the analysis of a subset
        // must never decide the answer by failing.
        let plan_revision = |tree: &Path,
                             commit: Option<&str>,
                             name: &str,
                             scope: Option<&ScopeRequest<'_>>|
         -> Result<(Planned, bool), String> {
            let workspace = match commit {
                Some(commit) => {
                    checkout(&top, tree, commit)?;
                    tree.join(&below)
                }
                None => root.clone(),
            };
            let out = keep.join(format!("{name}.digests"));
            match plan(binary, startup, &options, &workspace, &out, scope) {
                Ok(planned) => Ok((planned, false)),
                Err(error) if scope.is_some_and(|scope| scope.spec == spec_path.as_path()) => {
                    entire_after_retry(&workspace, &out, name, error)
                }
                Err(error) => Err(error),
            }
        };
        // The base: a digests file of the base commit, else planned.
        let stored = match &options.base_digests {
            Some(path) => match read_digests_file(path) {
                Ok(file)
                    if file.meta.get("commit") == Some(&base_commit)
                        && file.meta.get("key") == key.as_ref() =>
                {
                    eprintln!(
                        "bazel-rs affected: base digests from {} ({} targets)",
                        path.display(),
                        file.digests.len()
                    );
                    Some(Planned {
                        digests: file.digests,
                        scope: Scope::None,
                    })
                }
                Ok(file) => {
                    eprintln!(
                        "bazel-rs affected: {} does not hold this base (commit {}, key {}): planning the base",
                        path.display(),
                        file.meta.get("commit").map_or("none", String::as_str),
                        file.meta
                            .get("key")
                            .map_or("none", |key| &key[..key.len().min(12)])
                    );
                    None
                }
                Err(error) => {
                    eprintln!("bazel-rs affected: {error}: planning the base");
                    None
                }
            },
            None => None,
        };
        let from_file = stored.is_some();
        let every = |error: String| -> Result<Option<Planned>, String> {
            // A base that fails to plan: an error, or with
            // `--base_failure=all` target-determinator's
            // --before-query-error-behavior default: everything at the head
            // revision is affected.
            if !options.base_failure_all {
                return Err(format!(
                    "base {base_commit}: {error} (--base_failure=all: report every target of the head instead)"
                ));
            }
            eprintln!("bazel-rs affected: {error}: every target of the head revision is affected");
            Ok(None)
        };
        // Both revisions at once: as asked, else (auto) for a local change
        // when the host has the memory for two plannings. A global change
        // stays sequential unless asked (its revisions' repository
        // specifications differ: docs/WORKTREES.md, hazard 3).
        let concurrent = !from_file
            && head_commit.is_some()
            && match options.concurrent {
                Some(asked) => asked,
                None => scoped && auto_concurrent(),
            };
        // The head's worktree: the base's (the second planning starts from
        // the first's state), or its own when both are planned at once
        // (unless the head is planned in place).
        let head_tree = if concurrent && !head_in_place {
            let mut name = worktree.file_name().unwrap_or_default().to_os_string();
            name.push("-head");
            worktree.with_file_name(name)
        } else {
            worktree.clone()
        };
        let restricted = |scope: Option<&ScopeRequest<'_>>| {
            scope.is_some_and(|scope| scope.spec == spec_path.as_path())
        };
        let mut base_entire = false;
        let (mut base_planned, head) = if concurrent {
            // Both at once: the base and the head restricted to the scope;
            // the corrections below run sequentially (rare).
            let head_scope = if !scoped {
                None
            } else if options.write_digests.is_some() {
                Some(&report)
            } else {
                Some(&restrict)
            };
            let base_scope = scoped.then_some(&restrict);
            let head_commit = head_commit.as_deref().unwrap_or_default();
            // A clean working copy at HEAD is planned in place (its
            // snapshot saved for the commands after `affected`).
            let head_at = (!head_in_place).then_some(head_commit);
            checkout(&top, &worktree, &base_commit)?;
            let head_workspace = match head_at {
                Some(commit) => {
                    checkout(&top, &head_tree, commit)?;
                    head_tree.join(&below)
                }
                None => root.clone(),
            };
            eprintln!("bazel-rs affected: planning both revisions concurrently");
            let base_run = start_plan(
                binary,
                startup,
                &options,
                &worktree.join(&below),
                &keep.join("base.digests"),
                base_scope,
            );
            let head_run = start_plan(
                binary,
                startup,
                &options,
                &head_workspace,
                &keep.join("head.digests"),
                head_scope,
            );
            let base_result = base_run.and_then(Running::finish);
            let head_result = head_run.and_then(Running::finish);
            let base_planned = match base_result {
                Ok(planned) => Some(planned),
                Err(error) if restricted(base_scope) => {
                    match entire_after_retry(
                        &worktree.join(&below),
                        &keep.join("base.digests"),
                        "base",
                        error,
                    ) {
                        Ok((planned, entire)) => {
                            base_entire = entire;
                            Some(planned)
                        }
                        Err(error) => every(error)?,
                    }
                }
                Err(error) => every(error)?,
            };
            let head = match head_result {
                Ok(planned) => planned,
                Err(error) if restricted(head_scope) => {
                    entire_after_retry(&head_workspace, &keep.join("head.digests"), "head", error)?
                        .0
                }
                Err(error) => return Err(error),
            };
            // A root reaching the change at the base: the head entirely too.
            let head = match (&base_planned, &head.scope) {
                (
                    Some(Planned {
                        scope: Scope::Full(reason),
                        ..
                    }),
                    Scope::Scoped(_),
                ) if restricted(head_scope) => {
                    eprintln!("bazel-rs affected: base: full analysis: {reason}");
                    plan_revision(&head_tree, head_at, "head", None)?.0
                }
                _ => head,
            };
            (base_planned, head)
        } else {
            let base_planned = match stored {
                Some(planned) => Some(planned),
                None => match plan_revision(
                    &worktree,
                    Some(&base_commit),
                    "base",
                    scoped.then_some(&restrict),
                ) {
                    Ok((planned, entire)) => {
                        base_entire = entire;
                        Some(planned)
                    }
                    Err(error) => every(error)?,
                },
            };
            // A root reaching the change at the base: the head entirely too.
            let base_full = matches!(
                base_planned.as_ref().map(|planned| &planned.scope),
                Some(Scope::Full(_))
            );
            if let Some(Planned {
                scope: Scope::Full(reason),
                ..
            }) = &base_planned
            {
                eprintln!("bazel-rs affected: base: full analysis: {reason}");
            }
            let head_scope = if !scoped || base_full {
                None
            } else if options.write_digests.is_some() || base_entire {
                Some(&report)
            } else {
                Some(&restrict)
            };
            // A clean working copy at HEAD is the head revision itself:
            // planned in place, its analysis snapshot is the one a later
            // `build`/`test` of its targets derives its plan from
            // (crate::snapshot::derive) instead of analyzing them again.
            let head_at = if head_in_place {
                if let Some(cache) = &shared_cache {
                    seed_hidden_lockfile(cache, &worktree.join(&below), &root);
                }
                None
            } else {
                head_commit.as_deref()
            };
            let (head, _) = plan_revision(&head_tree, head_at, "head", head_scope)?;
            (base_planned, head)
        };
        if let Scope::Full(reason) = &head.scope {
            eprintln!("bazel-rs affected: head: full analysis: {reason}");
            // The base was restricted to a scope the head does not confirm:
            // plan it entirely.
            if !from_file
                && !base_entire
                && matches!(
                    base_planned.as_ref().map(|planned| &planned.scope),
                    Some(Scope::Scoped(_))
                )
            {
                base_planned = match plan_revision(&worktree, Some(&base_commit), "base", None) {
                    Ok((planned, _)) => Some(planned),
                    Err(error) => every(error)?,
                };
            }
        }
        if let (Some(path), Some(commit), Some(key)) = (&options.write_digests, &head_commit, &key)
        {
            write_digests_file(path, commit, key, &head.digests)?;
            eprintln!(
                "bazel-rs affected: wrote {} digests to {}",
                head.digests.len(),
                path.display()
            );
        }
        // The labels compared: the head's scope (a target outside it has
        // the same digest at both revisions), else every head label.
        let in_scope: Option<&std::collections::BTreeSet<String>> = match &head.scope {
            Scope::Scoped(scope) => Some(scope),
            Scope::None | Scope::Full(_) => None,
        };
        let base_digests = base_planned.map(|planned| planned.digests);
        let mut stdout = std::io::stdout().lock();
        let mut count = 0usize;
        let mut compared = 0usize;
        for (label, digest) in &head.digests {
            if in_scope.is_some_and(|scope| !scope.contains(label)) {
                continue;
            }
            compared += 1;
            let why = match &base_digests {
                Some(base) => reason(base, label, digest),
                None => Some("base failed"),
            };
            let why = match why {
                None if options.include_incompatible && digest == INCOMPATIBLE => {
                    match base_digests.as_ref().and_then(|base| base.get(label)) {
                        Some(before) if before == INCOMPATIBLE => None,
                        _ => Some("incompatible"),
                    }
                }
                why => why,
            };
            if let Some(why) = why {
                count += 1;
                if options.explain {
                    eprintln!("bazel-rs affected: {label}: {why}");
                }
                writeln!(stdout, "{}", display(label)).map_err(|error| error.to_string())?;
            }
        }
        eprintln!(
            "bazel-rs affected: {count} of {compared} compared target(s) affected in {:.1} s",
            started.elapsed().as_secs_f64()
        );
        Ok(0)
    })();
    if options.keep.is_none() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_store_tree_identities() {
        let id = "a".repeat(64);
        let path = format!("/c/repositories/direct/trees/{id}/x.go");
        assert_eq!(normalized(path.as_bytes(), b"/ws"), b"<tree>/x.go".to_vec());
        // The cache location is dropped with the identity.
        let other = format!("--x=/other/cache/repositories/trees/{id}/x.go");
        assert_eq!(
            normalized(other.as_bytes(), b"/ws"),
            b"--x=<tree>/x.go".to_vec()
        );
        let inside = format!("/ws/bazel-rs-out/repositories/trees/{id}/y");
        assert_eq!(normalized(inside.as_bytes(), b"/ws"), b"<tree>/y".to_vec());
        assert_eq!(
            normalized(b"x=/c1/repositories/external/r+/bin/aws", b"/ws"),
            b"x=<external>/r+/bin/aws".to_vec()
        );
        // Not 64 hex digits: kept.
        assert_eq!(
            normalized(b"/a/trees/abc/x", b"/ws"),
            b"/a/trees/abc/x".to_vec()
        );
    }

    #[test]
    fn displays_labels_like_target_determinator() {
        assert_eq!(display("//a/b:b"), "//a/b");
        assert_eq!(display("//a/b:c"), "//a/b:c");
        assert_eq!(display("//:gazelle"), "//:gazelle");
        assert_eq!(display("@@r//p:p"), "@@r//p");
    }

    #[test]
    fn normalizes_the_workspace_root() {
        assert_eq!(
            normalized(b"/a/ws/src/x:/a/ws", b"/a/ws"),
            b"<workspace>/src/x:<workspace>".to_vec()
        );
        assert_eq!(normalized(b"abc", b""), b"abc".to_vec());
    }

    #[test]
    fn action_owners() {
        assert_eq!(owner_key("//a:b"), "//a:b");
        assert_eq!(owner_key("//a:b#0"), "//a:b");
        assert_eq!(owner_key("//a:b#runfiles_manifest"), "//a:b");
        assert_eq!(owner_key("//a:b[0011aabb]#repo_mapping"), "//a:b[0011aabb]");
        assert_eq!(owner_key("//a:b[0011aabb]#0"), "//a:b[0011aabb]");
        assert_eq!(owner_key("//a:b%//:x.bzl%asp#0"), "//a:b");
        assert_eq!(owner_key("@@r+//a:b#1"), "@@r+//a:b");
    }

    #[test]
    fn reasons() {
        let base: BTreeMap<String, String> = [
            ("//a:x".to_owned(), "1".to_owned()),
            ("//a:y".to_owned(), "2".to_owned()),
        ]
        .into_iter()
        .collect();
        assert_eq!(reason(&base, "//a:x", "1"), None);
        assert_eq!(reason(&base, "//a:y", "3"), Some("changed"));
        assert_eq!(reason(&base, "//a:z", "1"), Some("new"));
        assert_eq!(reason(&base, "//a:x", INCOMPATIBLE), None);
        assert_eq!(reason(&base, "//a:x", FAILED), Some("analysis failed"));
    }

    #[test]
    fn parses_options_and_patterns() {
        let args: Vec<String> = [
            "--base=main",
            "--remote_cache=",
            "//src/...",
            "--",
            "-//src/x/...",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let options = parse(&args).unwrap();
        assert_eq!(options.base.as_deref(), Some("main"));
        assert_eq!(options.forwarded, vec!["--remote_cache=".to_owned()]);
        assert_eq!(
            options.patterns,
            vec!["//src/...".to_owned(), "-//src/x/...".to_owned()]
        );
        assert_eq!(options.command, "test");
        assert!(!options.include_manual);
        assert_eq!(
            parse(&["--base=x".to_owned()]).unwrap().patterns,
            vec!["//...".to_owned()]
        );
        let manual = parse(&["--base=x".to_owned(), "--include_manual".to_owned()]).unwrap();
        assert!(manual.include_manual);
        assert!(manual.forwarded.is_empty());
        let again = parse(&[
            "--base=x".to_owned(),
            "--include_manual".to_owned(),
            "--noinclude_manual".to_owned(),
        ])
        .unwrap();
        assert!(!again.include_manual);
    }

    #[test]
    fn concurrent_revisions_options() {
        let parsed = |args: &[&str]| {
            let mut all = vec!["--base=x".to_owned()];
            all.extend(args.iter().map(|arg| (*arg).to_owned()));
            parse(&all).map(|options| options.concurrent)
        };
        assert_eq!(parsed(&[]), Ok(None));
        assert_eq!(parsed(&["--concurrent_revisions"]), Ok(Some(true)));
        assert_eq!(parsed(&["--concurrent_revisions=true"]), Ok(Some(true)));
        assert_eq!(parsed(&["--concurrent_revisions=auto"]), Ok(None));
        assert_eq!(parsed(&["--noconcurrent_revisions"]), Ok(Some(false)));
        assert_eq!(parsed(&["--concurrent_revisions=false"]), Ok(Some(false)));
        assert!(parsed(&["--concurrent_revisions=maybe"]).is_err());
        // Not forwarded to the plannings (nor in the digests key).
        let options = parse(&["--base=x".to_owned(), "--concurrent_revisions".to_owned()]).unwrap();
        assert!(options.forwarded.is_empty());
    }

    #[test]
    fn available_memory() {
        let meminfo = "MemTotal:       263751720 kB\nMemFree:        11531464 kB\nMemAvailable:   143691468 kB\n";
        assert_eq!(available_mb(meminfo), Some(140323));
        assert_eq!(available_mb("MemTotal: 1 kB\n"), None);
    }

    #[test]
    fn the_digests_key_covers_include_manual() {
        let binary = [7u8; 32];
        let plain = parse(&["--base=x".to_owned()]).unwrap();
        let manual = parse(&["--base=x".to_owned(), "--include_manual".to_owned()]).unwrap();
        assert_eq!(
            digests_key_of(&binary, &[], &plain),
            digests_key_of(&binary, &[], &plain)
        );
        assert_ne!(
            digests_key_of(&binary, &[], &plain),
            digests_key_of(&binary, &[], &manual)
        );
    }
}
