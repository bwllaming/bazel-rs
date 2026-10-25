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
    digest.update(b"ordinary-repository-spec-digest-v3");
    digest.update([0]);
    digest.update((canonical_name.len() as u64).to_le_bytes());
    digest.update(canonical_name.as_bytes());
    digest.update(provenance_digest);
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
pub(crate) fn record(
    inputs: &crate::repository_ctx::ExecutionInputs,
    watch_names: &[String],
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
        version: if inputs.workspace.is_some() { 3 } else { 2 },
        templates: inputs
            .templates
            .iter()
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
    };
    serde_json::to_vec(&recorded)
        .map_err(|error| format!("cannot record repository inputs: {error}"))
}

/// Reobserve the recorded inputs against the current world. `Ok(false)` means
/// the world changed (or cannot be confirmed unchanged): the store re-executes
/// the repository rule, like Bazel refetching after a dependency change.
pub(crate) fn unchanged(
    repositories: &RepositorySet,
    workspace: &Path,
    tree: &Path,
    recorded: &[u8],
) -> Result<bool, String> {
    let recorded: RecordedInputs = serde_json::from_slice(recorded)
        .map_err(|error| format!("cannot parse recorded repository inputs: {error}"))?;
    let resolve = match recorded.version {
        1 => resolve_v1,
        2 | 3 => resolve_friendly,
        _ => return Ok(false),
    };
    // Version 3: the publication of the workspace it names only (as
    // written: a probe of version 2 or 1 with a workspace is foreign).
    if recorded.version == 3 || recorded.workspace.is_some() {
        let current = workspace_text(workspace);
        if recorded.version != 3 || recorded.workspace.as_deref() != Some(current.as_str()) {
            return Ok(false);
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
            return Ok(false);
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
            return Ok(false);
        }
    }
    for template in &recorded.templates {
        let Some(path) = resolve(repositories, workspace, tree, &template.source)? else {
            return Ok(false);
        };
        let Ok(bytes) = crate::observed_fs::read(&path) else {
            return Ok(false);
        };
        if sha256(&bytes) != template.sha256 {
            return Ok(false);
        }
    }
    for file in &recorded.files {
        let Some(path) = resolve(repositories, workspace, tree, &file.path)? else {
            return Ok(false);
        };
        let state = match file.kind.as_str() {
            "dirents" => crate::repository_ctx::recorded_dirents_state(&path),
            "dirtree" => crate::repository_ctx::recorded_dirtree_state(&path),
            _ => crate::repository_ctx::recorded_file_state(&path),
        };
        if state.as_deref() != Some(file.state.as_str()) {
            return Ok(false);
        }
    }
    Ok(true)
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
            label.name.clone()
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
    use super::*;

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
        let workspace = tempfile::tempdir_in("/tmp/opencode").unwrap();
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
