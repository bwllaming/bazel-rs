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

/// Names the file the planning process writes the target digests to.
pub const DIGESTS_VAR: &str = "BAZEL_RS_TARGET_DIGESTS";

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

/// `bytes` with every occurrence of the workspace root replaced by a fixed
/// token (the same commit planned in two directories digests alike) and
/// every repository store tree identity by another: a tree's identity
/// covers its specification and its extension group (docs/WORKTREES.md),
/// so an unrelated change of the group renames every tree of it while the
/// files — whose contents the digests hold — stay the same.
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
        } else if rest.starts_with(TREES)
            && rest.len() >= TREES.len() + TREE_ID_LEN
            && rest[TREES.len()..TREES.len() + TREE_ID_LEN]
                .iter()
                .all(u8::is_ascii_hexdigit)
        {
            out.extend_from_slice(b"/trees/<tree>");
            at = slash + TREES.len() + TREE_ID_LEN;
        } else {
            out.push(b'/');
            at = slash + 1;
        }
    }
    out.extend_from_slice(&bytes[at..]);
    out
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
        match postcard::to_stdvec(&own) {
            Ok(bytes) => frame(&mut hash, &normalized(&bytes, self.root)),
            Err(error) => frame(&mut hash, error.to_string().as_bytes()),
        }
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
            if let Ok(bytes) = postcard::to_stdvec(&run.settings) {
                frame(&mut hash, &normalized(&bytes, &digests.root));
            }
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
            "PERF-AFFECTED sources={} directory_entries={} local_ms={sources_ms} merkle_ms={}",
            digests.sources.len(),
            DIRECTORY_ENTRIES.load(std::sync::atomic::Ordering::Relaxed),
            started.elapsed().as_millis() - sources_ms
        );
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

/// Reads a digests file.
fn read_digests(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return Err(format!("{}: not a target digests file", path.display()));
    }
    Ok(lines
        .filter_map(|line| line.split_once('\t'))
        .map(|(label, digest)| (label.to_owned(), digest.to_owned()))
        .collect())
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
    forwarded: Vec<String>,
    patterns: Vec<String>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        command: "test".to_owned(),
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
            "--explain_affected" => options.explain = true,
            "--include_incompatible" => options.include_incompatible = true,
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

/// Plans `options.patterns` in `workspace` and returns its digests.
fn plan(
    binary: &Path,
    startup: &[String],
    options: &Options,
    workspace: &Path,
    out: &Path,
) -> Result<BTreeMap<String, String>, String> {
    let _ = std::fs::remove_file(out);
    let started = std::time::Instant::now();
    eprintln!(
        "bazel-rs affected: planning {} in {}",
        options.patterns.join(" "),
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
        // Our stdout is the affected labels: the planning's goes to stderr.
        .stdout(std::process::Stdio::from(
            std::io::stderr()
                .as_fd()
                .try_clone_to_owned()
                .map_err(|error| error.to_string())?,
        ));
    let status = crate::spawn_lock::spawn(&mut command)
        .and_then(|mut child| child.wait())
        .map_err(|error| format!("cannot run {}: {error}", binary.display()))?;
    eprintln!(
        "bazel-rs affected: planned in {:.1} s (exit {})",
        started.elapsed().as_secs_f64(),
        status.code().unwrap_or(-1)
    );
    // `test --nobuild` exits 1 after planning ("Couldn't start the build.
    // Unable to run tests", as Bazel does): the digests are written once
    // planning succeeded, whatever the exit code.
    if !status.success() && !out.exists() {
        return Err(format!(
            "planning in {} failed (exit {})",
            workspace.display(),
            status.code().unwrap_or(-1)
        ));
    }
    read_digests(out)
}

use std::os::fd::AsFd;

/// `bazel-rs [STARTUP...] affected --base=REV [--head=REV] [OPTIONS...] [PATTERNS...]`.
/// Returns the exit code.
pub fn run(binary: &Path, startup: &[String], args: &[String]) -> Result<i32, String> {
    let options = parse(args)?;
    let base = options
        .base
        .clone()
        .ok_or("bazel-rs affected: --base=REV is required")?;
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
        None => None,
    };
    // The worktree: given, else next to the repository's git directory.
    let worktree = match &options.worktree {
        Some(dir) if dir.is_absolute() => dir.clone(),
        Some(dir) => cwd.join(dir),
        None => {
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
    let result = (|| -> Result<i32, String> {
        checkout(&top, &worktree, &base_commit)?;
        let base_digests = match plan(
            binary,
            startup,
            &options,
            &worktree.join(&below),
            &keep.join("base.digests"),
        ) {
            Ok(digests) => Some(digests),
            Err(error) => {
                // target-determinator's --before-query-error-behavior
                // default: everything at the head revision is affected.
                eprintln!(
                    "bazel-rs affected: {error}: every target of the head revision is affected"
                );
                None
            }
        };
        // The head revision: in the same worktree (an incremental analysis
        // from the base's snapshot) when it is a commit; the working copy
        // itself when it is not given and the working copy is not clean.
        let head_workspace = match &head_commit {
            Some(commit) => {
                checkout(&top, &worktree, commit)?;
                worktree.join(&below)
            }
            None => {
                let dirty = git(&top, &["status", "--porcelain", "--untracked-files=no"])?;
                if dirty.is_empty() {
                    let commit = git(&top, &["rev-parse", "HEAD"])?;
                    checkout(&top, &worktree, &commit)?;
                    worktree.join(&below)
                } else {
                    eprintln!(
                        "bazel-rs affected: the working copy has changes: planning it in place"
                    );
                    root.clone()
                }
            }
        };
        let head_digests = plan(
            binary,
            startup,
            &options,
            &head_workspace,
            &keep.join("head.digests"),
        )?;
        let mut stdout = std::io::stdout().lock();
        let mut count = 0usize;
        for (label, digest) in &head_digests {
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
            "bazel-rs affected: {count} of {} target(s) affected",
            head_digests.len()
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
        assert_eq!(
            normalized(path.as_bytes(), b"/ws"),
            b"/c/repositories/direct/trees/<tree>/x.go".to_vec()
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
        assert_eq!(
            parse(&["--base=x".to_owned()]).unwrap().patterns,
            vec!["//...".to_owned()]
        );
    }
}
