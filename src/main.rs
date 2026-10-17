use bazel_rs::{bazelrc, engine::Engine, workspace};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const HELP: &str = "bazel-rs — experimental native build engine (NOT full Bazel parity)

Usage:
  bazel-rs [STARTUP...] [--cache-dir=PATH] query [QUERY-OPTION...] EXPRESSION
  bazel-rs [STARTUP...] [--cache-dir=PATH] build [--jobs=N] [CONFIG...] TARGET...
  bazel-rs [STARTUP...] mod graph --output=json [--registry=URL]
  bazel-rs version | --version | capabilities

Startup: --[no]system_rc --[no]workspace_rc --[no]home_rc --[no]ignore_all_rc_files
  --bazelrc=FILE; rc files are evaluated like Bazel 9.3 (--config, common/always,
  --announce_rc; docs/BAZELRC.md); unmodelled rc options fail naming file:line.
Patterns: //..., //package/..., //package:all, //package:*, //package:target, :target
Query: the Bazel 9.3 query language (deps, rdeps, allpaths, somepath, attr, filter,
  kind, labels, tests, buildfiles, loadfiles, siblings, some, visible, set, let,
  same_pkg_direct_rdeps, executables, union/+, intersect/^, except/-) with
  --output=label|label_kind|build|package|location|minrank|maxrank|graph,
  --order_output, --[no]implicit_deps, --[no]tool_deps, --[no]keep_going (docs/QUERY.md).
Build options: --jobs=N or --jobs N (positive integer; default: available CPUs, max 32)
Configuration (build): -c/--compilation_mode, --[no]stamp, --strip, --[no]force_pic,
  --define NAME=VALUE, --platforms, --host_platform, Starlark build settings
  --//pkg:flag=VALUE / --[no]//pkg:bool_flag (docs/CONFIGURATION.md).
Repository options: --registry=URL (file/http/https, repeatable; default https://bcr.bazel.build/),
  --ignore_dev_dependency
  --repository_disable_download[=BOOL] blocks uncached archive/overlay/patch acquisition.
  Verified caches, registry metadata and Git execution are unaffected (not an offline mode).
Native transition: --experimental-builtin-modules[=true|false] opts into genuine Bazel 9.3 built-ins.
Lockfile: --lockfile_mode=update (default)|refresh|error|off reads/writes MODULE.bazel.lock
  (registryFileHashes, selectedYankedVersions; extension sections kept verbatim, never added;
  docs/REGISTRY_LOCKFILE.md). --allow_yanked_versions=MODULE@VERSION,... (repeatable).
  Native-only --native-lockfile=workspace|readonly|/abs/path: readonly never writes a lockfile;
  a path is a private lockfile read instead of (seeded from) the workspace one and written instead.
  mod graph still requires --lockfile_mode=off for remote registries, including the default.
  Fast warm probes of real workspaces: --native-lockfile=/private/MODULE.bazel.lock.
External sources: local_path_override, registry local_path entries and verified archives.
Native rules: filegroup(name, srcs), genrule(name, srcs, outs, cmd)
Loading: local .bzl legacy macros, glob, package defaults and exports_files.
Analysis: custom rules/attributes/providers, declared files, write/run/run_shell actions.
No upstream fallback. Repository rules and module extensions have bounded native profiles.
No test command, complete repository/extension APIs, execution platforms,
remote execution, sandbox, SkyQuery or xml/proto query output yet.
Builds execute trusted shell commands locally; this is not a security sandbox.
";

fn run() -> Result<(), String> {
    let mut argv = std::env::args_os();
    let path_to_binary = argv
        .next()
        .map(|arg| arg.to_string_lossy().into_owned())
        .unwrap_or_default();
    let args = argv.collect::<Vec<_>>();
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
    let mut query_options = bazel_rs::engine::query::QueryOptions::default();
    let mut lockfile_mode = bazel_rs::lockfile::LockfileMode::default();
    let mut lockfile_target = bazel_rs::registry_session::LockfileTarget::Workspace;
    let mut allowed_yanked_flags = Vec::new();
    let mut raw_registries = Vec::new();
    let mut credential_options = bazel_rs::credentials::CredentialOptions::default();
    let mut remote_options = bazel_rs::remote_cache::RemoteOptions::default();
    let mut deferred = None;
    let rc_options;
    let rest = if command == "mod" {
        let (rest, rc) = mod_arguments(&options, &mut credential_options, &mut remote_options)?;
        rc_options = rc;
        rest
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
        if native.resolver.registries.is_empty() {
            // Bazel's default registry list (N37); any explicit --registry
            // replaces it.
            native
                .resolver
                .registries
                .push(bazel_rs::module_cli::registry_path(
                    bazel_rs::registry::DEFAULT_REGISTRY,
                )?);
            native
                .raw_registries
                .push(bazel_rs::registry::DEFAULT_REGISTRY.to_owned());
        }
        native.resolver.validate_registries()?;
        if native.lockfile_mode != bazel_rs::lockfile::LockfileMode::Off {
            for (raw, registry) in native
                .raw_registries
                .iter()
                .zip(&native.resolver.registries)
            {
                registry.check_lockfile_spelling(raw)?;
            }
        }
        lockfile_mode = native.lockfile_mode;
        lockfile_target = native.lockfile_target;
        allowed_yanked_flags = native.allowed_yanked_flags;
        raw_registries = native.raw_registries;
        jobs = native.jobs.unwrap_or(jobs);
        native.resolver.isolated_extension_usages = native.rc.isolated_extension_usages;
        resolver_options = native.resolver;
        repository_downloads_enabled = native.downloads;
        configuration_options = native.configuration;
        query_options = native.query;
        rc_options = native.rc;
        credential_options = native.credentials;
        remote_options = native.remote;
        options.residue.iter().map(|arg| arg.text.clone()).collect()
    };
    // The repository environment (client environment with --repo_env,
    // CommandEnvironment.java:342-416) and the downloader's proxies over it
    // (crate::download_proxy), fixed for the whole command.
    let repository_environment = bazel_rs::repo_env::repository_environment(
        std::env::vars_os(),
        &rc_options.repo_env,
        root.to_str(),
    );
    bazel_rs::download_proxy::install(
        bazel_rs::download_proxy::ProxyEnvironment::from_environment(
            repository_environment
                .iter()
                .map(|(name, value)| (name.as_os_str(), value.as_os_str())),
        ),
    )?;
    let cache = cache.unwrap_or_else(|| root.join("bazel-rs-out"));
    let cache = if cache.is_absolute() {
        cache
    } else {
        cwd.join(cache)
    };
    // --attempt_to_print_relative_paths (LocationPrinter, crate::location_printer).
    let _ = PRINTER.set(bazel_rs::location_printer::LocationPrinter::new(
        rc_options.attempt_to_print_relative_paths,
        root.clone(),
        cache.clone(),
    ));
    announce(&options);
    if let Some((error, position)) = deferred {
        for warning in &options.warnings[..position.warnings.min(options.warnings.len())] {
            warn(warning);
        }
        return Err(error);
    }
    // Credential helpers and the remote cache (N41) for every command.
    let remote_enabled = setup_credentials_and_remote(&root, &credential_options, &remote_options)?;
    // Option parser warnings (deprecated options) once the native options
    // parsed (runtime/BlazeOptionHandler.java:536-539), after the rc notes.
    for warning in configuration_options.warnings() {
        warn(warning);
    }
    // QueryOptionHelper.readQuery (runtime/commands/QueryOptionHelper.java:
    // 41-70), before any loading: the residue joined by spaces is the query
    // expression (else --query_file). Its failures ("missing query
    // expression. Type 'bazel help query' for syntax and help", ...) are the
    // command's failure detail without an event: exit 2, silent, like
    // genuine Bazel 9.3 (tests/fixtures/query_language, error.no_expression).
    let rest = if command == "query" {
        match bazel_rs::engine::query::read_query(&rest, &query_options) {
            Some(query) => vec![query],
            None => std::process::exit(2),
        }
    } else {
        rest
    };
    if command == "build" && rest.is_empty() {
        return Err("build requires at least one target".into());
    }
    // The command's arguments, for the Starlark options' `Did you mean` hint
    // (runtime/BlazeOptionHandler.java:428-440).
    let command_args = line.command_args.clone();
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
    // BazelLockFileFunction/RegistryFunction: one registry session per
    // invocation; BazelLockFileModule.afterCommand writes the lockfile
    // after the command, also when it failed after resolution.
    let allowed_yanked = bazel_rs::registry_session::parse_allowed_yanked(
        std::env::var("BZLMOD_ALLOW_YANKED_VERSIONS")
            .ok()
            .as_deref(),
        &allowed_yanked_flags,
    )?;
    let session = bazel_rs::registry_session::SessionHandle::new(
        bazel_rs::registry_session::RegistrySession::open(
            &root,
            lockfile_mode,
            &lockfile_target,
            Some(cache.join("repositories")),
            allowed_yanked,
        )?
        .with_spellings(
            resolver_options
                .registries
                .iter()
                .cloned()
                .zip(raw_registries.iter().cloned()),
        ),
    );
    resolver_options.registry_session = session.clone();
    let _lockfile_writer = LockfileWriter(session);
    let repositories = Arc::new(
        bazel_rs::repository::RepositorySet::with_options(root.clone(), resolver_options)
            .with_repository_downloads_enabled(repository_downloads_enabled)
            .with_source_cache(cache.join("repositories"))
            .with_repository_environment(repository_environment),
    );
    repositories.validate_root_module()?;
    // Bazel computes the main repository mapping, hence module resolution,
    // in every command; the lockfile records that resolution's remote
    // registry files. With a lockfile and a remote --registry, resolve
    // eagerly like Bazel; otherwise keep the native lazy profile
    // (NATIVE_BOUNDARY, docs/REGISTRY_LOCKFILE.md).
    if lockfile_mode != bazel_rs::lockfile::LockfileMode::Off
        && raw_registries
            .iter()
            .any(|raw| raw.starts_with("http://") || raw.starts_with("https://"))
        && root.join("MODULE.bazel").is_file()
    {
        repositories.resolve_graph()?;
    }
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
            warn(&warning);
        }
        let starlark = starlark?;
        engine.set_configuration(bazel_rs::configuration::Configuration::new(
            native.native,
            starlark,
        ));
    }
    // TargetPattern.Parser's relativeDirectory is the working directory below
    // the workspace root, whether or not it is a package
    // (cmdline/TargetPattern.java:783-789).
    let relative_directory = relative_directory(&root, &cwd)?;
    if command == "query" {
        let outcome = engine.query(&rest[0], &query_options, &relative_directory);
        // Pattern warnings precede the query's own events.
        for warning in engine.take_warnings() {
            warn(&warning);
        }
        let outcome = outcome?;
        for event in &outcome.events {
            eprintln!("{event}");
        }
        if let Some(fatal) = outcome.fatal {
            return Err(fatal);
        }
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&outcome.stdout)
            .and_then(|()| stdout.flush())
            .map_err(|e| format!("I/O error: {e}"))?;
        if outcome.exit_code != 0 {
            std::process::exit(outcome.exit_code);
        }
        return Ok(());
    }
    let labels = engine.select_for_build(&rest, &relative_directory);
    // Bazel reports pattern warnings as they happen, before a later error.
    for warning in engine.take_warnings() {
        warn(&warning);
    }
    let labels = labels?;
    let result = engine.build_with_jobs(&labels, &cache, jobs);
    // Analysis warnings precede the result, also when the build fails.
    for warning in engine.take_warnings() {
        warn(&warning);
    }
    let summary = result?;
    for (label, output) in summary.outputs {
        // The path's raw bytes: a lone-byte target name names the byte
        // itself (bazel_bytes::internal_path), not U+FFFD.
        use std::io::Write as _;
        use std::os::unix::ffi::OsStrExt as _;
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(format!("{label}\t").as_bytes())
            .and_then(|()| stdout.write_all(output.as_os_str().as_bytes()))
            .and_then(|()| stdout.write_all(b"\n"))
            .map_err(|error| error.to_string())?;
    }
    if remote_enabled {
        eprintln!(
            "bazel-rs: {} actions executed, {} cache hits, {} remote cache hits (experimental local execution; not sandboxed)",
            summary.executed, summary.cache_hits, summary.remote_hits
        );
    } else {
        eprintln!(
            "bazel-rs: {} actions executed, {} cache hits (experimental local execution; not sandboxed)",
            summary.executed, summary.cache_hits
        );
    }
    Ok(())
}

/// Writes the updated lockfile when the command ends (success or error).
struct LockfileWriter(bazel_rs::registry_session::SessionHandle);

impl Drop for LockfileWriter {
    fn drop(&mut self) {
        if let Some(warning) = self.0.session().and_then(|session| session.finish()) {
            warn(&warning);
        }
    }
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

fn relative_directory(root: &Path, cwd: &Path) -> Result<String, String> {
    let cwd = std::fs::canonicalize(cwd).map_err(|e| e.to_string())?;
    cwd.strip_prefix(root)
        .map_err(|e| e.to_string())?
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| "non-UTF-8 working directories are not supported".into())
}

/// The native command options, parsed by the existing option handlers.
struct NativeOptions {
    jobs: Option<usize>,
    /// The N40 rc options outside the configuration (crate::rc_options).
    rc: bazel_rs::rc_options::RcOptions,
    resolver: bazel_rs::module_graph::ResolverOptions,
    downloads: bool,
    configuration: bazel_rs::configuration::CommandLineOptions,
    /// QueryCommand's own options (QueryOptions/CommonQueryOptions).
    query: bazel_rs::engine::query::QueryOptions,
    /// --lockfile_mode (N37, docs/REGISTRY_LOCKFILE.md).
    lockfile_mode: bazel_rs::lockfile::LockfileMode,
    /// Native-only --native-lockfile.
    lockfile_target: bazel_rs::registry_session::LockfileTarget,
    /// --allow_yanked_versions values in order.
    allowed_yanked_flags: Vec<String>,
    /// --registry values as given (lockfile keys use Bazel's spelling).
    raw_registries: Vec<String>,
    /// AuthAndTLSOptions credential helpers (N41, crate::credentials).
    credentials: bazel_rs::credentials::CredentialOptions,
    /// The modelled RemoteOptions (N41, crate::remote_cache).
    remote: bazel_rs::remote_cache::RemoteOptions,
}

impl NativeOptions {
    fn new(command: &str) -> Self {
        Self {
            jobs: None,
            rc: bazel_rs::rc_options::RcOptions::default(),
            resolver: bazel_rs::module_graph::ResolverOptions {
                // Every lockfile mode supports remote registries (N37).
                allow_remote: matches!(command, "build" | "query"),
                ..Default::default()
            },
            downloads: true,
            credentials: Default::default(),
            remote: Default::default(),
            configuration: if command == "query" {
                // query is configuration-independent; it accepts (and validates)
                // the modelled CoreOptions (runtime/commands/QueryCommand.java:59-67).
                bazel_rs::configuration::CommandLineOptions::for_query()
            } else {
                bazel_rs::configuration::CommandLineOptions::default()
            },
            query: bazel_rs::engine::query::QueryOptions::default(),
            lockfile_mode: bazel_rs::lockfile::LockfileMode::default(),
            lockfile_target: bazel_rs::registry_session::LockfileTarget::Workspace,
            allowed_yanked_flags: Vec::new(),
            raw_registries: Vec::new(),
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
        let query = if command == "query" {
            self.query.parse(arg, next)?
        } else {
            None
        };
        let used_next = if let Some(used) = query {
            used
        } else if let Some(used) =
            self.rc
                .parse(command, arg, next, &unit.option.origin_suffix())?
        {
            used
        } else if let Some(disabled) =
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
            self.raw_registries.push(value.to_owned());
            true
        } else if let Some(value) = arg.strip_prefix("--registry=") {
            self.resolver
                .registries
                .push(bazel_rs::module_cli::registry_path(value)?);
            self.raw_registries.push(value.to_owned());
            false
        } else if arg == "--lockfile_mode" || arg.starts_with("--lockfile_mode=") {
            let (value, used) = match arg.strip_prefix("--lockfile_mode=") {
                Some(value) => (value, false),
                None => (next.ok_or("--lockfile_mode requires a value")?, true),
            };
            self.lockfile_mode = bazel_rs::lockfile::LockfileMode::parse(value)?;
            used
        } else if let Some(value) = arg.strip_prefix("--native-lockfile=") {
            self.lockfile_target = bazel_rs::registry_session::LockfileTarget::parse(value)?;
            false
        } else if arg == "--allow_yanked_versions" {
            self.allowed_yanked_flags.push(
                next.ok_or("--allow_yanked_versions requires a value")?
                    .to_owned(),
            );
            true
        } else if let Some(value) = arg.strip_prefix("--allow_yanked_versions=") {
            self.allowed_yanked_flags.push(value.to_owned());
            false
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
        } else if let Some(used) = self.credentials.parse(arg, next) {
            used?
        } else if let Some(used) = self.remote.parse(arg, next) {
            used?
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
    bazel_rs::configuration::unrecognized_option(text).unwrap_or_else(|| {
        format!("unsupported option {text:?}{suffix}; options are never silently ignored")
    })
}

/// The `mod` arguments: the residue's subcommand, the options in priority
/// order and the remaining residue, for crate::module_cli. Options from rc
/// files that the native `mod graph` profile does not model fail here with
/// their rc file:line.
fn mod_arguments(
    options: &bazelrc::Expanded,
    credentials: &mut bazel_rs::credentials::CredentialOptions,
    remote: &mut bazel_rs::remote_cache::RemoteOptions,
) -> Result<(Vec<String>, bazel_rs::rc_options::RcOptions), String> {
    // The credential helper and remote options of every command (AuthAndTLSOptions,
    // RemoteOptions; N41) are consumed here: errors in parse order, values in
    // priority order.
    let ours = |unit: &bazelrc::Unit,
                credentials: &mut bazel_rs::credentials::CredentialOptions,
                remote: &mut bazel_rs::remote_cache::RemoteOptions|
     -> Option<Result<bool, String>> {
        let arg = unit.option.text.as_str();
        let next = unit.value.as_ref().map(|value| value.text.as_str());
        credentials
            .parse(arg, next)
            .or_else(|| remote.parse(arg, next))
    };
    let rc_option = |rc: &mut bazel_rs::rc_options::RcOptions, unit: &bazelrc::Unit| {
        rc.parse(
            "mod",
            &unit.option.text,
            unit.value.as_ref().map(|value| value.text.as_str()),
            &unit.option.origin_suffix(),
        )
    };
    for (step, position) in &options.steps {
        let error = match step {
            bazelrc::Step::Fail(error) => error.clone(),
            bazelrc::Step::Unit(unit)
                if let Some(name) = bazelrc::native_only_option(&unit.option.text)
                    && unit.option.from_rc() =>
            {
                bazelrc::native_only_error(name, &unit.option.origin_suffix())
            }
            // The N40 options (crate::rc_options), then the N41 credential
            // helper and remote options, from rc files and the command line
            // alike.
            bazelrc::Step::Unit(unit) => match rc_option(&mut Default::default(), unit) {
                Err(error) => error,
                Ok(Some(_)) => continue,
                Ok(None) => match ours(unit, &mut Default::default(), &mut Default::default()) {
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => error,
                    None if unit.option.from_rc()
                        && !bazel_rs::module_cli::models_graph_option(&unit.option.text) =>
                    {
                        unmodelled_option(&unit.option)
                    }
                    None => continue,
                },
            },
        };
        return Err(early_exit(options, &error, *position));
    }
    let mut rc = bazel_rs::rc_options::RcOptions::default();
    let mut residue = options.residue.iter().map(|arg| arg.text.clone());
    let mut args = residue.next().into_iter().collect::<Vec<_>>();
    for unit in &options.priority {
        if rc_option(&mut rc, unit)?.is_some() {
            continue;
        }
        if let Some(result) = ours(unit, credentials, remote) {
            result?;
            continue;
        }
        args.push(unit.option.text.clone());
        args.extend(unit.value.iter().map(|value| value.text.clone()));
    }
    // MODULE.bazel evaluation semantics for crate::module_cli.
    args.push(format!(
        "--experimental_isolated_extension_usages={}",
        rc.isolated_extension_usages
    ));
    args.extend(residue);
    Ok((args, rc))
}

/// Credential helpers and the remote cache (CredentialModule,
/// BazelRepositoryModule.beforeCommand and RemoteModule.setup; N41,
/// docs/REMOTE_CACHE.md): helper paths resolve eagerly for every command; an
/// http(s) cache is set up for every command too (RemoteOptions is a common
/// option class), so an http:// cache invokes the helper at startup.
/// Returns whether a remote cache is active.
fn setup_credentials_and_remote(
    root: &Path,
    credential_options: &bazel_rs::credentials::CredentialOptions,
    remote_options: &bazel_rs::remote_cache::RemoteOptions,
) -> Result<bool, String> {
    let path = std::env::var("PATH").unwrap_or_default();
    let http_cache = {
        let lower = remote_options.cache.to_ascii_lowercase();
        remote_options.executor.is_empty()
            && (lower.starts_with("http://") || lower.starts_with("https://"))
    };
    let credentials =
        match bazel_rs::credentials::Credentials::new(credential_options, Some(root), &path) {
            Ok(credentials) => credentials,
            Err(error) => {
                // Both modules report the path error; the last exit wins
                // (BazelRepositoryModule.java:446-477, RemoteModule.java:604-621).
                eprintln!("ERROR: {error}");
                if http_cache {
                    eprintln!("ERROR: {error}");
                    return Err("Error initializing RemoteModule".into());
                }
                return Err("Error initializing credential helper".into());
            }
        };
    let remote = match bazel_rs::remote_cache::setup(remote_options, Some(&credentials)) {
        Ok(remote) => remote,
        Err(error) => {
            for line in &error.errors {
                eprintln!("ERROR: {line}");
            }
            return Err(error.exit);
        }
    };
    bazel_rs::credentials::install(credentials);
    let enabled = remote.is_some();
    if let Some(remote) = remote {
        bazel_rs::remote_cache::install(remote);
    }
    Ok(enabled)
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
            warn(&warning);
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
        warn(&warning);
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
        warn(warning);
    }
    error.to_owned()
}

/// Stored warnings, then the rc notes under --announce_rc
/// (runtime/BlazeCommandDispatcher.java:578-624).
fn announce(options: &bazelrc::Expanded) {
    for warning in &options.warnings {
        warn(warning);
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

/// The message printer of this command (--attempt_to_print_relative_paths),
/// set once the command options are parsed.
static PRINTER: std::sync::OnceLock<bazel_rs::location_printer::LocationPrinter> =
    std::sync::OnceLock::new();

fn located(message: &str) -> std::borrow::Cow<'_, str> {
    match PRINTER.get() {
        Some(printer) => printer.print(message),
        None => std::borrow::Cow::Borrowed(message),
    }
}

fn warn(warning: &str) {
    eprintln!("WARNING: {}", located(warning));
}

fn main() {
    if let Err(error) = run() {
        let error = located(&error);
        eprintln!("bazel-rs: {error}");
        std::process::exit(1);
    }
}
