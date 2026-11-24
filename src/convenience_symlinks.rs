//! The workspace's convenience symlinks (`bazel-bin`, `bazel-testlogs`,
//! `bazel-out`, `bazel-<workspace>`; docs/COMMANDS.md "Convenience
//! symlinks"), like Bazel 9.3.1's OutputDirectoryLinksUtils
//! (buildtool/OutputDirectoryLinksUtils.java, ConfigSymlink.java) driven by
//! ExecutionTool.handleConvenienceSymlinks / createConvenienceSymlinks
//! (genuine capture tests/fixtures/convenience_symlinks):
//!
//! * `build`, `test`, `run` and `coverage` update them once the analysis
//!   configured the top-level targets — also when the analysis or an
//!   action fails, never under `--nobuild` (and so not for `fetch
//!   TARGET...`), never when the target patterns fail to load; `query`,
//!   `cquery`, `aquery` and `info` leave them alone.
//! * They point into the native output base (`<state>/execroot/_main`,
//!   crate::output_tree): `PREFIXbin` and `PREFIXtestlogs` at the
//!   top-level targets' configuration's `bazel-out/<mnemonic>/{bin,testlogs}`,
//!   `PREFIXout` at `bazel-out`, `PREFIX<workspace directory name>` at the
//!   execution root; `PREFIXgenfiles` (at `bin`) only with
//!   `--noincompatible_skip_genfiles_symlink`, else an existing one is
//!   deleted.
//! * The configuration: the top-level targets' one when they share one; the
//!   top-level configuration when they have several and one of them is
//!   it; otherwise `bin`/`testlogs` are deleted with genuine's `cleared
//!   convenience symlink(s) …` warning. No analyzed target: the top-level
//!   configuration.
//! * FileSystemUtils.ensureSymbolicLink: a link already pointing at the
//!   target is left alone, any other symlink is replaced, anything that is
//!   not a symlink (a real directory or file) is left alone with genuine's
//!   `failed to create one or more convenience symlinks` warning. Deleting
//!   only ever removes symlinks.
//! * `--symlink_prefix` (default `bazel-`; `/` creates and deletes nothing,
//!   a prefix with `/` creates its directories),
//!   `--experimental_convenience_symlinks=normal|clean|ignore|log_only`
//!   and `--experimental_convenience_symlinks_bep_event` (the BEP
//!   ConvenienceSymlinksIdentified event, crate::bep).
//! * The build results print the artifacts through the links created (or,
//!   `log_only`, that would have been), relative to the working directory
//!   (PathPrettyPrinter): `bazel-bin/pkg/out.txt`.
//! * `clean` removes `PREFIX{bin,testlogs,genfiles,out,<workspace>}`
//!   symlinks and the prefix's own path and empty parents
//!   (removeOutputDirectoryLinks).
//!
//! Package discovery never follows directory links (crate::workspace), so
//! the links never add packages to `//...`. Globs skip them
//! ([`is_convenience_link`], ProcessPackageDirectory.isConvenienceSymlink's
//! test — Bazel's own globs follow them into the output tree, docs/
//! GLOB_SYMLINKS.md), and the analysis snapshot's glob listings leave them
//! out (crate::observed_fs::Projection), so creating or retargeting them
//! never changes a glob nor invalidates a snapshot.

use crate::execution_options::ConvenienceSymlinks as Mode;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// BuildRequestOptions.NO_CREATE_SYMLINKS_PREFIX.
const NO_CREATE_SYMLINKS_PREFIX: &str = "/";

/// BlazeWorkspace.DO_NOT_BUILD_FILE_NAME, written into the output base and
/// its `execroot/` (holding the workspace path): what
/// ProcessPackageDirectory.isInExecRoot looks for.
const DO_NOT_BUILD_HERE: &str = "DO_NOT_BUILD_HERE";

/// What the top-level targets' configurations are (by output directory
/// mnemonic), recorded by planning (crate::request::PlannedRequest).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TopLevelConfigs {
    /// The top-level configuration's mnemonic (`k8-fastbuild`).
    pub top: String,
    /// The distinct mnemonics of the analyzed requested targets'
    /// configurations (after their rule transitions), in request order;
    /// source files have none.
    pub targets: Vec<String>,
}

/// One command's convenience symlink settings ([`install`]).
#[derive(Clone, Debug)]
pub struct Settings {
    /// The workspace root.
    pub workspace: PathBuf,
    /// The working directory below the workspace (`pkg/sub`, empty at the
    /// root): the printed paths climb out of it.
    pub relative_directory: String,
    /// `--symlink_prefix` (default `bazel-`).
    pub prefix: String,
    pub mode: Mode,
    /// `--experimental_convenience_symlinks_bep_event`.
    pub bep_event: bool,
    /// `--incompatible_skip_genfiles_symlink`.
    pub skip_genfiles: bool,
    /// The native output base (`<state>`).
    pub output_base: PathBuf,
    /// The command updates the links (`build`/`test`/`run`/`coverage`
    /// with an execution phase).
    pub enabled: bool,
}

/// One entry of the BEP ConvenienceSymlinksIdentified event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identified {
    pub path: String,
    /// `None`: DELETE; `Some(target)`: CREATE with the target relative to
    /// the output base.
    pub target: Option<String>,
}

#[derive(Default)]
struct Outcome {
    /// The links created (or, `log_only`, that would have been), by name
    /// (relative to the workspace), with their absolute targets, in
    /// definition order.
    created: Vec<(String, PathBuf)>,
    identified: Vec<Identified>,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();
static OUTCOME: Mutex<Option<Outcome>> = Mutex::new(None);

/// What recognizes a convenience link ([`is_convenience_link`]): the
/// output base's `execroot` and the workspace directory's name.
struct Detection {
    exec_root_base: PathBuf,
    workspace_name: String,
}

static DETECTION: OnceLock<Detection> = OnceLock::new();

/// Install what recognizes this workspace's convenience links (every
/// command that loads packages: their globs skip the links).
pub fn install_detection(workspace: &Path, output_base: &Path) {
    let _ = DETECTION.set(Detection {
        exec_root_base: output_base.join("execroot"),
        workspace_name: base_name(workspace),
    });
}

/// Install this command's settings ([`update`] and [`pretty`] read them).
pub fn install(settings: Settings) {
    let _ = SETTINGS.set(settings);
}

fn base_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The link kinds in OutputDirectoryLinksUtils.STANDARD_LINK_DEFINITIONS
/// order (also the pretty printer's precedence).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bin,
    Testlogs,
    Genfiles,
    Out,
    ExecRoot,
}

const KINDS: [Kind; 5] = [
    Kind::Bin,
    Kind::Testlogs,
    Kind::Genfiles,
    Kind::Out,
    Kind::ExecRoot,
];

fn link_name(kind: Kind, prefix: &str, workspace_name: &str) -> String {
    match kind {
        Kind::Bin => format!("{prefix}bin"),
        Kind::Testlogs => format!("{prefix}testlogs"),
        Kind::Genfiles => format!("{prefix}genfiles"),
        Kind::Out => format!("{prefix}out"),
        Kind::ExecRoot => format!("{prefix}{workspace_name}"),
    }
}

/// ExecutionTool.createConvenienceSymlinks: the configurations the links
/// point into.
fn chosen(configs: &TopLevelConfigs) -> Vec<&str> {
    let mut distinct: Vec<&str> = Vec::new();
    for mnemonic in &configs.targets {
        if !distinct.contains(&mnemonic.as_str()) {
            distinct.push(mnemonic);
        }
    }
    match distinct.len() {
        0 => vec![configs.top.as_str()],
        1 => distinct,
        _ if distinct.contains(&configs.top.as_str()) => vec![configs.top.as_str()],
        _ => distinct,
    }
}

/// SymlinkDefinition.getLinkPaths of `kind`.
fn candidates(
    kind: Kind,
    settings: &Settings,
    configs: &[&str],
    execution_root: &Path,
) -> Vec<PathBuf> {
    let output_path = execution_root.join("bazel-out");
    let per_config = |leaf: &str| -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = Vec::new();
        for mnemonic in configs {
            let path = output_path.join(mnemonic).join(leaf);
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        paths
    };
    match kind {
        Kind::Bin => per_config("bin"),
        Kind::Testlogs => per_config("testlogs"),
        // Bazel 9 merges genfiles into bin (getGenfilesDirectory).
        Kind::Genfiles if settings.skip_genfiles => Vec::new(),
        Kind::Genfiles => per_config("bin"),
        Kind::Out => vec![output_path],
        Kind::ExecRoot => vec![execution_root.to_path_buf()],
    }
}

/// Update the convenience symlinks for a build whose top-level targets have
/// `configs` (ExecutionTool.handleConvenienceSymlinks): create, replace or
/// delete them, print genuine's warnings, and remember the created links
/// for [`pretty`] and the BEP event. Does nothing when the command does not
/// update them; runs once per command.
pub fn update(configs: &TopLevelConfigs) {
    let Some(settings) = SETTINGS.get() else {
        return;
    };
    if !settings.enabled {
        return;
    }
    let mut outcome = OUTCOME.lock().unwrap_or_else(|error| error.into_inner());
    if outcome.is_some() {
        return;
    }
    let mut result = Outcome::default();
    if settings.mode != Mode::Ignore && settings.prefix != NO_CREATE_SYMLINKS_PREFIX {
        result = create_links(settings, configs);
    }
    *outcome = Some(result);
}

/// [`update`] after a failed analysis (BuildTool's finally block: the
/// top-level configuration, no analyzed target), when the output base
/// exists: a first command whose analysis fails creates no output base
/// (natively nothing is written before execution; Bazel's output base
/// always exists).
pub fn update_after_failed_analysis(top: &str) {
    let Some(settings) = SETTINGS.get() else {
        return;
    };
    // snapshot-unrecorded: after planning; never an input of analysis.
    if !std::fs::metadata(&settings.output_base).is_ok_and(|metadata| metadata.is_dir()) {
        return;
    }
    update(&TopLevelConfigs {
        top: top.to_owned(),
        targets: Vec::new(),
    });
}

/// OutputDirectoryLinksUtils.createOutputDirectoryLinks.
fn create_links(settings: &Settings, configs: &TopLevelConfigs) -> Outcome {
    let execution_root = crate::output_tree::execution_root_below(&settings.output_base);
    let log_only = settings.mode == Mode::LogOnly;
    let workspace_name = base_name(&settings.workspace);
    let configs = chosen(configs);
    let mut outcome = Outcome::default();
    let mut failures: Vec<String> = Vec::new();
    let mut ambiguous: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    if !log_only && settings.mode != Mode::Clean {
        write_do_not_build_here(settings);
    }
    for kind in KINDS {
        let name = link_name(kind, &settings.prefix, &workspace_name);
        if names.contains(&name) {
            continue;
        }
        names.push(name.clone());
        if settings.mode == Mode::Clean {
            remove_link(settings, &name, &mut failures, &mut outcome, log_only);
            continue;
        }
        let paths = candidates(kind, settings, &configs, &execution_root);
        if let [target] = paths.as_slice() {
            create_link(
                settings,
                &name,
                target,
                &mut failures,
                &mut outcome,
                log_only,
            );
        } else {
            remove_link(settings, &name, &mut failures, &mut outcome, log_only);
            if paths.len() > 1 {
                ambiguous.push(name);
            }
        }
    }
    if !failures.is_empty() {
        crate::ui::note(&format!(
            "WARNING: failed to create one or more convenience symlinks for prefix '{}':\n  {}",
            settings.prefix,
            failures.join("\n  ")
        ));
    }
    if !ambiguous.is_empty() {
        crate::ui::note(&format!(
            "WARNING: cleared convenience symlink(s) {} because they wouldn't contain requested targets' outputs. Those targets self-transition to multiple distinct configurations",
            ambiguous.join(", ")
        ));
    }
    outcome
}

/// BlazeWorkspace.writeDoNotBuildHereFile: the workspace path in
/// `<output base>/DO_NOT_BUILD_HERE` and `<output base>/execroot/…`, so
/// Bazel (and [`is_convenience_link`]) recognize links into this output
/// base. Written only when missing or different.
fn write_do_not_build_here(settings: &Settings) {
    let content = settings.workspace.as_os_str().as_encoded_bytes();
    for directory in [
        settings.output_base.clone(),
        settings.output_base.join("execroot"),
    ] {
        let path = directory.join(DO_NOT_BUILD_HERE);
        // snapshot-unrecorded: written after planning; never an input of
        // analysis.
        if std::fs::read(&path).is_ok_and(|existing| existing == content) {
            continue;
        }
        let _ = std::fs::create_dir_all(&directory);
        let _ = std::fs::write(&path, content);
    }
}

/// OutputDirectoryLinksUtils.createLink.
fn create_link(
    settings: &Settings,
    name: &str,
    target: &Path,
    failures: &mut Vec<String>,
    outcome: &mut Outcome,
    log_only: bool,
) {
    let relative = target
        .strip_prefix(&settings.output_base)
        .unwrap_or(target)
        .to_string_lossy()
        .into_owned();
    outcome.identified.push(Identified {
        path: name.to_owned(),
        target: Some(relative),
    });
    if log_only {
        outcome
            .created
            .push((name.to_owned(), target.to_path_buf()));
        return;
    }
    if let Err(error) = std::fs::create_dir_all(target) {
        failures.push(format!(
            "cannot create directory {}: {}",
            target.display(),
            java_message(target, &error)
        ));
        return;
    }
    let link = settings.workspace.join(name);
    match ensure_symbolic_link(&link, target) {
        Ok(()) => outcome
            .created
            .push((name.to_owned(), target.to_path_buf())),
        Err(message) => failures.push(format!(
            "cannot create symbolic link {name} -> {}:  {message}",
            target.display()
        )),
    }
}

/// FileSystemUtils.ensureSymbolicLink: an existing link to `target` stays,
/// another symlink is replaced, anything else fails (`… is not a
/// symlink`); missing parents of the link are created. The replacement is
/// a new link renamed over the old one, so a concurrent reader (a copy
/// script, another build) never finds the link missing.
fn ensure_symbolic_link(link: &Path, target: &Path) -> Result<(), String> {
    // snapshot-unrecorded: the convenience links are written after
    // planning; never an input of analysis (globs skip them).
    match std::fs::read_link(link) {
        Ok(existing) if existing == target => Ok(()),
        Ok(_) => {
            let name = link
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let temporary =
                link.with_file_name(format!(".{name}.bazel-rs-{}.tmp", std::process::id()));
            let _ = std::fs::remove_file(&temporary);
            std::os::unix::fs::symlink(target, &temporary)
                .and_then(|()| std::fs::rename(&temporary, link))
                .map_err(|error| {
                    let _ = std::fs::remove_file(&temporary);
                    java_message(link, &error)
                })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent).map_err(|error| java_message(parent, &error))?;
            }
            match std::os::unix::fs::symlink(target, link) {
                // A concurrent command created it first.
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists
                        && points_at(link, target) =>
                {
                    Ok(())
                }
                result => result.map_err(|error| java_message(link, &error)),
            }
        }
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => {
            Err(format!("{} is not a symlink", link.display()))
        }
        Err(error) => Err(java_message(link, &error)),
    }
}

/// Whether the symlink `link` points at `target`.
fn points_at(link: &Path, target: &Path) -> bool {
    // snapshot-unrecorded: the convenience links are written after planning.
    std::fs::read_link(link).is_ok_and(|existing| existing == target)
}

/// The text of a Java IOException for `path` (`PATH (Reason)`).
fn java_message(path: &Path, error: &std::io::Error) -> String {
    let reason = error.to_string();
    let reason = reason
        .split(" (os error")
        .next()
        .unwrap_or(&reason)
        .to_owned();
    format!("{} ({reason})", path.display())
}

/// OutputDirectoryLinksUtils.removeLink: only a symlink is deleted.
fn remove_link(
    settings: &Settings,
    name: &str,
    failures: &mut Vec<String>,
    outcome: &mut Outcome,
    log_only: bool,
) {
    outcome.identified.push(Identified {
        path: name.to_owned(),
        target: None,
    });
    if log_only {
        return;
    }
    if let Err(error) = remove_symlink(&settings.workspace.join(name)) {
        failures.push(format!("{name}: {error}"));
    }
}

fn remove_symlink(link: &Path) -> Result<(), String> {
    // snapshot-unrecorded: the convenience links are written after
    // planning; never an input of analysis.
    if std::fs::symlink_metadata(link).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        std::fs::remove_file(link).map_err(|error| java_message(link, &error))?;
    }
    Ok(())
}

/// `clean` (OutputDirectoryLinksUtils.removeOutputDirectoryLinks): delete
/// the `prefix` links of `workspace` that are symlinks, then the prefix's
/// own path and its empty parent directories (removeDirectoryAndParents).
pub fn remove(workspace: &Path, prefix: &str) {
    if prefix == NO_CREATE_SYMLINKS_PREFIX {
        return;
    }
    let workspace_name = base_name(workspace);
    let mut failures = Vec::new();
    for kind in KINDS {
        let name = link_name(kind, prefix, &workspace_name);
        if let Err(error) = remove_symlink(&workspace.join(&name)) {
            failures.push(format!("{name}: {error}"));
        }
    }
    remove_directory_and_parents(workspace, prefix);
    if !failures.is_empty() {
        crate::ui::note(&format!(
            "WARNING: failed to remove one or more convenience symlinks for prefix '{prefix}':\n  {}",
            failures.join("\n  ")
        ));
    }
}

/// FileSystemUtils.removeDirectoryAndParents(workspace, prefix): the
/// prefix's path and then each parent, while they can be deleted (a
/// non-empty directory stops it).
fn remove_directory_and_parents(workspace: &Path, prefix: &str) {
    let fragment = Path::new(prefix);
    if fragment.is_absolute() {
        return;
    }
    let mut parts: Vec<&OsStr> = Vec::new();
    for component in fragment.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            // PathFragment keeps a leading `..`: never outside the
            // workspace.
            Component::ParentDir if parts.pop().is_none() => return,
            _ => {}
        }
    }
    while !parts.is_empty() {
        let path = parts
            .iter()
            .fold(workspace.to_path_buf(), |path, part| path.join(part));
        // snapshot-unrecorded: `clean` never plans.
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                let removed = if metadata.is_dir() {
                    std::fs::remove_dir(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                if removed.is_err() {
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return,
        }
        parts.pop();
    }
}

/// PathPrettyPrinter.getPrettyPath: `path` through the first created link
/// whose target contains it, relative to the working directory; `path`
/// itself without one (or with `--symlink_prefix=/`).
pub fn pretty(path: &Path) -> PathBuf {
    let Some(settings) = SETTINGS.get() else {
        return path.to_path_buf();
    };
    if settings.prefix == NO_CREATE_SYMLINKS_PREFIX {
        return path.to_path_buf();
    }
    let outcome = OUTCOME.lock().unwrap_or_else(|error| error.into_inner());
    let Some(outcome) = outcome.as_ref() else {
        return path.to_path_buf();
    };
    for (name, target) in &outcome.created {
        if let Ok(rest) = path.strip_prefix(target) {
            let mut pretty = PathBuf::new();
            for _ in Path::new(&settings.relative_directory).components() {
                pretty.push("..");
            }
            pretty.push(name);
            if !rest.as_os_str().is_empty() {
                pretty.push(rest);
            }
            return pretty;
        }
    }
    path.to_path_buf()
}

/// The ConvenienceSymlinksIdentified entries of this command, `None` when
/// the event is not posted (`--noexperimental_convenience_symlinks_bep_event`
/// or no update).
pub fn identified() -> Option<Vec<Identified>> {
    let settings = SETTINGS.get()?;
    if !settings.bep_event || !settings.enabled {
        return None;
    }
    let outcome = OUTCOME.lock().unwrap_or_else(|error| error.into_inner());
    Some(
        outcome
            .as_ref()
            .map(|outcome| outcome.identified.clone())
            .unwrap_or_default(),
    )
}

/// ProcessPackageDirectory.isConvenienceSymlink for the entry `name` of
/// `directory` (a symlink): its target (read, not followed) is below this
/// output base's `execroot`, or its name ends like a convenience link's
/// (`-bin`, `-genfiles`, `-testlogs` four levels, `-out` two levels,
/// `-<workspace name>` one level below a directory named `execroot` that
/// holds `DO_NOT_BUILD_HERE`) — Bazel's own links and any of an earlier
/// `--symlink_prefix`. Only a link whose name could be one is read.
pub fn is_convenience_link(directory: &Path, name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let detection = DETECTION.get();
    let workspace_name = detection.map(|detection| detection.workspace_name.as_str());
    let depth =
        if name.ends_with("-bin") || name.ends_with("-genfiles") || name.ends_with("-testlogs") {
            Some(4)
        } else if name.ends_with("-out") {
            Some(2)
        } else if workspace_name.is_some_and(|workspace| {
            !workspace.is_empty() && name.ends_with(&format!("-{workspace}"))
        }) {
            Some(1)
        } else {
            None
        };
    // Rule 1 (any name) needs the output base: only names a prefix can
    // end a link kind's with.
    let own = detection.filter(|detection| {
        [
            "bin",
            "genfiles",
            "testlogs",
            "out",
            &detection.workspace_name,
        ]
        .iter()
        .any(|suffix| !suffix.is_empty() && name.ends_with(suffix))
    });
    if depth.is_none() && own.is_none() {
        return false;
    }
    // snapshot-unrecorded: the classification is a function of the listing
    // the snapshot validates (crate::observed_fs::Projection recomputes it
    // when the listing changes); the links are no input of analysis.
    let Ok(target) = std::fs::read_link(directory.join(name)) else {
        return false;
    };
    if !target.is_absolute() {
        return false;
    }
    if let Some(detection) = own
        && target.starts_with(&detection.exec_root_base)
    {
        return true;
    }
    let Some(depth) = depth else {
        return false;
    };
    let components: Vec<Component<'_>> = target.components().collect();
    if components.len() <= depth {
        return false;
    }
    let candidate: PathBuf = components[..components.len() - depth].iter().collect();
    if candidate.file_name() != Some(OsStr::new("execroot")) {
        return false;
    }
    // snapshot-unrecorded: see above.
    std::fs::metadata(candidate.join(DO_NOT_BUILD_HERE)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configs(top: &str, targets: &[&str]) -> TopLevelConfigs {
        TopLevelConfigs {
            top: top.to_owned(),
            targets: targets.iter().map(|target| (*target).to_owned()).collect(),
        }
    }

    #[test]
    fn chosen_configuration_follows_create_convenience_symlinks() {
        assert_eq!(chosen(&configs("k8-fastbuild", &[])), ["k8-fastbuild"]);
        assert_eq!(chosen(&configs("k8-fastbuild", &["k8-opt"])), ["k8-opt"]);
        assert_eq!(
            chosen(&configs("k8-fastbuild", &["k8-opt", "k8-fastbuild"])),
            ["k8-fastbuild"]
        );
        assert_eq!(
            chosen(&configs("k8-fastbuild", &["k8-opt", "k8-dbg", "k8-opt"])),
            ["k8-opt", "k8-dbg"]
        );
    }

    #[test]
    fn convenience_links_are_recognized_by_name_and_execroot() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let execroot = root.join("ob/execroot");
        std::fs::create_dir_all(execroot.join("_main/bazel-out/k8-fastbuild/bin")).unwrap();
        std::fs::create_dir_all(root.join("ws")).unwrap();
        let ws = root.join("ws");
        let link = |name: &str, target: &Path| {
            std::os::unix::fs::symlink(target, ws.join(name)).unwrap();
        };
        let bin = execroot.join("_main/bazel-out/k8-fastbuild/bin");
        link("bazel-bin", &bin);
        link("foo-out", &execroot.join("_main/bazel-out"));
        link("x-bin", &root.join("ws"));
        link("other", &bin);
        // No DO_NOT_BUILD_HERE yet: not an execroot.
        assert!(!is_convenience_link(&ws, OsStr::new("bazel-bin")));
        std::fs::write(execroot.join(DO_NOT_BUILD_HERE), "/ws").unwrap();
        assert!(is_convenience_link(&ws, OsStr::new("bazel-bin")));
        assert!(is_convenience_link(&ws, OsStr::new("foo-out")));
        assert!(!is_convenience_link(&ws, OsStr::new("x-bin")));
        assert!(!is_convenience_link(&ws, OsStr::new("other")));
        assert!(!is_convenience_link(&ws, OsStr::new("missing-bin")));
    }

    #[test]
    fn remove_directory_and_parents_stops_at_a_non_empty_directory() {
        let temp = tempfile::tempdir().unwrap();
        let ws = temp.path();
        std::fs::create_dir_all(ws.join("out/x-")).unwrap();
        remove_directory_and_parents(ws, "out/x-");
        assert!(!ws.join("out").exists());
        std::fs::create_dir_all(ws.join("out")).unwrap();
        std::fs::write(ws.join("out/keep"), "").unwrap();
        remove_directory_and_parents(ws, "out/x-");
        assert!(ws.join("out/keep").exists());
        remove_directory_and_parents(ws, "/abs");
    }
}
