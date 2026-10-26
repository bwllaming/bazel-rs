//! `mod tidy` (ModCommand.runTidy, BazelModTidyFunction): the root module's
//! `use_repo` fixups (LockfileModuleExtensionMetadata.generateFixup) applied
//! by `@buildozer_binary//:buildozer.exe` of `@bazel_tools`, which also
//! formats MODULE.bazel.
use super::{Code, Failure};
use crate::module_graph::ResolvedGraph;
use crate::repository::RepositorySet;
use std::io::Write as _;
use std::path::Path;

/// The name a `use_extension` proxy is assigned to at the top level of the
/// MODULE file (ModuleExtensionUsage.Proxy.getProxyName): the identifier
/// before `= use_extension` on the call's line; empty when there is none.
fn proxy_name(location: &str) -> String {
    let mut parts = location.rsplitn(3, ':');
    let (Some(column), Some(line), Some(file)) = (parts.next(), parts.next(), parts.next()) else {
        return String::new();
    };
    let (Ok(column), Ok(line)) = (column.parse::<usize>(), line.parse::<usize>()) else {
        return String::new();
    };
    let Ok(text) = std::fs::read_to_string(file) else {
        return String::new();
    };
    let Some(text) = text.lines().nth(line.saturating_sub(1)) else {
        return String::new();
    };
    let before: String = text.chars().take(column.saturating_sub(1)).collect();
    let Some(assignment) = before
        .trim_end()
        .strip_suffix("use_extension")
        .map(str::trim_end)
        .and_then(|rest| rest.strip_suffix('='))
    else {
        return String::new();
    };
    let name = assignment.trim();
    let valid = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid {
        name.to_owned()
    } else {
        String::new()
    }
}

pub(crate) fn run(
    repositories: &RepositorySet,
    _graph: &ResolvedGraph,
    _ignore_dev_dependencies: bool,
    _out: &mut Vec<u8>,
) -> Result<(), Failure> {
    let (fixups, errors) = repositories
        .root_extension_fixups(&proxy_name)
        .map_err(Failure::unknown)?;
    for error in &errors {
        eprintln!("ERROR: {error}");
    }
    let buildozer = repositories
        .mapping("bazel_tools")
        .ok()
        .and_then(|mapping| mapping.get("buildozer_binary").cloned())
        .ok_or_else(|| {
            Failure::new(
                "Unexpected error while running buildozer: @buildozer_binary is not visible from @bazel_tools",
                Code::BuildozerFailed,
            )
        })?;
    let buildozer = repositories
        .root(&buildozer)
        .map_err(|error| {
            Failure::new(
                format!("Unexpected error while running buildozer: {error}"),
                Code::BuildozerFailed,
            )
        })?
        .join("buildozer.exe");
    let mut input = String::from("//MODULE.bazel:all|");
    for (_, commands) in &fixups {
        for command in commands {
            input.push_str(command);
            input.push('|');
        }
    }
    input.push_str("format\n");
    let workspace = repositories.main_root();
    let outcome = run_buildozer(&buildozer, workspace, &input).map_err(|error| {
        Failure::new(
            format!("Unexpected error while running buildozer: {error}"),
            Code::BuildozerFailed,
        )
    })?;
    match outcome.status.code() {
        Some(0) => {
            for (extension, _) in &fixups {
                crate::ui::note(&format!("INFO: Updated use_repo calls for {extension}"));
            }
        }
        // Buildozer exits with 3 when it changed nothing.
        Some(3) => {}
        code => {
            return Err(Failure::new(
                format!(
                    "Unexpected error while running buildozer: Process exited with status {}:\n{}",
                    code.unwrap_or(-1),
                    String::from_utf8_lossy(&outcome.stderr)
                ),
                Code::BuildozerFailed,
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Failure::new(
            format!(
                "Failed to process {} extension{} due to errors.",
                errors.len(),
                if errors.len() == 1 { "" } else { "s" }
            ),
            Code::ErrorDuringGraphInspection,
        ))
    }
}

fn run_buildozer(
    buildozer: &Path,
    workspace: &Path,
    input: &str,
) -> std::io::Result<std::process::Output> {
    let mut command = std::process::Command::new(buildozer);
    command
        .args(["-f", "-"])
        .current_dir(workspace)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = crate::spawn_lock::spawn(&mut command)?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input.as_bytes())?;
    }
    child.wait_with_output()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_names_come_from_the_assignment() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("MODULE.bazel");
        std::fs::write(
            &file,
            "go_deps = use_extension(\"@g//:e.bzl\", \"e\")\n  use_extension(\"//:x.bzl\", \"x\")\n",
        )
        .unwrap();
        let at = |line: usize, column: usize| format!("{}:{line}:{column}", file.display());
        assert_eq!(proxy_name(&at(1, 24)), "go_deps");
        assert_eq!(proxy_name(&at(2, 16)), "");
        assert_eq!(proxy_name("nowhere"), "");
    }
}
