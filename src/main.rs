use bazel_rs::{bazelrc, engine::Engine, workspace};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const HELP: &str = "bazel-rs — experimental native build engine (NOT full Bazel parity)

Usage:
  bazel-rs [STARTUP...] [--cache-dir=PATH] query PATTERN
  bazel-rs [STARTUP...] [--cache-dir=PATH] build [--jobs=N] [CONFIG...] TARGET...
  bazel-rs [STARTUP...] mod graph --output=json [--registry=URL]
  bazel-rs version | --version | capabilities

Startup: --[no]system_rc --[no]workspace_rc --[no]home_rc --[no]ignore_all_rc_files
  --bazelrc=FILE; rc files are evaluated like Bazel 9.3 (--config, common/always,
  --announce_rc; docs/BAZELRC.md); unmodelled rc options fail naming file:line.
Patterns: //..., //package/..., //package:all, //package:target, :target
Build options: --jobs=N or --jobs N (positive integer; default: available CPUs, max 32)
Configuration (build): -c/--compilation_mode, --[no]stamp, --strip, --[no]force_pic,
  --define NAME=VALUE, --platforms, --host_platform, Starlark build settings
  --//pkg:flag=VALUE / --[no]//pkg:bool_flag (docs/CONFIGURATION.md).
Repository options: --registry=URL (file/http/https, repeatable), --ignore_dev_dependency
  --repository_disable_download[=BOOL] blocks uncached archive/overlay/patch acquisition.
  Verified caches, registry metadata and Git execution are unaffected (not an offline mode).
Native transition: --experimental-builtin-modules[=true|false] opts into genuine Bazel 9.3 built-ins.
Remote registries require explicit --lockfile_mode=off; other lockfile modes are unsupported.
External sources: local_path_override, registry local_path entries and verified archives.
Native rules: filegroup(name, srcs), genrule(name, srcs, outs, cmd)
Loading: local .bzl legacy macros, glob, package defaults and exports_files.
Analysis: custom rules/attributes/providers, declared files, write/run/run_shell actions.
No upstream fallback. Repository rules and module extensions have bounded native profiles.
No test command, complete repository/extension APIs, execution platforms,
remote execution, sandbox or full query language yet.
Builds execute trusted shell commands locally; this is not a security sandbox.
";

fn run() -> Result<(), String> {
    let mut argv = std::env::args_os();
    let path_to_binary = argv
        .next()
        .map(|arg| arg.to_string_lossy().into_owned())
        .unwrap_or_default();
    let args = argv.collect::<Vec<_>>();
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
    let line = bazelrc::split_command_line(args)?;
    let cache = line.cache_dir.clone().map(PathBuf::from);
    let command = line.command.clone().unwrap_or_else(|| "help".into());
    let rest = line.command_args.clone();
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
        "query" | "build" | "mod" => {}
        _ => {
            return Err(format!(
                "unsupported command or startup option {command:?}; run --help for the native subset"
            ));
        }
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let root = workspace::workspace_root(&cwd)?;
    let options = evaluate_rc(&line, &command, &cwd, &root, &path_to_binary)?;
    let mut jobs = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(32);
    let mut resolver_options = bazel_rs::module_graph::ResolverOptions::default();
    let mut repository_downloads_enabled = true;
    let mut configuration_options = bazel_rs::configuration::CommandLineOptions::default();
    let mut deferred = None;
    let rest = if command == "mod" {
        mod_arguments(&options)?
    } else {
        // Errors in Bazel's parse order (rc options, the command line, then
        // each --config expansion), values in priority order (an expansion
        // takes the place of its --config).
        // A command that analyzes in a MODULE.bazel workspace ignores
        // unrecognized options in its first parsing round and reports them
        // when it reparses with the main repository mapping, after the rc
        // notes (OptionsParserImpl.java:811-815, BlazeCommandDispatcher.java:738-755, 1022).
        let second_round = command == "build" && root.join("MODULE.bazel").exists();
        for (step, position) in &options.steps {
            let result = match step {
                bazelrc::Step::Fail(error) => Err(error.clone()),
                bazelrc::Step::Unit(unit) => NativeOptions::new(&command).parse(&command, unit),
            };
            match result {
                Err(error) if second_round && error.contains(" :: Unrecognized option: ") => {
                    deferred.get_or_insert((error, *position));
                }
                Err(error) => return Err(early_exit(&options, &error, *position)),
                Ok(()) => {}
            }
        }
        let mut native = NativeOptions::new(&command);
        if deferred.is_none() {
            for unit in &options.priority {
                native.parse(&command, unit)?;
            }
        }
        native.resolver.validate_registries()?;
        jobs = native.jobs.unwrap_or(jobs);
        resolver_options = native.resolver;
        repository_downloads_enabled = native.downloads;
        configuration_options = native.configuration;
        options.residue.iter().map(|arg| arg.text.clone()).collect()
    };
    announce(&options);
    if let Some((error, position)) = deferred {
        for warning in &options.warnings[..position.warnings.min(options.warnings.len())] {
            eprintln!("WARNING: {warning}");
        }
        return Err(error);
    }
    // Option parser warnings (deprecated options) once the native options
    // parsed (runtime/BlazeOptionHandler.java:536-539), after the rc notes.
    for warning in configuration_options.warnings() {
        eprintln!("WARNING: {warning}");
    }
    if command == "query" && rest.len() != 1 {
        return Err("query requires exactly one literal target pattern; query expressions are not implemented".into());
    }
    if command == "build" && rest.is_empty() {
        return Err("build requires at least one target".into());
    }
    // The command's arguments, for the Starlark options' `Did you mean` hint
    // (runtime/BlazeOptionHandler.java:428-440).
    let command_args = line.command_args.clone();
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
    // Bazel reads a workspace-root `platform_mappings` file by default
    // (PlatformOptions.java:172-187); platform mappings are not modelled.
    if command == "build" && root.join("platform_mappings").exists() {
        return Err(format!(
            "platform mappings are not implemented in the native profile (found {})",
            root.join("platform_mappings").display()
        ));
    }
    let current_package = current_package(&root, &cwd)?;
    let repositories = Arc::new(
        bazel_rs::repository::RepositorySet::with_options(root.clone(), resolver_options)
            .with_repository_downloads_enabled(repository_downloads_enabled)
            .with_source_cache(cache.join("repositories")),
    );
    repositories.validate_root_module()?;
    let mut engine = Engine::with_repositories(repositories.clone());
    if command == "build" {
        // Label-typed options convert with the main repository mapping
        // (CoreOptionConverters.convertOptionsLabel, :351-357).
        let labels = |text: &str| {
            repositories.parse(text, "", "").or_else(|error| {
                // `bazel_tools` is always visible from the main repository
                // (an implicit dependency of every module).
                let repository = text.trim_start_matches('@').split("//").next();
                if text.starts_with('@') && repository == Some("bazel_tools") {
                    bazel_rs::configuration::parse_canonical_option_label(text)
                } else {
                    Err(error)
                }
            })
        };
        let native = configuration_options.configuration(&labels)?;
        // Starlark build settings are parsed after the native options and
        // before target patterns (runtime/BlazeCommandDispatcher.java:765-776);
        // only values different from the defaults enter the configuration.
        let starlark = engine.starlark_options(
            configuration_options.starlark_args(),
            &command_args,
            &labels,
            &current_package,
        );
        // Loading option targets reports deprecation warnings, also before a
        // later option error.
        for warning in engine.take_warnings() {
            eprintln!("WARNING: {warning}");
        }
        let starlark = starlark?;
        engine.set_configuration(bazel_rs::configuration::Configuration::new(
            native.native,
            starlark,
        ));
    }
    let labels = if command == "query" {
        engine.select(&rest, &current_package)
    } else {
        engine.select_for_build(&rest, &current_package)
    };
    // Bazel reports pattern warnings as they happen, before a later error.
    for warning in engine.take_warnings() {
        eprintln!("WARNING: {warning}");
    }
    let labels = labels?;
    if command == "query" {
        // Print only after all packages have loaded successfully.
        for label in labels {
            println!("{label}");
        }
    } else {
        let result = engine.build_with_jobs(&labels, &cache, jobs);
        // Analysis warnings precede the result, also when the build fails.
        for warning in engine.take_warnings() {
            eprintln!("WARNING: {warning}");
        }
        let summary = result?;
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

/// The native command options, parsed by the existing option handlers.
struct NativeOptions {
    jobs: Option<usize>,
    resolver: bazel_rs::module_graph::ResolverOptions,
    downloads: bool,
    configuration: bazel_rs::configuration::CommandLineOptions,
}

impl NativeOptions {
    fn new(command: &str) -> Self {
        Self {
            jobs: None,
            resolver: bazel_rs::module_graph::ResolverOptions::default(),
            downloads: true,
            configuration: if command == "query" {
                // query is configuration-independent; it accepts (and validates)
                // the modelled CoreOptions (runtime/commands/QueryCommand.java:59-67).
                bazel_rs::configuration::CommandLineOptions::for_query()
            } else {
                bazel_rs::configuration::CommandLineOptions::default()
            },
        }
    }

    /// One option (and its separate value) of `build` or `query`.
    fn parse(&mut self, command: &str, unit: &bazelrc::Unit) -> Result<(), String> {
        let arg = unit.option.text.as_str();
        let next = unit.value.as_ref().map(|value| value.text.as_str());
        if unit.option.from_rc()
            && let Some(name) = bazelrc::native_only_option(arg)
        {
            return Err(bazelrc::native_only_error(
                name,
                &unit.option.origin_suffix(),
            ));
        }
        let used_next = if let Some(disabled) =
            bazel_rs::module_cli::repository_disable_download_argument(arg)?
        {
            self.downloads = !disabled;
            false
        } else if let Some(value) = bazel_rs::configuration::boolean_option(
            arg,
            "incompatible_disable_starlark_host_transitions",
        ) {
            // A Starlark semantics option of every loading command
            // (packages/semantics/BuildLanguageOptions.java:590-598).
            self.resolver.disable_starlark_host_transitions = value?;
            false
        } else if let Some(value) =
            bazel_rs::configuration::boolean_option(arg, "experimental_google_legacy_api")
        {
            // Starlark semantics options of every loading command
            // (packages/semantics/BuildLanguageOptions.java:313-321,
            // 559-567, 653-660), read by java_common's getters.
            self.resolver.java_semantics.google_legacy_api = value?;
            false
        } else if let Some(value) = bazel_rs::configuration::boolean_option(
            arg,
            "incompatible_java_info_merge_runtime_module_flags",
        ) {
            self.resolver
                .java_semantics
                .java_info_merge_runtime_module_flags = value?;
            false
        } else if let Some(value) = bazel_rs::configuration::boolean_option(
            arg,
            "incompatible_disable_non_executable_java_binary",
        ) {
            self.resolver
                .java_semantics
                .disable_non_executable_java_binary = value?;
            false
        } else if let Some(profile) = bazel_rs::module_cli::builtin_profile_argument(arg)? {
            self.resolver.builtin_profile = profile;
            false
        } else if arg == "--override_repository" || arg.starts_with("--override_repository=") {
            return Err(format!(
                "unsupported option --override_repository{}; MODULE overrides do not replace the built-in repository filesystem",
                unit.option.origin_suffix()
            ));
        } else if arg == "--registry" {
            let value = next.ok_or("--registry requires a URL")?;
            self.resolver
                .registries
                .push(bazel_rs::module_cli::registry_path(value)?);
            true
        } else if let Some(value) = arg.strip_prefix("--registry=") {
            self.resolver
                .registries
                .push(bazel_rs::module_cli::registry_path(value)?);
            false
        } else if arg == "--lockfile_mode" || arg.starts_with("--lockfile_mode=") {
            let (value, used) = match arg.strip_prefix("--lockfile_mode=") {
                Some(value) => (value, false),
                None => (next.ok_or("--lockfile_mode requires a value")?, true),
            };
            if value != "off" {
                return Err(format!(
                    "unsupported --lockfile_mode={value}; lockfile support is not implemented; use --lockfile_mode=off"
                ));
            }
            self.resolver.allow_remote = true;
            used
        } else if matches!(
            arg,
            "--ignore_dev_dependency"
                | "--ignore_dev_dependency=true"
                | "--noignore_dev_dependency=false"
        ) {
            self.resolver.ignore_dev_dependencies = true;
            false
        } else if matches!(
            arg,
            "--noignore_dev_dependency"
                | "--noignore_dev_dependency=true"
                | "--ignore_dev_dependency=false"
        ) {
            self.resolver.ignore_dev_dependencies = false;
            false
        } else if command == "build" && arg == "--jobs" {
            self.jobs = Some(parse_jobs(
                next.ok_or("--jobs requires a positive integer")?,
            )?);
            true
        } else if let Some(value) = arg.strip_prefix("--jobs=").filter(|_| command == "build") {
            self.jobs = Some(parse_jobs(value)?);
            false
        } else if let bazel_rs::configuration::Parsed::Consumed(used) =
            self.configuration.parse(arg, next)?
        {
            used
        } else {
            return Err(unmodelled_option(&unit.option));
        };
        if let (Some(value), false) = (next, used_next) {
            return Err(format!(
                "internal error: {arg} took the separate value {value:?} but its native parser did not consume it"
            ));
        }
        Ok(())
    }
}

/// An option argument this profile does not model: Bazel's `Unrecognized
/// option` when it is certainly not a Bazel option, otherwise an explicit
/// unsupported-option error naming the rc file:line it came from (options
/// are never silently ignored).
fn unmodelled_option(arg: &bazelrc::Arg) -> String {
    let text = arg.text.as_str();
    let suffix = arg.origin_suffix();
    // Never echo a repository environment value (it may carry a secret).
    if text == "--repo_env" || text.starts_with("--repo_env=") {
        return format!(
            "unsupported option --repo_env{suffix}; repository environment overrides are not implemented"
        );
    }
    bazel_rs::configuration::unrecognized_option(text).unwrap_or_else(|| {
        format!("unsupported option {text:?}{suffix}; options are never silently ignored")
    })
}

/// The `mod` arguments: the residue's subcommand, the options in priority
/// order and the remaining residue, for crate::module_cli. Options from rc
/// files that the native `mod graph` profile does not model fail here with
/// their rc file:line.
fn mod_arguments(options: &bazelrc::Expanded) -> Result<Vec<String>, String> {
    for (step, position) in &options.steps {
        let error = match step {
            bazelrc::Step::Fail(error) => error.clone(),
            bazelrc::Step::Unit(unit)
                if let Some(name) = bazelrc::native_only_option(&unit.option.text)
                    && unit.option.from_rc() =>
            {
                bazelrc::native_only_error(name, &unit.option.origin_suffix())
            }
            bazelrc::Step::Unit(unit)
                if unit.option.from_rc()
                    && !bazel_rs::module_cli::models_graph_option(&unit.option.text) =>
            {
                unmodelled_option(&unit.option)
            }
            bazelrc::Step::Unit(_) => continue,
        };
        return Err(early_exit(options, &error, *position));
    }
    let mut residue = options.residue.iter().map(|arg| arg.text.clone());
    let mut args = residue.next().into_iter().collect::<Vec<_>>();
    for unit in &options.priority {
        args.push(unit.option.text.clone());
        args.extend(unit.value.iter().map(|value| value.text.clone()));
    }
    args.extend(residue);
    Ok(args)
}

/// The client's rc evaluation (OptionProcessor::ParseOptions,
/// src/main/cpp/option_processor.cc:523-564) and the server's rc structuring
/// and --config expansion for `command` (crate::bazelrc).
fn evaluate_rc(
    line: &bazelrc::CommandLine,
    command: &str,
    cwd: &Path,
    root: &Path,
    path_to_binary: &str,
) -> Result<bazelrc::Expanded, String> {
    let utf8 = |path: &Path| {
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("non-UTF-8 path {} is not supported", path.display()))
    };
    let var = |name: &str| std::env::var(name).unwrap_or_default();
    let env = bazelrc::ClientEnv {
        cwd: utf8(cwd)?,
        workspace: utf8(root)?,
        home: var("HOME"),
        bazelrc_env: var("BAZELRC"),
        path_to_binary: path_to_binary.to_owned(),
        system_rc: bazelrc::SYSTEM_BAZELRC.to_owned(),
        isatty: is_standard_terminal(),
        terminal_columns: terminal_columns(),
        emacs: is_emacs_terminal(),
    };
    let mut warnings = Vec::new();
    let rc_files = if bazelrc::search_nullary(&line.startup_args, "ignore_all_rc_files", false) {
        Vec::new()
    } else {
        let read = bazelrc::read_rc_files(line, &env, &mut warnings);
        // The client logs these as it reads the files.
        for warning in warnings.drain(..) {
            eprintln!("WARNING: {warning}");
        }
        read?
    };
    let mut provenance = Vec::new();
    if let Err(error) =
        bazelrc::process_startup_options(&rc_files, line, &mut warnings, &mut provenance)
    {
        for note in provenance {
            eprintln!("INFO: {note}");
        }
        return Err(error);
    }
    for warning in warnings {
        eprintln!("WARNING: {warning}");
    }
    let blazerc = bazelrc::blazerc_options(&env, &rc_files);
    Ok(bazelrc::expand(
        command,
        &blazerc,
        &line.command_args,
        "linux",
    ))
}

/// An option error: the rc notes containing the invalid argument and the
/// stored warnings, then the error (BlazeCommandDispatcher.replayEarlyExitEvents,
/// runtime/BlazeCommandDispatcher.java:923-948).
fn early_exit(options: &bazelrc::Expanded, error: &str, position: bazelrc::Position) -> String {
    for note in options.early_exit_notes(error, position) {
        eprintln!("INFO: {note}");
    }
    for warning in &options.warnings[..position.warnings.min(options.warnings.len())] {
        eprintln!("WARNING: {warning}");
    }
    error.to_owned()
}

/// Stored warnings, then the rc notes under --announce_rc
/// (runtime/BlazeCommandDispatcher.java:578-624).
fn announce(options: &bazelrc::Expanded) {
    for warning in &options.warnings {
        eprintln!("WARNING: {warning}");
    }
    if options.announce {
        for note in options.all_notes() {
            eprintln!("INFO: {note}");
        }
    }
}

/// IsEmacsTerminal (src/main/cpp/blaze_util_posix.cc:803-810).
fn is_emacs_terminal() -> bool {
    std::env::var_os("EMACS").is_some_and(|value| value == "t")
        || std::env::var_os("INSIDE_EMACS").is_some()
}

/// IsStandardTerminal (src/main/cpp/blaze_util_posix.cc:812-828).
fn is_standard_terminal() -> bool {
    use std::io::IsTerminal;
    let term = std::env::var("TERM").unwrap_or_default();
    let emacs = is_emacs_terminal();
    if emacs && term == "eterm-color" {
        return true;
    }
    if term.is_empty()
        || ["dumb", "emacs", "xterm-mono", "symbolics", "9term"].contains(&term.as_str())
        || emacs
    {
        return false;
    }
    std::io::stdout().is_terminal() && std::io::stderr().is_terminal()
}

/// GetTerminalColumns (src/main/cpp/blaze_util_posix.cc:830-844) without the
/// TIOCGWINSZ ioctl (this crate forbids unsafe code): `$COLUMNS`, else 80.
fn terminal_columns() -> i64 {
    std::env::var("COLUMNS")
        .ok()
        .filter(|value| !value.is_empty())
        .and_then(|value| value.trim_start().parse::<i64>().ok())
        .unwrap_or(80)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("bazel-rs: {error}");
        std::process::exit(1);
    }
}
