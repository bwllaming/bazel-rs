use bazel_rs::{bazelrc, engine::Engine, workspace};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const HELP: &str = "bazel-rs — experimental native build engine (NOT full Bazel parity)

Usage:
  bazel-rs [STARTUP...] [--cache-dir=PATH] query [QUERY-OPTION...] EXPRESSION
  bazel-rs [STARTUP...] [--cache-dir=PATH] build [--jobs=N] [CONFIG...] TARGET...
  bazel-rs [STARTUP...] [--cache-dir=PATH] run [BUILD OPTIONS...] [RUN OPTIONS...] TARGET [-- ARGS...]
  bazel-rs [STARTUP...] [--cache-dir=PATH] test [BUILD OPTIONS...] [TEST OPTIONS...] TARGET...
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
Memory ceiling: --memory-limit=<MB> (build/run/test/query) or BAZEL_RS_MAX_MEMORY_MB
  caps bazel-rs's own RSS (watchdog only, exit 33 = Bazel's OOM; no RLIMIT_AS, so
  child actions keep host limits); flag wins over env; unset = sliding-scale default
  (70% of RAM below 32 GB, 50% from 32-128 GB, flat 64 GB at/above 128 GB).
  Prefer the env var: --memory-limit in a shared .bazelrc breaks genuine Bazel
  (docs/REPOSITORY_WARM_PERF.md).
Repository verify: --verify-repo-tampering forces full re-hash of warm
  repository publications (default: stat-fingerprint trust, Bazel's model;
  env BAZEL_RS_VERIFY_REPOS=full is equivalent).
Runfiles (build/run): --[no]build_runfile_links, --[no]build_runfile_manifests,
  --enable_runfiles=auto|yes|no, --[no]incompatible_compact_repo_mapping_manifest
  (runfiles trees, MANIFEST and _repo_mapping like Bazel 9.3; docs/RUNFILES_TREES.md).
Run options: --script_path=PATH, --[no]run, --run_env=NAME[=VALUE]|=NAME, --[no]run_in_cwd,
  --[no]omit_run_args, --run_under=COMMAND-PREFIX, --shell_executable=PATH.
Configuration (build): -c/--compilation_mode, --[no]stamp, --strip, --[no]force_pic,
  --define NAME=VALUE, --platforms, --host_platform, Starlark build settings
  --//pkg:flag=VALUE / --[no]//pkg:bool_flag (docs/CONFIGURATION.md).
Repository options: --registry=URL (file/http/https, repeatable; default https://bcr.bazel.build/),
  --ignore_dev_dependency
  --repository_disable_download[=BOOL] blocks uncached archive/overlay/patch acquisition.
  Verified caches, registry metadata and Git execution are unaffected (not an offline mode).
Native transition: --experimental-builtin-modules[=true|false] opts into genuine Bazel 9.3 built-ins.
Lockfile: --lockfile_mode=update (default)|refresh|error|off reads/writes MODULE.bazel.lock
  (registryFileHashes, selectedYankedVersions; moduleExtensions/facts entries computed,
  reused and written with Bazel's digests since N52; docs/REGISTRY_LOCKFILE.md).
  --allow_yanked_versions=MODULE@VERSION,... (repeatable).
  Native-only --native-lockfile=workspace|readonly|/abs/path: readonly never writes a lockfile
  (the hidden one included); a path is a private lockfile read instead of (seeded from) the
  workspace one and written instead.
  mod graph still requires --lockfile_mode=off for remote registries, including the default.
  Fast warm probes of real workspaces: --native-lockfile=/private/MODULE.bazel.lock.
External sources: local_path_override, registry local_path entries and verified archives.
Native rules: filegroup(name, srcs), genrule(name, srcs, outs, cmd)
Loading: local .bzl legacy macros, glob, package defaults and exports_files.
Analysis: custom rules/attributes/providers, declared files, write/run/run_shell actions.
No upstream fallback. Repository rules and module extensions have bounded native profiles.
Test: bazel-rs test [BUILD OPTIONS...] [TEST OPTIONS...] TARGET... (local TestRunnerAction semantics;
  docs/TEST_COMMAND.md); run also runs test targets. No coverage, complete repository/extension APIs, execution platforms,
remote execution, sandbox, SkyQuery or xml/proto query output yet.
Builds execute trusted shell commands locally; this is not a security sandbox.
";

fn run() -> Result<(), String> {
    bazel_rs::perf_mark("main.start");
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
    // The sandbox worker (crate::sandbox): the executor starts this binary
    // with the hidden `__sandbox-worker` subcommand where Bazel starts its
    // linux-sandbox tool. Dispatched before anything else: the worker is not
    // a bazel command and shares none of the client state.
    if args
        .first()
        .is_some_and(|arg| arg == bazel_rs::sandbox::WORKER_ARG)
    {
        std::process::exit(bazel_rs::sandbox::worker_main(&args[1..]));
    }
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
        "query" | "build" | "mod" | "run" | "test" => {}
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
    let mut loading_phase_threads: Option<usize> = None;
    let mut memory_limit_flag_mb: Option<u64> = None;
    let mut resolver_options = bazel_rs::module_graph::ResolverOptions::default();
    let mut repository_downloads_enabled = true;
    let mut configuration_options = bazel_rs::configuration::CommandLineOptions::default();
    let mut query_options = bazel_rs::engine::query::QueryOptions::default();
    let mut lockfile_mode = bazel_rs::lockfile::LockfileMode::default();
    let mut lockfile_target = bazel_rs::registry_session::LockfileTarget::Workspace;
    let mut action_dump: Option<PathBuf> = None;
    let mut allowed_yanked_flags = Vec::new();
    let mut raw_registries = Vec::new();
    let mut credential_options = bazel_rs::credentials::CredentialOptions::default();
    let mut remote_options = bazel_rs::remote_cache::RemoteOptions::default();
    let mut spawn_options = bazel_rs::sandbox::SpawnOptions::default();
    let mut runfiles_options = bazel_rs::runfiles_tree::RunfilesOptions::default();
    let mut workspace_status = bazel_rs::workspace_status::WorkspaceStatusOptions::default();
    let mut no_build = false;
    let mut output_selection = bazel_rs::output_group_info::OutputSelection::default();
    let mut aspects = Vec::new();
    let mut aspects_parameters = Vec::new();
    let mut show_result = 1usize;
    let mut run_options = bazel_rs::run_command::RunOptions::default();
    let mut test_options = bazel_rs::test_command::TestOptions::default();
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
        let second_round = builds(&command) && root.join("MODULE.bazel").exists();
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
        memory_limit_flag_mb = native.memory_limit_mb;
        lockfile_mode = native.lockfile_mode;
        lockfile_target = native.lockfile_target;
        action_dump = native.action_dump;
        allowed_yanked_flags = native.allowed_yanked_flags;
        raw_registries = native.raw_registries;
        jobs = native.jobs.unwrap_or(jobs);
        loading_phase_threads = native.loading_phase_threads;
        native.resolver.isolated_extension_usages = native.rc.isolated_extension_usages;
        resolver_options = native.resolver;
        repository_downloads_enabled = native.downloads;
        configuration_options = native.configuration;
        query_options = native.query;
        rc_options = native.rc;
        credential_options = native.credentials;
        remote_options = native.remote;
        spawn_options = native.spawn;
        runfiles_options = native.runfiles;
        workspace_status = native.workspace_status;
        no_build = native.no_build;
        output_selection = native.output.clone();
        aspects = native.aspects;
        aspects_parameters = native.aspects_parameters;
        show_result = native.show_result;
        run_options = native.run;
        test_options = native.test;
        options.residue.iter().map(|arg| arg.text.clone()).collect()
    };
    // Memory ceiling (native-only safety valve, crate::memory_limit):
    // --memory-limit=<MB> wins over BAZEL_RS_MAX_MEMORY_MB (Bazel's
    // CLI-over-env convention). Unset both = the sliding-scale default
    // cap (70% of MemTotal below 32 GB, 50% from 32–128 GB, flat 64 GB
    // at/above 128 GB), announced once below. Installed before any
    // heavy work.
    // Watchdog-only (no RLIMIT_AS, which would hard-cap every child):
    // exceeding the RSS cap kills live child groups and exits 33
    // (Bazel's OUT_OF_MEMORY) instead of an OOM kill of the host.
    // The env var is the recommended place to set this: --memory-limit
    // in a shared .bazelrc also reaches genuine Bazel, which rejects
    // the unknown option. The cap bounds bazel-rs itself, not child
    // actions (a JVM/node action keeps the host's limits).
    let explicit_mb = match memory_limit_flag_mb {
        Some(mb) => bazel_rs::memory_limit::validate_cap_mb(mb).map(Some)?,
        None => std::env::var("BAZEL_RS_MAX_MEMORY_MB").ok().map_or(Ok(None), |value| {
            if value.trim().is_empty() {
                return Ok(None);
            }
            match bazel_rs::memory_limit::parse_cap_mb(&value) {
                Some(mb) => bazel_rs::memory_limit::validate_cap_mb(mb).map(Some),
                None => {
                    eprintln!(
                        "bazel-rs: warning: ignoring invalid BAZEL_RS_MAX_MEMORY_MB={value:?} (want a positive integer number of MB)"
                    );
                    Ok(None)
                }
            }
        })?,
    };
    let (memory_limit_mb, default_capped) = match bazel_rs::memory_limit::effective_cap(explicit_mb)
    {
        Some((cap_mb, is_default)) => (Some(cap_mb), is_default),
        None => (None, false),
    };
    if let Some(cap_mb) = memory_limit_mb {
        if default_capped {
            bazel_rs::memory_limit::mark_default_cap();
            // Announce the exact policy that produced the cap.
            let description = bazel_rs::memory_limit::default_cap()
                .map(|(_, text)| text)
                .unwrap_or_else(|| format!("{cap_mb} MB"));
            eprintln!(
                "bazel-rs: memory limit defaulted to {description}; set BAZEL_RS_MAX_MEMORY_MB (recommended) or pass --memory-limit=<MB> to change it; the cap bounds bazel-rs itself, not child actions"
            );
        }
        bazel_rs::memory_limit::install_ceiling(cap_mb);
    }
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
    // The spawn strategy and sandbox options (N59) for every executing
    // command. Unknown strategy identifiers are rejected once here, at
    // startup, with Bazel's STRATEGY_NOT_FOUND text and exit code 2
    // (SpawnStrategyRegistry.toStrategy:718-727).
    if let Err(error) = bazel_rs::sandbox::validate(&spawn_options) {
        eprintln!("ERROR: {error}");
        std::process::exit(2);
    }
    bazel_rs::sandbox::install(spawn_options);
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
    if (command == "build" || command == "test") && rest.is_empty() {
        return Err(format!("{command} requires at least one target"));
    }
    // TestCommand.exec (runtime/commands/TestCommand.java:98-107).
    if command == "test" && test_options.output == bazel_rs::test_command::TestOutput::Streamed {
        eprintln!(
            "WARNING: Streamed test output requested. All tests will be run without sharding, one at a time"
        );
    }
    // RunCommand.exec (runtime/commands/RunCommand.java:254-262).
    if command == "run" && rest.is_empty() {
        return Err("Must specify a target to run".into());
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
    if builds(&command) && root.join("platform_mappings").exists() {
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
        .with_hidden_lockfile(bazel_rs::registry_session::hidden_lockfile_path(
            &cache, &root,
        ))
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
    // BazelLockFileModule.afterCommand (Bazel 9.3 :63-100) runs in
    // LockfileMode.UPDATE/REFRESH only and combines BazelModuleResolution-
    // Value, BazelDepGraphValue and the SingleExtensionValues of the
    // extensions "that have been evaluated in the current build" (:105-111,
    // :112-141) — demand-driven, like SingleExtensionEvalFunction. Native:
    // the loading phase — module resolution and every demanded extension
    // evaluation — must succeed before MODULE.bazel.lock may be written
    // (a failed command writes nothing); `off` and `error` never write.
    let lockfile_gate = LockfileGate {
        session: _lockfile_writer.0.clone(),
        repositories: repositories.clone(),
    };
    // --embed_label is validated by its converter before anything runs
    // (WorkspaceStatusAction.OneLineStringConverter :216-233).
    bazel_rs::workspace_status::validate_embed_label(&workspace_status.embed_label)?;
    let mut engine = Engine::with_repositories(repositories.clone());
    engine.set_runfiles_options(runfiles_options);
    engine.set_workspace_status(workspace_status);
    engine.set_execute_build(!no_build);
    // --loading_phase_threads (LoadingPhaseThreadsOption.java, P2 of the
    // parallel-analysis program): both loaders' pattern expansion uses it
    // (Engine::expand_pattern; query resolves patterns through it too).
    // "auto" (the default) keeps the engine's available-processors value.
    engine.set_loading_phase_threads(match (command.as_str(), loading_phase_threads) {
        ("query", explicit) => explicit.unwrap_or(query_options.loading_phase_threads),
        (_, explicit) => explicit.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
        }),
    });
    // BuildRequest.getTopLevelArtifactContext (BuildRequest.java:412-421):
    // the requested output groups; a command that runs the tests it builds
    // keeps the `default` group for failure reporting (shouldRunTests).
    engine.set_output_selection(output_selection);
    engine.set_tests_requested(command == "test");
    // N58: the top-level aspects of builds (`--aspects`, with
    // `--aspects_parameters`); the labels resolve against the main
    // repository mapping inside the engine (BuildView.
    // createTopLevelAspectKeys).
    if !aspects.is_empty() || !aspects_parameters.is_empty() {
        let resolved = resolve_aspects(&engine, &aspects)?;
        engine.set_requested_aspects(resolved, aspects_parameters.into_iter().collect());
    }
    if builds(&command) {
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
        bazel_rs::perf_mark("select.start");
        let outcome = engine.query(&rest[0], &query_options, &relative_directory);
        bazel_rs::perf_mark("select.end");
        bazel_rs::repository_store::timing::report(loading_threads_for_report(
            &command,
            loading_phase_threads,
            &query_options,
        ));
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
        // Loading — module resolution and every demanded extension
        // resolution (evaluation or lockfile reuse) — succeeded: the
        // lockfile write is allowed.
        lockfile_gate.open();
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&outcome.stdout)
            .and_then(|()| stdout.flush())
            .map_err(|e| format!("I/O error: {e}"))?;
        if outcome.exit_code != 0 {
            bazel_rs::loading::module_timings::report();
            std::process::exit(outcome.exit_code);
        }
        bazel_rs::memory_limit::report_peak(memory_limit_mb);
        return Ok(());
    }
    if command == "test" {
        return test_targets(TestContext {
            engine: &mut engine,
            patterns: &rest,
            relative_directory: &relative_directory,
            cache: &cache,
            jobs,
            root: &root,
            runfiles: runfiles_options,
            options: &test_options,
            remote_enabled,
            lockfile_gate: &lockfile_gate,
        });
    }
    bazel_rs::perf_mark("select.start");
    let labels = if command == "run" {
        // The first residue argument is the target; the rest are its
        // arguments (RunCommand.java:251-252).
        engine.select_for_build(&rest[..1], &relative_directory)
    } else {
        engine.select_for_build(&rest, &relative_directory)
    };
    bazel_rs::perf_mark("select.end");
    // Bazel reports pattern warnings as they happen, before a later error.
    for warning in engine.take_warnings() {
        warn(&warning);
    }
    let labels = labels?;
    // Target-pattern loading succeeded: analysis or execution failures from
    // here on no longer block the lockfile write, like a Bazel command that
    // fails after loading (BazelLockFileModule.afterCommand still runs).
    lockfile_gate.open();
    if command == "run" {
        return run_target(RunContext {
            engine: &mut engine,
            labels,
            pattern: &rest[0],
            residue: &rest[1..],
            cache: &cache,
            jobs,
            root: &root,
            cwd: &cwd,
            runfiles: runfiles_options,
            options: &run_options,
            remote_enabled,
            test: &test_options,
        });
    }
    bazel_rs::repository_store::timing::report(loading_threads_for_report(
        &command,
        loading_phase_threads,
        &query_options,
    ));
    let result = engine.build_with_options(&labels, &cache, jobs, action_dump.as_deref());
    // Analysis warnings precede the result, also when the build fails.
    for warning in engine.take_warnings() {
        warn(&warning);
    }
    for error in engine.take_errors() {
        eprintln!("ERROR: {error}");
    }
    let summary = result?;
    let non_empty_targets = summary.non_empty_targets;
    for (label, output) in &summary.outputs {
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
    // BuildResultPrinter.outputTargetsAndAspects (:118-160): the
    // `--show_result` budget (default 1) counts every non-empty target and
    // aspect result; exceeding it aborts the whole result printing.
    {
        let non_empty_aspects = summary
            .aspects
            .iter()
            .filter(|(_, _, artifacts)| !artifacts.is_empty())
            .count();
        if non_empty_targets + non_empty_aspects <= show_result {
            for (aspect, label, artifacts) in &summary.aspects {
                // outputAspects (:312-332): an aspect line is printed only
                // when the aspect has artifacts in a requested, reportable
                // output group.
                if !artifacts.is_empty() {
                    eprintln!("Aspect {aspect} of {label} up-to-date:");
                    for path in artifacts {
                        eprintln!("  {}", path.display());
                    }
                }
            }
        }
    }
    if remote_enabled {
        eprintln!(
            "bazel-rs: {} actions executed, {} cache hits, {} remote cache hits (experimental local execution; {})",
            summary.executed,
            summary.cache_hits,
            summary.remote_hits,
            execution_note()
        );
    } else {
        eprintln!(
            "bazel-rs: {} actions executed, {} cache hits (experimental local execution; {})",
            summary.executed,
            summary.cache_hits,
            execution_note()
        );
    }
    bazel_rs::memory_limit::report_peak(memory_limit_mb);
    Ok(())
}

/// Opens the MODULE.bazel.lock write (update/refresh only) once the whole
/// module resolution — the dep graph and every module extension the command
/// demanded — evaluated successfully. BazelLockFileModule.afterCommand
/// (Bazel 9.3 :63-100) writes only with BazelModuleResolutionValue,
/// BazelDepGraphValue and the lockfile value present, using the
/// SingleExtensionValues "evaluated in the current build" (:105-141). The
/// native gate is fail-closed and STRICTER than Bazel: a command that fails
/// before this point writes nothing at all, whereas genuine Bazel's
/// afterCommand combines whatever evaluated and still writes when a demanded
/// extension failed (E1c2 observation: rc=1, lockfile rewritten; expected
/// native mismatch).
struct LockfileGate {
    session: bazel_rs::registry_session::SessionHandle,
    repositories: Arc<bazel_rs::repository::RepositorySet>,
}

impl LockfileGate {
    fn open(&self) {
        let Some(session) = self.session.session() else {
            return;
        };
        // `off` and `error` never write; they keep the lazy profile and do
        // not resolve anything for the lockfile (BazelLockFileModule.
        // ENABLED_IN_MODES, :53-54).
        if !session.mode().writes() {
            return;
        }
        // The count only feeds the write-path diagnostic; Bazel prints
        // nothing here.
        let resolved = self.repositories.resolved_module_extensions();
        session.mark_module_extensions_resolved(resolved);
    }
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

/// Commands that build (`run` and `test` inherit the build options).
fn builds(command: &str) -> bool {
    command == "build" || command == "run" || command == "test"
}

struct RunContext<'a> {
    engine: &'a mut Engine,
    labels: Vec<bazel_rs::label::Label>,
    pattern: &'a str,
    residue: &'a [String],
    cache: &'a Path,
    jobs: usize,
    root: &'a Path,
    cwd: &'a Path,
    runfiles: bazel_rs::runfiles_tree::RunfilesOptions,
    options: &'a bazel_rs::run_command::RunOptions,
    remote_enabled: bool,
    test: &'a bazel_rs::test_command::TestOptions,
}

/// `run` (runtime/commands/RunCommand.java:250-391): build exactly one
/// executable target and its runfiles tree, then execute it in its
/// runfiles directory (`<tree>/_main`) with the run environment, or write
/// the `--script_path` script. The process replaces this one (exit code,
/// stdin and terminal pass through).
fn run_target(context: RunContext<'_>) -> Result<(), String> {
    use bazel_rs::run_command::{RunInputs, command_line, random_uuid};
    let RunContext {
        engine,
        labels,
        pattern,
        residue,
        cache,
        jobs,
        root,
        cwd,
        runfiles,
        options,
        remote_enabled,
        test,
    } = context;
    // validateTargets (RunCommand.java:1052-1111).
    if labels.len() > 1 {
        let mut names: Vec<String> = labels.iter().map(|label| label.to_string()).collect();
        let truncated = names.len() > 5;
        names.truncate(5);
        names.sort();
        return Err(format!(
            "Only a single target can be run. Your target pattern {pattern} expanded to the targets {}{}",
            names.join(", "),
            if truncated { "[TRUNCATED]" } else { "" }
        ));
    }
    let Some(label) = labels.first() else {
        return Err("No targets found to run".into());
    };
    match engine.runnable(label)? {
        bazel_rs::engine::Runnable::No => {
            return Err(format!("Cannot run target {label}: Not executable"));
        }
        bazel_rs::engine::Runnable::Test | bazel_rs::engine::Runnable::Yes => {}
    }
    if let Some(value) = options.run_under.as_deref()
        && let Some(first) = value.split_whitespace().next()
        && (first.starts_with("//") || first.starts_with('@'))
    {
        return Err(format!(
            "--run_under={value}: a label prefix (LabelRunUnder) is not supported by the native profile; use a command prefix"
        ));
    }
    let result = engine.build_for_run(label, cache, jobs);
    for warning in engine.take_warnings() {
        warn(&warning);
    }
    for error in engine.take_errors() {
        eprintln!("ERROR: {error}");
    }
    let (summary, target) = result?;
    for (label, output) in &summary.outputs {
        eprintln!("{label}\t{}", output.display());
    }
    // BuildResultPrinter.outputTargetsAndAspects (:118-160): the
    // `--show_result` budget (default 1) counts every non-empty target and
    // aspect result; exceeding it aborts the whole result printing.
    {
        let non_empty_targets = summary
            .outputs
            .iter()
            .map(|(label, _)| label)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let non_empty_aspects = summary
            .aspects
            .iter()
            .filter(|(_, _, artifacts)| !artifacts.is_empty())
            .count();
        // The --show_result budget (not threaded here; aspects are a
        // build-request feature, and the default budget is 1).
        if non_empty_targets + non_empty_aspects <= 1 {
            for (aspect, label, artifacts) in &summary.aspects {
                // outputAspects (:312-332): an aspect line is printed only
                // when the aspect has artifacts in a requested, reportable
                // output group.
                if !artifacts.is_empty() {
                    eprintln!("Aspect {aspect} of {label} up-to-date:");
                    for path in artifacts {
                        eprintln!("  {}", path.display());
                    }
                }
            }
        }
    }
    if remote_enabled {
        eprintln!(
            "bazel-rs: {} actions executed, {} cache hits, {} remote cache hits (experimental local execution; {})",
            summary.executed,
            summary.cache_hits,
            summary.remote_hits,
            execution_note()
        );
    } else {
        eprintln!(
            "bazel-rs: {} actions executed, {} cache hits (experimental local execution; {})",
            summary.executed,
            summary.cache_hits,
            execution_note()
        );
    }
    // getBuiltTargets/fullyValidateTarget (RunCommand.java:435-568, 1136-1185).
    let Some(target) = target else {
        return Err(format!("Cannot run target {label}: Not executable"));
    };
    if !runfiles.build_runfile_manifests {
        return Err("--nobuild_runfile_manifests is incompatible with the \"run\" command".into());
    }
    if target.settings.test {
        return run_test(RunTest {
            engine,
            label,
            target: &target,
            residue,
            cache,
            root,
            cwd,
            runfiles,
            options,
            test,
        });
    }
    let (executable, execroot, runfiles_working_directory) = match &target.runfiles {
        Some(tree) => (
            tree.executable.clone(),
            tree.root.clone(),
            Some(if runfiles.enable_runfiles {
                tree.directory.join(bazel_rs::runfiles::WORKSPACE_NAME)
            } else {
                tree.directory.clone()
            }),
        ),
        None => (target.executable.clone(), cache.to_path_buf(), None),
    };
    {
        use std::os::unix::fs::PermissionsExt;
        let executable_bits = std::fs::metadata(&executable)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
        if !executable_bits {
            return Err(format!(
                "Non-existent or non-executable {}",
                executable.display()
            ));
        }
    }
    let utf8 = |path: &Path| {
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("non-UTF-8 path {} is not supported", path.display()))
    };
    let client_env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let build_id = random_uuid()?;
    let line = command_line(&RunInputs {
        workspace: &utf8(root)?,
        working_directory: &utf8(cwd)?,
        execroot: &utf8(&execroot)?,
        build_id: &build_id,
        client_env: &client_env,
        options,
        target_env: &target.settings.environment,
        target_inherited: &target.settings.inherited,
        executable: &utf8(&executable)?,
        binary_args: &target.settings.args,
        residue,
        runfiles_working_directory: runfiles_working_directory
            .as_deref()
            .map(utf8)
            .transpose()?
            .as_deref(),
    });
    if let Some(script) = &options.script_path {
        // handleScriptPath/writeScript (RunCommand.java:653-686, 1042-1049).
        let path = cwd.join(script);
        // An executable written in-process: under the exclusive spawn lock.
        bazel_rs::spawn_lock::exclusive(true, || {
            std::fs::write(&path, line.script(&options.shell))
                .map_err(|e| format!("Error writing run script: {e}"))?;
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .map_err(|e| format!("Error writing run script: {e}"))?
                .permissions()
                .mode();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode | 0o111))
                .map_err(|e| format!("Error writing run script: {e}"))
        })?;
        return Ok(());
    }
    let prefix = if options.run { "Running" } else { "Runnable" };
    eprintln!(
        "bazel-rs: {prefix} command line: {}",
        line.pretty(options.omit_run_args)
    );
    if !options.run {
        return Ok(());
    }
    // The client's ExecuteRunRequest (src/main/cpp/blaze.cc:2167-2195):
    // clear, then set the environment, change directory and exec.
    let argv = line.argv(&options.shell);
    let mut process = std::process::Command::new(&argv[0]);
    process.args(&argv[1..]);
    for name in &line.clear {
        process.env_remove(name);
    }
    process.envs(&line.environment);
    process.current_dir(&line.working_directory);
    let error = bazel_rs::spawn_lock::exec(&mut process);
    Err(format!("cannot execute {}: {error}", argv[0]))
}

struct TestContext<'a> {
    engine: &'a mut Engine,
    patterns: &'a [String],
    relative_directory: &'a str,
    cache: &'a Path,
    jobs: usize,
    root: &'a Path,
    runfiles: bazel_rs::runfiles_tree::RunfilesOptions,
    options: &'a bazel_rs::test_command::TestOptions,
    /// Whether the remote cache is set up (N41): the build summary names its
    /// hits. Test actions themselves are local-only (docs/TEST_COMMAND.md).
    remote_enabled: bool,
    /// Opens the lockfile write once test-target loading succeeded.
    lockfile_gate: &'a LockfileGate,
}

/// Label.print: `//pkg:name` or `@@repo//pkg:name`.
fn label_print(label: &bazel_rs::label::Label) -> String {
    if label.repository.is_empty() {
        format!("//{}:{}", label.package, label.name)
    } else {
        format!("@@{}//{}:{}", label.repository, label.package, label.name)
    }
}

/// The test spec of a built test target (TestActionBuilder.createTestAction,
/// analysis/test/TestActionBuilder.java:224-480).
fn test_spec(
    label: &bazel_rs::label::Label,
    info: bazel_rs::test_command::TestRuleInfo,
    location: Option<String>,
    target: &bazel_rs::engine::RunTarget,
) -> Result<bazel_rs::test_runner::TestSpec, String> {
    let utf8 = |path: &Path| {
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("non-UTF-8 path {} is not supported", path.display()))
    };
    let Some(entries) = target.entries.clone() else {
        return Err(format!(
            "{}: test target has no runfiles support",
            label_print(label)
        ));
    };
    let package = if label.repository.is_empty() {
        label.package.clone()
    } else if label.package.is_empty() {
        format!("external/{}", label.repository)
    } else {
        format!("external/{}/{}", label.repository, label.package)
    };
    let testlogs_package = if package.is_empty() {
        format!("bazel-out/{}/testlogs", target.mnemonic)
    } else {
        format!("bazel-out/{}/testlogs/{package}", target.mnemonic)
    };
    Ok(bazel_rs::test_runner::TestSpec {
        label: label_print(label),
        info,
        location,
        executable: target.executable.clone(),
        executable_exec: utf8(&target.executable_exec)?,
        binary: target.executable_short_path.clone(),
        entries,
        testlogs_package,
        args: target.settings.args.clone(),
        target_env: target.settings.environment.clone(),
        target_inherited: target.settings.inherited.clone(),
        action_env: target.action_env.clone(),
        test_env: target.test_env.clone(),
    })
}

/// `test` (runtime/commands/TestCommand.java:124-243): build the targets,
/// run the selected tests (crate::test_runner), print the summary and exit
/// with Bazel's code: 0 when every test passed, 3 (TESTS_FAILED) otherwise,
/// 4 (NO_TESTS_FOUND) without tests, 1 when the build failed.
fn test_targets(context: TestContext<'_>) -> Result<(), String> {
    use bazel_rs::test_command::{should_output_log, summary_text};
    let TestContext {
        engine,
        patterns,
        relative_directory,
        cache,
        jobs,
        root,
        runfiles,
        options,
        remote_enabled,
        lockfile_gate,
    } = context;
    let selected = engine.select_for_test(patterns, relative_directory);
    for warning in engine.take_warnings() {
        warn(&warning);
    }
    let (mut build, candidates) = selected?;
    // Test-target loading succeeded: the lockfile write is allowed.
    lockfile_gate.open();
    let mut tests = Vec::new();
    let mut infos = std::collections::BTreeMap::new();
    let any_candidate = !candidates.is_empty();
    for label in candidates {
        let (info, location) = engine
            .test_rule_info(&label)?
            .ok_or_else(|| format!("internal error: {label} is not a test rule"))?;
        if options.accepts(&info) {
            infos.insert(label.clone(), (info, location));
            tests.push(label);
        }
    }
    // TargetPatternPhaseFunction.java:136-139.
    if tests.is_empty() && any_candidate {
        eprintln!("WARNING: All specified test targets were excluded by filters");
    }
    if options.build_tests_only {
        build = tests.clone();
    }
    let result = engine.build_for_tests(&build, &tests, cache, jobs);
    for warning in engine.take_warnings() {
        warn(&warning);
    }
    for error in engine.take_errors() {
        eprintln!("ERROR: {error}");
    }
    let (summary, targets) = match result {
        Ok(built) => built,
        Err(error) => return test_build_failure(&error, &tests, options),
    };
    // Bazel reports built targets on stderr; stdout carries test output
    // and the summary.
    for (label, output) in &summary.outputs {
        eprintln!("{label}\t{}", output.display());
    }
    // BuildResultPrinter.outputTargetsAndAspects (:118-160): the
    // `--show_result` budget (default 1) counts every non-empty target and
    // aspect result; exceeding it aborts the whole result printing.
    {
        let non_empty_targets = summary
            .outputs
            .iter()
            .map(|(label, _)| label)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let non_empty_aspects = summary
            .aspects
            .iter()
            .filter(|(_, _, artifacts)| !artifacts.is_empty())
            .count();
        // The --show_result budget (not threaded here; aspects are a
        // build-request feature, and the default budget is 1).
        if non_empty_targets + non_empty_aspects <= 1 {
            for (aspect, label, artifacts) in &summary.aspects {
                // outputAspects (:312-332): an aspect line is printed only
                // when the aspect has artifacts in a requested, reportable
                // output group.
                if !artifacts.is_empty() {
                    eprintln!("Aspect {aspect} of {label} up-to-date:");
                    for path in artifacts {
                        eprintln!("  {}", path.display());
                    }
                }
            }
        }
    }
    if remote_enabled {
        eprintln!(
            "bazel-rs: {} actions executed, {} cache hits, {} remote cache hits (experimental local execution; {})",
            summary.executed,
            summary.cache_hits,
            summary.remote_hits,
            execution_note()
        );
    } else {
        eprintln!(
            "bazel-rs: {} actions executed, {} cache hits (experimental local execution; {})",
            summary.executed,
            summary.cache_hits,
            execution_note()
        );
    }
    if tests.is_empty() {
        eprintln!("ERROR: No test targets were found, yet testing was requested");
        std::process::exit(4);
    }
    let mut specs = Vec::new();
    for label in &tests {
        let target = targets
            .get(label)
            .ok_or_else(|| format!("internal error: {label} was not built as a test"))?;
        let (info, location) = infos.remove(label).expect("filtered test");
        specs.push(test_spec(label, info, location, target)?);
    }
    let tools = bazel_rs::test_runner::Tools::embedded(cache)?;
    let client_env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let runner = bazel_rs::test_runner::Runner {
        cache,
        workspace: root,
        options,
        client_env: &client_env,
        jobs,
        runfiles_enabled: runfiles.enable_runfiles,
        tools: &tools,
    };
    let outcome = runner.run(&specs)?;
    let mut sorted: Vec<&bazel_rs::test_command::TestSummary> = outcome.summaries.iter().collect();
    sorted.sort_by(|a, b| a.compare(b));
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    for summary in sorted {
        if summary.num_cached > 0 && should_output_log(options.output, summary.status.passed()) {
            let _ = stdout.write_all(&bazel_rs::test_runner::cached_output(summary, options));
        }
    }
    let _ =
        stdout.write_all(summary_text(&outcome.summaries, outcome.executed, options).as_bytes());
    let _ = stdout.flush();
    drop(stdout);
    // The build's failure summary names the first action error once
    // (BuildResultPrinter; genuine capture M8).
    // Under --keep_going the build completes and repeats no error (M36).
    if let Some(error) = outcome.errors.first().filter(|_| !options.keep_going) {
        eprintln!("ERROR: {error}");
    }
    let passed = outcome
        .summaries
        .iter()
        .all(|summary| summary.status.passed());
    // A target that failed to build (a test action error under
    // --keep_going) makes the build fail: exit 1, the more important code
    // (DetailedExitCodeComparator, TestCommand.java:236-240).
    let failed_to_build = outcome
        .summaries
        .iter()
        .any(|summary| summary.status == bazel_rs::test_command::TestStatus::FailedToBuild);
    bazel_rs::loading::module_timings::report();
    std::process::exit(if failed_to_build {
        1
    } else if passed {
        0
    } else {
        3
    });
}

/// A failed build of `bazel test` (TestCommand.java:200-243 with
/// AggregatingTestListener.differentialAnalyzeAndReport): the summary
/// shows each test that failed to build (named by the failing actions'
/// messages) as FAILED TO BUILD and the others as NO STATUS (never run,
/// --nokeep_going); exit code 1. Under --keep_going Bazel would still run
/// the tests that built, which the native all-or-nothing build cannot.
fn test_build_failure(
    error: &str,
    tests: &[bazel_rs::label::Label],
    options: &bazel_rs::test_command::TestOptions,
) -> Result<(), String> {
    use bazel_rs::test_command::{TestStatus, TestSummary, TestTimeout, summary_text};
    if options.keep_going {
        return Err(format!(
            "{error}\n--keep_going with a build failure is not supported by the native test command: the native build stops at the first failing action, so tests that would build are not run"
        ));
    }
    eprintln!("bazel-rs: {error}");
    let mentions = |label: &str| {
        error.match_indices(label).any(|(index, _)| {
            error[index + label.len()..]
                .chars()
                .next()
                .is_none_or(|next| !(next.is_alphanumeric() || "_-./+".contains(next)))
        })
    };
    let summaries: Vec<TestSummary> = tests
        .iter()
        .map(|label| {
            let shown = label_print(label);
            let mut summary = TestSummary::aggregate(&shown, TestTimeout::Moderate, &[], false);
            summary.status = if mentions(&shown) {
                TestStatus::FailedToBuild
            } else {
                TestStatus::NoStatus
            };
            summary
        })
        .collect();
    print!("{}", summary_text(&summaries, 0, options));
    use std::io::Write;
    let _ = std::io::stdout().flush();
    bazel_rs::loading::module_timings::report();
    std::process::exit(1);
}

struct RunTest<'a> {
    engine: &'a mut Engine,
    label: &'a bazel_rs::label::Label,
    target: &'a bazel_rs::engine::RunTarget,
    residue: &'a [String],
    cache: &'a Path,
    root: &'a Path,
    cwd: &'a Path,
    runfiles: bazel_rs::runfiles_tree::RunfilesOptions,
    options: &'a bazel_rs::run_command::RunOptions,
    test: &'a bazel_rs::test_command::TestOptions,
}

/// `run` of a test target (RunCommand.getTestCommandLine,
/// runtime/commands/RunCommand.java:754-850): test-setup.sh runs the test
/// in an execution root with the test environment (computeTestEnvironment
/// with the run variables), BUILD_EXECROOT set (test-setup.sh then execs
/// the test without job control). Natively the execution root is
/// `<cache>/test-run/<key>/execroot`: test-setup.sh at its exec path and
/// the runfiles tree linked at `<executable>.runfiles`.
fn run_test(context: RunTest<'_>) -> Result<(), String> {
    use bazel_rs::run_command::{ENV_TO_CLEAR, random_uuid, shell_escape};
    let RunTest {
        engine,
        label,
        target,
        residue,
        cache,
        root,
        cwd,
        runfiles,
        options,
        test,
    } = context;
    let Some((info, location)) = engine.test_rule_info(label)? else {
        return Err(format!("internal error: {label} is not a test rule"));
    };
    let spec = test_spec(label, info, location, target)?;
    // TestProvider.getTestStatusArtifacts must be a single action.
    let shards = test.sharding.shards(spec.info.shard_count).max(0) as u32;
    if shards > 1 || test.runs_per_test > 1 {
        return Err("Run command does not support tests with more than one shard or run (--runs_per_test and shard_count must be 1)".into());
    }
    if options.run_under.is_some() {
        return Err("--run_under with a test target is not supported by the native profile".into());
    }
    let Some(tree) = &target.runfiles else {
        return Err(format!("{}: test target has no runfiles tree", spec.label));
    };
    let tools = bazel_rs::test_runner::Tools::embedded(cache)?;
    let key = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        spec.label.hash(&mut hasher);
        target.mnemonic.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    };
    let execroot = cache.join("test-run").join(key).join("execroot");
    if execroot.exists() {
        add_user_write(&execroot);
        std::fs::remove_dir_all(&execroot)
            .map_err(|e| format!("cannot remove {}: {e}", execroot.display()))?;
    }
    let setup = execroot.join(bazel_rs::test_runner::SETUP_EXEC_PATH);
    std::fs::create_dir_all(setup.parent().expect("setup parent"))
        .map_err(|e| format!("cannot create {}: {e}", execroot.display()))?;
    // test-setup.sh is executed below: copied under the exclusive spawn lock.
    bazel_rs::spawn_lock::exclusive(true, || std::fs::copy(&tools.setup, &setup))
        .map_err(|e| format!("cannot write {}: {e}", setup.display()))?;
    let link = execroot.join(format!("{}.runfiles", spec.executable_exec));
    std::fs::create_dir_all(link.parent().expect("runfiles parent"))
        .map_err(|e| format!("cannot create {}: {e}", execroot.display()))?;
    std::os::unix::fs::symlink(&tree.directory, &link)
        .map_err(|e| format!("cannot link {}: {e}", link.display()))?;
    let utf8 = |path: &Path| {
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("non-UTF-8 path {} is not supported", path.display()))
    };
    let client_env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let tmp = match &test.test_tmpdir {
        Some(directory) => utf8(&root.join(directory).join("_tmp"))?,
        None => "_tmp".to_owned(),
    };
    let tmp = format!(
        "{tmp}/{}",
        bazel_rs::test_command::tmp_dir_name(&spec.executable_exec, 0, 0)
    );
    let mut environment = std::collections::BTreeMap::from([
        ("BUILD_WORKSPACE_DIRECTORY".to_owned(), utf8(root)?),
        ("BUILD_WORKING_DIRECTORY".to_owned(), utf8(cwd)?),
        ("BUILD_EXECROOT".to_owned(), utf8(&execroot)?),
        ("BUILD_ID".to_owned(), random_uuid()?),
    ]);
    let name = label.name.clone();
    environment.extend(bazel_rs::test_command::test_environment(
        &bazel_rs::test_command::EnvInputs {
            action_env: &spec.action_env,
            test_env: &spec.test_env,
            client_env: &client_env,
            options: test,
            target_env: &spec.target_env,
            target_inherited: &spec.target_inherited,
            label: &spec.label,
            size: spec.info.size,
            timeout_seconds: test.timeout_seconds(spec.info.timeout),
            workspace: bazel_rs::test_runner::WORKSPACE,
            binary: &spec.binary,
            runfiles_dir: &format!("{}.runfiles", spec.executable_exec),
            tmp_dir: &tmp,
            base_dir: &format!("{}/{name}", spec.testlogs_package),
            shard: 0,
            total_shards: 0,
            run: 0,
            total_runs: 1,
            runfiles_enabled: runfiles.enable_runfiles,
        },
    ));
    let mut args = vec![
        bazel_rs::test_runner::SETUP_EXEC_PATH.to_owned(),
        spec.binary.clone(),
    ];
    args.extend(spec.args.iter().cloned());
    args.extend(test.args.iter().cloned());
    let fixed = args.len();
    args.extend(residue.iter().cloned());
    let escaped: Vec<String> = args.iter().map(|arg| shell_escape(arg)).collect();
    let command = escaped.join(" ");
    if let Some(script) = &options.script_path {
        let text = format!(
            "#!{}\ncd {} && \\\n  exec env \\\n    {} \\\n    {} \\\n  {command} \"$@\"",
            options.shell,
            shell_escape(&utf8(&execroot)?),
            ENV_TO_CLEAR
                .iter()
                .map(|name| format!("-u {name}"))
                .collect::<Vec<_>>()
                .join(" \\\n    "),
            environment
                .iter()
                .map(|(name, value)| format!("{}={}", shell_escape(name), shell_escape(value)))
                .collect::<Vec<_>>()
                .join(" \\\n    ")
        );
        let path = cwd.join(script);
        std::fs::write(&path, text).map_err(|e| format!("Error writing run script: {e}"))?;
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .map_err(|e| format!("Error writing run script: {e}"))?
            .permissions()
            .mode();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode | 0o111))
            .map_err(|e| format!("Error writing run script: {e}"))?;
        return Ok(());
    }
    let pretty = if residue.is_empty() {
        escaped.join(" ")
    } else if options.omit_run_args {
        format!("{} <args omitted>", escaped[..fixed].join(" "))
    } else {
        escaped.join(" ")
    };
    let prefix = if options.run { "Running" } else { "Runnable" };
    eprintln!("bazel-rs: {prefix} command line: {pretty}");
    if !options.run {
        return Ok(());
    }
    let mut process = std::process::Command::new(&options.shell);
    process.args(["-c", &command]);
    for name in ENV_TO_CLEAR {
        process.env_remove(name);
    }
    process.envs(&environment);
    process.current_dir(&execroot);
    let error = bazel_rs::spawn_lock::exec(&mut process);
    Err(format!("cannot execute {}: {error}", options.shell))
}

/// `chmod -R u+w PATH`, in process and with its semantics: the named path
/// is followed, symbolic links met during the traversal are not changed or
/// followed, a directory gets u+w before it is read, and failures are
/// ignored (the former helper's status was ignored too).
fn add_user_write(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fn visit(path: &Path, metadata: std::fs::Metadata) {
        let mode = metadata.permissions().mode() & 0o7777;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o200));
        if metadata.is_dir()
            && let Ok(entries) = std::fs::read_dir(path)
        {
            for entry in entries.flatten() {
                let child = entry.path();
                if let Ok(metadata) = std::fs::symlink_metadata(&child)
                    && !metadata.file_type().is_symlink()
                {
                    visit(&child, metadata);
                }
            }
        }
    }
    if let Ok(metadata) = std::fs::metadata(path) {
        visit(path, metadata);
    }
}

fn loading_threads_for_report(
    command: &str,
    explicit: Option<usize>,
    query_options: &bazel_rs::engine::query::QueryOptions,
) -> usize {
    match (command, explicit) {
        ("query", e) => e.unwrap_or(query_options.loading_phase_threads),
        (_, Some(v)) => v,
        _ => std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
    }
}

/// `--loading_phase_threads` (LoadingPhaseThreadsOption.java): "auto" or
/// a positive integer; "auto" keeps the engine default (the available
/// processors, as Bazel's converter expands it).
fn parse_loading_phase_threads(value: &str) -> Result<usize, String> {
    if value == "auto" {
        return Ok(std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1));
    }
    if !value.is_empty()
        && value.bytes().all(|b| b.is_ascii_digit())
        && let Ok(threads) = value.parse::<usize>()
        && threads > 0
    {
        return Ok(threads);
    }
    Err(format!(
        "While parsing option --loading_phase_threads={value}: not a positive integer or 'auto'"
    ))
}

fn parse_memory_limit_mb(value: &str) -> Result<u64, String> {
    bazel_rs::memory_limit::parse_cap_mb(value).ok_or_else(|| {
        format!("--memory-limit requires a positive integer number of MB, got {value:?}")
    })
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

/// One comma-separated strategy list value
/// (CommaSeparatedNonEmptyOptionListConverter: split on commas, empty
/// segments drop out, order preserved).
fn strategy_list(value: &str) -> Result<Vec<String>, String> {
    let list = value
        .split(',')
        .filter(|identifier| !identifier.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if list.is_empty() {
        return Err(format!(
            "--spawn_strategy requires a comma-separated list of strategies, got {value:?}"
        ));
    }
    Ok(list)
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
    /// --loading_phase_threads (P2 of the parallel-analysis program,
    /// docs/PARALLEL_ANALYSIS.md); None keeps the engine's default
    /// (LoadingPhaseThreadsOption "auto": the available processors).
    loading_phase_threads: Option<usize>,
    /// --memory-limit=<MB> (native-only): watchdog-only RSS ceiling for
    /// bazel-rs itself (not child actions); flag wins over
    /// BAZEL_RS_MAX_MEMORY_MB; unset = the sliding-scale default
    /// (crate::memory_limit). Prefer the env var in shared setups.
    memory_limit_mb: Option<u64>,
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
    /// Native-only --native-action-dump (build/test): append one JSON object
    /// per planned action to the file (docs/CC_ACTIONS.md).
    action_dump: Option<PathBuf>,
    /// --allow_yanked_versions values in order.
    allowed_yanked_flags: Vec<String>,
    /// --registry values as given (lockfile keys use Bazel's spelling).
    raw_registries: Vec<String>,
    /// AuthAndTLSOptions credential helpers (N41, crate::credentials).
    credentials: bazel_rs::credentials::CredentialOptions,
    /// The modelled RemoteOptions (N41, crate::remote_cache).
    remote: bazel_rs::remote_cache::RemoteOptions,
    /// Runfiles tree options of build and run (N23f, crate::runfiles_tree).
    runfiles: bazel_rs::runfiles_tree::RunfilesOptions,
    /// The requested top-level output groups of build/run/test (N56,
    /// `--output_groups`/`--run_validations`).
    output: bazel_rs::output_group_info::OutputSelection,
    /// `--aspects` (N58): top-level aspect labels `{bzl-file-label}%{name}`
    /// or native aspect names, in order.
    aspects: Vec<String>,
    /// `--aspects_parameters` (key=value, in order).
    aspects_parameters: Vec<(String, String)>,
    /// `--show_result` (BuildRequestOptions.maxResultTargets): how many
    /// non-empty target/aspect results print.
    show_result: usize,
    /// RunCommand.RunOptions (N23f, crate::run_command).
    run: bazel_rs::run_command::RunOptions,
    test: bazel_rs::test_command::TestOptions,
    /// The spawn strategy and sandbox options (N59, crate::sandbox).
    spawn: bazel_rs::sandbox::SpawnOptions,
    /// --workspace_status_command/--embed_label (WorkspaceStatusAction
    /// .Options; N63, crate::workspace_status).
    workspace_status: bazel_rs::workspace_status::WorkspaceStatusOptions,
    /// `--nobuild` (BuildRequestOptions.performExecutionPhase :154-160):
    /// stop before the execution phase; the workspace status action (an
    /// execution-phase artifact) is not planned.
    no_build: bool,
}

impl NativeOptions {
    fn new(command: &str) -> Self {
        Self {
            jobs: None,
            loading_phase_threads: None,
            memory_limit_mb: None,
            rc: bazel_rs::rc_options::RcOptions::default(),
            resolver: bazel_rs::module_graph::ResolverOptions {
                // Every lockfile mode supports remote registries (N37).
                allow_remote: matches!(command, "build" | "query" | "run" | "test"),
                ..Default::default()
            },
            downloads: true,
            credentials: Default::default(),
            remote: Default::default(),
            runfiles: bazel_rs::runfiles_tree::RunfilesOptions::default(),
            output: bazel_rs::output_group_info::OutputSelection::default(),
            aspects: Vec::new(),
            aspects_parameters: Vec::new(),
            show_result: 1,
            run: bazel_rs::run_command::RunOptions::default(),
            test: bazel_rs::test_command::TestOptions::default(),
            spawn: bazel_rs::sandbox::SpawnOptions::default(),
            workspace_status: bazel_rs::workspace_status::WorkspaceStatusOptions::default(),
            no_build: false,
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
            action_dump: None,
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
        } else if builds(command) && arg == "--workspace_status_command" {
            // WorkspaceStatusAction.Options (:70-97) is a module command
            // option of `build` only (BazelWorkspaceStatusModule
            // .getCommandOptions :323-328); every command that inherits
            // build options accepts it (BlazeCommandUtils.getOptions
            // :90-96 recursing inheritsOptionsFrom: RunCommand and
            // TestCommand inherit BuildCommand, CoverageCommand inherits
            // TestCommand), and rc `build` lines apply to those commands
            // the same way. `query` does NOT inherit build options
            // (QueryCommand lists its own options, no inheritsOptionsFrom)
            // and rejects it as unrecognized. The native's builds() is
            // build/run/test, hence the gate.
            self.workspace_status.command = next
                .ok_or("--workspace_status_command requires a path")?
                .to_owned();
            true
        } else if let Some(value) = arg
            .strip_prefix("--workspace_status_command=")
            .filter(|_| builds(command))
        {
            self.workspace_status.command = value.to_owned();
            false
        } else if builds(command) && arg == "--embed_label" {
            // The --embed_label converter (OneLineStringConverter
            // :216-233) rejects a multi-line value at parse time.
            let value = next.ok_or("--embed_label requires a label")?;
            bazel_rs::workspace_status::validate_embed_label(value)?;
            self.workspace_status.embed_label = value.to_owned();
            true
        } else if let Some(value) = arg
            .strip_prefix("--embed_label=")
            .filter(|_| builds(command))
        {
            bazel_rs::workspace_status::validate_embed_label(value)?;
            self.workspace_status.embed_label = value.to_owned();
            false
        } else if builds(command) && matches!(arg, "--nobuild" | "--nobuild=true" | "--build=false")
        {
            // BuildRequestOptions.performExecutionPhase (:154-160):
            // --nobuild (the --build boolean's negation) stops before the
            // execution phase.
            self.no_build = true;
            false
        } else if builds(command) && matches!(arg, "--build" | "--build=true") {
            self.no_build = false;
            false
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
        } else if let Some(value) = arg.strip_prefix("--native-action-dump=") {
            self.action_dump = Some(PathBuf::from(value));
            false
        } else if matches!(
            arg,
            "--verify_repo_tampering"
                | "--verify_repo_tampering=true"
                | "--noverify_repo_tampering=false"
        ) {
            // Native-only: full re-hash warm verification (Step B);
            // BAZEL_RS_VERIFY_REPOS=full is the env equivalent.
            bazel_rs::repository_store::set_verify_full();
            false
        } else if matches!(
            arg,
            "--noverify_repo_tampering"
                | "--noverify_repo_tampering=true"
                | "--verify_repo_tampering=false"
        ) {
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
        } else if builds(command) && arg == "--jobs" {
            self.jobs = Some(parse_jobs(
                next.ok_or("--jobs requires a positive integer")?,
            )?);
            true
        } else if let Some(value) = arg.strip_prefix("--jobs=").filter(|_| builds(command)) {
            self.jobs = Some(parse_jobs(value)?);
            false
        } else if builds(command) && arg == "--loading_phase_threads" {
            self.loading_phase_threads = Some(parse_loading_phase_threads(
                next.ok_or("--loading_phase_threads requires a positive integer or 'auto'")?,
            )?);
            true
        } else if let Some(value) = arg
            .strip_prefix("--loading_phase_threads=")
            .filter(|_| builds(command))
        {
            self.loading_phase_threads = Some(parse_loading_phase_threads(value)?);
            false
        } else if arg == "--memory-limit" {
            self.memory_limit_mb = Some(parse_memory_limit_mb(
                next.ok_or("--memory-limit requires a value in MB")?,
            )?);
            true
        } else if let Some(value) = arg.strip_prefix("--memory-limit=") {
            self.memory_limit_mb = Some(parse_memory_limit_mb(value)?);
            false
        } else if builds(command) && arg == "--spawn_strategy" {
            // CommaSeparatedNonEmptyOptionListConverter (ExecutionOptions.
            // java:63-75): highest priority first; the option is not
            // allowMultiple, so a repeated flag replaces the list.
            self.spawn.spawn_strategy =
                strategy_list(next.ok_or("--spawn_strategy requires a value")?)?;
            true
        } else if let Some(value) = arg
            .strip_prefix("--spawn_strategy=")
            .filter(|_| builds(command))
        {
            self.spawn.spawn_strategy = strategy_list(value)?;
            false
        } else if builds(command) && arg == "--genrule_strategy" {
            // ExecutionOptions.java:77-87: Genrule-only, deprecated in favor
            // of --strategy=Genrule=; a repeated flag replaces the list.
            self.spawn.genrule_strategy =
                strategy_list(next.ok_or("--genrule_strategy requires a value")?)?;
            true
        } else if let Some(value) = arg
            .strip_prefix("--genrule_strategy=")
            .filter(|_| builds(command))
        {
            self.spawn.genrule_strategy = strategy_list(value)?;
            false
        } else if builds(command)
            && arg == "--strategy"
            && let Some(value) = next
        {
            self.parse_strategy(value)?;
            true
        } else if builds(command)
            && let Some(value) = arg.strip_prefix("--strategy=")
        {
            self.parse_strategy(value)?;
            false
        } else if builds(command)
            && let Some(value) = bazel_rs::configuration::boolean_option(arg, "sandbox_debug")
        {
            // SandboxOptions.java:87-97.
            self.spawn.sandbox_debug = value?;
            false
        } else if builds(command)
            && let Some(value) =
                bazel_rs::configuration::boolean_option(arg, "sandbox_default_allow_network")
                    .or_else(|| {
                        bazel_rs::configuration::boolean_option(
                            arg,
                            "experimental_sandbox_default_allow_network",
                        )
                    })
        {
            // SandboxOptions.java:283-292: network access is allowed in
            // sandboxes by default (the old name is accepted like Bazel's).
            self.spawn.sandbox_default_allow_network = value?;
            false
        } else if builds(command)
            && matches!(
                arg,
                "--sandbox_writable_path" | "--sandbox_block_path" | "--sandbox_tmpfs_path"
            )
        {
            let path = next.ok_or_else(|| format!("{arg} requires a value"))?;
            self.parse_sandbox_path(arg, path)?;
            true
        } else if builds(command)
            && let Some(value) = arg
                .strip_prefix("--sandbox_writable_path=")
                .or_else(|| arg.strip_prefix("--sandbox_block_path="))
                .or_else(|| arg.strip_prefix("--sandbox_tmpfs_path="))
        {
            self.parse_sandbox_path(arg, value)?;
            false
        } else if builds(command)
            && let Some(value) = bazel_rs::configuration::boolean_option(arg, "run_validations")
                .or_else(|| {
                    bazel_rs::configuration::boolean_option(arg, "experimental_run_validations")
                })
        {
            // BuildRequestOptions.runValidationActions (BuildRequestOptions.
            // java:178-190; experimental_run_validations is the old name).
            self.output.run_validations = value?;
            false
        } else if builds(command)
            && let Some(value) =
                bazel_rs::configuration::boolean_option(arg, "experimental_use_validation_aspect")
        {
            // The ASPECT ValidationMode of OutputGroupInfo.determineOutputGroups
            // (OutputGroupInfo.java:246-250): validation outputs are promoted
            // by the ValidateTarget aspect (N58, rules/core/ValidateTarget.java).
            self.output.use_validation_aspect = value?;
            false
        } else if builds(command) && arg == "--show_result" {
            let value = next.ok_or("--show_result requires a value")?;
            self.show_result = value.parse().map_err(|_| {
                format!("While parsing option --show_result={value}: not an integer")
            })?;
            true
        } else if let Some(value) = arg
            .strip_prefix("--show_result=")
            .filter(|_| builds(command))
        {
            self.show_result = value.parse().map_err(|_| {
                format!("While parsing option --show_result={value}: not an integer")
            })?;
            false
        } else if builds(command) && arg == "--aspects" {
            // BuildRequestOptions.aspects (CommaSeparatedOptionListConverter,
            // allowMultiple); the label%name parsing happens with the
            // repository mapping, in the engine.
            let value = next.ok_or("--aspects requires a value")?;
            self.aspects.extend(
                value
                    .split(',')
                    .filter(|token| !token.is_empty())
                    .map(str::to_owned),
            );
            true
        } else if builds(command) && arg == "--aspects_parameters" {
            // BuildRequestOptions.aspectsParameters (AssignmentConverter).
            let value = next.ok_or("--aspects_parameters requires a value")?;
            let (key, assigned) = value.split_once('=').ok_or_else(|| {
                format!("While parsing option --aspects_parameters={value}: expected key=value")
            })?;
            self.aspects_parameters
                .push((key.to_owned(), assigned.to_owned()));
            true
        } else if let Some(value) = arg.strip_prefix("--aspects=").filter(|_| builds(command)) {
            self.aspects.extend(
                value
                    .split(',')
                    .filter(|token| !token.is_empty())
                    .map(str::to_owned),
            );
            false
        } else if let Some(value) = arg
            .strip_prefix("--aspects_parameters=")
            .filter(|_| builds(command))
        {
            let (key, assigned) = value.split_once('=').ok_or_else(|| {
                format!("While parsing option --aspects_parameters={value}: expected key=value")
            })?;
            self.aspects_parameters
                .push((key.to_owned(), assigned.to_owned()));
            false
        } else if builds(command) && arg == "--output_groups" {
            // CommaSeparatedOptionListConverter with allowMultiple: repeated
            // flags accumulate, empty segments drop out
            // (BuildRequestOptions.java:160-177).
            let value = next.ok_or("--output_groups requires a value")?;
            self.output.parse_groups(value);
            true
        } else if let Some(value) = arg
            .strip_prefix("--output_groups=")
            .filter(|_| builds(command))
        {
            self.output.parse_groups(value);
            false
        } else if let Some(result) = builds(command).then(|| self.runfiles.parse(arg)).flatten() {
            result?;
            false
        } else if let Some(used) = (command == "run" || command == "test")
            .then(|| self.parse_run(arg, next))
            .transpose()?
            .flatten()
        {
            used
        } else if let Some(used) = self.credentials.parse(arg, next) {
            used?
        } else if let Some(used) = self.remote.parse(arg, next) {
            used?
        } else if command == "test" && self.test.parse_keep_going(arg)? {
            false
        } else if let Some(used) = builds(command)
            .then(|| self.test.parse(arg, next))
            .transpose()?
            .flatten()
        {
            used
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

    /// One `--strategy=<mnemonic>=<strategies>` (ExecutionOptions.java:89-93,
    /// StringToStringListConverter: `name=value1[,value2]*`; a repeated flag
    /// for the same mnemonic replaces it — a multimap put,
    /// BazelStrategyModule.java:84-89 into SpawnStrategyRegistry.java:
    /// 359-361, which keeps one list per mnemonic).
    fn parse_strategy(&mut self, value: &str) -> Result<(), String> {
        let Some((name, list)) = value.split_once('=') else {
            return Err(format!(
                "--strategy requires a 'mnemonic=strategy[,strategy...]' value, got {value:?}"
            ));
        };
        let list = strategy_list(list)?;
        if list.is_empty() {
            return Err(format!(
                "--strategy requires a 'mnemonic=strategy[,strategy...]' value, got {value:?}"
            ));
        }
        self.spawn.strategy.retain(|(other, _)| other != name);
        self.spawn.strategy.push((name.to_owned(), list));
        Ok(())
    }

    /// One `--sandbox_{writable,block,tmpfs}_path` value (SandboxOptions;
    /// allowMultiple, absolute paths in Bazel; a relative path resolves
    /// against the client cwd as AbsolutePathFragmentConverter would).
    fn parse_sandbox_path(&mut self, arg: &str, value: &str) -> Result<(), String> {
        if value.is_empty() {
            return Err(format!("{arg} requires a path"));
        }
        let path = std::path::Path::new(value);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| format!("{arg}: cannot resolve the client directory: {e}"))?
                .join(path)
        };
        match arg.split_once('=').map(|(name, _)| name).unwrap_or(arg) {
            "--sandbox_writable_path" => self.spawn.sandbox_writable_path.push(path),
            "--sandbox_block_path" => self.spawn.sandbox_block_path.push(path),
            "--sandbox_tmpfs_path" => self.spawn.sandbox_tmpfs_path.push(path),
            other => {
                return Err(format!(
                    "internal error: {other} is not a sandbox path option"
                ));
            }
        }
        Ok(())
    }
}

impl NativeOptions {
    /// One RunCommand.RunOptions option (or `--run_under`/`--shell_executable`):
    /// `Some(used_next)` when it is one.
    fn parse_run(&mut self, arg: &str, next: Option<&str>) -> Result<Option<bool>, String> {
        let run = &mut self.run;
        for (name, slot) in [
            ("run", &mut run.run),
            ("run_in_cwd", &mut run.run_in_cwd),
            ("omit_run_args", &mut run.omit_run_args),
        ] {
            if let Some(value) = bazel_rs::configuration::boolean_option(arg, name) {
                *slot = value?;
                return Ok(Some(false));
            }
        }
        for name in ["script_path", "run_env", "run_under", "shell_executable"] {
            let flag = format!("--{name}");
            let (value, used) = if arg == flag {
                (
                    next.ok_or_else(|| format!("{flag} requires a value"))?,
                    true,
                )
            } else if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
                (value, false)
            } else {
                continue;
            };
            match name {
                "script_path" => run.script_path = Some(value.to_owned()),
                // Converters.EnvVarsConverter: N42's parser (crate::action_env).
                "run_env" => {
                    run.run_env
                        .push(bazel_rs::action_env::EnvVar::parse(value).map_err(|error| {
                            format!("While parsing option --run_env={value}: {error}")
                        })?)
                }
                "run_under" => {
                    // RunUnderConverter (analysis/config/RunUnderConverter.java:33-57).
                    if value.split_whitespace().next().is_none() {
                        return Err(format!(
                            "While parsing option --run_under={value}: Empty command"
                        ));
                    }
                    run.run_under = Some(value.to_owned());
                }
                _ => run.shell = value.to_owned(),
            }
            return Ok(Some(used));
        }
        Ok(None)
    }
}

/// `--aspects` label parsing (BuildView.createTopLevelAspectKeys,
/// BuildView.java:549-615): `{bzl-file-label}%{name}`, with the legacy
/// relative spelling converted to an absolute label; a native aspect name
/// (ValidateTarget) stays as its class name.
fn resolve_aspects(
    engine: &bazel_rs::engine::Engine,
    aspects: &[String],
) -> Result<Vec<String>, String> {
    let mut resolved = Vec::new();
    for aspect in aspects {
        let Some(delimiter) = aspect.find('%') else {
            // A native aspect class name; only the validation aspect exists
            // natively (BuildRequest.getAspects appends ValidateTarget).
            if aspect == "ValidateTarget" {
                resolved.push(aspect.clone());
            } else {
                return Err(format!("Aspect '{aspect}' is unknown"));
            }
            continue;
        };
        let mut bzl = aspect[..delimiter].to_owned();
        if !bzl.starts_with("//") && !bzl.starts_with('@') {
            // "Legacy" behavior of the '--aspects' parameter: a relative
            // `foo/bar.bzl%a` becomes `//foo:bar.bzl%a` (BuildView.java:566-583).
            if let Some(stripped) = bzl.strip_prefix('/') {
                bzl = stripped.to_owned();
            }
            bzl = match bzl.rfind('/') {
                Some(slash) => format!("//{}:{}", &bzl[..slash], &bzl[slash + 1..]),
                None => format!("//:{bzl}"),
            };
            if !bzl.ends_with(".bzl") {
                bzl.push_str(".bzl");
            }
        }
        let name = &aspect[delimiter + 1..];
        let label = engine
            .parse_option_label(&bzl)
            .map_err(|error| format!("Invalid aspect '{aspect}': {error}"))?;
        resolved.push(format!("{label}%{name}"));
    }
    Ok(resolved)
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
    let result = run();
    // N62 breakdown aid (BAZEL_RS_TIMING_MODULES=1): report before any
    // exit path, idempotent, silent unless enabled.
    bazel_rs::loading::module_timings::report();
    if let Err(error) = result {
        let error = located(&error);
        eprintln!("bazel-rs: {error}");
        // A sandbox setup failure is an environmental failure of the
        // strategy (Bazel's EnvironmentalExecException class): exit code 36.
        // The typed flag, never the error text: an action's own output could
        // contain the phrase.
        let code = if bazel_rs::sandbox::setup_failure_occurred() {
            36
        } else {
            1
        };
        std::process::exit(code);
    }
}

/// The build summary's execution-mode note: which strategy executed the
/// actions (N59). "sandboxed" when every executed spawn ran under the sandbox
/// strategy, "not sandboxed" otherwise (a --spawn_strategy=standalone request
/// or a kernel without Landlock support).
fn execution_note() -> &'static str {
    if bazel_rs::sandbox::all_sandboxed() {
        "sandboxed"
    } else {
        "not sandboxed"
    }
}
