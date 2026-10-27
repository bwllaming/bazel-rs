use bazel_rs::{bazelrc, engine::Engine, test_runner::label_print, workspace};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// The process-wide Rust allocator: jemalloc (feature "jemalloc", on by
// default; README "Global allocator"). No `unsafe` here: the GlobalAlloc impl
// lives in tikv-jemallocator, so unsafe_code stays "deny".
#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const HELP: &str = "bazel-rs — experimental native build engine (NOT full Bazel parity)

Usage:
  bazel-rs [STARTUP...] [--cache-dir=PATH] query [QUERY-OPTION...] EXPRESSION
  bazel-rs [STARTUP...] [--cache-dir=PATH] cquery [BUILD/QUERY OPTIONS...] EXPRESSION
  bazel-rs [STARTUP...] [--cache-dir=PATH] config --output=json --dump_all
  bazel-rs [STARTUP...] [--cache-dir=PATH] build [--jobs=N] [CONFIG...] TARGET...
  bazel-rs [STARTUP...] [--cache-dir=PATH] run [BUILD OPTIONS...] [RUN OPTIONS...] TARGET [-- ARGS...]
  bazel-rs [STARTUP...] [--cache-dir=PATH] test [BUILD OPTIONS...] [TEST OPTIONS...] TARGET...
  bazel-rs [STARTUP...] [--cache-dir=PATH] info [BUILD OPTIONS...] [--show_make_env] [KEY...]
  bazel-rs [STARTUP...] [--cache-dir=PATH] fetch [BUILD OPTIONS...] [TARGET... | --repo=@NAME... | --all | --configure] [--force]
  bazel-rs [STARTUP...] [--cache-dir=PATH] clean [--expunge] [--async | --expunge_async]
  bazel-rs [STARTUP...] shutdown [--iff_heap_size_greater_than=N]
  bazel-rs [STARTUP...] [--cache-dir=PATH] gc [--dry_run] [GC OPTIONS...]
  bazel-rs [STARTUP...] [--cache-dir=PATH] mod SUBCOMMAND [MOD OPTIONS...] [ARG...]
  bazel-rs [STARTUP...] canonicalize-flags [--for_command=COMMAND] -- FLAG...
  bazel-rs version | --version | capabilities | help [COMMAND|startup_options|target-syntax|info-keys|native]

Cache garbage collection (docs/CACHE_GC.md): every command ends with a bounded collection when
  the last one is --experimental_disk_cache_gc_idle_delay (default 1h) old and no other bazel-rs
  process uses the cache: entries unused for --experimental_disk_cache_gc_max_age (default 60d),
  repository trees unused for --repo_contents_cache_gc_max_age (default 14d), least recently used
  entries over --experimental_disk_cache_gc_max_size (default 10% of free disk, 10-64 GiB; 0: off),
  abandoned temporaries and deleted worktrees' state. `gc` collects now and reports;
  BAZEL_RS_CACHE_GC=off disables the automatic collection.
Startup: --[no]system_rc --[no]workspace_rc --[no]home_rc --[no]ignore_all_rc_files
  --[no]quiet (only errors and command output; docs/STARTUP_QUIET.md)
  --bazelrc=FILE --output_base=PATH; rc files are evaluated like Bazel 9.3 (--config, common/always,
  --announce_rc; docs/BAZELRC.md); unmodelled rc options fail naming file:line.
Patterns: //..., //package/..., //package:all, //package:*, //package:target, :target
Query: the Bazel 9.3 query language (deps, rdeps, allpaths, somepath, attr, filter,
  kind, labels, tests, buildfiles, loadfiles, siblings, some, visible, set, let,
  same_pkg_direct_rdeps, executables, union/+, intersect/^, except/-) with
  --output=label|label_kind|build|package|location|minrank|maxrank|graph,
  --order_output, --[no]implicit_deps, --[no]tool_deps, --[no]keep_going (docs/QUERY.md).
Cquery: configured/provider analysis without actions or workspace status; label,
  label_kind, proto, streamed_proto; --transitions=lite, --output_file, --universe_scope,
  deps/rdeps/kind/filter/attr/labels/some/set/let/config and set algebra. Config dumps
  the last successful cquery's actual native configurations. Starlark output is not
  implemented; target-determinator needs -filter-incompatible-targets=false (docs/CQUERY.md).
Info: the Bazel 9.3 items (bazel-bin, execution_root, output_base, ...) mapped to the
  native layout; built outputs are linked below <state>/execroot/_main, <state> the
  workspace's state directory (the output base, see Cache below; docs/INFO.md).
Fetch: materializes repositories like a build would (docs/COMMANDS.md): the repositories
  the targets' loading and analysis need (build --nobuild), the named --repo=@apparent /
  @@canonical ones, or every repository (--all, the default) / every configure=True one
  (--configure); --force executes their repository rules again.
Clean: deletes this workspace's output tree and per-workspace state below
  <cache>/worktrees/<name>-<hash>/ (not the hidden lockfile); --expunge deletes that whole
  directory. The shared store (repositories, action cache, test results) is never touched.
  Shutdown: no server exists, so it does nothing (docs/COMMANDS.md).
Build options: --jobs=N or --jobs N (positive integer; default: available CPUs, max 32)
Memory ceiling: --memory-limit=<MB> (build/run/test/query) or BAZEL_RS_MAX_MEMORY_MB
  caps bazel-rs's own RSS (watchdog only, exit 33 = Bazel's OOM; no RLIMIT_AS, so
  child actions keep host limits); flag wins over env; unset = the smaller of
  16 GB and 50% of RAM. Prefer the env var: --memory-limit in a shared .bazelrc
  breaks genuine Bazel (docs/MEMORY.md).
Repository verify: --verify-repo-tampering forces full re-hash of warm
  repository publications (default: stat-fingerprint trust, Bazel's model;
  env BAZEL_RS_VERIFY_REPOS=full is equivalent).
Cache: --cache-dir=PATH, else <workspace>/bazel-rs-out; in a linked git worktree
  the main checkout's (<main>/bazel-rs-out: repositories, actions, test results
  are shared). Each workspace's own state (analysis snapshots, test result index,
  hidden lockfile, `run` of tests) is <cache>/worktrees/<name>-<hash>/
  (docs/WORKTREES.md). --output_base replaces this state directory, not the shared cache.
Digest memo: settled files' SHA-256 by path and stat key persist in
  <cache>/digests/v2.dmemo across commands; BAZEL_RS_DIGEST_MEMO=off bypasses
  it while set (its records stay); deleting <cache>/digests/ drops them
  (docs/DIGEST_MEMO.md).
Analysis snapshot (on by default): a build/test/run saves its analysis in
  <state>/snapshots/ (the workspace's state directory above) and skips loading
  and analysis while every recorded input is unchanged. BAZEL_RS_SNAPSHOT=off
  (or 0, false) turns it off: no lookup, no recording, nothing written.
  BAZEL_RS_SNAPSHOT=verify (when a stale result is suspected) also runs the full
  analysis on every hit, logs a difference to <state>/snapshots/verify.log and
  then executes the full analysis's plan; BAZEL_RS_SNAPSHOT_EXPLAIN=1 says why
  it missed or was not saved (docs/ANALYSIS_SNAPSHOT.md).
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
Built-in modules: like Bazel 9.3, @bazel_tools and its MODULE closure are always injected
  (query/build/run/test). Native-only --experimental-builtin-modules=false selects the explicit-module
  profile (no @bazel_tools); --experimental-builtin-modules[=true] is the default (a no-op).
  mod: graph, deps, all_paths, path, explain, show_repo, show_extension, dump_repo_mapping,
  tidy with Bazel 9.3's options (docs/MOD_COMMAND.md).
Lockfile: --lockfile_mode=update (default)|refresh|error|off reads/writes MODULE.bazel.lock
  (registryFileHashes, selectedYankedVersions; moduleExtensions/facts entries computed,
  reused and written with Bazel's digests since N52; docs/REGISTRY_LOCKFILE.md).
  --allow_yanked_versions=MODULE@VERSION,... (repeatable).
  Native-only --native-lockfile=workspace|readonly|/abs/path: readonly never writes a lockfile
  (the hidden one included); a path is a private lockfile read instead of (seeded from) the
  workspace one and written instead.
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
    // a bazel command and shares none of the client state (no diagnostics
    // either: its stderr is the action's).
    if args
        .first()
        .is_some_and(|arg| arg == bazel_rs::sandbox::WORKER_ARG)
    {
        std::process::exit(bazel_rs::sandbox::worker_main(&args[1..]));
    }
    bazel_rs::perf_mark("main.start");
    let line = bazelrc::split_command_line(args)?;
    let cache = line.cache_dir.clone().map(PathBuf::from);
    let command = line.command.clone().unwrap_or_else(|| "help".into());
    let rest = line.command_args.clone();
    match command.as_str() {
        // HelpCommand (crate::help); the native notes are `help native`.
        "help" | "--help" | "-h" => match bazel_rs::help::render(&rest, HELP) {
            Ok(text) => {
                print!("{text}");
                return Ok(());
            }
            Err(error) => fail_with(2, &error),
        },
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
        "shutdown" => return shutdown_command(&line, &path_to_binary),
        // CanonicalizeCommand (crate::canonicalize); rc options do not
        // reach the flags to canonicalize.
        "canonicalize-flags" => match bazel_rs::canonicalize::run(&rest) {
            Ok(text) => {
                print!("{text}");
                return Ok(());
            }
            Err(error) => fail_with(2, &error),
        },
        "query" | "cquery" | "config" | "build" | "mod" | "run" | "test" | "info" | "clean"
        | "fetch" | "gc" => {}
        "vendor" => {
            return Err(
                "the vendor command is not implemented in the native profile (Bazel 9.3 `vendor --vendor_dir=DIR` copies the fetched repositories into DIR for later builds to read); `bazel-rs fetch` materializes repositories into the shared repository store"
                    .to_owned(),
            );
        }
        // BlazeCommandDispatcher: a name that is no Bazel command at all
        // (`sync` was removed with WORKSPACE support) — exit 2.
        _ if !command.starts_with('-') && !bazelrc::is_bazel_command(&command) => {
            eprintln!("Command '{command}' not found. Try 'bazel-rs help'.");
            exit(2);
        }
        _ => {
            return Err(format!(
                "unsupported command or startup option {command:?}; run --help for the native subset"
            ));
        }
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let root = workspace::workspace_root(&cwd)?;
    // `gc` applies the policy a `build` of this workspace would: its rc
    // options are build's.
    let rc_command = if command == "gc" {
        "build"
    } else {
        command.as_str()
    };
    let options = evaluate_rc(&line, rc_command, &cwd, &root, &path_to_binary)?;
    if command == "config" {
        return config_command(&line, &options, &cwd, &root);
    }
    if command == "info" {
        return info_command(&line, &options, &cwd, root);
    }
    if command == "gc" {
        return gc_command(&line, &options, &cwd, &root);
    }
    if command == "clean" {
        return clean_command(&line, &options, &cwd, root);
    }
    // FetchCommand inherits the test (and so the build) options.
    let option_command = if command == "fetch" {
        "test"
    } else {
        command.as_str()
    };
    let mut fetch = FetchOptions::default();
    // --jobs default: available CPUs, max 32 (the "N26" bound — the
    // action worker pool never exceeds 32 however many CPUs the host
    // has; explicit --jobs=N is kept exactly). Memory-wise the action
    // workers are cheap beside loading: each runs child processes
    // (whose RSS the cap does not count) and holds only its action
    // record; the in-process heap pool is the loading workers, bounded
    // separately by the cap below.
    let mut jobs = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(32);
    let loading_phase_threads: Option<usize>;
    let memory_limit_flag_mb: Option<u64>;
    let mut resolver_options;
    let repository_downloads_enabled;
    let configuration_options;
    let query_options;
    let cquery_options;
    let lockfile_mode;
    let lockfile_target;
    let action_dump: Option<PathBuf>;
    let allowed_yanked_flags;
    let raw_registries;
    let credential_options;
    let remote_options;
    let mut spawn_options;
    let worker_options;
    let runfiles_options;
    let workspace_status;
    let mut no_build;
    let output_selection;
    let aspects;
    let aspects_parameters;
    let show_result;
    let run_options;
    let test_options;
    let mut deferred = None;
    let ui_options;
    let bep_options;
    let profile_options;
    let exec_log_options;
    let mod_options;
    let rc_options;
    let rest: Vec<String> = {
        let first_options = first_pass_flag_alias_options(&options, &command);
        // Errors in Bazel's parse order (rc options, the command line, then
        // each --config expansion), values in priority order (an expansion
        // takes the place of its --config).
        // A command that analyzes in a MODULE.bazel workspace ignores
        // unrecognized options in its first parsing round and reports them
        // when it reparses with the main repository mapping, after the rc
        // notes (OptionsParserImpl.java:811-815, BlazeCommandDispatcher.java:738-755, 1022).
        let second_round = analyzes(option_command) && root.join("MODULE.bazel").exists();
        for (step, position) in &first_options.steps {
            let result = match step {
                bazelrc::Step::Fail(error) => Err(error.clone()),
                bazelrc::Step::Unit(unit) => parse_command_option(
                    &mut NativeOptions::new(option_command),
                    &mut FetchOptions::default(),
                    &command,
                    unit,
                ),
            };
            if let Err(error) = result {
                let unit = match step {
                    bazelrc::Step::Unit(unit) => Some(unit),
                    _ => None,
                };
                let defer = second_round
                    && alias_candidate_error(&error, unit)
                        .map_err(|error| early_exit(&options, &error, *position))?;
                if defer {
                    deferred.get_or_insert((error, *position));
                } else {
                    return Err(early_exit(&options, &error, *position));
                }
            }
        }
        let mut native = NativeOptions::new(option_command);
        for unit in &first_options.priority {
            if let Err(error) = parse_command_option(&mut native, &mut fetch, &command, unit)
                && !(second_round && alias_candidate_error(&error, Some(unit))?)
            {
                return Err(error);
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
        // `auto` in the native parser is the LOADING_AUTO marker: not
        // an explicit count (main bounds the auto expansion by the cap).
        loading_phase_threads = native
            .loading_phase_threads
            .filter(|threads| *threads != LOADING_AUTO);
        native.resolver.isolated_extension_usages = native.rc.isolated_extension_usages;
        resolver_options = native.resolver;
        repository_downloads_enabled = native.downloads;
        configuration_options = native.configuration;
        query_options = native.query;
        cquery_options = native.cquery;
        rc_options = native.rc;
        credential_options = native.credentials;
        remote_options = native.remote;
        spawn_options = native.spawn;
        worker_options = native.worker;
        runfiles_options = native.runfiles;
        workspace_status = native.workspace_status;
        no_build = native.no_build;
        ui_options = native.ui;
        bep_options = native.bep;
        profile_options = native.profile;
        exec_log_options = native.exec_log;
        output_selection = native.output.clone();
        aspects = native.aspects;
        aspects_parameters = native.aspects_parameters;
        show_result = native.show_result;
        run_options = native.run;
        test_options = native.test;
        mod_options = native.module;
        options.residue.iter().map(|arg| arg.text.clone()).collect()
    };
    // ModOptions' converters fail the command before it runs (exit 2).
    if let Some(error) = mod_options.error() {
        fail_with(2, error);
    }
    // FetchCommand.validateOptions, then the request: target patterns are
    // a `build --nobuild` (TargetFetcher), anything else fetches
    // repositories once the repository set exists (below).
    let fetch_mode = if command == "fetch" {
        if !fetch.fetch {
            fail_with(2, "You cannot run fetch with --nofetch");
        }
        let requested = [
            fetch.all,
            fetch.configure,
            !fetch.repos.is_empty(),
            !rest.is_empty(),
        ];
        if requested.iter().filter(|given| **given).count() > 1 {
            fail_with(2, "Only one fetch option can be provided for fetch command");
        }
        if fetch.force {
            bazel_rs::repository_store::set_force_fetch();
        }
        if rest.is_empty() {
            Some(FetchMode::Repositories)
        } else {
            no_build = true;
            FETCH_TARGETS.store(true, std::sync::atomic::Ordering::Relaxed);
            Some(FetchMode::Targets)
        }
    } else {
        None
    };
    let command = if fetch_mode == Some(FetchMode::Targets) {
        "build".to_owned()
    } else {
        command
    };
    // The interactive progress display (crate::progress, docs/PROGRESS.md):
    // `build`, `test` and `run` on a terminal (Bazel's curses mode), never
    // otherwise; `fetch TARGET...` is a `build` here (TargetFetcher), the
    // other `fetch` forms, `clean` and `shutdown` show none. Everything
    // printed from here on reaches the terminal through its thread; every
    // exit path below ends it ([`exit`]).
    if let Some(config) = bazel_rs::progress::decide(
        &command,
        &ui_options,
        &bazel_rs::progress::TerminalEnvironment::from_process(
            is_standard_terminal(),
            usize::try_from(terminal_columns()).unwrap_or(80),
        ),
    ) && let Err(error) = bazel_rs::progress::start(config)
    {
        eprintln!("bazel-rs: warning: the progress display is off: {error}");
    }
    // Memory ceiling (native-only safety valve, crate::memory_limit):
    // --memory-limit=<MB> wins over BAZEL_RS_MAX_MEMORY_MB (Bazel's
    // CLI-over-env convention). Unset both = the default cap (the
    // smaller of 16 GB and 50% of MemTotal), announced once below.
    // Installed before any heavy work.
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
                    bazel_rs::note!(
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
            bazel_rs::note!(
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
    // The cache root (crate::worktree, docs/WORKTREES.md): `--cache-dir`,
    // else found from the git metadata above the workspace — a linked git
    // worktree shares its main checkout's `bazel-rs-out`, anything else
    // keeps `<workspace>/bazel-rs-out`. The workspace's own state (analysis
    // snapshots, test result index, hidden lockfile, `run` execution roots
    // of tests) lives in `<cache>/worktrees/<name>-<hash>/` either way.
    let explicit_cache = cache.is_some();
    let layout = bazel_rs::worktree::detect(&root);
    let cache = cache.unwrap_or_else(|| layout.default_cache());
    let cache = if cache.is_absolute() {
        cache
    } else {
        cwd.join(cache)
    };
    bazel_rs::worktree::install(&root);
    if !explicit_cache && let Some(note) = layout.legacy_cache_note(&cache) {
        bazel_rs::note!("{note}");
    }
    // The build event files, the profile and the execution log (all off by
    // default; docs/BEP.md, docs/PROFILE.md).
    if builds(&command) {
        install_reports(
            ReportContext {
                command: &command,
                line: &line,
                options: &options,
                cwd: &cwd,
                root: &root,
                cache: &cache,
            },
            &bep_options,
            &profile_options,
            &exec_log_options,
        );
    }
    // The output base lock, shared (docs/COMMANDS.md): a running `clean`
    // of this workspace is waited for, and `clean` waits for this command.
    hold_output_base_lock(&cache, &root, &command);
    // The persistent file-digest memo of this cache (crate::digest_memo),
    // saved by `digest_memo::finish` when the command ends.
    bazel_rs::digest_memo::install(&cache);
    // Garbage collection (crate::cache_gc, docs/CACHE_GC.md): the command
    // holds the cache's lock shared until it exits (a collection takes it
    // exclusively), records which worktree uses the cache, and collects at
    // its end under its options.
    bazel_rs::cache_gc::install(&cache);
    bazel_rs::cache_gc::record_workspace(&root);
    bazel_rs::cache_gc::set_options(&rc_options.gc);
    // print()'s DEBUG lines name main-repository `.bzl` files by path.
    bazel_rs::debug_print::set_workspace(&root);
    // --attempt_to_print_relative_paths (LocationPrinter, crate::location_printer).
    let _ = PRINTER.set(bazel_rs::location_printer::LocationPrinter::new(
        rc_options.attempt_to_print_relative_paths,
        root.clone(),
        cache.clone(),
    ));
    announce(&options);
    if let Some((error, position)) = deferred
        && !analyzes(&command)
    {
        for warning in &options.warnings[..position.warnings.min(options.warnings.len())] {
            warn(warning);
        }
        return Err(error);
    }
    // Credential helpers and the remote cache (N41) for every command.
    let remote_enabled = setup_credentials_and_remote(&root, &credential_options, &remote_options)?;
    // RemoteModule registers the `remote` strategy with any remote cache or
    // executor; only an executor makes it usable and the default.
    spawn_options.remote_registered = remote_enabled;
    spawn_options.remote_executor = !remote_options.executor.is_empty();
    // The spawn strategy and sandbox options (N59) for every executing
    // command. Unknown strategy identifiers are rejected once here, at
    // startup, with Bazel's STRATEGY_NOT_FOUND text and exit code 2
    // (SpawnStrategyRegistry.toStrategy:718-727).
    if let Err(error) = bazel_rs::sandbox::validate(&spawn_options) {
        eprintln!("ERROR: {error}");
        exit(2);
    }
    bazel_rs::sandbox::install(spawn_options);
    // Persistent workers (crate::worker): stopped when the command ends.
    bazel_rs::worker::install(worker_options);
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
    let rest = if command == "query" || command == "cquery" {
        let query = if command == "cquery" {
            &cquery_options.common
        } else {
            &query_options
        };
        match bazel_rs::engine::query::read_query(&rest, query) {
            Some(query) => vec![query],
            None => exit(2),
        }
    } else {
        rest
    };
    if (command == "build" || command == "test") && rest.is_empty() {
        return Err(format!("{command} requires at least one target"));
    }
    // TestCommand.exec (runtime/commands/TestCommand.java:98-107).
    if command == "test" && test_options.output == bazel_rs::test_command::TestOutput::Streamed {
        bazel_rs::note!(
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
    // Bazel reads a workspace-root `platform_mappings` file by default
    // (PlatformOptions.java:172-187); platform mappings are not modelled.
    if analyzes(&command) && root.join("platform_mappings").exists() {
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
    // The analysis-snapshot check point (PLAN-L2 §1a): everything above is
    // bootstrap (part of the snapshot key G). With BAZEL_RS_SNAPSHOT=record
    // the recording window opens here and closes at the end of planning;
    // with BAZEL_RS_SNAPSHOT=on — the default (L2-W7); `off` skips all of
    // this — (build, test, run) a snapshot whose inputs
    // are all unchanged is a hit — what planning printed is replayed and
    // the saved plan executes — and a miss records and captures this full
    // analysis to save it (crate::snapshot). BAZEL_RS_SNAPSHOT=verify is
    // `on`, and a hit also runs the full analysis below in its shadow, which
    // plan_request compares with the hit before the hit executes
    // (crate::snapshot::verify).
    let mut snapshot = bazel_rs::snapshot::Scope::default();
    // `fetch` neither looks up nor saves an analysis snapshot: it must
    // materialize the repositories (a hit would skip them).
    let snapshot_mode = if fetch_mode.is_some() {
        bazel_rs::snapshot::Mode::Off
    } else {
        bazel_rs::snapshot::Mode::from_env()
    };
    let cquery_expression = if command == "cquery" {
        Some(cquery_options.expression(&rest[0])?)
    } else {
        None
    };
    let mut cquery_session = None;
    if let Some(expression) = &cquery_expression {
        let patterns = cquery_options.universe(expression);
        let relative = relative_directory(&root, &cwd)?;
        let allow_yanked = std::env::var("BZLMOD_ALLOW_YANKED_VERSIONS").ok();
        let started = bazel_rs::cquery_state::start(
            bazel_rs::snapshot::key::Inputs {
                command: "cquery",
                workspace: &root,
                cache: &cache,
                cwd: &cwd,
                relative_directory: &relative,
                current_package: &current_package,
                startup: &line.startup_args,
                options: &options.priority,
                targets: &patterns,
                allow_yanked: allow_yanked.as_deref(),
                full_validation: bazel_rs::repository_store::verify_mode()
                    == bazel_rs::repository_store::VerifyMode::Full,
            },
            bazel_rs::snapshot::hit::ReplayInputs {
                options: resolver_options.clone(),
                environment: repository_environment.clone(),
                downloads_enabled: repository_downloads_enabled,
            },
        );
        cquery_session = started.session;
        if let Some(graph) = started.hit {
            return cquery_output(&graph, expression, &cquery_options, &relative, &cache);
        }
    }
    match snapshot_mode {
        bazel_rs::snapshot::Mode::Record => {
            snapshot.record = bazel_rs::snapshot::record::start(
                &command,
                &repository_environment,
                bazel_rs::snapshot::telemetry::Context {
                    cache: cache.clone(),
                    mode: bazel_rs::snapshot::telemetry::MODE_RECORD,
                    command: command.clone(),
                    targets: rest.clone(),
                },
            );
        }
        mode @ (bazel_rs::snapshot::Mode::On | bazel_rs::snapshot::Mode::Verify)
            if builds(&command) =>
        {
            let verify = mode == bazel_rs::snapshot::Mode::Verify;
            let targets = if command == "run" {
                &rest[..rest.len().min(1)]
            } else {
                &rest[..]
            };
            let canonical = |path: &Path| std::fs::canonicalize(path).unwrap_or(path.to_path_buf());
            let (key_root, key_cache) = (canonical(&root), canonical(&cache));
            let key_relative_directory = relative_directory(&root, &cwd).ok();
            let allow_yanked = std::env::var("BZLMOD_ALLOW_YANKED_VERSIONS").ok();
            match key_relative_directory {
                None => {}
                Some(key_relative_directory) => {
                    match bazel_rs::snapshot::check(bazel_rs::snapshot::CheckInputs {
                        key: bazel_rs::snapshot::key::Inputs {
                            command: &command,
                            workspace: &key_root,
                            cache: &key_cache,
                            cwd: &cwd,
                            relative_directory: &key_relative_directory,
                            current_package: &current_package,
                            startup: &line.startup_args,
                            options: &options.priority,
                            targets,
                            allow_yanked: allow_yanked.as_deref(),
                            full_validation: bazel_rs::repository_store::verify_mode()
                                == bazel_rs::repository_store::VerifyMode::Full,
                        },
                        cache: &cache,
                        replay: bazel_rs::snapshot::hit::ReplayInputs {
                            options: resolver_options.clone(),
                            environment: repository_environment.clone(),
                            downloads_enabled: repository_downloads_enabled,
                        },
                        workspace_status: workspace_status.clone(),
                        workspace: &root,
                        verify,
                    }) {
                        bazel_rs::snapshot::Checked::Hit(hit) if verify => {
                            // The full analysis runs below, in the hit's
                            // shadow; plan_request compares and hands the
                            // hit back for its own execution (or, on a
                            // mismatch, executes the full analysis's plan).
                            bazel_rs::perf_mark("snapshot.hit");
                            snapshot.verify = bazel_rs::snapshot::verify::start(
                                hit,
                                bazel_rs::snapshot::verify::Context {
                                    command: command.clone(),
                                    targets: targets.to_vec(),
                                    explain: bazel_rs::snapshot::explain(),
                                },
                            );
                        }
                        bazel_rs::snapshot::Checked::Hit(hit) => {
                            // The saved plan executes exactly like a planned
                            // one; first what planning printed.
                            bazel_rs::perf_mark("snapshot.hit");
                            bazel_rs::snapshot::marker(bazel_rs::snapshot::MARKER_HIT_END);
                            let mut snapshot = bazel_rs::snapshot::Scope::default();
                            let planned = planned_hit(hit, &mut snapshot);
                            let loading_threads = loading_threads_for_report(
                                &command,
                                loading_phase_threads,
                                &query_options,
                                memory_limit_mb,
                            );
                            return match command.as_str() {
                                "test" => test_tail(
                                    planned,
                                    Vec::new(),
                                    TestTail {
                                        cache: &cache,
                                        jobs,
                                        root: &root,
                                        runfiles: runfiles_options,
                                        options: &test_options,
                                        remote_enabled,
                                        pattern_failure: None,
                                        snapshot,
                                    },
                                ),
                                "run" => {
                                    let label = match &planned.request {
                                        Ok(bazel_rs::request::PlannedRequest {
                                            command:
                                                bazel_rs::request::RequestCommand::Run { label, .. },
                                            ..
                                        }) => label.clone(),
                                        _ => {
                                            return Err(
                                            "internal error: the snapshot of `run` holds no run target"
                                                .to_owned(),
                                        );
                                        }
                                    };
                                    run_tail(
                                        planned,
                                        &label,
                                        RunTail {
                                            residue: &rest[1..],
                                            cache: &cache,
                                            jobs,
                                            root: &root,
                                            cwd: &cwd,
                                            runfiles: runfiles_options,
                                            options: &run_options,
                                            remote_enabled,
                                            test: &test_options,
                                            pattern_failure: None,
                                            snapshot,
                                        },
                                    )
                                }
                                _ => build_tail(
                                    planned,
                                    BuildTail {
                                        cache: &cache,
                                        jobs,
                                        action_dump: action_dump.as_deref(),
                                        show_result,
                                        remote_enabled,
                                        loading_threads,
                                        memory_limit_mb,
                                        pattern_failure: None,
                                        snapshot,
                                        keep_going: test_options.keep_going,
                                        run: false,
                                    },
                                ),
                            };
                        }
                        bazel_rs::snapshot::Checked::Miss(save) => snapshot.save = save,
                    }
                }
            }
        }
        _ => {}
    }
    let session = bazel_rs::registry_session::SessionHandle::new(
        bazel_rs::registry_session::RegistrySession::open(
            &root,
            lockfile_mode,
            &lockfile_target,
            Some(cache.join("repositories")),
            allowed_yanked,
        )?
        .with_seeded_hidden_lockfile(
            bazel_rs::registry_session::hidden_lockfile_path(&cache, &root),
            &bazel_rs::worktree::hidden_lockfile_seeds(
                &cache,
                &layout,
                bazel_rs::lockfile::LOCKFILE_NAME,
                |workspace| {
                    bazel_rs::registry_session::legacy_hidden_lockfile_path(&cache, workspace)
                },
            ),
        )
        .with_spellings(
            resolver_options
                .registries
                .iter()
                .cloned()
                .zip(raw_registries.iter().cloned()),
        ),
    );
    resolver_options.registry_session = session.clone();
    snapshot.set_registry_session(session.clone());
    let _lockfile_writer = LockfileWriter::new(session);
    let ignore_dev_dependencies = resolver_options.ignore_dev_dependencies;
    let repositories = Arc::new(
        bazel_rs::repository::RepositorySet::with_options(root.clone(), resolver_options)
            .with_repository_downloads_enabled(repository_downloads_enabled)
            .with_source_cache(cache.join("repositories"))
            .with_repository_environment(repository_environment),
    );
    // Never dropped (L2-W4, PLAN-L2 §5): the repository set holds only
    // memory (the module graph, repository specifications, the `.bzl`
    // module cache), which the process exit frees anyway; its drop at the
    // end of the command cost about 80 ms on ~/code oauth2token.
    std::mem::forget(Arc::clone(&repositories));
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
    if fetch_mode == Some(FetchMode::Repositories) {
        return fetch_repositories(&repositories, &fetch, &lockfile_gate);
    }
    if command == "mod" {
        // ModCommand (crate::mod_command): stdout is the result, a failure
        // its ERROR event and exit code. The lockfile records a successful
        // module resolution also when the subcommand itself fails
        // (BazelLockFileModule.afterCommand).
        let mut stdout = Vec::new();
        let result = bazel_rs::mod_command::run(
            &repositories,
            &mod_options,
            &rest,
            ignore_dev_dependencies,
            &mut stdout,
        );
        if repositories.resolve_graph().is_ok() {
            lockfile_gate.open();
        }
        {
            use std::io::Write as _;
            let mut out = std::io::stdout().lock();
            out.write_all(&stdout).map_err(|error| error.to_string())?;
            out.flush().map_err(|error| error.to_string())?;
        }
        if let Err(failure) = result {
            drop(_lockfile_writer);
            fail_with(failure.code.exit_code(), &failure.message);
        }
        return Ok(());
    }
    // --embed_label is validated by its converter before anything runs
    // (WorkspaceStatusAction.OneLineStringConverter :216-233).
    bazel_rs::workspace_status::validate_embed_label(&workspace_status.embed_label)?;
    // W7: the engine (and the repository set it shares) is never torn
    // down: the process exits when the command ends, and every write
    // that must outlive it — the lockfile (`LockfileWriter`), the digest
    // memo (`digest_memo::finish`), the snapshot recording, test logs —
    // is explicit. Freeing the analysis state node by node only delays
    // the exit.
    let mut engine = std::mem::ManuallyDrop::new(Engine::with_repositories(repositories.clone()));
    engine.set_skip_teardown(true);
    engine.set_runfiles_options(runfiles_options);
    engine.set_workspace_status(workspace_status);
    engine.set_execute_build(!no_build && command != "cquery");
    engine.set_keep_going(test_options.keep_going && analyzes(&command));
    // --loading_phase_threads (LoadingPhaseThreadsOption.java, P2 of the
    // parallel-analysis program): both loaders' pattern expansion uses it
    // (Engine::expand_pattern; query resolves patterns through it too).
    // "auto" (the default) expands to the available processors BOUNDED
    // by the memory cap (perf follow-up 3: min(auto, cap/64 MB per
    // worker heap), floored at 2 — crate::memory_limit); an explicit
    // integer is the user's stated intent and is never rewritten.
    engine.set_loading_phase_threads(loading_threads_for_report(
        &command,
        loading_phase_threads,
        &query_options,
        memory_limit_mb,
    ));
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
    if analyzes(&command) {
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
        let (configuration_options, aliases) =
            resolve_flag_alias_options(&options, &repositories, &command, &labels)?;
        let mut native = configuration_options.configuration(&labels)?;
        native.native.flag_aliases = aliases;
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
        let top = bazel_rs::configuration::Configuration::new(native.native, starlark);
        // A `--cpu` that is not a valid output directory component is
        // Bazel's InvalidMnemonicException for the top-level configuration.
        bazel_rs::config_registry::validate_top(&top)?;
        engine.set_configuration(top);
    }
    // TargetPattern.Parser's relativeDirectory is the working directory below
    // the workspace root, whether or not it is a package
    // (cmdline/TargetPattern.java:783-789).
    let relative_directory = relative_directory(&root, &cwd)?;
    if let Some(expression) = cquery_expression {
        let graph =
            engine.cquery_graph(&cquery_options.universe(&expression), &relative_directory)?;
        for warning in engine.take_warnings() {
            warn(&warning);
        }
        // The query graph owns no provider/analysis heaps. Unlike the CLI's
        // exit-only build path, encoding/export still has substantial work to
        // do; release the dead engine before allocating the snapshot buffers.
        drop(std::mem::ManuallyDrop::into_inner(engine));
        bazel_rs::perf_mark("cquery.engine_released");
        lockfile_gate.open();
        if let Some(session) = cquery_session {
            session.finish(&graph, &_lockfile_writer.0)?;
        }
        bazel_rs::observed_fs::close_window();
        cquery_output(
            &graph,
            &expression,
            &cquery_options,
            &relative_directory,
            &cache,
        )?;
        snapshot.finish();
        return Ok(());
    }
    if command == "query" {
        bazel_rs::perf_mark("select.start");
        let outcome = engine.query(&rest[0], &query_options, &relative_directory);
        bazel_rs::perf_mark("select.end");
        // A query's planning is its loading.
        engine.record_link_anchors();
        bazel_rs::observed_fs::close_window();
        bazel_rs::repository_store::timing::report(loading_threads_for_report(
            &command,
            loading_phase_threads,
            &query_options,
            memory_limit_mb,
        ));
        // Pattern warnings precede the query's own events.
        for warning in engine.take_warnings() {
            warn(&warning);
        }
        let outcome = outcome?;
        for event in &outcome.events {
            // The query's ERROR events stay under --quiet; its WARNING and
            // INFO events are informational (crate::ui).
            if event.starts_with("ERROR: ") {
                eprintln!("{event}");
            } else {
                bazel_rs::note!("{event}");
            }
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
            drop(snapshot);
            write_lockfile();
            bazel_rs::digest_memo::finish();
            exit(outcome.exit_code);
        }
        bazel_rs::memory_limit::report_peak(memory_limit_mb);
        snapshot.finish();
        return Ok(());
    }
    if command == "test" {
        return test_targets(TestContext {
            engine,
            patterns: &rest,
            relative_directory: &relative_directory,
            lockfile_gate: &lockfile_gate,
            tail: TestTail {
                cache: &cache,
                jobs,
                root: &root,
                runfiles: runfiles_options,
                options: &test_options,
                remote_enabled,
                pattern_failure: None,
                snapshot,
            },
        });
    }
    bazel_rs::perf_mark("select.start");
    let accepts = |info: &bazel_rs::test_command::TestRuleInfo| test_options.accepts(info);
    // `run` builds its first residue argument, the target; the rest are its
    // arguments (RunCommand.java:251-252, runBuild :394-421). Its build
    // request goes through the same target pattern phase as `build`
    // (BuildTool.evaluateTargetPatterns, :467-493): test suites are
    // expanded (`--build_tests_only` and the test filters apply) before
    // RunCommand.validateTargets sees the targets.
    let patterns = if command == "run" {
        &rest[..1]
    } else {
        &rest[..]
    };
    let selected = engine.target_pattern_phase(
        patterns,
        &relative_directory,
        &bazel_rs::engine::TargetPatternPhaseOptions {
            determine_tests: false,
            build_tests_only: test_options.build_tests_only,
            expand_test_suites: test_options.expand_test_suites,
            test_filter: &accepts,
        },
    );
    bazel_rs::perf_mark("select.end");
    // Bazel reports pattern warnings as they happen, before a later error.
    for warning in engine.take_warnings() {
        warn(&warning);
    }
    let selected = selected?;
    // Target-pattern loading succeeded: analysis or execution failures from
    // here on no longer block the lockfile write, like a Bazel command that
    // fails after loading (BazelLockFileModule.afterCommand still runs).
    lockfile_gate.open();
    report_target_pattern_phase(&selected);
    let pattern_failure = pattern_phase_failure(&selected);
    let labels = selected.build;
    if command == "run" {
        return run_target(RunContext {
            engine,
            labels,
            pattern: &rest[0],
            tail: RunTail {
                residue: &rest[1..],
                cache: &cache,
                jobs,
                root: &root,
                cwd: &cwd,
                runfiles: runfiles_options,
                options: &run_options,
                remote_enabled,
                test: &test_options,
                pattern_failure,
                snapshot,
            },
        });
    }
    // Plan -> PlannedRequest -> hand-over -> finalize -> execute (L2-W4).
    let planned = plan_request(
        std::mem::ManuallyDrop::into_inner(engine),
        bazel_rs::engine::RequestSpec::build(&labels),
        &mut snapshot,
    );
    build_tail(
        planned,
        BuildTail {
            cache: &cache,
            jobs,
            action_dump: action_dump.as_deref(),
            show_result,
            remote_enabled,
            loading_threads: loading_threads_for_report(
                &command,
                loading_phase_threads,
                &query_options,
                memory_limit_mb,
            ),
            memory_limit_mb,
            pattern_failure,
            snapshot,
            keep_going: test_options.keep_going,
            run: false,
        },
    )
}

/// `bazel-rs info [KEY...]` (crate::info, docs/INFO.md). InfoCommand
/// inherits the build options (`inheritsOptionsFrom = BuildCommand`, rc
/// `build` lines included) and never loads, analyzes or fetches: no
/// module resolution, no lockfile, no snapshot, no memory watchdog.
/// `bazel-rs gc [--dry_run] [GC OPTIONS]` (native-only; docs/CACHE_GC.md):
/// collect the cache's garbage now, waiting for the other bazel-rs
/// processes that use it, and report what was freed. The policy is the one
/// a `build` here would apply at its end: the garbage collection options of
/// the rc files' `build` (and `common`) lines and of the command line; the
/// rc files' other options are not this command's.
fn gc_command(
    line: &bazelrc::CommandLine,
    options: &bazelrc::Expanded,
    cwd: &Path,
    root: &Path,
) -> Result<(), String> {
    // Errors in Bazel's parse order; the values below in priority order.
    let mut validated = bazel_rs::cache_gc::GcOptions::default();
    let mut dry_run = false;
    for (step, _) in &options.steps {
        let unit = match step {
            bazelrc::Step::Fail(error) => return Err(error.clone()),
            bazelrc::Step::Unit(unit) => unit,
        };
        let arg = unit.option.text.as_str();
        let next = unit.value.as_ref().map(|value| value.text.as_str());
        if !unit.option.from_rc() && matches!(arg, "--dry_run" | "--dry-run") {
            dry_run = true;
            continue;
        }
        match bazel_rs::rc_options::gc_option(&mut validated, arg, next)? {
            Some(_) => {}
            None if unit.option.from_rc() => {}
            None => {
                return Err(format!(
                    "unsupported option {arg:?} for gc (it takes --dry_run and the garbage collection options --experimental_disk_cache_gc_max_size, --experimental_disk_cache_gc_max_age and --repo_contents_cache_gc_max_age)"
                ));
            }
        }
    }
    // The values in priority order (an rc line before the command line).
    let mut ordered = bazel_rs::cache_gc::GcOptions::default();
    for unit in &options.priority {
        let arg = unit.option.text.as_str();
        let next = unit.value.as_ref().map(|value| value.text.as_str());
        let _ = bazel_rs::rc_options::gc_option(&mut ordered, arg, next);
    }
    if let Some(argument) = options.residue.first() {
        return Err(format!("gc takes no arguments; got {:?}", argument.text));
    }
    let cache = match line.cache_dir.clone().map(PathBuf::from) {
        Some(cache) => cache,
        None => bazel_rs::worktree::detect(root).default_cache(),
    };
    let cache = if cache.is_absolute() {
        cache
    } else {
        cwd.join(cache)
    };
    if !cache.is_dir() {
        println!("No cache at {}: nothing to collect", cache.display());
        return Ok(());
    }
    let report = bazel_rs::cache_gc::collect_now(&cache, &ordered, dry_run);
    println!("Garbage collection of {}:", cache.display());
    for (category, counts) in &report.categories {
        println!(
            "  {category:<12} {} of {} entries, {} of {}",
            counts.deleted,
            counts.entries,
            bazel_rs::cache_gc::display_bytes(counts.deleted_bytes),
            bazel_rs::cache_gc::display_bytes(counts.bytes),
        );
    }
    if let Some(budget) = report.budget {
        println!(
            "  size budget  {}",
            bazel_rs::cache_gc::display_bytes(budget)
        );
    }
    println!("{}", capitalized(&report.summary()));
    println!("  ({})", report.phase_summary());
    Ok(())
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn info_command(
    line: &bazelrc::CommandLine,
    options: &bazelrc::Expanded,
    cwd: &Path,
    root: PathBuf,
) -> Result<(), String> {
    fn parse(
        native: &mut NativeOptions,
        info: &mut bazel_rs::info::Options,
        unit: &bazelrc::Unit,
    ) -> Result<(), String> {
        let arg = unit.option.text.as_str();
        let next = unit.value.as_ref().map(|value| value.text.as_str());
        match info.parse(arg, next)? {
            Some(used) => {
                if let (Some(value), false) = (next, used) {
                    return Err(format!(
                        "internal error: {arg} took the separate value {value:?} but its native parser did not consume it"
                    ));
                }
                Ok(())
            }
            None => native.parse("build", unit),
        }
    }
    // Errors in Bazel's parse order, values in priority order (like the
    // other commands).
    for (step, position) in &options.steps {
        let result = match step {
            bazelrc::Step::Fail(error) => Err(error.clone()),
            bazelrc::Step::Unit(unit) => parse(
                &mut NativeOptions::new("build"),
                &mut bazel_rs::info::Options::default(),
                unit,
            ),
        };
        if let Err(error) = result {
            return Err(early_exit(options, &error, *position));
        }
    }
    let mut native = NativeOptions::new("build");
    let mut info = bazel_rs::info::Options::default();
    for unit in &options.priority {
        parse(&mut native, &mut info, unit)?;
    }
    announce(options);
    for warning in native.configuration.warnings() {
        warn(warning);
    }
    // Label-typed options convert without the main repository mapping (no
    // module resolution): no info value depends on a label's repository.
    let labels = |text: &str| {
        bazel_rs::configuration::parse_canonical_option_label(text)
            .or_else(|error| bazel_rs::label::deferred_label(text, "", "").ok_or(error))
    };
    let top = native.configuration.configuration(&labels)?;
    let mnemonic = bazel_rs::config_registry::mnemonic(&top, &top)?;
    let configuration = &top.native;
    let target_cpu = (configuration.platforms.as_slice()
        == [bazel_rs::configuration::default_host_platform()])
    .then(|| bazel_rs::info::host_platform_cpu().to_owned());
    // CommandEnvironment (:350-383): the inherited --action_env and
    // --test_env names with their client values.
    let client = |entries: &[bazel_rs::action_env::EnvVar]| {
        let mut names = std::collections::BTreeSet::new();
        for entry in entries {
            match entry {
                bazel_rs::action_env::EnvVar::Inherit(name) => {
                    names.insert(name.clone());
                }
                bazel_rs::action_env::EnvVar::Set(name, _)
                | bazel_rs::action_env::EnvVar::Unset(name) => {
                    names.remove(name);
                }
            }
        }
        names
            .into_iter()
            .filter_map(|name| std::env::var(&name).ok().map(|value| (name, value)))
            .collect::<Vec<_>>()
    };
    let mut test_inherited = Vec::new();
    for entry in &configuration.test_env {
        if let bazel_rs::action_env::EnvVar::Inherit(name) = entry {
            test_inherited.push(bazel_rs::action_env::EnvVar::Inherit(name.clone()));
        }
    }
    let resolver = &native.resolver;
    let starlark_semantics = [
        (
            "experimental_google_legacy_api",
            resolver.java_semantics.google_legacy_api,
        ),
        (
            "incompatible_disable_non_executable_java_binary",
            resolver.java_semantics.disable_non_executable_java_binary,
        ),
        (
            "incompatible_disable_starlark_host_transitions",
            resolver.disable_starlark_host_transitions,
        ),
        (
            "incompatible_java_info_merge_runtime_module_flags",
            resolver.java_semantics.java_info_merge_runtime_module_flags,
        ),
    ]
    .into_iter()
    .filter(|(_, value)| *value)
    .map(|(name, value)| format!("{name}={value}"))
    .collect();
    // The ceiling a build would install (main's policy), not installed.
    let explicit_mb = match native.memory_limit_mb {
        Some(mb) => Some(bazel_rs::memory_limit::validate_cap_mb(mb)?),
        None => std::env::var("BAZEL_RS_MAX_MEMORY_MB")
            .ok()
            .and_then(|value| bazel_rs::memory_limit::parse_cap_mb(&value)),
    };
    let memory_limit_mb = bazel_rs::memory_limit::effective_cap(explicit_mb).map(|(mb, _)| mb);
    // The cache of `run` (crate::worktree: a linked git worktree's default
    // is its main checkout's) and the workspace's state directory in it, the
    // output base (docs/WORKTREES.md); read only, nothing is created.
    let cache = command_cache(line, cwd, &root);
    let output_base = bazel_rs::worktree::output_base_for(&cache, &root);
    let context = bazel_rs::info::Context {
        workspace: root,
        cache,
        output_base,
        mnemonic,
        compilation_mode: configuration.compilation_mode.to_owned(),
        target_cpu,
        defines: configuration.define.clone(),
        client_action_env: client(&configuration.action_env),
        client_test_env: client(&test_inherited),
        starlark_semantics,
        memory_limit_mb,
    };
    let keys: Vec<String> = options.residue.iter().map(|arg| arg.text.clone()).collect();
    let outcome = bazel_rs::info::run(&context, &info, &keys, &mut std::io::stdout().lock())?;
    if let Some(message) = outcome.unknown {
        // ExitCode.COMMAND_LINE_ERROR, InfoCommand.Code.KEY_NOT_RECOGNIZED.
        eprintln!("ERROR: {message}");
        exit(2);
    }
    Ok(())
}

/// What the end of `build` reads besides the planned request.
struct BuildTail<'a> {
    cache: &'a Path,
    jobs: usize,
    action_dump: Option<&'a Path>,
    show_result: usize,
    remote_enabled: bool,
    /// The effective loading-thread count, for the repository timing report.
    loading_threads: usize,
    memory_limit_mb: Option<u64>,
    /// The target pattern phase's failure (a test suite's expansion error,
    /// [`pattern_phase_failure`]): the build runs, then the command fails.
    /// Always `None` on a snapshot hit (such a run is never saved).
    pattern_failure: Option<&'static str>,
    snapshot: bazel_rs::snapshot::Scope,
    /// `--keep_going` (docs/KEEP_GOING.md).
    keep_going: bool,
    /// `run --keep_going` of targets RunCommand.validateTargets rejected
    /// (built anyway): a failed build ends with `Build failed. Not running
    /// target`.
    run: bool,
}

/// `build` after planning, on a full analysis and on a snapshot hit alike:
/// finalize, execute, report.
fn build_tail(planned: Planned, tail: BuildTail<'_>) -> Result<(), String> {
    let BuildTail {
        cache,
        jobs,
        action_dump,
        show_result,
        remote_enabled,
        loading_threads,
        memory_limit_mb,
        pattern_failure,
        snapshot,
        keep_going: keep_going_requested,
        run,
    } = tail;
    report_keep_going_errors(&planned.request);
    let result = planned.request.and_then(|request| {
        bazel_rs::request::execute(
            request,
            planned.status,
            &bazel_rs::request::ExecuteOptions {
                cache,
                jobs,
                action_dump,
            },
        )
    });
    // After analysis and execution (review i20): repositories demanded while
    // analyzing — most of a build's — are counted too, which a report
    // printed after target-pattern selection missed.
    bazel_rs::repository_store::timing::report(loading_threads);
    // Analysis warnings precede the result, also when the build fails.
    report_diagnostics(planned.diagnostics);
    let (summary, keep_going) = match result {
        Ok(executed) => {
            let keep_going = executed.keep_going.filter(|result| {
                report_keep_going(result, executed.execute);
                result.failed()
            });
            (executed.summary, keep_going)
        }
        Err(error) => {
            snapshot.failed(&error);
            return Err(error);
        }
    };
    let non_empty_targets = summary.non_empty_targets;
    // `fetch TARGET...` (TargetFetcher): a no-build build that reports only
    // that the repositories are fetched.
    let fetch = FETCH_TARGETS.load(std::sync::atomic::Ordering::Relaxed);
    for (label, output) in summary.outputs.iter().filter(|_| !fetch) {
        // `run` reports its built targets on stderr (its stdout is the
        // program's).
        if run {
            eprintln!("{label}\t{}", output.display());
            continue;
        }
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
        if !fetch && non_empty_targets + non_empty_aspects <= show_result {
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
    if fetch {
        // No native summary: fetch reports the fetched repositories only.
    } else if remote_enabled {
        bazel_rs::note!(
            "bazel-rs: {} actions executed, {} cache hits, {} remote cache hits (experimental local execution; {})",
            summary.executed,
            summary.cache_hits,
            summary.remote_hits,
            execution_note()
        );
    } else {
        bazel_rs::note!(
            "bazel-rs: {} actions executed, {} cache hits (experimental local execution; {})",
            summary.executed,
            summary.cache_hits,
            execution_note()
        );
    }
    bazel_rs::memory_limit::report_peak(memory_limit_mb);
    // --keep_going: a build with failures fails once everything else built.
    if let Some(result) = keep_going {
        snapshot.failed("the --keep_going build failed");
        bazel_rs::snapshot::hit::heal_after_failure(None);
        if let Some(message) = keep_going_failure_detail(&result, pattern_failure) {
            eprintln!("ERROR: {message}");
        }
        eprintln!("ERROR: Build did NOT complete successfully");
        if run {
            eprintln!("ERROR: Build failed. Not running target");
        }
        write_lockfile();
        bazel_rs::digest_memo::finish();
        // An environmental failure of a strategy (a sandbox setup failure,
        // a worker spawn's IOException) is exit code 36 under --keep_going
        // too.
        exit(if bazel_rs::sandbox::setup_failure_occurred() {
            36
        } else {
            1
        });
    }
    snapshot.finish();
    // BuildView.createAnalysisFailureDetail (analysis/BuildView.java:760-776):
    // a test suite's expansion error fails the command once it has built.
    if let Some(message) = pattern_failure {
        eprintln!("ERROR: {message}");
        if keep_going_requested {
            eprintln!("ERROR: Build did NOT complete successfully");
        }
        write_lockfile();
        bazel_rs::digest_memo::finish();
        exit(1);
    }
    if fetch {
        write_lockfile();
        // Genuine prints the build's summary first (the progress display's,
        // when it is on), then the fetch result.
        bazel_rs::progress::finish(0);
        bazel_rs::note!(
            "INFO: All external dependencies for the requested targets fetched successfully."
        );
    }
    Ok(())
}

/// A planned request — of the full-analysis path, handed over, or decoded
/// from a snapshot hit — with its analysis diagnostics and the workspace
/// status for its placeholders.
struct Planned {
    request: Result<bazel_rs::request::PlannedRequest, String>,
    diagnostics: bazel_rs::request::Diagnostics,
    status: bazel_rs::request::StatusSource,
}

/// The full-analysis path of `build`, `test` and `run` (L2-W4, PLAN-L2
/// §1d): plan the request, hand it over (crate::snapshot::codec::hand_over:
/// a codec round trip under BAZEL_RS_SNAPSHOT_ROUNDTRIP=1), take its
/// diagnostics (printed after execution) and the computed workspace
/// status, then free the engine: execution and reporting read only the
/// planned request. Under BAZEL_RS_SNAPSHOT=on the capture of what planning
/// printed ends when planning returns, and the planned request is saved
/// (in the background) before its diagnostics are taken. Under
/// BAZEL_RS_SNAPSHOT=verify this full analysis may be a hit's shadow: it
/// is compared with the hit (a mismatch exits 70), and the hit's own plan
/// executes.
fn plan_request(
    mut engine: Engine,
    spec: bazel_rs::engine::RequestSpec<'_>,
    snapshot: &mut bazel_rs::snapshot::Scope,
) -> Planned {
    let request = engine.plan_request(spec);
    snapshot.end_window();
    let request = request.and_then(bazel_rs::snapshot::codec::hand_over);
    let computed = engine.take_workspace_status();
    // Planning ran the workspace status command (a hit runs it too: the
    // snapshot records it, crate::snapshot::format::Inputs).
    let status_computed = computed.is_some();
    let status = bazel_rs::request::StatusSource::Planned(computed);
    let failed = request.is_err().then(|| bazel_rs::request::Diagnostics {
        warnings: engine.take_warnings(),
        errors: engine.take_errors(),
    });
    release_engine(engine);
    if let Some(verify) = snapshot.verify.take() {
        // Compared before its diagnostics are taken (they are part of the
        // planned request's encoding). A mismatch (reported, logged, the
        // snapshot removed; strict: exit 70) executes this full analysis's
        // own plan below, unsaved.
        match (*verify).conclude(&request, failed.as_ref(), status_computed) {
            bazel_rs::snapshot::verify::Concluded::Agreed(hit) => {
                bazel_rs::perf_mark("snapshot.verified");
                drop(request);
                return planned_hit(hit, snapshot);
            }
            bazel_rs::snapshot::verify::Concluded::Disagreed => {
                bazel_rs::perf_mark("snapshot.verify_mismatch");
            }
        }
    }
    let (diagnostics, request) = match request {
        Ok(mut request) => {
            snapshot.planned(&request, status_computed);
            (request.take_diagnostics(), Ok(request))
        }
        Err(error) => (failed.unwrap_or_default(), Err(error)),
    };
    Planned {
        request,
        diagnostics,
        status,
    }
}

/// A snapshot hit's planned request (crate::snapshot::hit): what planning
/// printed is written to stderr first, then the decoded request executes
/// like a planned one, with the workspace status the hit computed; the
/// hit's session (refresh, self-healing) joins `snapshot`.
fn planned_hit(
    hit: Box<bazel_rs::snapshot::hit::Hit>,
    snapshot: &mut bazel_rs::snapshot::Scope,
) -> Planned {
    let hit = *hit;
    {
        use std::io::Write as _;
        let mut stderr = std::io::stderr().lock();
        // Filtered like the full analysis's forwarded bytes (crate::ui).
        let _ = stderr
            .write_all(&bazel_rs::ui::filtered(&hit.window_stderr))
            .and_then(|()| stderr.flush());
    }
    let mut request = hit.request;
    snapshot.hit = Some(hit.session);
    Planned {
        diagnostics: request.take_diagnostics(),
        status: bazel_rs::request::StatusSource::Planned(hit.status),
        request: Ok(request),
    }
}

/// Free the analysis state (packages, configured rules, the Starlark values
/// they hold) before execution (L2-W4, PLAN-L2 §5): on a background thread
/// that is never joined, so the drop overlaps execution and a drop still
/// running when the command ends is skipped (the process exits; nothing in
/// the engine has an effect on drop). A build that does not execute
/// (`--nobuild`) ends right after planning: its engine is never torn down
/// (W7, Engine::skips_teardown; planning already left its maps to the OS).
/// Measurement switches: `BAZEL_RS_DEBUG_ENGINE_DROP=inline` drops it here
/// on the main thread, `=leak` never drops it.
fn release_engine(engine: Engine) {
    match std::env::var_os("BAZEL_RS_DEBUG_ENGINE_DROP").as_deref() {
        Some(mode) if mode == "inline" => drop(engine),
        Some(mode) if mode == "leak" => std::mem::forget(engine),
        _ if engine.skips_teardown() => std::mem::forget(engine),
        _ => {
            // A thread that cannot be spawned drops the engine right here.
            let _ = std::thread::Builder::new()
                .name("engine-drop".to_owned())
                .spawn(move || drop(engine));
        }
    }
}

/// The analysis warnings and errors of a request, printed after execution,
/// also when the build failed; an error taints the snapshot recording.
fn report_diagnostics(diagnostics: bazel_rs::request::Diagnostics) {
    for warning in diagnostics.warnings {
        warn(&warning);
    }
    for error in diagnostics.errors {
        eprintln!("ERROR: {error}");
        bazel_rs::observed_fs::taint_after_window(bazel_rs::observed_fs::Taint::Errors, || {
            format!("analysis error: {error}")
        });
    }
}

/// `--keep_going` (docs/KEEP_GOING.md): the analysis's root-cause errors,
/// each once in the engine's sequential order, before execution (Bazel
/// reports them as the analysis runs, before the execution's).
fn report_keep_going_errors(request: &Result<bazel_rs::request::PlannedRequest, String>) {
    if let Ok(request) = request
        && let Some(plan) = &request.keep_going
    {
        for error in &plan.errors {
            eprintln!("ERROR: {}", located(error));
        }
    }
}

/// `--keep_going` after execution: each failed action's error (by action
/// id), `errors encountered while analyzing target 'L', it will not be
/// built.` per requested target whose analysis failed (with its `Analysis
/// failed` cause when a dependency failed; SkyframeErrorProcessor
/// .logOrPrintWarningsKeepGoing :671-685), and BuildView's `Build succeeded
/// for only N of M top-level targets` (:440-448).
fn report_keep_going(result: &bazel_rs::request::KeepGoingResult, execute: bool) {
    for error in &result.plan.input_errors {
        eprintln!("ERROR: {}", located(error));
    }
    for (id, error) in &result.actions {
        eprintln!("ERROR: {}", located(&format!("action '{id}': {error}")));
    }
    for (label, dependency) in &result.plan.failed {
        let mut message = format!(
            "errors encountered while analyzing target '{}', it will not be built.",
            label_print(label)
        );
        if *dependency {
            message.push_str("\nAnalysis failed");
        }
        warn(&message);
    }
    let succeeded = result.succeeded();
    if 0 < succeeded && succeeded < result.plan.top_level {
        bazel_rs::note!(
            "INFO: {} succeeded for only {succeeded} of {} top-level targets",
            if execute { "Build" } else { "Analysis" },
            result.plan.top_level
        );
    }
}

/// The failure detail a failed `--keep_going` build ends with
/// (BuildTool.java:688-703, BuildView.createAnalysisFailureDetail
/// :760-784): none after an execution failure (its BuildFailedException
/// has no message), else the target pattern phase's, else `command
/// succeeded, but not all targets were analyzed`.
fn keep_going_failure_detail(
    result: &bazel_rs::request::KeepGoingResult,
    pattern_failure: Option<&'static str>,
) -> Option<&'static str> {
    if !result.actions.is_empty()
        || !result.unbuilt.is_empty()
        || !result.plan.input_errors.is_empty()
    {
        None
    } else if pattern_failure.is_some() {
        pattern_failure
    } else if !result.plan.failed.is_empty() {
        Some("command succeeded, but not all targets were analyzed")
    } else {
        None
    }
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

/// Writes the updated lockfile when the command ends (success or error):
/// when it drops at the end of the command, or earlier through
/// [`write_lockfile`] where a command ends the process itself (`test`'s
/// exit code, `run`'s exec). BazelLockFileModule.afterCommand runs after
/// every command: genuine Bazel 9.3 writes MODULE.bazel.lock for `test`
/// (passing, failing, `--nobuild`) and `run` as for `build`.
struct LockfileWriter(bazel_rs::registry_session::SessionHandle);

/// The registry session whose updated lockfile the command still writes
/// (taken by the first [`write_lockfile`]: written once).
static LOCKFILE_SESSION: std::sync::Mutex<Option<bazel_rs::registry_session::SessionHandle>> =
    std::sync::Mutex::new(None);

impl LockfileWriter {
    fn new(session: bazel_rs::registry_session::SessionHandle) -> Self {
        *LOCKFILE_SESSION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session.clone());
        Self(session)
    }
}

impl Drop for LockfileWriter {
    fn drop(&mut self) {
        write_lockfile();
    }
}

/// Write the updated lockfile (if any) now; later calls do nothing. Called
/// before `test` exits with its code and before `run` replaces the process,
/// so they write it like every other command.
fn write_lockfile() {
    let session = LOCKFILE_SESSION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(warning) = session
        .as_ref()
        .and_then(|session| session.session())
        .and_then(|session| session.finish())
    {
        warn(&warning);
    }
}

/// Commands that build (`run` and `test` inherit the build options).
fn builds(command: &str) -> bool {
    command == "build" || command == "run" || command == "test"
}

/// Configuration and provider analysis does not imply an execution phase.
fn analyzes(command: &str) -> bool {
    builds(command) || command == "cquery"
}

fn cquery_output(
    graph: &bazel_rs::engine::cquery::Graph,
    expression: &bazel_rs::query_language::Expr,
    options: &bazel_rs::engine::cquery::Options,
    relative: &str,
    cache: &Path,
) -> Result<(), String> {
    bazel_rs::perf_mark("cquery.output_begin");
    if options.common.output_file.is_empty() {
        graph.write(expression, options, relative, std::io::stdout().lock())?;
    } else {
        let path = Path::new(&options.common.output_file);
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        graph.write(expression, options, relative, &mut file)?;
        file.persist(path).map_err(|e| e.to_string())?;
    }
    bazel_rs::cquery_state::save_configurations(cache, graph)?;
    bazel_rs::perf_mark("cquery.output_end");
    Ok(())
}

fn config_command(
    line: &bazelrc::CommandLine,
    options: &bazelrc::Expanded,
    cwd: &Path,
    root: &Path,
) -> Result<(), String> {
    let mut json = false;
    let mut dump = false;
    let mut common = NativeOptions::new("config");
    for unit in &options.priority {
        match unit.option.text.as_str() {
            "--output=json" => json = true,
            "--output" if unit.value.as_ref().is_some_and(|v| v.text == "json") => json = true,
            "--dump_all" | "--dump_all=true" => dump = true,
            _ => common.parse("config", unit)?,
        }
    }
    if !json || !dump || !options.residue.is_empty() {
        return Err(
            "native config supports --output=json --dump_all of the last successful cquery".into(),
        );
    }
    bazel_rs::worktree::install(root);
    let cache = line
        .cache_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| bazel_rs::worktree::detect(root).default_cache());
    let cache = if cache.is_absolute() {
        cache
    } else {
        cwd.join(cache)
    };
    bazel_rs::cquery_state::dump_configurations(&cache)
}

struct RunContext<'a> {
    /// Never torn down unless planned (W7: an error before planning exits
    /// without it; [`plan_request`] takes it out).
    engine: std::mem::ManuallyDrop<Engine>,
    labels: Vec<bazel_rs::label::Label>,
    pattern: &'a str,
    tail: RunTail<'a>,
}

/// What `run` reads after planning besides the planned request.
struct RunTail<'a> {
    residue: &'a [String],
    cache: &'a Path,
    jobs: usize,
    root: &'a Path,
    cwd: &'a Path,
    runfiles: bazel_rs::runfiles_tree::RunfilesOptions,
    options: &'a bazel_rs::run_command::RunOptions,
    remote_enabled: bool,
    test: &'a bazel_rs::test_command::TestOptions,
    /// The target pattern phase's failure (a test suite's expansion error):
    /// the build runs, then fails (BuildView.createAnalysisFailureDetail).
    /// Always `None` on a snapshot hit ([`pattern_phase_failure`]).
    pattern_failure: Option<&'static str>,
    /// The analysis snapshot's sessions: finished once the build part ends.
    snapshot: bazel_rs::snapshot::Scope,
}

/// `run` (runtime/commands/RunCommand.java:250-391): build exactly one
/// executable target and its runfiles tree, then execute it in its
/// runfiles directory (`<tree>/_main`) with the run environment, or write
/// the `--script_path` script. The process replaces this one (exit code,
/// stdin and terminal pass through).
fn run_target(context: RunContext<'_>) -> Result<(), String> {
    let RunContext {
        mut engine,
        labels,
        pattern,
        mut tail,
    } = context;
    let options = tail.options;
    // validateTargets (RunCommand.java:1052-1111) over the target pattern
    // phase's targets (test suites expanded). Under --keep_going each
    // failure is a warning (`MESSAGE. Will continue anyway`, :1110-1122),
    // the targets build, and getBuiltTargets/fullyValidateTarget fail the
    // command after a successful build (:434-470, :1136-1160).
    let mut rejected = None;
    if labels.len() > 1 {
        let mut names: Vec<String> = labels.iter().map(|label| label.to_string()).collect();
        let truncated = names.len() > 5;
        names.truncate(5);
        names.sort();
        rejected = Some(format!(
            "Only a single target can be run. Your target pattern {pattern} expanded to the targets {}{}",
            names.join(", "),
            if truncated { "[TRUNCATED]" } else { "" }
        ));
    } else if labels.is_empty() {
        rejected = Some("No targets found to run".to_owned());
    } else if let bazel_rs::engine::Runnable::No = engine.runnable(&labels[0])? {
        rejected = Some(format!("Cannot run target {}: Not executable", labels[0]));
    }
    if let Some(error) = rejected {
        if !tail.test.keep_going {
            return Err(error);
        }
        warn(&format!("{error}. Will continue anyway"));
        // A failing command: never saved as the snapshot of this `run`.
        bazel_rs::observed_fs::taint(bazel_rs::observed_fs::Taint::Errors, || {
            format!("run --keep_going: {error}")
        });
        let planned = plan_request(
            std::mem::ManuallyDrop::into_inner(engine),
            bazel_rs::engine::RequestSpec::build(&labels),
            &mut tail.snapshot,
        );
        build_tail(
            planned,
            BuildTail {
                cache: tail.cache,
                jobs: tail.jobs,
                action_dump: None,
                show_result: 1,
                remote_enabled: tail.remote_enabled,
                loading_threads: 0,
                memory_limit_mb: None,
                pattern_failure: tail.pattern_failure,
                snapshot: tail.snapshot,
                keep_going: true,
                run: true,
            },
        )?;
        return Err(error);
    }
    let label = &labels[0];
    if let Some(value) = options.run_under.as_deref()
        && let Some(first) = value.split_whitespace().next()
        && (first.starts_with("//") || first.starts_with('@'))
    {
        return Err(format!(
            "--run_under={value}: a label prefix (LabelRunUnder) is not supported by the native profile; use a command prefix"
        ));
    }
    // `run` of a test target reads the test properties (run_test): looked
    // up now, with the target loaded; an error surfaces there.
    let test_info = engine.test_rule_info(label).map(|info| {
        info.map(|(info, location)| bazel_rs::request::PlannedTest {
            label: label.clone(),
            info,
            location,
        })
    });
    // Build the target, its executable and its runfiles tree (always
    // created, RunCommand.ensureRunfilesBuilt, :996-1040;
    // RunCommand.runBuild/getBuiltTargets, :394-568).
    let planned = plan_request(
        std::mem::ManuallyDrop::into_inner(engine),
        bazel_rs::engine::RequestSpec {
            command: bazel_rs::request::RequestCommand::Run {
                label: label.clone(),
                test: test_info,
            },
            build: std::slice::from_ref(label),
            runs: std::slice::from_ref(label),
            trees: true,
        },
        &mut tail.snapshot,
    );
    run_tail(planned, label, tail)
}

/// `run` after planning, on a full analysis and on a snapshot hit alike:
/// build the target, then run it (or write its script).
fn run_tail(
    planned: Planned,
    label: &bazel_rs::label::Label,
    tail: RunTail<'_>,
) -> Result<(), String> {
    use bazel_rs::run_command::{RunInputs, command_line, random_uuid};
    let RunTail {
        residue,
        cache,
        jobs,
        root,
        cwd,
        runfiles,
        options,
        remote_enabled,
        test,
        pattern_failure,
        snapshot,
    } = tail;
    report_keep_going_errors(&planned.request);
    let result = planned.request.and_then(|request| {
        bazel_rs::request::execute(
            request,
            planned.status,
            &bazel_rs::request::ExecuteOptions {
                cache,
                jobs,
                action_dump: None,
            },
        )
    });
    report_diagnostics(planned.diagnostics);
    // The build part is the snapshot's scope (the run itself replaces this
    // process).
    match &result {
        Ok(executed) if executed.keep_going.as_ref().is_some_and(|kg| kg.failed()) => {
            snapshot.failed("the --keep_going build failed")
        }
        Ok(_) => snapshot.finish(),
        Err(error) => snapshot.failed(error),
    }
    let bazel_rs::request::Executed {
        summary,
        mut targets,
        command: planned_command,
        execute,
        keep_going,
    } = result?;
    // --keep_going: the build part failed (RunCommand.runBuild:
    // `Build failed. Not running target`).
    if let Some(result) = keep_going.filter(|result| {
        report_keep_going(result, execute);
        result.failed()
    }) {
        if let Some(message) = keep_going_failure_detail(&result, pattern_failure) {
            eprintln!("ERROR: {message}");
        }
        eprintln!("ERROR: Build did NOT complete successfully");
        eprintln!("ERROR: Build failed. Not running target");
        write_lockfile();
        bazel_rs::digest_memo::finish();
        exit(1);
    }
    let target = targets.remove(label);
    let test_info = match planned_command {
        bazel_rs::request::RequestCommand::Run { test, .. } => test,
        _ => Err(format!(
            "internal error: {label} was not planned as a run target"
        )),
    };
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
        bazel_rs::note!(
            "bazel-rs: {} actions executed, {} cache hits, {} remote cache hits (experimental local execution; {})",
            summary.executed,
            summary.cache_hits,
            summary.remote_hits,
            execution_note()
        );
    } else {
        bazel_rs::note!(
            "bazel-rs: {} actions executed, {} cache hits (experimental local execution; {})",
            summary.executed,
            summary.cache_hits,
            execution_note()
        );
    }
    // A test suite's expansion error fails the build once it ran
    // (BuildView.createAnalysisFailureDetail, analysis/BuildView.java:760-776),
    // so RunCommand.runBuild does not run the target (:421-426; genuine 9.3
    // `run //pkg:with_lib`, tests/fixtures/test_suite run_strict).
    if let Some(message) = pattern_failure {
        eprintln!("ERROR: {message}");
        return Err("Build failed. Not running target".into());
    }
    if !execute {
        // --nobuild: analysis succeeded and 0 actions executed, but no
        // target was built to run (RunCommand.getBuiltTargets finds no
        // successful targets; genuine 9.3 scratch capture
        // `run --nobuild //pkg:tool` exits 2 with this message; native
        // reports the same text through its exit-1 channel).
        return Err("No targets found to run".into());
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
            test_info,
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
    // The progress display ends before the target takes the terminal.
    bazel_rs::progress::finish(0);
    let prefix = if options.run { "Running" } else { "Runnable" };
    bazel_rs::note!(
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
    // The build event files end before the target runs, with the command
    // the client executes (ExecRequestConstructed), like Bazel's.
    bazel_rs::bep::exec_request(bazel_rs::bep::ExecRequest {
        working_directory: PathBuf::from(&line.working_directory),
        argv: argv.iter().map(std::ffi::OsString::from).collect(),
        environment: line
            .environment
            .iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect(),
    });
    finish_reports(0, None);
    write_lockfile();
    bazel_rs::digest_memo::finish();
    // The target runs in this process's place: the last uses are recorded,
    // the collection waits for a later command.
    bazel_rs::cache_gc::finish(false);
    bazel_rs::worker::shutdown();
    let error = bazel_rs::spawn_lock::exec(&mut process);
    Err(format!("cannot execute {}: {error}", argv[0]))
}

struct TestContext<'a> {
    /// Never torn down unless planned (W7: an error before planning exits
    /// without it; [`plan_request`] takes it out).
    engine: std::mem::ManuallyDrop<Engine>,
    patterns: &'a [String],
    relative_directory: &'a str,
    /// Opens the lockfile write once test-target loading succeeded.
    lockfile_gate: &'a LockfileGate,
    tail: TestTail<'a>,
}

/// What `test` reads after planning besides the planned request.
struct TestTail<'a> {
    cache: &'a Path,
    jobs: usize,
    root: &'a Path,
    runfiles: bazel_rs::runfiles_tree::RunfilesOptions,
    options: &'a bazel_rs::test_command::TestOptions,
    /// Whether the remote cache is set up (N41): the build summary names its
    /// hits. Test actions themselves are local-only (docs/TEST_COMMAND.md).
    remote_enabled: bool,
    /// The target pattern phase's failure ([`pattern_phase_failure`]): the
    /// tests run, then the command fails. Always `None` on a snapshot hit.
    pattern_failure: Option<&'static str>,
    /// The analysis snapshot's sessions: finished once the build part ends.
    snapshot: bazel_rs::snapshot::Scope,
}

/// `test` (runtime/commands/TestCommand.java:124-243): build the targets,
/// run the selected tests (crate::test_runner), print the summary and exit
/// with Bazel's code: 0 when every test passed, 3 (TESTS_FAILED) otherwise,
/// 4 (NO_TESTS_FOUND) without tests, 1 when the build failed.
fn test_targets(context: TestContext<'_>) -> Result<(), String> {
    let TestContext {
        mut engine,
        patterns,
        relative_directory,
        lockfile_gate,
        mut tail,
    } = context;
    let options = tail.options;
    let accepts = |info: &bazel_rs::test_command::TestRuleInfo| options.accepts(info);
    let selected = engine.target_pattern_phase(
        patterns,
        relative_directory,
        &bazel_rs::engine::TargetPatternPhaseOptions {
            determine_tests: true,
            build_tests_only: options.build_tests_only,
            expand_test_suites: options.expand_test_suites,
            test_filter: &accepts,
        },
    );
    for warning in engine.take_warnings() {
        warn(&warning);
    }
    let selected = selected?;
    // Test-target loading succeeded: the lockfile write is allowed.
    lockfile_gate.open();
    report_target_pattern_phase(&selected);
    tail.pattern_failure = pattern_phase_failure(&selected);
    let build = selected.build;
    let tests = selected.tests.unwrap_or_default();
    let mut infos = std::collections::BTreeMap::new();
    for label in &tests {
        let (info, location) = engine
            .test_rule_info(label)?
            .ok_or_else(|| format!("internal error: {label} is not a test rule"))?;
        infos.insert(label.clone(), (info, location));
    }
    let planned_tests = tests
        .iter()
        .map(|label| {
            let (info, location) = infos.remove(label).expect("filtered test");
            bazel_rs::request::PlannedTest {
                label: label.clone(),
                info,
                location,
            }
        })
        .collect();
    // Build `build` for `bazel test` (BuildTool with `setRunTests`,
    // runtime/commands/TestCommand.java:150-175): the targets to build, and
    // for each test of `tests` (a subset) its executable and the artifacts
    // of its runfiles, which the test action stages by content
    // (crate::test_runner). The runfiles tree itself is built only like
    // any `build` (`--build_runfile_links`).
    let planned = plan_request(
        std::mem::ManuallyDrop::into_inner(engine),
        bazel_rs::engine::RequestSpec {
            command: bazel_rs::request::RequestCommand::Test {
                build: build.clone(),
                tests: planned_tests,
            },
            build: &build,
            runs: &tests,
            trees: false,
        },
        &mut tail.snapshot,
    );
    test_tail(planned, tests, tail)
}

/// `test` after planning, on a full analysis and on a snapshot hit alike:
/// build, run the tests, print the summary and exit with Bazel's code.
/// `selected` are the tests a failed planning reports.
fn test_tail(
    planned: Planned,
    selected: Vec<bazel_rs::label::Label>,
    tail: TestTail<'_>,
) -> Result<(), String> {
    use bazel_rs::test_command::{should_output_log, summary_text};
    let TestTail {
        cache,
        jobs,
        root,
        runfiles,
        options,
        remote_enabled,
        pattern_failure,
        snapshot,
    } = tail;
    // The tests a failed build reports (also those of a decoded request).
    let test_labels = match &planned.request {
        Ok(request) => request.test_labels(),
        Err(_) => selected,
    };
    // PERF(TEST): the test actions run inside the execution phase, each as
    // soon as its test's inputs are built (crate::request::execute_with_tests,
    // like Bazel's TestRunnerActions in the build's one execution graph), so
    // they overlap the rest of the build; the summary is printed after it.
    // A test the plan cannot specify (crate::request::test_preflight) fails
    // the command after the build, before any test runs, as before.
    let mut unspecified = None;
    let session_tests = match &planned.request {
        Ok(request) if request.execute => match &request.command {
            bazel_rs::request::RequestCommand::Test { tests, .. } if !tests.is_empty() => {
                match bazel_rs::request::test_preflight(request) {
                    Ok(()) => Some(tests.clone()),
                    Err(error) => {
                        unspecified = Some(error);
                        None
                    }
                }
            }
            _ => None,
        },
        _ => None,
    };
    let tools = match &session_tests {
        Some(_) => Some(bazel_rs::test_runner::Tools::embedded(cache)?),
        None => None,
    };
    let client_env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let session = match (session_tests, &tools) {
        (Some(tests), Some(tools)) => Some(bazel_rs::test_runner::Session::new(
            bazel_rs::test_runner::Runner {
                cache,
                workspace: root,
                options,
                client_env: &client_env,
                runfiles_enabled: runfiles.enable_runfiles,
                tools,
            },
            tests,
        )),
        _ => None,
    };
    report_keep_going_errors(&planned.request);
    let result = planned.request.and_then(|request| {
        bazel_rs::request::execute_with_tests(
            request,
            planned.status,
            &bazel_rs::request::ExecuteOptions {
                cache,
                jobs,
                action_dump: None,
            },
            session.as_ref(),
        )
    });
    report_diagnostics(planned.diagnostics);
    // --keep_going: the failures of the build part (docs/KEEP_GOING.md).
    let mut result = result;
    let keep_going = match &mut result {
        Ok(executed) => executed.keep_going.take().filter(|result| {
            report_keep_going(result, executed.execute);
            result.failed()
        }),
        Err(_) => None,
    };
    let build_failed = keep_going.is_some();
    // Tests whose inputs did not all build (FAILED TO BUILD).
    let unbuilt: std::collections::BTreeSet<bazel_rs::label::Label> = keep_going
        .as_ref()
        .map(|result| result.unbuilt.iter().cloned().collect())
        .unwrap_or_default();
    // The build part is the snapshot's scope (test results are execution
    // outcomes; the test runner exits the process).
    match &result {
        Ok(_) if build_failed => snapshot.failed("the --keep_going build failed"),
        Ok(_) => snapshot.finish(),
        Err(error) => snapshot.failed(error),
    }
    let bazel_rs::request::Executed {
        summary,
        targets: _,
        command: planned_command,
        execute,
        keep_going: _,
    } = match result {
        Ok(executed) => executed,
        Err(error) => {
            return test_build_failure(
                &error,
                &test_labels,
                options,
                session.map(bazel_rs::test_runner::Session::finish),
            );
        }
    };
    let planned_tests = match planned_command {
        bazel_rs::request::RequestCommand::Test { tests, .. } => tests,
        _ => Vec::new(),
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
        bazel_rs::note!(
            "bazel-rs: {} actions executed, {} cache hits, {} remote cache hits (experimental local execution; {})",
            summary.executed,
            summary.cache_hits,
            summary.remote_hits,
            execution_note()
        );
    } else {
        bazel_rs::note!(
            "bazel-rs: {} actions executed, {} cache hits (experimental local execution; {})",
            summary.executed,
            summary.cache_hits,
            execution_note()
        );
    }
    if planned_tests.is_empty() {
        eprintln!("ERROR: No test targets were found, yet testing was requested");
        write_lockfile();
        bazel_rs::digest_memo::finish();
        exit(4);
    }
    if !execute {
        // --nobuild: the build succeeded with 0 actions but nothing was
        // built to run (TestCommand: getSuccessfulTargets is null without
        // the execution phase, so the command reports `Couldn't start
        // the build. Unable to run tests`, exit 1; genuine 9.3 scratch
        // capture `test --nobuild //pkg:t`).
        eprintln!("ERROR: Couldn't start the build. Unable to run tests");
        write_lockfile();
        bazel_rs::digest_memo::finish();
        exit(1);
    }
    let outcome = match session {
        Some(session) => session.finish(),
        None => {
            return Err(unspecified
                .unwrap_or_else(|| "internal error: the planned tests did not run".to_owned()));
        }
    };
    if let Some(error) = outcome.failure {
        return Err(error);
    }
    let mut outcome = outcome;
    // --keep_going: a test whose inputs failed to build never ran: FAILED
    // TO BUILD (AggregatingTestListener: targetFailure; genuine capture
    // tests/fixtures/keep_going).
    for (summary, test) in outcome.summaries.iter_mut().zip(&planned_tests) {
        if unbuilt.contains(&test.label) && summary.test_times.is_empty() {
            summary.status = bazel_rs::test_command::TestStatus::FailedToBuild;
        }
    }
    if bazel_rs::bep::active() {
        bazel_rs::bep::test_summaries(
            outcome
                .summaries
                .iter()
                .zip(&planned_tests)
                .map(|(summary, test)| bazel_rs::bep::TestSummaryRecord {
                    label: summary.label.clone(),
                    status: summary.status,
                    total_run_count: (test.info.shard_count.max(1)
                        * options.runs_per_test.max(1) as i32),
                    shard_count: test.info.shard_count.max(0),
                    num_cached: summary.num_cached as i32,
                    passed: summary.passed_logs.clone(),
                    failed: summary.failed_logs.clone(),
                })
                .collect(),
        );
    }
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
    let passed = outcome
        .summaries
        .iter()
        .all(|summary| summary.status.passed());
    // BuildView.createAnalysisFailureDetail (analysis/BuildView.java:760-776):
    // a test suite's expansion error fails the build once it has run, and
    // passing tests say so (TestCommand.java:216-223).
    if (pattern_failure.is_some() || build_failed) && passed {
        let _ =
            stdout.write_all(b"All tests passed but there were other errors during the build.\n\n");
    }
    let _ = stdout.flush();
    drop(stdout);
    // The build's failure summary names the first action error once
    // (BuildResultPrinter; genuine capture M8).
    // Under --keep_going the build completes and repeats no error (M36).
    if let Some(error) = outcome.errors.first().filter(|_| !options.keep_going) {
        eprintln!("ERROR: {error}");
    }
    // A target that failed to build (a test action error under
    // --keep_going) makes the build fail: exit 1, the more important code
    // (DetailedExitCodeComparator, TestCommand.java:236-240).
    let failed_to_build = outcome
        .summaries
        .iter()
        .any(|summary| summary.status == bazel_rs::test_command::TestStatus::FailedToBuild);
    match &keep_going {
        Some(result) => {
            if let Some(message) = keep_going_failure_detail(result, pattern_failure) {
                eprintln!("ERROR: {message}");
            }
            eprintln!("ERROR: Build did NOT complete successfully");
        }
        None => {
            if let Some(message) = pattern_failure {
                eprintln!("ERROR: {message}");
                if options.keep_going {
                    eprintln!("ERROR: Build did NOT complete successfully");
                }
            }
        }
    }
    if failed_to_build || build_failed || !passed {
        // A snapshot hit whose tests failed: a damaged repository tree the
        // hit trusted heals (crate::snapshot::hit, L2-W5g).
        bazel_rs::snapshot::hit::heal_after_failure(None);
    }
    write_lockfile();
    bazel_rs::loading::module_timings::report();
    bazel_rs::digest_memo::finish();
    exit(
        if failed_to_build || build_failed || pattern_failure.is_some() {
            1
        } else if passed {
            0
        } else {
            3
        },
    );
}

/// The target pattern phase's events (TargetPatternPhaseFunction.java
/// :134-139, 189-191): test-suite expansion errors as they were reported,
/// `All specified test targets were excluded by filters`, and `Target
/// pattern parsing failed.` for an expansion error of the tests to run.
fn report_target_pattern_phase(phase: &bazel_rs::engine::TargetPatternPhase) {
    // --keep_going: the skipped packages' loading errors, then (after the
    // warning, as genuine 9.3 prints them) the skipped patterns.
    for error in &phase.loading_errors {
        eprintln!("ERROR: {}", located(error));
    }
    for error in &phase.errors {
        eprintln!("ERROR: {error}");
    }
    if phase.all_excluded {
        warn("All specified test targets were excluded by filters");
    }
    if phase.pattern_error {
        warn("Target pattern parsing failed.");
    }
    for skipped in &phase.skipped {
        eprintln!("ERROR: {}", located(skipped));
    }
}

/// BuildView.createAnalysisFailureDetail's loading messages (:760-776).
fn target_pattern_failure(pattern_error: bool, post_expansion_error: bool) -> Option<&'static str> {
    if pattern_error {
        Some("command succeeded, but there were errors parsing the target pattern")
    } else if post_expansion_error {
        Some("command succeeded, but there were loading phase errors")
    } else {
        None
    }
}

/// The failure a target pattern phase leaves for the end of the command
/// ([`target_pattern_failure`]). A phase that reported test-suite expansion
/// errors makes the command fail after its build, so the run is never saved
/// as an analysis snapshot (crate::observed_fs::Taint::Errors, like an
/// analysis error; under BAZEL_RS_SNAPSHOT=verify a shadow full analysis
/// that reports one disagrees with its hit): a snapshot hit, which skips
/// the phase and replays what it printed, never has a failure to report.
fn pattern_phase_failure(phase: &bazel_rs::engine::TargetPatternPhase) -> Option<&'static str> {
    let failure = target_pattern_failure(phase.pattern_error, phase.post_expansion_error);
    if failure.is_some() || !phase.errors.is_empty() {
        bazel_rs::observed_fs::taint(bazel_rs::observed_fs::Taint::Errors, || {
            format!(
                "the target pattern phase reported {} test-suite expansion error(s)",
                phase.errors.len()
            )
        });
    }
    failure
}

/// A failed build of `bazel test` (TestCommand.java:200-243 with
/// AggregatingTestListener.differentialAnalyzeAndReport): a test all of
/// whose actions reported before the build failed keeps its result (its
/// summary was posted; PERF(TEST): tests run inside the build), every other
/// test shows FAILED TO BUILD when the failing actions' messages name it and
/// NO STATUS otherwise (never run or interrupted, --nokeep_going); the exit
/// code is 1. Under --keep_going Bazel would still run the tests that built,
/// which the native all-or-nothing build cannot.
fn test_build_failure(
    error: &str,
    tests: &[bazel_rs::label::Label],
    options: &bazel_rs::test_command::TestOptions,
    outcome: Option<bazel_rs::test_runner::Outcome>,
) -> Result<(), String> {
    use bazel_rs::test_command::{
        TestStatus, TestSummary, TestTimeout, should_output_log, summary_text,
    };
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
        .enumerate()
        .map(|(index, label)| {
            let reported = outcome.as_ref().and_then(|outcome| {
                Some((
                    outcome.summaries.get(index)?,
                    *outcome.completed.get(index)?,
                ))
            });
            match reported {
                // Every action of the test reported: its posted summary.
                Some((summary, true)) => return summary.clone(),
                // Some did (a shard ran, another was interrupted or never
                // started): the summary as it stands, NO STATUS
                // (TestResultAggregator.markUnbuilt with skipTargetsOnFailure;
                // its actions ran).
                Some((summary, false)) if !summary.test_times.is_empty() => {
                    let mut summary = summary.clone();
                    summary.status = TestStatus::NoStatus;
                    return summary;
                }
                _ => {}
            }
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
    // TerminalTestResultNotifier.notify: the locally cached results' logs,
    // then the summary.
    let mut text = Vec::new();
    let mut sorted: Vec<&TestSummary> = summaries.iter().collect();
    sorted.sort_by(|a, b| a.compare(b));
    for summary in sorted {
        if summary.num_cached > 0 && should_output_log(options.output, summary.status.passed()) {
            text.extend(bazel_rs::test_runner::cached_output(summary, options));
        }
    }
    let executed = summaries
        .iter()
        .filter(|summary| summary.action_ran)
        .count();
    text.extend(summary_text(&summaries, executed, options).into_bytes());
    // TestCommand.java:216-223: every test passed, the build did not.
    if !summaries.is_empty() && summaries.iter().all(|summary| summary.status.passed()) {
        text.extend(b"All tests passed but there were other errors during the build.\n\n");
    }
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(&text);
    let _ = stdout.flush();
    drop(stdout);
    write_lockfile();
    bazel_rs::loading::module_timings::report();
    bazel_rs::digest_memo::finish();
    exit(1);
}

struct RunTest<'a> {
    /// The run target's test properties (crate::request::RequestCommand::Run).
    test_info: Result<Option<bazel_rs::request::PlannedTest>, String>,
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
/// `<state>/test-run/<key>/execroot` in the workspace's state directory
/// (crate::worktree: a `run` of the same test in another worktree of a
/// shared cache never replaces it): test-setup.sh at its exec path and
/// the runfiles tree linked at `<executable>.runfiles`.
fn run_test(context: RunTest<'_>) -> Result<(), String> {
    use bazel_rs::run_command::{ENV_TO_CLEAR, random_uuid, shell_escape};
    let RunTest {
        test_info,
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
    let Some(planned) = test_info? else {
        return Err(format!("internal error: {label} is not a test rule"));
    };
    let spec = bazel_rs::test_runner::TestSpec::new(label, planned.info, planned.location, target)?;
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
    let execroot = bazel_rs::worktree::state_directory(cache)
        .join("test-run")
        .join(key)
        .join("execroot");
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
    // The progress display ends before the target takes the terminal.
    bazel_rs::progress::finish(0);
    let prefix = if options.run { "Running" } else { "Runnable" };
    bazel_rs::note!("bazel-rs: {prefix} command line: {pretty}");
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
    bazel_rs::bep::exec_request(bazel_rs::bep::ExecRequest {
        working_directory: execroot.clone(),
        argv: [options.shell.clone(), "-c".to_owned(), command.clone()]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect(),
        environment: environment
            .iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect(),
    });
    finish_reports(0, None);
    write_lockfile();
    bazel_rs::digest_memo::finish();
    // The target runs in this process's place: the last uses are recorded,
    // the collection waits for a later command.
    bazel_rs::cache_gc::finish(false);
    bazel_rs::worker::shutdown();
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

/// The effective loading-thread count (also the timing report's):
/// an explicit `--loading_phase_threads=N` (either parser) is kept
/// exactly; the "auto" expansion (available processors) is bounded by
/// the memory cap (crate::memory_limit::bound_auto_loading_threads).
fn loading_threads_for_report(
    command: &str,
    explicit: Option<usize>,
    query_options: &bazel_rs::engine::query::QueryOptions,
    cap_mb: Option<u64>,
) -> usize {
    let auto = || {
        bazel_rs::memory_limit::bound_auto_loading_threads(
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1),
            cap_mb,
        )
    };
    match (command, explicit) {
        ("query", e) => e
            .or(query_options.loading_phase_threads)
            .unwrap_or_else(auto),
        (_, Some(v)) => v,
        _ => auto(),
    }
}

/// Raw parse marker for `--loading_phase_threads=auto` in the native
/// (build/run/test) parser: `usize::MAX` stands for "auto" until main
/// resolves it against the memory cap; explicit integers pass through.
/// (`query` tracks auto as `QueryOptions::loading_phase_threads: None`.)
const LOADING_AUTO: usize = usize::MAX;

/// `--loading_phase_threads` (LoadingPhaseThreadsOption.java): "auto" or
/// a positive integer; "auto" is the LOADING_AUTO marker, expanded by
/// main through the memory-cap bound (as Bazel's converter expands it
/// to the available processors, then capped).
fn parse_loading_phase_threads(value: &str) -> Result<usize, String> {
    if value == "auto" {
        return Ok(LOADING_AUTO);
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
    /// BAZEL_RS_MAX_MEMORY_MB; unset = the default cap, the smaller of
    /// 16 GB and 50% of RAM (crate::memory_limit). Prefer the env var in
    /// shared setups.
    memory_limit_mb: Option<u64>,
    /// The N40 rc options outside the configuration (crate::rc_options).
    rc: bazel_rs::rc_options::RcOptions,
    resolver: bazel_rs::module_graph::ResolverOptions,
    downloads: bool,
    configuration: bazel_rs::configuration::CommandLineOptions,
    /// QueryCommand's own options (QueryOptions/CommonQueryOptions).
    query: bazel_rs::engine::query::QueryOptions,
    cquery: bazel_rs::engine::cquery::Options,
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
    /// The persistent worker options (crate::worker).
    worker: bazel_rs::worker::WorkerOptions,
    /// --workspace_status_command/--embed_label (WorkspaceStatusAction
    /// .Options; N63, crate::workspace_status).
    workspace_status: bazel_rs::workspace_status::WorkspaceStatusOptions,
    /// `--nobuild` (BuildRequestOptions.performExecutionPhase :154-160):
    /// stop before the execution phase; the workspace status action (an
    /// execution-phase artifact) is not planned.
    no_build: bool,
    /// The UI options of the interactive progress display (UiOptions,
    /// crate::progress).
    ui: bazel_rs::progress::UiOptions,
    /// The build event files of build/test/run (crate::bep, docs/BEP.md).
    bep: bazel_rs::bep::Options,
    /// `--profile` (crate::profile, docs/PROFILE.md).
    profile: bazel_rs::profile::Options,
    /// The spawn execution log (crate::exec_log).
    exec_log: bazel_rs::exec_log::Options,
    /// ModCommand's ModOptions (crate::mod_command).
    module: bazel_rs::mod_command::ModOptions,
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
                allow_remote: matches!(
                    command,
                    "build" | "query" | "cquery" | "run" | "test" | "mod"
                ),
                // Bazel 9.3 always injects @bazel_tools and its MODULE
                // closure (ModuleFileFunction, BazelRepositoryModule); the
                // native explicit-module profile is an opt-out
                // (--experimental-builtin-modules=false).
                builtin_profile: bazel_rs::module_graph::BuiltinProfile::Bazel93Builtins,
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
            worker: bazel_rs::worker::WorkerOptions::default(),
            workspace_status: bazel_rs::workspace_status::WorkspaceStatusOptions::default(),
            no_build: false,
            ui: bazel_rs::progress::UiOptions::default(),
            bep: bazel_rs::bep::Options::default(),
            profile: bazel_rs::profile::Options::default(),
            exec_log: bazel_rs::exec_log::Options::default(),
            module: bazel_rs::mod_command::ModOptions::default(),
            configuration: if command == "query" {
                // query is configuration-independent; it accepts (and validates)
                // the modelled CoreOptions (runtime/commands/QueryCommand.java:59-67).
                bazel_rs::configuration::CommandLineOptions::for_query()
            } else {
                bazel_rs::configuration::CommandLineOptions::default()
            },
            query: bazel_rs::engine::query::QueryOptions::default(),
            cquery: bazel_rs::engine::cquery::Options::default(),
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
        } else if command == "cquery" {
            self.cquery.parse(arg, next)?
        } else {
            None
        };
        // The build event files, the profile and the execution log of the
        // commands that build (crate::bep, crate::profile, crate::exec_log).
        let reports = builds(command);
        // Cquery inherits TestCommand options, but only the explicit
        // analysis dispatch above decides whether anything executes.
        let command = if command == "cquery" { "test" } else { command };
        let used_next = if let Some(used) = query {
            used
        } else if let Some(used) = (command == "mod")
            .then(|| self.module.parse(arg, next))
            .flatten()
        {
            used
        } else if let Some(used) = self.rc.parse(
            if command == "cquery" { "test" } else { command },
            arg,
            next,
            &unit.option.origin_suffix(),
        )? {
            used
        } else if let Some(used) = self.ui.parse(arg, next) {
            // UiOptions (every command): the progress display's.
            used?
        } else if let Some(disabled) =
            bazel_rs::module_cli::repository_disable_download_argument(arg)?
        {
            self.downloads = !disabled;
            false
        } else if arg == "--incompatible_autoload_externally=" {
            // This engine never autoloads native symbols from external rules.
            // The explicit empty Bazel setting requests precisely that model;
            // nonempty autoload requests remain unsupported, not ignored.
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
        } else if builds(command) && arg.starts_with("--nobuild=") {
            // The `no`-prefixed boolean form takes no value
            // (OptionsParserImpl: `--nobuild=false :: Unexpected value
            // after boolean option: --nobuild=false`, genuine capture).
            return Err(
                match bazel_rs::configuration::boolean_option(arg, "build") {
                    Some(Ok(_)) | None => {
                        format!("{arg} :: Unexpected value after boolean option: {arg}")
                    }
                    Some(Err(error)) => error,
                },
            );
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
            && let Some(consumed) = self.parse_worker_option(arg, next)?
        {
            consumed
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
            && let Some(parsed) =
                bazel_rs::rc_options::value_option(arg, next, "sandbox_add_mount_pair")
        {
            let (form, value, used) = parsed?;
            self.parse_mount_pair(&form, value, &unit.option.origin_suffix())?;
            used
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
        } else if builds(command) && bazel_rs::runfiles_tree::is_enable_runfiles(arg) {
            // CoreOptions.enableRunfiles is a configuration value
            // (config_setting `values`, the exec propagation, the identity:
            // crate::configuration::VALUE_OPTIONS) that the runfiles trees
            // read too: the configuration converts and records it first, so
            // its errors are the TriStateConverter's.
            self.configuration.parse(arg, None)?;
            if let Some(result) = self.runfiles.parse(arg) {
                result?;
            }
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
        } else if let Some(used) = reports.then(|| self.bep.parse(arg, next)).flatten() {
            used?
        } else if let Some(used) = reports.then(|| self.profile.parse(arg, next)).flatten() {
            used?
        } else if let Some(used) = reports.then(|| self.exec_log.parse(arg, next)).flatten() {
            used?
        } else if let Some(used) = self.credentials.parse(arg, next) {
            used?
        } else if let Some(used) = self.remote.parse(arg, next) {
            used?
        } else if builds(command) && self.test.parse_keep_going(arg)? {
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

    /// The persistent worker options (WorkerOptions.java; crate::worker):
    /// `Some(consumed next)` when `arg` is one of them.
    fn parse_worker_option(
        &mut self,
        arg: &str,
        next: Option<&str>,
    ) -> Result<Option<bool>, String> {
        const BOOLEANS: &[&str] = &[
            "worker_quit_after_build",
            "worker_verbose",
            "worker_sandboxing",
            "worker_multiplex",
            "experimental_worker_multiplex",
            "experimental_worker_multiplex_sandboxing",
            "experimental_worker_strict_flagfiles",
        ];
        for name in BOOLEANS {
            if let Some(value) = bazel_rs::configuration::boolean_option(arg, name) {
                let value = value?;
                let worker = &mut self.worker;
                match *name {
                    "worker_quit_after_build" => worker.quit_after_build = value,
                    "worker_verbose" => worker.verbose = value,
                    "worker_sandboxing" => worker.sandboxing = value,
                    "worker_multiplex" | "experimental_worker_multiplex" => {
                        worker.multiplex = value
                    }
                    "experimental_worker_multiplex_sandboxing" => {
                        worker.multiplex_sandboxing = value
                    }
                    _ => worker.strict_flagfiles = value,
                }
                return Ok(Some(false));
            }
        }
        const VALUED: &[&str] = &[
            "worker_max_instances",
            "worker_max_multiplex_instances",
            "experimental_worker_max_multiplex_instances",
            "worker_extra_flag",
            "experimental_worker_allowlist",
        ];
        let Some(dashless) = arg.strip_prefix("--") else {
            return Ok(None);
        };
        let (name, value, consumed) = match dashless.split_once('=') {
            Some((name, value)) if VALUED.contains(&name) => (name, value, false),
            None if VALUED.contains(&dashless) => (
                dashless,
                next.ok_or_else(|| format!("--{dashless} requires a value"))?,
                true,
            ),
            _ => return Ok(None),
        };
        let worker = &mut self.worker;
        match name {
            "worker_max_instances" => {
                if let Some(entry) = bazel_rs::worker::parse_max_instances(name, value)? {
                    worker.max_instances.push(entry);
                }
            }
            "worker_max_multiplex_instances" | "experimental_worker_max_multiplex_instances" => {
                if let Some(entry) =
                    bazel_rs::worker::parse_max_instances("worker_max_multiplex_instances", value)?
                {
                    worker.max_multiplex_instances.push(entry);
                }
            }
            "worker_extra_flag" => {
                // AssignmentConverter: `name=value`.
                let Some((mnemonic, flag)) = value.split_once('=') else {
                    return Err(format!(
                        "While parsing option --worker_extra_flag={value}: Variable definitions \
                         must be in the form of a 'name=value' assignment"
                    ));
                };
                worker
                    .extra_flags
                    .push((mnemonic.to_owned(), flag.to_owned()));
            }
            _ => {
                // CommaSeparatedOptionListConverter; the last flag wins.
                worker.allowlist = Some(
                    value
                        .split(',')
                        .filter(|entry| !entry.is_empty())
                        .map(str::to_owned)
                        .collect(),
                );
            }
        }
        Ok(Some(consumed))
    }

    /// One `--sandbox_add_mount_pair` value (SandboxOptions.java:173-180,
    /// MountPairConverter): converter errors are Bazel's; an identity mount
    /// of an absolute path (`/tmp`, `/tmp:/tmp` — ~/code's `build:linux_*`
    /// line) is recorded with no further effect (crate::sandbox::SpawnOptions
    /// ::sandbox_add_mount_pair); any other pair would need a remount the
    /// Landlock sandbox cannot make and fails explicitly.
    fn parse_mount_pair(&mut self, form: &str, value: &str, origin: &str) -> Result<(), String> {
        let (source, target) = bazel_rs::sandbox::mount_pair(value)
            .map_err(|error| format!("While parsing option {form}: {error}"))?;
        if source != target || !Path::new(&source).is_absolute() {
            return Err(format!(
                "unsupported option --sandbox_add_mount_pair{origin}: only identity mounts of an absolute path (PATH or PATH:PATH) are modelled; the native sandbox cannot remount a path (docs/SANDBOX.md)"
            ));
        }
        self.spawn
            .sandbox_add_mount_pair
            .push(PathBuf::from(source));
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

/// Credential helpers and the remote cache (CredentialModule,
/// BazelRepositoryModule.beforeCommand and RemoteModule.setup; N41,
/// docs/REMOTE_CACHE.md): helper paths resolve eagerly for every command; an
/// http(s) or gRPC cache is set up for every command too (RemoteOptions is a common
/// option class), so an http:// cache invokes the helper at startup.
/// Returns whether a remote cache is active.
fn setup_credentials_and_remote(
    root: &Path,
    credential_options: &bazel_rs::credentials::CredentialOptions,
    remote_options: &bazel_rs::remote_cache::RemoteOptions,
) -> Result<bool, String> {
    let path = std::env::var("PATH").unwrap_or_default();
    // RemoteModule.beforeCommand creates the credentials for any remote
    // cache (http(s) or gRPC).
    let remote_cache = !remote_options.executor.is_empty() || !remote_options.cache.is_empty();
    let credentials =
        match bazel_rs::credentials::Credentials::new(credential_options, Some(root), &path) {
            Ok(credentials) => credentials,
            Err(error) => {
                // Both modules report the path error; the last exit wins
                // (BazelRepositoryModule.java:446-477, RemoteModule.java:604-621).
                eprintln!("ERROR: {error}");
                if remote_cache {
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
    // --quiet (crate::ui): the command line's value until the rc files'
    // `startup` lines are processed too.
    bazel_rs::ui::set_quiet(bazelrc::search_nullary(&line.startup_args, "quiet", false));
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
    let startup =
        match bazelrc::process_startup_options(&rc_files, line, &mut warnings, &mut provenance) {
            Ok(startup) => startup,
            Err(error) => {
                for note in provenance {
                    bazel_rs::note!("INFO: {note}");
                }
                return Err(error);
            }
        };
    bazel_rs::ui::set_quiet(startup.quiet);
    if let Some(path) = startup.output_base {
        bazel_rs::worktree::install_output_base(cwd, &path)?;
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
        bazel_rs::note!("INFO: {note}");
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
            bazel_rs::note!("INFO: {note}");
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

/// A WARNING event (informational: dropped under `--quiet`, crate::ui).
fn warn(warning: &str) {
    bazel_rs::note!("WARNING: {}", located(warning));
}

/// FetchCommand's own options (FetchOptions, PackageOptions.fetch).
#[derive(Debug)]
struct FetchOptions {
    all: bool,
    configure: bool,
    repos: Vec<String>,
    force: bool,
    /// `--[no]fetch` (PackageOptions): fetch refuses `--nofetch`.
    fetch: bool,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            all: false,
            configure: false,
            repos: Vec::new(),
            force: false,
            fetch: true,
        }
    }
}

impl FetchOptions {
    /// One fetch option: `Some(used_next)` when it is one.
    fn parse(&mut self, arg: &str, next: Option<&str>) -> Result<Option<bool>, String> {
        for (name, slot) in [
            ("all", &mut self.all),
            ("configure", &mut self.configure),
            ("force", &mut self.force),
            ("fetch", &mut self.fetch),
        ] {
            if let Some(value) = bazel_rs::configuration::boolean_option(arg, name) {
                *slot = value?;
                return Ok(Some(false));
            }
        }
        if arg == "--repo" {
            let value = next.ok_or("Expected value after --repo")?;
            self.repos.push(value.to_owned());
            return Ok(Some(true));
        }
        if let Some(value) = arg.strip_prefix("--repo=") {
            self.repos.push(value.to_owned());
            return Ok(Some(false));
        }
        Ok(None)
    }
}

/// What `fetch` fetches (FetchCommand.exec).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FetchMode {
    /// Target patterns: `build --nobuild` (TargetFetcher.fetchTargets).
    Targets,
    /// `--repo`, `--all`/no argument, `--configure`.
    Repositories,
}

/// This command is `fetch TARGET...` (a `build --nobuild` that reports
/// fetching only; a failure is `Fetching some target dependencies failed
/// with errors: ...`, exit 2).
static FETCH_TARGETS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// One option unit of a pipeline command: `fetch`'s own options first,
/// then the native options (of `test` for `fetch`, which inherits them).
fn parse_command_option(
    native: &mut NativeOptions,
    fetch: &mut FetchOptions,
    command: &str,
    unit: &bazelrc::Unit,
) -> Result<(), String> {
    if analyzes(command) && bazel_rs::flag_alias::definition_value(unit).is_some() {
        // Registered/replayed in parse order after MODULE resolution.
        return Ok(());
    }
    if command == "fetch" {
        let arg = unit.option.text.as_str();
        let next = unit.value.as_ref().map(|value| value.text.as_str());
        if let Some(used) = fetch.parse(arg, next)? {
            if let (Some(value), false) = (next, used) {
                return Err(format!(
                    "internal error: {arg} took the separate value {value:?} but its native parser did not consume it"
                ));
            }
            return Ok(());
        }
        return native.parse("test", unit);
    }
    native.parse(command, unit)
}

fn alias_candidate_error(error: &str, unit: Option<&bazelrc::Unit>) -> Result<bool, String> {
    // Only unrecognized/unmodelled *names* can be supplied by an alias.
    // Never defer an implemented option's rejected value/profile: execution
    // options may be absent from snapshot keys and a hit must not hide errors.
    if error.contains(" :: Unrecognized option: ") {
        return Ok(true);
    }
    if error.starts_with("unsupported option \"")
        && error.contains("options are never silently ignored")
        && let Some(unit) = unit
    {
        return bazel_rs::flag_alias::defer_unmodelled_option(&unit.option.text);
    }
    Ok(false)
}

/// Explicit --flag_alias already affects the first parsing round. In
/// particular, --flag_alias=compilation_mode=//:text --compilation_mode=arbitrary
/// must not feed "arbitrary" to the native compilation-mode converter.
fn first_pass_flag_alias_options(options: &bazelrc::Expanded, command: &str) -> bazelrc::Expanded {
    use bazel_rs::flag_alias;
    let mut result = options.clone();
    if !analyzes(command) {
        return result;
    }
    let mut aliases = flag_alias::Aliases::new();
    let mut mapped = Vec::new();
    for (step, _) in &mut result.steps {
        if let bazelrc::Step::Unit(unit) = step {
            let original = unit.clone();
            let rewrite = (|| -> Result<(), String> {
                if let Some(value) = flag_alias::definition_value(unit) {
                    let (name, label) = flag_alias::definition(
                        value,
                        &bazel_rs::configuration::parse_canonical_option_label,
                    )
                    .map_err(|error| {
                        format!("While parsing option --flag_alias={value}: {error}")
                    })?;
                    aliases.insert(name, label);
                } else if let Some(text) = flag_alias::rewrite(&unit.option.text, &aliases) {
                    check_alias_option_surface(command, unit)?;
                    unit.option.text = text;
                }
                Ok(())
            })();
            if let Err(error) = rewrite {
                // Do not report this error ahead of an earlier native parse
                // failure. Keep it in the same step stream as every option.
                *step = bazelrc::Step::Fail(error);
                break;
            }
            mapped.push((original, unit.clone()));
        }
    }
    for unit in &mut result.priority {
        if let Some(index) = mapped.iter().position(|(original, _)| original == unit) {
            *unit = mapped.remove(index).1;
        }
    }
    result
}

fn check_alias_option_surface(command: &str, unit: &bazelrc::Unit) -> Result<(), String> {
    use bazel_rs::configuration::{CommandLineOptions, Parsed};
    let next = unit.value.as_ref().map(|v| v.text.as_str());
    let original = CommandLineOptions::default().parse(&unit.option.text, next);
    if matches!(original, Ok(Parsed::NotModelled))
        && parse_command_option(
            &mut NativeOptions::new(command),
            &mut FetchOptions::default(),
            command,
            unit,
        )
        .is_ok()
    {
        return Err(format!(
            "flag alias {} shadows a non-configuration option; aliasing bootstrap/execution options is not implemented in the native profile",
            unit.option.text
        ));
    }
    if unit.value.is_some() {
        return Err(format!(
            "flag alias {} requires --name=value syntax; split native-option values cannot be used as Starlark flag values",
            unit.option.text
        ));
    }
    Ok(())
}

/// The second option parsing round, inside the snapshot recording window.
/// MODULE aliases precede rc/CLI definitions. Use parse order for substitutions
/// (config expansions are parsed last), but priority order for option values.
fn resolve_flag_alias_options(
    options: &bazelrc::Expanded,
    repositories: &bazel_rs::repository::RepositorySet,
    command: &str,
    labels: &bazel_rs::configuration::LabelParser<'_>,
) -> Result<
    (
        bazel_rs::configuration::CommandLineOptions,
        bazel_rs::flag_alias::Aliases,
    ),
    String,
> {
    use bazel_rs::{configuration::CommandLineOptions, flag_alias};
    // toBuilder() retains the first pass's explicit alias mappings. A
    // previously unknown name may therefore be used before its definition
    // in the second pass. MODULE definitions are injected *after* that seed.
    let mut aliases = flag_alias::Aliases::new();
    for (step, _) in &options.steps {
        if let bazelrc::Step::Unit(unit) = step
            && let Some(input) = flag_alias::definition_value(unit)
        {
            let (name, label) = flag_alias::definition(input, labels)
                .map_err(|e| format!("While parsing option --flag_alias={input}: {e}"))?;
            aliases.insert(name, label);
        }
    }
    aliases.extend(flag_alias::module_aliases(repositories)?);
    let mut rewritten = Vec::new();
    for (step, _) in &options.steps {
        let bazelrc::Step::Unit(unit) = step else {
            if let bazelrc::Step::Fail(error) = step {
                return Err(error.clone());
            }
            continue;
        };
        if let Some(input) = flag_alias::definition_value(unit) {
            let (name, label) = flag_alias::definition(input, labels)
                .map_err(|e| format!("While parsing option --flag_alias={input}: {e}"))?;
            aliases.insert(name, label);
            continue;
        }
        let mut mapped = unit.clone();
        if let Some(text) = flag_alias::rewrite(&unit.option.text, &aliases) {
            // Other command consumers have already bootstrapped (registry,
            // remote policy, execution options). Never accept an alias while
            // silently retaining a native side effect from the first pass.
            check_alias_option_surface(command, unit)?;
            mapped.option.text = text;
        }
        parse_command_option(
            &mut NativeOptions::new(command),
            &mut FetchOptions::default(),
            command,
            &mapped,
        )?;
        rewritten.push((unit.clone(), mapped));
    }
    let mut result = CommandLineOptions::default();
    for unit in &options.priority {
        if flag_alias::definition_value(unit).is_some() {
            continue;
        }
        let index = rewritten
            .iter()
            .position(|(original, _)| original == unit)
            .ok_or_else(|| {
                format!(
                    "internal error: missing alias parse step for {}",
                    unit.option.text
                )
            })?;
        let (_, mapped) = rewritten.remove(index);
        result.parse(
            &mapped.option.text,
            mapped.value.as_ref().map(|v| v.text.as_str()),
        )?;
    }
    Ok((result, aliases))
}

/// An `ERROR: message` failure with Bazel's exit code (the digest memo
/// saved first, like every exit).
fn fail_with(code: i32, message: &str) -> ! {
    eprintln!("ERROR: {message}");
    bazel_rs::digest_memo::finish();
    exit(code);
}

/// `fetch --repo=...`, `fetch --all` and `fetch --configure`
/// (FetchCommand.fetchRepos/fetchAll, docs/COMMANDS.md): the repositories
/// materialized exactly as a build demands them (warm publications reused
/// unless `--force`). Failures exit 8 like Bazel (an interrupted-class
/// detailed exit code).
fn fetch_repositories(
    repositories: &bazel_rs::repository::RepositorySet,
    fetch: &FetchOptions,
    lockfile_gate: &LockfileGate,
) -> Result<(), String> {
    if !fetch.repos.is_empty() {
        // RepositoryFetcher.collectRepositoryNames: every name first.
        let mut names = Vec::new();
        for repo in &fetch.repos {
            match repositories.fetch_repository_name(repo) {
                Ok(name) => names.push(name),
                Err(detail) => fail_with(8, &format!("Invalid repo name: {detail}")),
            }
        }
        let mut failures = Vec::new();
        for name in names {
            let canonical = match name {
                Ok(canonical) => canonical,
                Err(failure) => {
                    failures.push(failure);
                    continue;
                }
            };
            match repositories.repository_defined(&canonical) {
                Ok(true) if canonical.is_empty() => {}
                Ok(true) => {
                    if let Err(error) = repositories.root(&canonical) {
                        fail_with(8, &error);
                    }
                }
                Ok(false) => failures.push(format!("Repository '@@{canonical}' is not defined")),
                Err(error) => fail_with(8, &error),
            }
        }
        if !failures.is_empty() {
            fail_with(
                8,
                &format!(
                    "Fetching some repos failed with errors: {}",
                    failures.join("; ")
                ),
            );
        }
        lockfile_gate.open();
        write_lockfile();
        bazel_rs::note!("INFO: All requested repos fetched successfully.");
        return Ok(());
    }
    let names = match repositories.all_repository_names(fetch.configure) {
        Ok(names) => names,
        Err(error) => fail_with(8, &error),
    };
    for name in &names {
        if let Err(error) = repositories.root(name) {
            fail_with(8, &error);
        }
    }
    lockfile_gate.open();
    write_lockfile();
    bazel_rs::note!("INFO: All external dependencies fetched successfully.");
    Ok(())
}

/// The output base lock of this process (bazel_rs::output_base), held
/// until the process exits.
static OUTPUT_BASE_LOCK: std::sync::OnceLock<bazel_rs::output_base::Lock> =
    std::sync::OnceLock::new();

/// Hold the output base lock of `root`'s state directory in `cache`
/// shared for the rest of the process. Best effort: a state directory
/// that cannot hold a lock file (a read-only cache) runs unlocked, and so
/// does a command on a cache that does not exist yet — it must create
/// nothing when it fails before writing (a `clean` meanwhile finds nothing
/// to delete).
fn hold_output_base_lock(cache: &Path, root: &Path, command: &str) {
    let state = bazel_rs::worktree::output_base_for(cache, root);
    if !cache.is_dir() && !state.is_dir() {
        return;
    }
    if let Ok(lock) = bazel_rs::output_base::acquire(
        &state,
        bazel_rs::output_base::LockMode::Shared,
        command,
        true,
        &mut |line| bazel_rs::ui::note(line),
    ) {
        let _ = OUTPUT_BASE_LOCK.set(lock);
    }
}

/// The cache of a command that does not run the pipeline (`info`,
/// `clean`): `--cache-dir`, else the workspace's default (crate::worktree).
fn command_cache(line: &bazelrc::CommandLine, cwd: &Path, root: &Path) -> PathBuf {
    let cache = line
        .cache_dir
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| bazel_rs::worktree::detect(root).default_cache());
    if cache.is_absolute() {
        cache
    } else {
        cwd.join(cache)
    }
}

/// `bazel-rs clean [--expunge] [--async]` (CleanCommand, docs/COMMANDS.md):
/// the workspace's output tree and per-workspace state, or with
/// `--expunge` its whole state directory (the native output base); the
/// shared store is never touched. Inherits the build options.
fn clean_command(
    line: &bazelrc::CommandLine,
    options: &bazelrc::Expanded,
    cwd: &Path,
    root: PathBuf,
) -> Result<(), String> {
    fn parse(
        native: &mut NativeOptions,
        clean: &mut bazel_rs::output_base::CleanOptions,
        unit: &bazelrc::Unit,
    ) -> Result<(), String> {
        let arg = unit.option.text.as_str();
        for (name, slot) in [
            ("expunge", &mut clean.expunge),
            ("async", &mut clean.asynchronous),
        ] {
            if let Some(value) = bazel_rs::configuration::boolean_option(arg, name) {
                *slot = value?;
                return Ok(());
            }
        }
        // --expunge_async expands to --expunge --async.
        if arg == "--expunge_async" {
            clean.expunge = true;
            clean.asynchronous = true;
            return Ok(());
        }
        native.parse("build", unit)
    }
    for (step, position) in &options.steps {
        let result = match step {
            bazelrc::Step::Fail(error) => Err(error.clone()),
            bazelrc::Step::Unit(unit) => parse(
                &mut NativeOptions::new("build"),
                &mut Default::default(),
                unit,
            ),
        };
        if let Err(error) = result {
            return Err(early_exit(options, &error, *position));
        }
    }
    let mut native = NativeOptions::new("build");
    let mut clean = bazel_rs::output_base::CleanOptions::default();
    for unit in &options.priority {
        parse(&mut native, &mut clean, unit)?;
    }
    announce(options);
    // CleanCommand.exec: residue (Starlark options aside) is an error,
    // ARGUMENTS_NOT_RECOGNIZED (exit 2).
    if !options.residue.is_empty() {
        let residue: Vec<&str> = options
            .residue
            .iter()
            .map(|arg| arg.text.as_str())
            .collect();
        fail_with(2, &format!("Unrecognized arguments: {}", residue.join(" ")));
    }
    // canUseAsync.
    bazel_rs::note!(
        "INFO: {}",
        if clean.asynchronous {
            "Starting clean."
        } else {
            "Starting clean (this may take a while). Use --async if the clean takes more than several minutes."
        }
    );
    // Like every Bazel command, clean has an output base (it creates the
    // state directory if needed, to hold its lock).
    let cache = command_cache(line, cwd, &root);
    let state = bazel_rs::worktree::output_base_for(&cache, &root);
    let lock = match bazel_rs::output_base::acquire(
        &state,
        bazel_rs::output_base::LockMode::Exclusive,
        "clean",
        true,
        &mut |line| bazel_rs::ui::note(line),
    ) {
        Ok(lock) => lock,
        Err(bazel_rs::output_base::LockError::Io(error)) => fail_with(36, &error),
        Err(bazel_rs::output_base::LockError::Held) => fail_with(
            9,
            "Exiting because the output base lock is held and --noblock_for_lock was given.",
        ),
    };
    let result =
        bazel_rs::output_base::clean(&state, clean, &mut |line| bazel_rs::note!("INFO: {line}"));
    drop(lock);
    if let Err(error) = result {
        fail_with(error.exit_code, &error.message);
    }
    Ok(())
}

/// `bazel-rs shutdown [--iff_heap_size_greater_than=N]` (ShutdownCommand):
/// bazel-rs has no server, so there is nothing to stop — exit 0 and print
/// nothing, like genuine Bazel when no server runs. The options are
/// validated like Bazel (mustRunInWorkspace = false, allowResidue = false).
fn shutdown_command(line: &bazelrc::CommandLine, path_to_binary: &str) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let root = workspace::workspace_root(&cwd).unwrap_or_default();
    let options = evaluate_rc(line, "shutdown", &cwd, &root, path_to_binary)?;
    for (step, position) in &options.steps {
        let unit = match step {
            bazelrc::Step::Fail(error) => return Err(early_exit(&options, error, *position)),
            bazelrc::Step::Unit(unit) => unit,
        };
        let arg = unit.option.text.as_str();
        let value = if arg == "--iff_heap_size_greater_than" {
            unit.value.as_ref().map(|value| value.text.as_str())
        } else {
            arg.strip_prefix("--iff_heap_size_greater_than=")
        };
        if let Some(value) = value {
            if value.trim().parse::<i32>().is_err() {
                fail_with(
                    2,
                    &format!(
                        "While parsing option --iff_heap_size_greater_than={value}: '{value}' is not an int"
                    ),
                );
            }
            continue;
        }
        // rc `common` lines apply to every command; shutdown has no other
        // option that changes anything. The UI options (`--curses`,
        // `--color`, ...) are common options of every command: validated,
        // without effect (shutdown shows no progress display).
        if unit.option.from_rc() {
            continue;
        }
        let next = unit.value.as_ref().map(|value| value.text.as_str());
        match bazel_rs::progress::UiOptions::default().parse(arg, next) {
            Some(Ok(_)) => continue,
            Some(Err(error)) => fail_with(2, &error),
            None => {}
        }
        fail_with(2, &format!("{arg} :: Unrecognized option: {arg}"));
    }
    announce(&options);
    if !options.residue.is_empty() {
        let residue: Vec<&str> = options
            .residue
            .iter()
            .map(|arg| arg.text.as_str())
            .collect();
        fail_with(2, &format!("Unrecognized arguments: {}", residue.join(" ")));
    }
    Ok(())
}

fn main() {
    // The process-wrapper (crate::process_wrapper): the executor starts this
    // binary with the hidden `__process-wrapper` subcommand where Bazel
    // starts its process-wrapper tool, to supervise one spawn. Dispatched
    // before anything else, on the raw arguments (a repository command's
    // argv need not be UTF-8): it is not a bazel command, shares none of the
    // client state and prints nothing (its stderr is the spawn's).
    let mut raw = std::env::args_os().skip(1);
    if raw
        .next()
        .is_some_and(|arg| arg == bazel_rs::process_wrapper::ARG)
    {
        std::process::exit(bazel_rs::process_wrapper::main(&raw.collect::<Vec<_>>()));
    }
    bazel_rs::progress::process_start();
    // This process is the CLI binary: spawns run under its re-executed
    // process-wrapper.
    bazel_rs::process_wrapper::enable();
    // A panic unwinding out of `run` ends the progress display (its message
    // is in the display's pipe by then).
    let _progress = ProgressGuard;
    let result = run();
    // Persistent workers live for this command only (no server).
    bazel_rs::worker::shutdown();
    // BAZEL_RS_SNAPSHOT=verify: a hit whose shadow full analysis ended
    // before planning completed is a mismatch (reported and logged; the
    // full analysis's result stands, or exit 70 under
    // BAZEL_RS_SNAPSHOT_VERIFY_STRICT=1).
    bazel_rs::snapshot::verify::exit_if_abandoned(result.as_ref().err().map(String::as_str));
    // Save the digest memo's new records (if any), also after a failure.
    bazel_rs::digest_memo::finish();
    // N62 breakdown aid (BAZEL_RS_TIMING_MODULES=1): report before any
    // exit path, idempotent, silent unless enabled.
    bazel_rs::loading::module_timings::report();
    if let Err(error) = result {
        // A snapshot hit whose build part succeeded, failing afterwards
        // (`test` staging a runfile, `run`'s preparation): a damaged tree
        // the hit trusted heals (crate::snapshot::hit).
        bazel_rs::snapshot::hit::heal_after_failure(Some(&error));
        let error = located(&error);
        // FetchCommand.fetchTarget: QUERY_EVALUATION_ERROR, exit 2.
        if FETCH_TARGETS.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("ERROR: Fetching some target dependencies failed with errors: {error}");
            exit(2);
        }
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
        finish_reports(code, Some(&error));
        exit(code);
    }
    bazel_rs::progress::finish(0);
    finish_reports(0, None);
    finish_cache();
}

/// Exit with `code`, ending the progress display first (its summary, and
/// whatever is still in its pipe).
fn exit(code: i32) -> ! {
    bazel_rs::worker::shutdown();
    bazel_rs::progress::finish(code);
    finish_reports(code, None);
    finish_cache();
    std::process::exit(code)
}

/// Write the build event files, the profile and the execution log (each a
/// no-op unless requested) as the command ends with `code`.
fn finish_reports(code: i32, failure: Option<&str>) {
    bazel_rs::exec_log::finish();
    bazel_rs::profile::finish();
    bazel_rs::bep::finish(code, failure);
}

/// What the build event files' command line events read.
struct ReportContext<'a> {
    command: &'a str,
    line: &'a bazelrc::CommandLine,
    options: &'a bazelrc::Expanded,
    cwd: &'a Path,
    root: &'a Path,
    cache: &'a Path,
}

/// Start the requested build event files, profile and execution log
/// (crate::bep, crate::profile, crate::exec_log); an invalid combination
/// fails the command like Bazel's option checks (exit 2).
fn install_reports(
    context: ReportContext<'_>,
    bep: &bazel_rs::bep::Options,
    profile: &bazel_rs::profile::Options,
    exec_log: &bazel_rs::exec_log::Options,
) {
    if let Err(error) = exec_log.validate().and_then(|()| bep.validate()) {
        fail_with(2, &error);
    }
    let output_base = bazel_rs::worktree::state_directory(context.cache);
    let profile_file = profile.file(context.cwd, &output_base);
    if let Some(path) = &profile_file {
        bazel_rs::profile::install(path.clone(), output_base.clone(), profile);
    }
    bazel_rs::exec_log::install(exec_log, context.cwd);
    if !bep.enabled() {
        return;
    }
    bazel_rs::profile::enable_recording();
    let options = context
        .options
        .priority
        .iter()
        .map(|unit| {
            let combined = match &unit.value {
                Some(value) => format!("{}={}", unit.option.text, value.text),
                None => unit.option.text.clone(),
            };
            let (name, value) = bazel_rs::bep::split_option(&combined);
            bazel_rs::bep::OptionEntry {
                combined,
                name,
                value,
                source: match &unit.option.origin {
                    bazelrc::Origin::CommandLine => "command line options".to_owned(),
                    bazelrc::Origin::Rc { file, .. } => file.clone(),
                },
                explicit: !unit.option.from_rc(),
            }
        })
        .collect();
    let residue: Vec<String> = context
        .options
        .residue
        .iter()
        .map(|arg| arg.text.clone())
        .collect();
    let patterns = if context.command == "run" {
        residue.iter().take(1).cloned().collect()
    } else {
        residue.clone()
    };
    let header = bazel_rs::bep::Header {
        command: context.command.to_owned(),
        working_directory: context.cwd.to_path_buf(),
        workspace_directory: context.root.to_path_buf(),
        startup: context.line.startup_args.clone(),
        arguments: context.line.command_args.clone(),
        options,
        residue,
        patterns,
        execution_root: bazel_rs::output_tree::execution_root(context.cache),
        profile: profile_file,
    };
    if let Err(error) = bazel_rs::bep::install(bep, header) {
        fail_with(2, &error);
    }
}

/// The end of the command for the cache: record the last uses of its
/// entries and, when it is due and no other process uses the cache, collect
/// its garbage (docs/CACHE_GC.md).
fn finish_cache() {
    bazel_rs::cache_gc::finish(true);
}

/// Ends the progress display when a panic unwinds out of `main`.
struct ProgressGuard;

impl Drop for ProgressGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            bazel_rs::progress::abandon();
        }
    }
}

/// The build summary's execution-mode note: which strategy executed the
/// actions (N59). "sandboxed" when every executed spawn ran under the sandbox
/// strategy, "not sandboxed" otherwise (a --spawn_strategy=standalone request
/// or a kernel without Landlock support).
/// With a remote executor it starts with the number of remotely executed
/// spawns (docs/REMOTE_EXECUTION.md).
fn execution_note() -> String {
    let local = if bazel_rs::sandbox::all_sandboxed() {
        "sandboxed"
    } else {
        "not sandboxed"
    };
    match bazel_rs::remote_cache::active().filter(|remote| remote.executor().is_some()) {
        Some(remote) => format!(
            "{} executed remotely, local spawns {local}",
            remote.executions()
        ),
        None => local.to_owned(),
    }
}
