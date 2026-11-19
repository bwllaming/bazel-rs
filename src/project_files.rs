//! PROJECT.scl project files (analysis/Project.java,
//! skyframe/ProjectFilesLookupFunction.java, skyframe/ProjectFunction.java).
//! With `--enforce_project_configs` (default true; native accepts only the
//! default, and no `--scl_config`), a building command looks up each
//! requested target's innermost PROJECT.scl (walking up its package path
//! through the enclosing packages) and loads the files it finds before the
//! analysis (AnalysisPhaseRunner.evaluateProjectFiles, :282-290): a file that
//! fails to load, or a project alias whose `actual` fails, ends the command
//! with `error loading project files: MESSAGE` (a loading failure, exit 1).
//!
//! A loaded project that defines configurations (`buildable_units`, legacy
//! `configs`) would have its default flags applied to the build
//! (Project.applySclConfig, FlagSetFunction): not modelled, an explicit
//! failure. A project without configurations changes nothing.
//!
//! Only main-repository targets are looked up. The lookups are observed
//! reads (crate::observed_fs), so an analysis snapshot sees a project file
//! appear or change.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use starlark::values::Value;
use starlark::values::dict::DictRef;

use crate::label::Label;
use crate::repository::RepositorySet;

const PROJECT_FILE_NAME: &str = "PROJECT.scl";

/// The parent directory of a package path (`""` for a top-level one),
/// `None` for the root.
fn parent(package: &str) -> Option<&str> {
    if package.is_empty() {
        None
    } else {
        Some(package.rsplit_once('/').map_or("", |(parent, _)| parent))
    }
}

/// ProjectFilesLookupFunction: the innermost project file of the package
/// containing `directory`, memoized per directory (`memo`).
fn innermost_project_file(
    root: &Path,
    directory: &str,
    memo: &mut HashMap<String, Option<Label>>,
) -> Result<Option<Label>, String> {
    if let Some(found) = memo.get(directory) {
        return Ok(found.clone());
    }
    // ContainingPackageLookup: the directory itself when it is a package.
    let found = if crate::workspace::is_package(root, directory)? {
        let path = root.join(directory).join(PROJECT_FILE_NAME);
        match crate::observed_fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => Some(Label {
                repository: String::new(),
                package: directory.to_owned(),
                name: PROJECT_FILE_NAME.to_owned(),
            }),
            Ok(_) => {
                let relative = if directory.is_empty() {
                    PROJECT_FILE_NAME.to_owned()
                } else {
                    format!("{directory}/{PROJECT_FILE_NAME}")
                };
                return Err(format!(
                    "Error finding project files: {relative} isn't a file"
                ));
            }
            Err(_) => match parent(directory) {
                Some(up) => innermost_project_file(root, up, memo)?,
                None => None,
            },
        }
    } else {
        match parent(directory) {
            Some(up) => innermost_project_file(root, up, memo)?,
            None => None,
        }
    };
    memo.insert(directory.to_owned(), found.clone());
    Ok(found)
}

/// Whether `value` (the `project` global) defines configurations, after
/// following project aliases; errors are ProjectFunction's.
fn project_value(
    repositories: &Arc<RepositorySet>,
    label: &Label,
    depth: usize,
) -> Result<(Label, bool), String> {
    if depth > 16 {
        return Err(format!("project alias cycle at {label}"));
    }
    // BzlLoadFunction's package lookup: a label outside every package.
    if label.repository.is_empty() {
        let root = repositories.root("")?;
        let mut candidate = Some(label.package.as_str());
        let mut contained = None;
        while let Some(package) = candidate {
            if crate::workspace::is_package(root, package)? {
                contained = Some(package);
                break;
            }
            candidate = parent(package);
        }
        if let Some(containing) = contained
            && containing != label.package
        {
            let full = format!("{}/{}", label.package, label.name);
            let relative = if containing.is_empty() {
                full.as_str()
            } else {
                &full[containing.len() + 1..]
            };
            return Err(format!(
                "Label '{label}' is invalid because '{}' is not a package; perhaps you meant to put the colon here: '//{containing}:{relative}'?",
                label.package
            ));
        }
        if contained.is_none() {
            return Err(format!(
                "Every .bzl file must have a corresponding package, but '{label}' does not have one. Please create a BUILD file in the same or any parent directory. Note that this BUILD file does not need to do anything except exist."
            ));
        }
    }
    let globals = repositories.build_phase_globals().1;
    let prints = crate::loading::LoadPrints::default();
    let loaded =
        crate::loading::load_doc_module_in(repositories.clone(), label, &globals, &prints)?;
    let module = &loaded.module;
    let Ok(project) = module.get("project") else {
        return Err(
            "Project files must define exactly one top-level variable called \"project\""
                .to_owned(),
        );
    };
    starlark::environment::Module::with_temp_heap(|module_heap| {
        let value: Value = module_heap.heap().access_owned_frozen_value(&project);
        if let Some(dict) = DictRef::from_value(value) {
            let actual = dict
                .iter()
                .find(|(key, _)| key.unpack_str() == Some("actual"))
                .map(|(_, value)| value);
            if let Some(actual) = actual {
                let Some(text) = actual.unpack_str() else {
                    return Err(format!(
                        "project[\"actual\"]: expected string, got {}",
                        actual.to_str()
                    ));
                };
                if dict.len() > 1 {
                    return Err(format!(
                        "project[\"actual\"] is present, but other keys are present as well: [{}]",
                        dict.iter()
                            .map(|(key, _)| key.to_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                let target = Label::parse_canonical(text).map_err(|error| error.to_string())?;
                return project_value(repositories, &target, depth + 1);
            }
            let configured = dict.iter().any(|(key, value)| {
                key.unpack_str() == Some("configs")
                    && DictRef::from_value(value).is_some_and(|configs| !configs.is_empty())
            });
            return Ok((label.clone(), configured));
        }
        // The proto schema (a struct): its `buildable_units`.
        let units = value
            .get_attr("buildable_units", module_heap.heap())
            .map_err(|error| error.to_string())?;
        let configured = match units {
            Some(units) => units.length().map_or(true, |length| length > 0),
            None => false,
        };
        Ok((label.clone(), configured))
    })
}

/// AnalysisPhaseRunner.evaluateProjectFiles for `labels` (the requested
/// targets): `Ok(Some(INFO line))` for a single project, `Ok(None)` when no
/// target has one, else the command's error.
pub(crate) fn check(
    root: &Path,
    repositories: &Arc<RepositorySet>,
    labels: &[Label],
) -> Result<Option<String>, String> {
    let mut projects: Vec<Label> = Vec::new();
    let mut memo = HashMap::new();
    let mut without_project = false;
    for label in labels {
        if !label.repository.is_empty() {
            without_project = true;
            continue;
        }
        let found = innermost_project_file(root, &label.package, &mut memo)?;
        match found {
            Some(project) if !projects.contains(&project) => projects.push(project),
            Some(_) => {}
            None => without_project = true,
        }
    }
    if projects.is_empty() {
        return Ok(None);
    }
    let mut canonical: Vec<(Label, bool)> = Vec::new();
    for project in &projects {
        let resolved = project_value(repositories, project, 0)
            .map_err(|error| format!("error loading project files: {error}"))?;
        if !canonical.iter().any(|(label, _)| *label == resolved.0) {
            canonical.push(resolved);
        }
    }
    if let Some((label, _)) = canonical.iter().find(|(_, configured)| *configured) {
        return Err(format!(
            "project configurations of {label} (PROJECT.scl buildable units applied to the build, --enforce_project_configs) are not implemented in the native profile"
        ));
    }
    let _ = without_project;
    Ok(match canonical.as_slice() {
        [(label, _)] => Some(format!("Reading project settings from {label}.")),
        _ => None,
    })
}
