//! Repository specification identity and warm-publication revalidation.
//!
//! A direct or generated repository's specification is a pure function of its
//! definition, bound attributes, mappings, host markers and declared
//! environment values, serialized BEFORE any execution
//! (RepositoryFunction's key: rule class, attributes, environment and
//! host factors). The store keeps one publication pointer per specification;
//! a warm invocation revalidates the recorded watchable inputs — templates,
//! watched files/directories, observed environment — against the current
//! world and reuses the publication without running the rule, exactly like
//! Bazel reusing `external/<name>` until a dependency changes
//! (RepositoryFunction.java dirty checking; Skyframe FileValue and
//! RepositoryEnvironmentValue dependencies).

use crate::label::Label;
use crate::repository::RepositorySet;
use crate::repository_environment::DeclaredEnvironment;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One template read's revalidation probe: the recorded source spelling and
/// the digest of the bytes the rule consumed.
#[derive(Serialize, Deserialize)]
struct TemplateProbe {
    source: String,
    sha256: String,
}

/// One watched file/directory observation (RepoRecordedInput).
#[derive(Serialize, Deserialize)]
struct FileProbe {
    path: String,
    kind: String,
    state: String,
}

/// The observed-environment probe: names only, never values — a native
/// policy (Bazel's .marker stores `ENV:` values in plain text; the native
/// manifest never persists environment values).
#[derive(Serialize, Deserialize)]
struct WatchedProbe {
    sha256: String,
    names: Vec<String>,
}

/// Recorded watchable inputs persisted in a publication manifest. These bytes
/// never enter the content identity; they exist so a later invocation can
/// reobserve the same inputs and decide reuse (Bazel's dependency dirty
/// checking) without executing the rule.
///
/// Version 2 (review i20): every probe path is a RepoCacheFriendlyPath
/// (RepoRecordedInput.java:325-405) — `@@//rel`, `@@NAME//rel` or an
/// absolute path outside every repository, label reads included, exactly
/// the key a Bazel marker file records — or `stage:rel` inside the
/// publication itself. Version 1 recorded label reads as canonical labels
/// and paths into other repositories as host paths ([`resolve_v1`]); it is
/// still revalidated as written, except that a probe into the repository
/// store is never trusted ([`unchanged`]).
///
/// Version 3 (docs/WORKTREES.md) is version 2 plus `workspace`: the
/// canonical workspace root of an execution whose result depends on where
/// the workspace is (crate::repository_ctx::ExecutionInputs::workspace); it
/// validates only in that workspace, so the worktrees sharing a cache never
/// reuse each other's publication. Only such probes are written as version
/// 3; every other probe keeps its version-2 bytes.
///
/// Version 4 (affected-reuse-cold) is version 3 (or 2, without a
/// workspace) plus `trees`: the other repositories a label of the execution
/// resolved into whose canonical store tree the published bytes name (an
/// absolute link into it, its path written into a file), each with that
/// tree (`direct/trees/<identity>` below the store). A generated
/// repository's specification covers only its own (crate::repository's
/// ORDINARY_PROFILE), so a sibling fetched anew changes nothing it records
/// otherwise; the reuse requires each to resolve to the same tree still.
/// Only such probes are written as version 4.
///
/// Version 5 (repo-mapping-key) adds `mappings`: the repository mapping
/// lookups of `Label()` calls while the rule ran (from, apparent name,
/// canonical result; Bazel's REPO_MAPPING recorded inputs), which the
/// specification no longer covers (it frames only the mapping entries the
/// definition's loads used, crate::repository). The other fields keep their
/// meaning (a `workspace` and `trees` may be present too). Only probes with
/// lookups are written as version 5.
///
/// Any change to the probe bytes — a field, a path spelling, a state
/// encoding, what a probe covers — must bump `version`. Binaries sharing a
/// cache rely on it in both directions: an older binary never trusts a
/// version it does not know ([`unchanged`] answers `Ok(false)`, so it
/// executes the rule) and never republishes over a newer version
/// (`repository_store::upgrades_recorded`), while this binary upgrades the
/// probes of an older version once. Changed bytes under an unchanged
/// version would be validated by the wrong rules, and each binary would
/// replace the other's probes on every switch.
#[derive(Serialize, Deserialize)]
struct RecordedInputs {
    version: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    templates: Vec<TemplateProbe>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    files: Vec<FileProbe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watched_env: Option<WatchedProbe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    /// Version 4: (canonical repository, its tree below the store).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    trees: Vec<(String, String)>,
    /// Version 5: (from, apparent, canonical or null) mapping lookups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    mappings: Vec<(String, String, Option<String>)>,
}

/// `recorded` (this binary's probes, [`record`]) with the trees of `trees`
/// ([`RecordedInputs`] version 4).
pub(crate) fn with_trees(recorded: &[u8], trees: Vec<(String, String)>) -> Result<Vec<u8>, String> {
    let mut probes: RecordedInputs = serde_json::from_slice(recorded)
        .map_err(|error| format!("cannot parse recorded repository inputs: {error}"))?;
    probes.version = probes.version.max(4);
    probes.trees = trees;
    serde_json::to_vec(&probes).map_err(|error| format!("cannot record repository inputs: {error}"))
}

/// `recorded` without the probes of files inside the publication itself
/// (`stage:` spellings): what can be checked before a tree of the remote
/// repository contents cache is downloaded (crate::remote_repo_cache; the
/// tree's verified contents decide the others, and an upload requires all
/// probes to hold for the published tree). `None` when the bytes are no
/// probes this binary reads.
pub(crate) fn without_stage_probes(recorded: &[u8]) -> Option<Vec<u8>> {
    let mut probes: RecordedInputs = serde_json::from_slice(recorded).ok()?;
    if !(1..=5).contains(&probes.version) {
        return None;
    }
    probes
        .templates
        .retain(|probe| !probe.source.starts_with("stage:"));
    probes
        .files
        .retain(|probe| !probe.path.starts_with("stage:"));
    serde_json::to_vec(&probes).ok()
}

/// Whether recorded probes of a remote repository contents cache entry
/// (crate::remote_repo_cache; untrusted bytes) have a form an upload can
/// carry: version 2 or 4, no `trees` (store paths), no `workspace`, and no
/// probe through `canonical`'s own label (`@@<canonical>//…`: its own tree,
/// which only `stage:` names).
pub(crate) fn remote_probes_acceptable(recorded: &[u8], canonical: &str) -> bool {
    let Ok(probes) = serde_json::from_slice::<RecordedInputs>(recorded) else {
        return false;
    };
    let own = format!("@@{canonical}//");
    matches!(probes.version, 2 | 4 | 5)
        && probes.trees.is_empty()
        && probes.workspace.is_none()
        && !probes
            .templates
            .iter()
            .map(|probe| probe.source.as_str())
            .chain(probes.files.iter().map(|probe| probe.path.as_str()))
            .any(|path| path.starts_with(&own))
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// The canonical `workspace` root as recorded probes spell it (Latin-1
/// text, crate::repository_ctx::ExecutionInputs::workspace), and as the
/// repository store names a workspace's own specification pointer
/// (crate::repository_store::workspace_pointer).
pub(crate) fn workspace_text(workspace: &Path) -> String {
    crate::host_os::latin1_text(workspace.as_os_str().as_encoded_bytes())
}

/// Serialize the repository specification: length-framed provenance, the host
/// markers and the declared environment's captured values. Pure in its inputs;
/// equal bytes address the same publication pointer. Retained as the
/// byte-exact reference the digest form mirrors; production callers use the
/// digest forms below.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn specification(
    repositories: &RepositorySet,
    provenance: &[u8],
    declared: &DeclaredEnvironment,
) -> Result<Vec<u8>, String> {
    let host = crate::host_os::host_spec(repositories.host()?.as_ref())?;
    // RepositoryEnvironmentFunction values for the declared names, captured
    // before execution. A read error serializes a fixed marker: it differs
    // from every captured value, and the execution that follows fails with
    // the real error anyway.
    let mut environment = BTreeMap::new();
    for name in declared.names() {
        let value = match repositories.environment().lookup(name) {
            Ok(Some(value)) => serde_json::Value::String(value),
            Ok(None) => serde_json::Value::Null,
            Err(_) => serde_json::Value::Bool(false),
        };
        environment.insert(name.to_owned(), value);
    }
    let environment = serde_json::to_vec(&environment)
        .map_err(|error| format!("cannot serialize declared repository environment: {error}"))?;
    let mut spec = Vec::with_capacity(24 + provenance.len() + host.len() + environment.len());
    for bytes in [provenance, host.as_slice(), environment.as_slice()] {
        spec.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        spec.extend_from_slice(bytes);
    }
    Ok(spec)
}

/// Digest-form specification: the same pure inputs as [`specification`]
/// with the provenance's embedded `.bzl` sources replaced by their content
/// digests (see `repository_store::spec_digest_identity`). Addresses the
/// same pointer when the underlying source bytes are unchanged, without
/// serializing them. `provenance_fields` is the provenance JSON object
/// (direct) or the framed ordinary provenance pre-digested by the caller.
/// Reference form whose inner framing production mirrors in
/// `specification_digest_streaming` (identity equivalence is pinned by
/// the repository_store tests; kept cfg(test) so no closure-bearing
/// `Value` is ever built in production).
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn specification_digest(
    repositories: &RepositorySet,
    provenance_fields: &serde_json::Value,
    source_entries: &serde_json::Map<String, serde_json::Value>,
    declared: &DeclaredEnvironment,
) -> Result<String, String> {
    let host = crate::host_os::host_spec(repositories.host()?.as_ref())?;
    let mut environment = BTreeMap::new();
    for name in declared.names() {
        let value = match repositories.environment().lookup(name) {
            Ok(Some(value)) => serde_json::Value::String(value),
            Ok(None) => serde_json::Value::Null,
            Err(_) => serde_json::Value::Bool(false),
        };
        environment.insert(name.to_owned(), value);
    }
    let environment = serde_json::to_vec(&environment)
        .map_err(|error| format!("cannot serialize declared repository environment: {error}"))?;
    let mut digest = sha2::Sha256::new();
    use sha2::Digest as _;
    digest.update(crate::repository_store::SPEC_DIGEST_PROFILE);
    digest.update([0]);
    digest.update(
        crate::repository_store::spec_digest_identity(provenance_fields, source_entries)?
            .as_bytes(),
    );
    digest.update((host.len() as u64).to_le_bytes());
    digest.update(&host);
    digest.update((environment.len() as u64).to_le_bytes());
    digest.update(&environment);
    Ok(format!("{:x}", digest.finalize()))
}

/// Streaming direct spec key (review R1): `frame` writes the provenance
/// object with digest-form sources, exactly the bytes
/// [`specification_digest`] frames through `spec_digest_identity`, but
/// without materializing the closure-bearing `Value`. The inner
/// identity is SHA-256 over `SPEC_DIGEST_PROFILE \0 <be-len> <framed>`;
/// the outer key frames that hex string + host + environment as before.
/// Serialization errors propagate (item 4).
pub(crate) fn specification_digest_streaming(
    repositories: &RepositorySet,
    frame_count: impl Fn(&mut crate::repository_store::CountWriter) -> Result<(), String>,
    frame_hash: impl Fn(&mut crate::repository_store::SeededDigestWriter) -> Result<(), String>,
    declared: &DeclaredEnvironment,
) -> Result<String, String> {
    let host = crate::host_os::host_spec(repositories.host()?.as_ref())?;
    let mut environment = BTreeMap::new();
    for name in declared.names() {
        let value = match repositories.environment().lookup(name) {
            Ok(Some(value)) => serde_json::Value::String(value),
            Ok(None) => serde_json::Value::Null,
            Err(_) => serde_json::Value::Bool(false),
        };
        environment.insert(name.to_owned(), value);
    }
    let environment = serde_json::to_vec(&environment)
        .map_err(|error| format!("cannot serialize declared repository environment: {error}"))?;
    // Inner identity = sha256(profile || 0 || be(len) || framed bytes).
    // Pass 1 counts, pass 2 hashes into a pre-seeded digest. Both passes
    // stream only per-repo fields + the group's cached digest-entry
    // bytes — never the closure.
    let mut counter = crate::repository_store::CountWriter(0);
    frame_count(&mut counter)?;
    let mut seeded = crate::repository_store::SeededDigestWriter::new(
        crate::repository_store::SPEC_DIGEST_PROFILE,
        counter.0,
    );
    frame_hash(&mut seeded)?;
    let identity = seeded.finish_hex();
    use sha2::Digest as _;
    let mut digest = sha2::Sha256::new();
    digest.update(crate::repository_store::SPEC_DIGEST_PROFILE);
    digest.update([0]);
    digest.update(identity.as_bytes());
    digest.update((host.len() as u64).to_le_bytes());
    digest.update(&host);
    digest.update((environment.len() as u64).to_le_bytes());
    digest.update(&environment);
    Ok(format!("{:x}", digest.finalize()))
}

/// Ordinary spec key from the group's ONCE-computed provenance digest
/// (review R1, ordinary twin): the per-repository work is framing the
/// canonical name + host + environment around 32 digest bytes — never
/// re-hashing the shared closure-bearing provenance per sibling.
/// Distinct profile byte-string from the byte form it replaces, so old
/// pointers simply miss once and republish (content identity, which
/// still frames the full provenance bytes, is unchanged).
pub(crate) fn specification_digest_mixed(
    repositories: &RepositorySet,
    canonical_name: &str,
    provenance_digest: &[u8; 32],
    specification_digest: &[u8; 32],
    declared: &DeclaredEnvironment,
) -> Result<String, String> {
    let host = crate::host_os::host_spec(repositories.host()?.as_ref())?;
    let mut environment = BTreeMap::new();
    for name in declared.names() {
        let value = match repositories.environment().lookup(name) {
            Ok(Some(value)) => serde_json::Value::String(value),
            Ok(None) => serde_json::Value::Null,
            Err(_) => serde_json::Value::Bool(false),
        };
        environment.insert(name.to_owned(), value);
    }
    let environment = serde_json::to_vec(&environment)
        .map_err(|error| format!("cannot serialize declared repository environment: {error}"))?;
    let mut digest = sha2::Sha256::new();
    use sha2::Digest as _;
    // v3: the slots' profile bump (crate::repository_store::SPEC_PROFILE).
    // v4: the repository's own specification beside its group's
    // provenance (crate::repository's ORDINARY_PROFILE).
    digest.update(b"ordinary-repository-spec-digest-v4");
    digest.update([0]);
    digest.update((canonical_name.len() as u64).to_le_bytes());
    digest.update(canonical_name.as_bytes());
    digest.update(provenance_digest);
    digest.update(specification_digest);
    digest.update((host.len() as u64).to_le_bytes());
    digest.update(&host);
    digest.update((environment.len() as u64).to_le_bytes());
    digest.update(&environment);
    Ok(format!("{:x}", digest.finalize()))
}

/// Reference byte form the mixed form replaces (Step A framing).
/// `specification_digest_mixed` deliberately addresses a new pointer:
/// old ordinary pointers simply miss once and republish. Kept cfg(test)
/// so production never re-hashes provenance bytes per repository.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn specification_digest_bytes(
    repositories: &RepositorySet,
    provenance: &[u8],
    declared: &DeclaredEnvironment,
) -> Result<String, String> {
    let host = crate::host_os::host_spec(repositories.host()?.as_ref())?;
    let mut environment = BTreeMap::new();
    for name in declared.names() {
        let value = match repositories.environment().lookup(name) {
            Ok(Some(value)) => serde_json::Value::String(value),
            Ok(None) => serde_json::Value::Null,
            Err(_) => serde_json::Value::Bool(false),
        };
        environment.insert(name.to_owned(), value);
    }
    let environment = serde_json::to_vec(&environment)
        .map_err(|error| format!("cannot serialize declared repository environment: {error}"))?;
    let mut digest = sha2::Sha256::new();
    use sha2::Digest as _;
    digest.update(crate::repository_store::SPEC_DIGEST_PROFILE);
    digest.update([0]);
    digest.update((provenance.len() as u64).to_le_bytes());
    digest.update(provenance);
    digest.update((host.len() as u64).to_le_bytes());
    digest.update(&host);
    digest.update((environment.len() as u64).to_le_bytes());
    digest.update(&environment);
    Ok(format!("{:x}", digest.finalize()))
}

/// Record the watchable inputs of one execution for later revalidation.
/// Repositories depend only on their declared `environ` values (framed into
/// the specification itself) and these recorded inputs — `os.environ` and
/// unrequested environment changes record nothing, like Bazel's
/// RepoRecordedInput set (ENV: only through declared names or getenv).
///
/// A `stage:` template probe (a template read from inside the repository
/// directory) whose file the execution itself rewrote or removed afterwards
/// — `ctx.template("src/m4.h", "src/m4.h", …)` applied twice, as rules_m4
/// and rules_flex do — is left out: it names a file of the publication,
/// whose final bytes the manifest pins, and could never hold again, so the
/// repository re-executed on every command. Genuine Bazel watches no path
/// under the repository's working directory at all
/// (StarlarkBaseExternalContext.toRepoCacheFriendlyPath, AUTO: null). A
/// probe that still holds, or whose path in `stage` crosses a symlink (the
/// read may have followed it out of the stage), is kept.
pub(crate) fn record(
    inputs: &crate::repository_ctx::ExecutionInputs,
    watch_names: &[String],
    stage: &Path,
) -> Result<Vec<u8>, String> {
    // A label read's probe is its file's RepoCacheFriendlyPath (Bazel
    // watches the label's path, `@@repo//pkg/name`); the identity keeps the
    // canonical label (ExecutionInputs), so no publication is rekeyed.
    let probe = |text: &str, label: bool| -> Result<String, String> {
        if !label {
            return Ok(text.to_owned());
        }
        let label = Label::parse_canonical(text).map_err(|error| error.to_string())?;
        Ok(crate::lockfile_extensions::FriendlyPath::of_label(&label).to_string())
    };
    let recorded = RecordedInputs {
        version: if !inputs.repo_mappings.is_empty() {
            5
        } else if inputs.workspace.is_some() {
            3
        } else {
            2
        },
        templates: inputs
            .templates
            .iter()
            .filter(|template| template.label || !rewritten_in_stage(stage, template))
            .map(|template| {
                Ok(TemplateProbe {
                    source: probe(&template.source, template.label)?,
                    sha256: sha256(&template.bytes),
                })
            })
            .collect::<Result<_, String>>()?,
        files: inputs
            .files
            .iter()
            .map(|file| {
                Ok(FileProbe {
                    path: probe(&file.path, file.label)?,
                    kind: file.kind.to_owned(),
                    state: file.state.clone(),
                })
            })
            .collect::<Result<_, String>>()?,
        watched_env: inputs.watched_env.as_ref().map(|watched| WatchedProbe {
            sha256: watched.sha256.clone(),
            names: watch_names.to_vec(),
        }),
        workspace: inputs.workspace.clone(),
        trees: Vec::new(),
        mappings: inputs.repo_mappings.iter().cloned().collect(),
    };
    serde_json::to_vec(&recorded)
        .map_err(|error| format!("cannot record repository inputs: {error}"))
}

/// Whether the `stage:` template `template` read bytes that its file in
/// `stage` (the finished execution's repository directory) no longer holds,
/// the path crossing no symlink: the execution rewrote or removed it after
/// the read ([`record`]).
fn rewritten_in_stage(stage: &Path, template: &crate::repository_ctx::TemplateInput) -> bool {
    let Some(relative) = template.source.strip_prefix("stage:") else {
        return false;
    };
    let Some(path) = safe_child(stage, relative) else {
        return false;
    };
    let mut prefix = stage.to_path_buf();
    for component in Path::new(relative).components() {
        prefix.push(component);
        // snapshot-unrecorded: the execution's own stage, published whole
        // (its manifest pins every file); a symlink keeps the probe.
        match std::fs::symlink_metadata(&prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => return false,
            Ok(_) => {}
            // Removed after the read (or a parent replaced): the probe can
            // never hold.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) => return false,
        }
    }
    // snapshot-unrecorded: as above.
    match std::fs::read(&path) {
        Ok(bytes) => bytes != template.bytes,
        Err(_) => false,
    }
}

/// Reobserve the recorded inputs against the current world. `Ok(false)` means
/// the world changed (or cannot be confirmed unchanged): the store re-executes
/// the repository rule, like Bazel refetching after a dependency change.
/// Historical proof paths: friendly workspace, stage, and named repositories
/// resolved by the proof-only RepositorySet (current producer proof before
/// loading). Symlink hops and arbitrary absolute derived paths remain excluded.
pub(crate) fn unchanged_for_proof(
    repositories: &RepositorySet,
    workspace: &Path,
    tree: &Path,
    recorded: &[u8],
) -> Result<bool, String> {
    let probes: RecordedInputs = serde_json::from_slice(recorded).map_err(|e| e.to_string())?;
    if !matches!(probes.version, 2 | 3)
        || probes
            .files
            .iter()
            .any(|file| !matches!(file.kind.as_str(), "file" | "dirents" | "dirtree"))
    {
        return Ok(false);
    }
    let cache = repositories.source_cache_path()?;
    for text in probes
        .templates
        .iter()
        .map(|probe| probe.source.as_str())
        .chain(probes.files.iter().map(|probe| probe.path.as_str()))
    {
        let (root, relative, stage) = if let Some(relative) = text.strip_prefix("stage:") {
            (tree, relative, true)
        } else if let Some(relative) = text.strip_prefix("@@//") {
            (workspace, relative, false)
        } else if let Some((repository, relative)) = text
            .strip_prefix("@@")
            .and_then(|text| text.split_once("//"))
        {
            // root() is proof-only on this RepositorySet: generated roots
            // require their own current spec/runtime receipt before loading.
            (repositories.root(repository)?, relative, true)
        } else {
            return Ok(false);
        };
        if !proof_path(root, relative, (!stage).then_some(cache)) {
            return Ok(false);
        }
    }
    unchanged(repositories, workspace, tree, recorded)
}

/// A path usable without another producer's proof. Missing paths are valid
/// inputs too, but no ancestor can escape the root or enter the store.
pub(crate) fn proof_path(root: &Path, relative: &str, blocked: Option<&Path>) -> bool {
    let mut path = root.to_path_buf();
    for component in Path::new(relative).components() {
        if !matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        ) {
            return false;
        }
        path.push(component);
        if blocked.is_some_and(|blocked| path.starts_with(blocked)) {
            return false;
        }
        match crate::observed_fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => return false,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
    }
    true
}

/// Normal warm reuse uses the full current resolver (including external roots).
pub(crate) fn unchanged(
    repositories: &RepositorySet,
    workspace: &Path,
    tree: &Path,
    recorded: &[u8],
) -> Result<bool, String> {
    let change = first_change(repositories, workspace, tree, recorded);
    if diagnosing() {
        LAST_CHANGE.with(|last| {
            *last.borrow_mut() = match &change {
                Ok(None) => None,
                Ok(Some(change)) => Some(change.clone()),
                Err(error) => Some(format!("error: {error}")),
            }
        });
    }
    change.map(|change| change.is_none())
}

thread_local! {
    /// The first recorded input [`unchanged`] last found changed on this
    /// thread, kept only while [`diagnosing`] (the remote repo contents
    /// cache's `verbose` statistics name it, [`take_last_change`]).
    static LAST_CHANGE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Whether [`unchanged`] keeps the reason of its last `false` answer
/// (`BAZEL_RS_REMOTE_REPO_CACHE_STATS=verbose`).
fn diagnosing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(crate::remote_repo_cache::verbose)
}

/// The first changed recorded input of this thread's last [`unchanged`]
/// call that answered `false` (or failed), when [`diagnosing`]; cleared.
pub(crate) fn take_last_change() -> Option<String> {
    LAST_CHANGE.with(|last| last.borrow_mut().take())
}

/// [`unchanged`]'s check, naming the first recorded input that does not
/// hold (`None`: every one holds). The order of the checks is
/// [`unchanged`]'s; the text is for diagnostics only.
fn first_change(
    repositories: &RepositorySet,
    workspace: &Path,
    tree: &Path,
    recorded: &[u8],
) -> Result<Option<String>, String> {
    let recorded: RecordedInputs = serde_json::from_slice(recorded)
        .map_err(|error| format!("cannot parse recorded repository inputs: {error}"))?;
    let resolve = match recorded.version {
        1 => resolve_v1,
        2..=5 => resolve_friendly,
        version => return Ok(Some(format!("probe version {version} unknown"))),
    };
    // Version 3 (and 4 with one): the publication of the workspace it names
    // only (as written: a probe of version 2 or 1 with a workspace is
    // foreign).
    if recorded.version == 3 || recorded.workspace.is_some() {
        let current = workspace_text(workspace);
        if !matches!(recorded.version, 3..=5)
            || recorded.workspace.as_deref() != Some(current.as_str())
        {
            return Ok(Some(format!(
                "workspace {:?} (now {current:?})",
                recorded.workspace.as_deref().unwrap_or("")
            )));
        }
    }
    // Version 4: each repository whose tree the published bytes name still
    // resolves to that tree (written only as version 4).
    if recorded.version == 4 || !recorded.trees.is_empty() {
        if !matches!(recorded.version, 4 | 5) || recorded.trees.is_empty() {
            return Ok(Some(format!(
                "trees in a version {} probe",
                recorded.version
            )));
        }
        let source_cache = repositories.source_cache_path()?;
        let store = crate::observed_fs::canonicalize(source_cache)
            .unwrap_or_else(|_| source_cache.to_path_buf());
        for (name, tree) in &recorded.trees {
            let current = repositories
                .root(name)
                .ok()
                .and_then(|root| crate::observed_fs::canonicalize(root).ok());
            let current = current
                .as_deref()
                .and_then(|current| current.strip_prefix(&store).ok())
                .and_then(Path::to_str);
            if current != Some(tree.as_str()) {
                return Ok(Some(format!(
                    "tree of @@{name}: {tree} (now {})",
                    current.unwrap_or("unresolved")
                )));
            }
        }
    }
    if recorded.version == 1 {
        // Version 1 spelled paths into other repositories as host paths into
        // their content-addressed store trees, which are never rewritten: such
        // a probe cannot observe that the repository was fetched anew (a
        // stale reuse Bazel's `@@repo//path` key never allows), so it is
        // never trusted — the rule runs once more and records version 2.
        let source_cache = repositories.source_cache_path()?;
        let stores = [
            source_cache.to_path_buf(),
            crate::observed_fs::canonicalize(source_cache)
                .unwrap_or_else(|_| source_cache.to_path_buf()),
        ];
        let in_store = |recorded: &str| {
            let path = match recorded.strip_prefix("@@//") {
                Some(relative) => workspace.join(relative),
                None if Path::new(recorded).is_absolute() => PathBuf::from(recorded),
                None => return false,
            };
            stores.iter().any(|store| path.starts_with(store))
        };
        if recorded
            .templates
            .iter()
            .map(|template| &template.source)
            .chain(recorded.files.iter().map(|file| &file.path))
            .any(|recorded| in_store(recorded))
        {
            return Ok(Some(
                "a version 1 probe into the repository store".to_owned(),
            ));
        }
    }
    // Version 5: every recorded `Label()` mapping lookup resolves as it did
    // (RepoRecordedInput.RecordedRepoMapping.isOutdated).
    if (recorded.version == 5) == recorded.mappings.is_empty() {
        return Ok(Some(format!(
            "{} mapping lookups in a version {} probe",
            recorded.mappings.len(),
            recorded.version
        )));
    }
    for (from, apparent, canonical) in &recorded.mappings {
        let current = crate::loading::current_repo_mapping(repositories, from, apparent);
        if current.as_ref() != Some(canonical) {
            return Ok(Some(format!(
                "mapping @{apparent} from @@{from}: {canonical:?} (now {current:?})"
            )));
        }
    }
    if let Some(watched) = &recorded.watched_env {
        // RepositoryEnvironmentValue re-checks: reobserve exactly the recorded
        // names and compare digests. Any observation failure is a change.
        let digest = repositories
            .environment()
            .watched_digest(&watched.names)
            .ok()
            .flatten();
        if digest.as_deref() != Some(watched.sha256.as_str()) {
            return Ok(Some(format!(
                "watched environment {}",
                watched.names.join(",")
            )));
        }
    }
    for template in &recorded.templates {
        let Some(path) = resolve(repositories, workspace, tree, &template.source)? else {
            return Ok(Some(format!("template {} (unresolvable)", template.source)));
        };
        let current = crate::observed_fs::read(&path)
            .map(|bytes| sha256(&bytes))
            .unwrap_or_else(|_| "unreadable".to_owned());
        if current != template.sha256 {
            return Ok(Some(format!(
                "template {} at {}: {} (now {current})",
                template.source,
                path.display(),
                template.sha256
            )));
        }
    }
    for file in &recorded.files {
        let Some(path) = resolve(repositories, workspace, tree, &file.path)? else {
            return Ok(Some(format!("{} {} (unresolvable)", file.kind, file.path)));
        };
        let state = match file.kind.as_str() {
            "dirents" => crate::repository_ctx::recorded_dirents_state(&path),
            "dirtree" => crate::repository_ctx::recorded_dirtree_state(&path),
            _ => crate::repository_ctx::recorded_file_state(&path),
        };
        if state.as_deref() != Some(file.state.as_str()) {
            return Ok(Some(format!(
                "{} {} at {}: {} (now {})",
                file.kind,
                file.path,
                path.display(),
                file.state,
                state.as_deref().unwrap_or("unobservable")
            )));
        }
    }
    Ok(None)
}

/// Resolve one version-2 recorded spelling to a current path: a
/// stage-relative template (inside the published tree itself) or a
/// RepoCacheFriendlyPath (RepoCacheFriendlyPath.getRootedPath,
/// RepoRecordedInput.java:379-399): `@@//rel` in the workspace,
/// `@@NAME//rel` under repository NAME's root (demanded like the original
/// read), an absolute host path as is.
fn resolve_friendly(
    repositories: &RepositorySet,
    workspace: &Path,
    tree: &Path,
    recorded: &str,
) -> Result<Option<PathBuf>, String> {
    if let Some(relative) = recorded.strip_prefix("stage:") {
        return Ok(safe_child(tree, relative));
    }
    if let Some(rest) = recorded.strip_prefix("@@") {
        let Some((name, relative)) = rest.split_once("//") else {
            return Ok(None);
        };
        let root = if name.is_empty() {
            workspace.to_path_buf()
        } else {
            repositories.root(name)?.to_path_buf()
        };
        return Ok(if relative.is_empty() {
            Some(root)
        } else {
            safe_child(&root, relative)
        });
    }
    if Path::new(recorded).is_absolute() {
        return Ok(Some(PathBuf::from(recorded)));
    }
    Ok(None)
}

/// Resolve one version-1 recorded input spelling to a current path: a
/// stage-relative template (inside the published tree itself), `@@//rel` in
/// the workspace, a canonical `@@repo//pkg:file` label (demanded like the
/// original read), or an absolute host path.
fn resolve_v1(
    repositories: &RepositorySet,
    workspace: &Path,
    tree: &Path,
    recorded: &str,
) -> Result<Option<PathBuf>, String> {
    if let Some(relative) = recorded.strip_prefix("stage:") {
        return Ok(safe_child(tree, relative));
    }
    if let Some(relative) = recorded.strip_prefix("@@//") {
        return Ok(safe_child(workspace, relative));
    }
    if recorded.starts_with("@@") || recorded.starts_with("//") {
        let label = Label::parse_canonical(recorded).map_err(|error| error.to_string())?;
        let root = repositories.root(&label.repository)?.to_path_buf();
        let relative = if label.package.is_empty() {
            label.name.to_string()
        } else {
            format!("{}/{}", label.package, label.name)
        };
        return Ok(safe_child(&root, &relative));
    }
    if Path::new(recorded).is_absolute() {
        return Ok(Some(PathBuf::from(recorded)));
    }
    Ok(None)
}

/// A lexically safe child path, or None when the recorded relative name is not
/// a plain relative path (never climb out of a root).
fn safe_child(root: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(root.join(relative))
}

#[cfg(test)]
mod tests {

    #[test]
    fn remote_probes_have_an_uploadable_form() {
        let ok = |json: &str| super::remote_probes_acceptable(json.as_bytes(), "self");
        assert!(ok(r#"{"version":2}"#));
        assert!(ok(
            r#"{"version":2,"files":[{"path":"@@other//x","kind":"file","state":"s"}]}"#
        ));
        assert!(ok(r#"{"version":4}"#));
        assert!(!ok(r#"{"version":1}"#));
        assert!(!ok(r#"{"version":3,"workspace":"/w"}"#));
        assert!(!ok(r#"{"version":2,"workspace":"/w"}"#));
        assert!(!ok(r#"{"version":4,"trees":[["a","direct/trees/x"]]}"#));
        assert!(!ok(
            r#"{"version":2,"files":[{"path":"@@self//x","kind":"file","state":"s"}]}"#
        ));
        assert!(!ok(
            r#"{"version":2,"templates":[{"source":"@@self//t","sha256":"x"}]}"#
        ));
        assert!(!ok("not json"));
    }

    use super::*;

    /// The verbose statistics' reason: the first recorded input that does
    /// not hold, with its recorded and current state.
    #[test]
    fn the_first_changed_input_is_named() {
        let workspace = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        std::fs::write(workspace.path().join("a.txt"), "a\n").unwrap();
        std::fs::write(workspace.path().join("b.txt"), "changed\n").unwrap();
        let repositories = RepositorySet::new(workspace.path().to_path_buf());
        let tree = workspace.path().join("tree");
        let recorded = format!(
            r#"{{"version":2,"files":[{{"path":"@@//a.txt","kind":"file","state":"file:{}"}},{{"path":"@@//b.txt","kind":"file","state":"file:{}"}},{{"path":"@@//c.txt","kind":"file","state":"missing"}}]}}"#,
            sha256(b"a\n"),
            sha256(b"b\n")
        );
        let change = first_change(&repositories, workspace.path(), &tree, recorded.as_bytes());
        assert_eq!(
            change,
            Ok(Some(format!(
                "file @@//b.txt at {}: file:{} (now file:{})",
                workspace.path().join("b.txt").display(),
                sha256(b"b\n"),
                sha256(b"changed\n")
            )))
        );
        std::fs::write(workspace.path().join("b.txt"), "b\n").unwrap();
        let unchanged = first_change(&repositories, workspace.path(), &tree, recorded.as_bytes());
        assert_eq!(unchanged, Ok(None));
    }

    /// Keep-but-never-trust (review i20): probes of a version this binary
    /// does not know — a newer binary's — never validate, so the rule
    /// executes, while `repository_store::upgrades_recorded` keeps them as
    /// written (`newer_recorded_probes_are_never_replaced`). The same bytes
    /// as version 2 validate (no probe, or a probe of an unchanged file):
    /// the version alone decides. Bytes this binary cannot parse at all (a
    /// version beyond `u8`, a format that is not JSON) are never trusted
    /// either: an error, which `repository_store::try_reuse` takes as a
    /// change.
    #[test]
    fn probes_of_a_newer_version_are_never_trusted() {
        let workspace = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        std::fs::write(workspace.path().join("ws.txt"), "workspace file\n").unwrap();
        let repositories = RepositorySet::new(workspace.path().to_path_buf());
        let tree = workspace.path().join("tree");
        let unchanged =
            |recorded: &str| unchanged(&repositories, workspace.path(), &tree, recorded.as_bytes());
        let file = format!(
            r#""files":[{{"path":"@@//ws.txt","kind":"file","state":"file:{}"}}]"#,
            sha256(b"workspace file\n")
        );
        assert_eq!(unchanged(r#"{"version":2}"#), Ok(true));
        assert_eq!(unchanged(&format!(r#"{{"version":2,{file}}}"#)), Ok(true));
        assert_eq!(unchanged(r#"{"version":3}"#), Ok(false));
        assert_eq!(unchanged(&format!(r#"{{"version":3,{file}}}"#)), Ok(false));
        assert_eq!(
            unchanged(r#"{"version":3,"probes":["a newer spelling"]}"#),
            Ok(false)
        );
        for unreadable in [r#"{"version":256}"#, "\0a binary probe format"] {
            assert_ne!(unchanged(unreadable), Ok(true), "{unreadable:?}");
        }
    }
}
