use bazel_rs::{engine::Engine, workspace};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const HELP: &str = "bazel-rs — experimental native build engine (NOT full Bazel parity)

Usage:
  bazel-rs [--ignore_all_rc_files] [--cache-dir=PATH] query PATTERN
  bazel-rs [--ignore_all_rc_files] [--cache-dir=PATH] build [--jobs=N] TARGET...
  bazel-rs [--ignore_all_rc_files] mod graph --output=json [--registry=URL]
  bazel-rs version | --version | capabilities

Patterns: //..., //package/..., //package:all, //package:target, :target
Build options: --jobs=N or --jobs N (positive integer; default: available CPUs, max 32)
Repository options: --registry=URL (file/http/https, repeatable), --ignore_dev_dependency
Native transition: --experimental-builtin-modules[=true|false] opts into genuine Bazel 9.3 built-ins.
Remote registries require explicit --lockfile_mode=off; other lockfile modes are unsupported.
External sources: local_path_override, registry local_path entries and verified archives.
Native rules: filegroup(name, srcs), genrule(name, srcs, outs, cmd)
Loading: local .bzl legacy macros, glob, package defaults and exports_files.
Analysis: custom rules/attributes/providers, declared files, write/run/run_shell actions.
No upstream fallback. Repository rules and module extensions have bounded native profiles.
No test command, complete repository/extension APIs, toolchains,
remote execution, rc expansion, sandbox or full query language yet.
Builds execute trusted shell commands locally; this is not a security sandbox.
";

fn run() -> Result<(), String> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    // Reject override options before any startup, arity or command diagnostic
    // could echo a secret-bearing inline or split argument.
    for arg in &args {
        bazel_rs::module_cli::reject_unsupported_repo_env_argument(arg)?;
    }
    let args = args
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "command-line arguments must be valid UTF-8".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut args = args.into_iter().peekable();
    let mut ignore_rc = false;
    let mut cache = None;
    while let Some(arg) = args.peek() {
        if arg == "--ignore_all_rc_files" {
            ignore_rc = true;
            args.next();
        } else if let Some(value) = arg.strip_prefix("--cache-dir=") {
            if value.is_empty() {
                return Err("--cache-dir requires a nonempty path".into());
            }
            cache = Some(PathBuf::from(value));
            args.next();
        } else {
            break;
        }
    }
    let command = args.next().unwrap_or_else(|| "help".into());
    let mut rest = args.collect::<Vec<_>>();
    match command.as_str() {
        "help" | "--help" | "-h" if rest.is_empty() => {
            print!("{HELP}");
            return Ok(());
        }
        "version" | "--version" if rest.is_empty() => {
            println!(
                "bazel-rs {} (experimental; compatibility target Bazel {})",
                env!("CARGO_PKG_VERSION"),
                bazel_rs::COMPATIBILITY_VERSION,
            );
            return Ok(());
        }
        "capabilities" if rest.is_empty() => {
            println!("{}", include_str!("../docs/capabilities.json"));
            return Ok(());
        }
        "build" if rest.is_empty() => return Err("build requires at least one target".into()),
        "query" | "build" | "mod" => {}
        _ => {
            return Err(format!(
                "unsupported command or startup option {command:?}; run --help for the native subset"
            ));
        }
    }
    let mut jobs = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(32);
    let mut resolver_options = bazel_rs::module_graph::ResolverOptions::default();
    if matches!(command.as_str(), "build" | "query") {
        let mut positional = Vec::new();
        let mut args = rest.into_iter();
        while let Some(arg) = args.next() {
            if let Some(profile) = bazel_rs::module_cli::builtin_profile_argument(&arg)? {
                resolver_options.builtin_profile = profile;
            } else if arg == "--override_repository" || arg.starts_with("--override_repository=") {
                return Err("unsupported option --override_repository; MODULE overrides do not replace the built-in repository filesystem".to_owned());
            } else if arg == "--registry" {
                let value = args.next().ok_or("--registry requires a URL")?;
                resolver_options
                    .registries
                    .push(bazel_rs::module_cli::registry_path(&value)?);
            } else if let Some(value) = arg.strip_prefix("--registry=") {
                resolver_options
                    .registries
                    .push(bazel_rs::module_cli::registry_path(value)?);
            } else if arg == "--lockfile_mode" || arg.starts_with("--lockfile_mode=") {
                let value = if let Some(value) = arg.strip_prefix("--lockfile_mode=") {
                    value.to_owned()
                } else {
                    args.next().ok_or("--lockfile_mode requires a value")?
                };
                if value != "off" {
                    return Err(format!(
                        "unsupported --lockfile_mode={value}; lockfile support is not implemented; use --lockfile_mode=off"
                    ));
                }
                resolver_options.allow_remote = true;
            } else if matches!(
                arg.as_str(),
                "--ignore_dev_dependency"
                    | "--ignore_dev_dependency=true"
                    | "--noignore_dev_dependency=false"
            ) {
                resolver_options.ignore_dev_dependencies = true;
            } else if matches!(
                arg.as_str(),
                "--noignore_dev_dependency"
                    | "--noignore_dev_dependency=true"
                    | "--ignore_dev_dependency=false"
            ) {
                resolver_options.ignore_dev_dependencies = false;
            } else {
                positional.push(arg);
            }
        }
        rest = positional;
        resolver_options.validate_registries()?;
        if command == "query" && rest.len() != 1 {
            return Err("query requires exactly one literal target pattern; query expressions are not implemented".into());
        }
    }
    if command == "build" {
        let mut targets = Vec::new();
        let mut args = rest.into_iter();
        while let Some(arg) = args.next() {
            if arg == "--jobs" {
                jobs = parse_jobs(&args.next().ok_or("--jobs requires a positive integer")?)?;
            } else if let Some(value) = arg.strip_prefix("--jobs=") {
                jobs = parse_jobs(value)?;
            } else {
                targets.push(arg);
            }
        }
        rest = targets;
        if rest.is_empty() {
            return Err("build requires at least one target".into());
        }
    }
    if command != "mod"
        && let Some(flag) = rest.iter().find(|arg| arg.starts_with('-'))
    {
        return Err(format!(
            "unsupported option {flag:?}; options are never silently ignored"
        ));
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let root = workspace::workspace_root(&cwd)?;
    if !ignore_rc {
        check_rc_files(&root)?;
    }
    let cache = cache.unwrap_or_else(|| root.join("bazel-rs-out"));
    let cache = if cache.is_absolute() {
        cache
    } else {
        cwd.join(cache)
    };
    if command == "mod" {
        let output =
            bazel_rs::module_cli::run_with_source_cache(&root, &rest, &cache.join("repositories"))?;
        print!("{output}");
        eprintln!(
            "bazel-rs: bounded module graph profile; graph commands do not execute module extensions"
        );
        return Ok(());
    }
    let current_package = current_package(&root, &cwd)?;
    let repositories = Arc::new(
        bazel_rs::repository::RepositorySet::with_options(root.clone(), resolver_options)
            .with_source_cache(cache.join("repositories")),
    );
    repositories.validate_root_module()?;
    let mut engine = Engine::with_repositories(repositories);
    let labels = engine.select(&rest, &current_package)?;
    if command == "query" {
        // Print only after all packages have loaded successfully.
        for label in labels {
            println!("{label}");
        }
    } else {
        let summary = engine.build_with_jobs(&labels, &cache, jobs)?;
        for (label, output) in summary.outputs {
            println!("{label}\t{}", output.display());
        }
        eprintln!(
            "bazel-rs: {} actions executed, {} cache hits (experimental local execution; not sandboxed)",
            summary.executed, summary.cache_hits
        );
    }
    Ok(())
}

fn parse_jobs(value: &str) -> Result<usize, String> {
    if !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && let Ok(jobs) = value.parse::<usize>()
        && jobs > 0
    {
        return Ok(jobs);
    }
    Err(format!("--jobs requires a positive integer, got {value:?}"))
}

fn current_package(root: &Path, cwd: &Path) -> Result<String, String> {
    for dir in cwd.ancestors() {
        if dir.join("BUILD.bazel").is_file() || dir.join("BUILD").is_file() || dir == root {
            return dir
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| "non-UTF-8 package paths are not supported".into());
        }
    }
    Err("could not determine current package".into())
}

fn check_rc_files(root: &Path) -> Result<(), String> {
    let mut files = vec![PathBuf::from("/etc/bazel.bazelrc"), root.join(".bazelrc")];
    if let Some(home) = std::env::var_os("HOME") {
        files.push(PathBuf::from(home).join(".bazelrc"));
    }
    for file in files {
        if file.exists() {
            return Err(format!(
                "rc evaluation is not implemented (found {}); use --ignore_all_rc_files only if intentionally testing without rc configuration",
                file.display()
            ));
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("bazel-rs: {error}");
        std::process::exit(1);
    }
}
