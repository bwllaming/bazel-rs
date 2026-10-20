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
#[derive(Serialize, Deserialize)]
struct RecordedInputs {
    version: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    templates: Vec<TemplateProbe>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    files: Vec<FileProbe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watched_env: Option<WatchedProbe>,
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Serialize the repository specification: length-framed provenance, the host
/// markers and the declared environment's captured values. Pure in its inputs;
/// equal bytes address the same publication pointer. Retained as the
/// byte-exact reference the digest form mirrors (tests pin equivalence of
/// inputs); production callers use the digest forms below.
#[cfg(test)]
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
pub(crate) fn specification_digest(
    repositories: &RepositorySet,
    provenance_fields: &serde_json::Value,
    sources: &BTreeMap<String, crate::loading::SourceProvenance>,
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
    digest.update(b"direct-repository-spec-digest-v1");
    digest.update([0]);
    digest.update(
        crate::repository_store::spec_digest_identity(provenance_fields, sources).as_bytes(),
    );
    digest.update((host.len() as u64).to_le_bytes());
    digest.update(&host);
    digest.update((environment.len() as u64).to_le_bytes());
    digest.update(&environment);
    Ok(format!("{:x}", digest.finalize()))
}

/// Digest-form specification over already-serialized provenance bytes
/// (the ordinary path frames them once for the content identity).
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
    digest.update(b"direct-repository-spec-digest-v1");
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
    let recorded = RecordedInputs {
        version: 1,
        templates: inputs
            .templates
            .iter()
            .map(|template| TemplateProbe {
                source: template.source.clone(),
                sha256: sha256(&template.bytes),
            })
            .collect(),
        files: inputs
            .files
            .iter()
            .map(|file| FileProbe {
                path: file.path.clone(),
                kind: file.kind.to_owned(),
                state: file.state.clone(),
            })
            .collect(),
        watched_env: inputs.watched_env.as_ref().map(|watched| WatchedProbe {
            sha256: watched.sha256.clone(),
            names: watch_names.to_vec(),
        }),
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
    if recorded.version != 1 {
        return Ok(false);
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
        let Ok(bytes) = std::fs::read(&path) else {
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

/// Resolve one recorded input spelling to a current path: a stage-relative
/// template (inside the published tree itself), `@@//rel` in the workspace, a
/// canonical `@@repo//pkg:file` label (demanded like the original read), or an
/// absolute host path (RepoCacheFriendlyPath's reverse mapping).
fn resolve(
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
