//! `bazel-rs info` (runtime/commands/InfoCommand.java and the InfoItem
//! implementations under runtime/commands/info/; docs/INFO.md): the
//! hardwired items and the module items (`command_log`,
//! CommandLogModule.java:118-134; `repository_cache`,
//! BazelRepositoryModule.java:189-208; `peak-heap-size`,
//! PostGCMemoryUseRecorder.java:245-262) in one map sorted by key (the
//! TreeMap of InfoCommand.getInfoItemMap, :346-352), each mapped to the
//! native layout. `info` never loads, analyzes or fetches anything: every
//! value comes from the options, the workspace, the cache directory and the
//! process.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The values `info` reads, gathered by the CLI before any item is printed.
#[derive(Clone, Debug)]
pub struct Context {
    /// The workspace root (WorkspaceInfoItem).
    pub workspace: PathBuf,
    /// The native cache directory (`--cache-dir`, default
    /// `<workspace>/bazel-rs-out` or, in a linked git worktree, the main
    /// checkout's; crate::worktree): the store every workspace using it
    /// shares, like Bazel's output user root (`repository_cache`).
    pub cache: PathBuf,
    /// The output base: the workspace's state directory in the cache,
    /// `<cache>/worktrees/<name>-<hash>` (crate::worktree, docs/WORKTREES.md),
    /// one per workspace like Bazel's `<output_user_root>/<md5>`.
    pub output_base: PathBuf,
    /// The top-level configuration's output directory name
    /// (`k8-fastbuild`, crate::config_registry::mnemonic).
    pub mnemonic: String,
    /// `--compilation_mode`.
    pub compilation_mode: String,
    /// `$(TARGET_CPU)`: the target platform's CPU constraint value name;
    /// `None` when it needs the platform target loaded (an explicit
    /// `--platforms`), which `info` does not do.
    pub target_cpu: Option<String>,
    /// The normalized `--define` entries (getNormalizedCommandLineBuildVariables).
    pub defines: Vec<(String, String)>,
    /// `client-env`: (name, value) of the inherited `--action_env` names,
    /// then of the inherited `--test_env` names, each sorted by name.
    pub client_action_env: Vec<(String, String)>,
    pub client_test_env: Vec<(String, String)>,
    /// The Starlark semantics options that differ from their defaults, as
    /// `name=value`.
    pub starlark_semantics: Vec<String>,
    /// The memory ceiling in MB (`--memory-limit`, `BAZEL_RS_MAX_MEMORY_MB`
    /// or the default cap): `max-heap-size`.
    pub memory_limit_mb: Option<u64>,
    /// `--repository_cache` (crate::repository_cache): `None` when unset
    /// (the native download store is the cache), `Some(None)` for the empty
    /// value (genuine prints `null`).
    pub repository_cache: Option<Option<PathBuf>>,
}

/// One info item: its name, whether `info` without keys skips it
/// (InfoItem.isHidden) and how its value is computed.
#[derive(Clone, Debug)]
pub struct Item {
    pub name: String,
    pub hidden: bool,
    kind: Kind,
}

#[derive(Clone, Debug)]
enum Kind {
    BazelBin,
    BazelGenfiles,
    BazelTestlogs,
    BuildLanguage,
    CharacterEncoding,
    ClientEnv,
    CommandLog,
    CommittedHeapSize,
    DefaultPackagePath,
    DefaultsPackage,
    ExecutionRoot,
    GcCount,
    GcTime,
    InstallBase,
    InstallMd5,
    JavaHome,
    JavaRuntime,
    JavaVm,
    LocalResources,
    MaxHeapSize,
    OutputBase,
    OutputPath,
    PackagePath,
    PeakHeapSize,
    Release,
    RepositoryCache,
    ServerLog,
    ServerPid,
    StarlarkSemantics,
    UsedHeapSize,
    UsedHeapSizeAfterGc,
    WorkerMetrics,
    Workspace,
    /// A make variable (MakeInfoItem, `--show_make_env`).
    Make(String),
}

/// Every item of Bazel 9.3's `info` (name, hidden, kind).
const ITEMS: &[(&str, bool, Kind)] = &[
    ("bazel-bin", false, Kind::BazelBin),
    ("bazel-genfiles", false, Kind::BazelGenfiles),
    ("bazel-testlogs", false, Kind::BazelTestlogs),
    ("build-language", true, Kind::BuildLanguage),
    ("character-encoding", false, Kind::CharacterEncoding),
    ("client-env", true, Kind::ClientEnv),
    ("command_log", false, Kind::CommandLog),
    ("committed-heap-size", false, Kind::CommittedHeapSize),
    ("default-package-path", true, Kind::DefaultPackagePath),
    ("defaults-package", true, Kind::DefaultsPackage),
    ("execution_root", false, Kind::ExecutionRoot),
    ("gc-count", false, Kind::GcCount),
    ("gc-time", false, Kind::GcTime),
    ("install_base", false, Kind::InstallBase),
    ("install_md5", true, Kind::InstallMd5),
    ("java-home", false, Kind::JavaHome),
    ("java-runtime", false, Kind::JavaRuntime),
    ("java-vm", false, Kind::JavaVm),
    ("local_resources", false, Kind::LocalResources),
    ("max-heap-size", false, Kind::MaxHeapSize),
    ("output_base", false, Kind::OutputBase),
    ("output_path", false, Kind::OutputPath),
    ("package_path", false, Kind::PackagePath),
    ("peak-heap-size", true, Kind::PeakHeapSize),
    ("release", false, Kind::Release),
    ("repository_cache", false, Kind::RepositoryCache),
    ("server_log", false, Kind::ServerLog),
    ("server_pid", false, Kind::ServerPid),
    ("starlark-semantics", true, Kind::StarlarkSemantics),
    ("used-heap-size", false, Kind::UsedHeapSize),
    ("used-heap-size-after-gc", true, Kind::UsedHeapSizeAfterGc),
    ("worker_metrics", true, Kind::WorkerMetrics),
    ("workspace", false, Kind::Workspace),
];

/// The InfoCommand.Options of the command line.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// `--show_make_env`.
    pub show_make_env: bool,
    /// `--info_output_type=response_proto`: the values travel in the
    /// server's response extensions, never to stdout
    /// (RemoteRequestedInfoItemHandler); a client without a server prints
    /// nothing.
    pub response_proto: bool,
}

impl Options {
    /// One InfoCommand option: `Some(used_next)` when `arg` is one.
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Result<Option<bool>, String> {
        if let Some(value) = crate::configuration::boolean_option(arg, "show_make_env") {
            self.show_make_env = value?;
            return Ok(Some(false));
        }
        let (value, used) = if arg == "--info_output_type" {
            (next.ok_or("--info_output_type requires a value")?, true)
        } else if let Some(value) = arg.strip_prefix("--info_output_type=") {
            (value, false)
        } else {
            return Ok(None);
        };
        // EnumConverter (case-insensitive) of InfoItemOutputType.
        match value.to_ascii_lowercase().as_str() {
            "stdout" => self.response_proto = false,
            "response_proto" => self.response_proto = true,
            _ => {
                return Err(format!(
                    "While parsing option --info_output_type={value}: Not a valid InfoItem output type: '{value}' (should be stdout or response_proto)"
                ));
            }
        }
        Ok(Some(used))
    }
}

/// The items of one invocation, sorted by name like Bazel's TreeMap: the
/// fixed items and, with `--show_make_env`, the make variables (a make
/// variable replaces an item of the same name, `items.put`).
pub fn items(context: &Context, options: &Options) -> Result<Vec<Item>, String> {
    let mut items: Vec<Item> = ITEMS
        .iter()
        .map(|(name, hidden, kind)| Item {
            name: (*name).to_owned(),
            hidden: *hidden,
            kind: kind.clone(),
        })
        .collect();
    if options.show_make_env {
        for (name, _) in make_environment(context)? {
            items.retain(|item| item.name != name);
            items.push(Item {
                name: name.clone(),
                hidden: false,
                kind: Kind::Make(name),
            });
        }
    }
    items.sort_by(|a, b| crate::native_rules::java_string_cmp(&a.name, &b.name));
    Ok(items)
}

/// BuildConfigurationValue.getMakeEnvironment (:611-616): the global make
/// variables (:322-333), then the `--define` entries, later entries
/// replacing earlier ones.
pub fn make_environment(context: &Context) -> Result<Vec<(String, String)>, String> {
    let target_cpu = context.target_cpu.clone().ok_or(
        "info --show_make_env with an explicit --platforms is not implemented in the native profile: $(TARGET_CPU) needs the platform target loaded, which info does not do (docs/INFO.md)",
    )?;
    let bin = crate::config_registry::output_root(&context.mnemonic);
    let mut environment = vec![
        ("TARGET_CPU".to_owned(), target_cpu),
        (
            "COMPILATION_MODE".to_owned(),
            context.compilation_mode.clone(),
        ),
        ("BINDIR".to_owned(), bin.clone()),
        ("GENDIR".to_owned(), bin),
    ];
    for (name, value) in &context.defines {
        match environment.iter_mut().find(|(seen, _)| seen == name) {
            Some(entry) => entry.1 = value.clone(),
            None => environment.push((name.clone(), value.clone())),
        }
    }
    Ok(environment)
}

/// What one `info` printed and how it ended.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// `unknown key(s): ...` (exit 2, KEY_NOT_RECOGNIZED) once the known
    /// keys are printed.
    pub unknown: Option<String>,
}

/// InfoCommand.exec (:178-261): every non-hidden item as `key: value`
/// without keys, the value alone for one key, `key: value` lines for
/// several (in request order, repeats included).
pub fn run(
    context: &Context,
    options: &Options,
    keys: &[String],
    out: &mut dyn Write,
) -> Result<Outcome, String> {
    let items = items(context, options)?;
    let print_keys = keys.len() != 1;
    let mut emit = |item: &Item| -> Result<(), String> {
        let value = value(context, &item.kind)?;
        if options.response_proto {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity(item.name.len() + value.len() + 3);
        if print_keys {
            bytes.extend_from_slice(item.name.as_bytes());
            bytes.extend_from_slice(b": ");
        }
        bytes.extend_from_slice(&value);
        out.write_all(&bytes)
            .map_err(|error| format!("Cannot write info block: {error}"))
    };
    if keys.is_empty() {
        for item in items.iter().filter(|item| !item.hidden) {
            emit(item)?;
        }
    } else {
        let mut unknown: Vec<&str> = Vec::new();
        for key in keys {
            match items.iter().find(|item| item.name == *key) {
                Some(item) => emit(item)?,
                None if !unknown.contains(&key.as_str()) => unknown.push(key),
                None => {}
            }
        }
        if !unknown.is_empty() {
            let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
            let listed: Vec<String> = unknown
                .iter()
                .map(|key| format!("'{key}'{}", crate::native_rules::did_you_mean(key, &names)))
                .collect();
            out.flush()
                .map_err(|error| format!("Cannot write info block: {error}"))?;
            return Ok(Outcome {
                unknown: Some(format!("unknown key(s): {}", listed.join(", "))),
            });
        }
    }
    out.flush()
        .map_err(|error| format!("Cannot write info block: {error}"))?;
    Ok(Outcome::default())
}

/// InfoItem.print (runtime/InfoItem.java:75-83): the value and a newline.
fn line(text: impl std::fmt::Display) -> Vec<u8> {
    format!("{text}\n").into_bytes()
}

fn path_line(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(b'\n');
    bytes
}

/// StringUtilities.prettyPrintBytes (util/StringUtilities.java:96-106):
/// decimal units, truncated.
pub fn pretty_print_bytes(bytes: u64) -> String {
    if bytes < 10_000 {
        format!("{bytes}B")
    } else if bytes < 10_000_000 {
        format!("{}KB", bytes / 1_000)
    } else if bytes < 100_000_000_000 {
        format!("{}MB", bytes / 1_000_000)
    } else {
        format!("{}GB", bytes / 1_000_000_000)
    }
}

fn value(context: &Context, kind: &Kind) -> Result<Vec<u8>, String> {
    let output_path = crate::output_tree::output_path_below(&context.output_base);
    let configuration = output_path.join(&context.mnemonic);
    Ok(match kind {
        Kind::BazelBin | Kind::BazelGenfiles => path_line(&configuration.join("bin")),
        Kind::BazelTestlogs => path_line(&configuration.join("testlogs")),
        Kind::BuildLanguage => {
            return Err(
                "info build-language is not implemented in the native profile: the build language protocol buffer (BuildLanguageInfoItem) needs Bazel's rule class registry (docs/INFO.md)"
                    .to_owned(),
            );
        }
        // Rust strings and every path bazel-rs accepts are UTF-8.
        Kind::CharacterEncoding => {
            line("file.encoding = UTF-8, defaultCharset = UTF-8, sun.jnu.encoding = UTF-8")
        }
        Kind::ClientEnv => {
            let mut text = String::new();
            for (name, value) in &context.client_action_env {
                text.push_str(&format!("build --action_env={name}={value}\n"));
            }
            for (name, value) in &context.client_test_env {
                text.push_str(&format!("build --test_env={name}={value}\n"));
            }
            line(text)
        }
        Kind::CommandLog => path_line(&context.output_base.join("command.log")),
        Kind::CommittedHeapSize | Kind::UsedHeapSize | Kind::UsedHeapSizeAfterGc => line(
            crate::memory_limit::rss_bytes()
                .map_or_else(|| "unknown".to_owned(), pretty_print_bytes),
        ),
        Kind::DefaultPackagePath | Kind::PackagePath => line("%workspace%"),
        Kind::DefaultsPackage => line(""),
        Kind::ExecutionRoot => path_line(&crate::output_tree::execution_root_below(
            &context.output_base,
        )),
        // No garbage collector.
        Kind::GcCount => line("0"),
        Kind::GcTime => line("0ms"),
        Kind::InstallBase => path_line(&install_base()),
        Kind::InstallMd5 => line(install_md5()?),
        // No JVM.
        Kind::JavaHome => line(""),
        Kind::JavaRuntime | Kind::JavaVm => {
            line("none (bazel-rs is a native binary without a JVM)")
        }
        Kind::LocalResources => line(format!(
            "RAM={}MB, CPU={:.1}",
            crate::memory_limit::mem_total_mb().unwrap_or(0),
            std::thread::available_parallelism().map_or(1, usize::from) as f64
        )),
        Kind::MaxHeapSize => line(context.memory_limit_mb.map_or_else(
            || "unknown".to_owned(),
            |mb| pretty_print_bytes(mb * 1024 * 1024),
        )),
        Kind::OutputBase => path_line(&context.output_base),
        Kind::OutputPath => path_line(&output_path),
        Kind::PeakHeapSize => line("unknown"),
        Kind::Release => line(format!(
            "release {}+bazel-rs.{}",
            crate::COMPATIBILITY_VERSION,
            env!("CARGO_PKG_VERSION")
        )),
        Kind::RepositoryCache => match &context.repository_cache {
            Some(Some(path)) => path_line(path),
            // RepositoryCacheInfoItem: print(null).
            Some(None) => line("null"),
            None => path_line(&context.cache.join("repositories").join("downloads")),
        },
        // DebugLoggerConfigurator.getServerLogPath().orElse(""): no log file.
        Kind::ServerLog => line(""),
        // The process that answers (genuine `--batch`: the batch JVM's pid).
        Kind::ServerPid => line(std::process::id()),
        Kind::StarlarkSemantics => {
            let mut entries = context.starlark_semantics.clone();
            entries.push("internal_bazel_only_utf_8_byte_strings=true".to_owned());
            entries.sort_by(|a, b| crate::native_rules::java_string_cmp(a, b));
            line(format!("StarlarkSemantics{{{}}}", entries.join(", ")))
        }
        Kind::WorkerMetrics => line("No persistent workers active."),
        Kind::Workspace => path_line(&context.workspace),
        Kind::Make(name) => line(
            make_environment(context)?
                .into_iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value)
                .unwrap_or_default(),
        ),
    })
}

/// The directory of the running binary (Bazel: the extracted install base).
fn install_base() -> PathBuf {
    std::env::current_exe()
        .ok()
        .map(|exe| std::fs::canonicalize(&exe).unwrap_or(exe))
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_default()
}

/// 32 hex digits identifying the running binary (Bazel: the install
/// base's MD5): the first 16 bytes of its BLAKE3 digest, the identity of
/// crate::snapshot::exe_id.
fn install_md5() -> Result<String, String> {
    let bytes = std::fs::read("/proc/self/exe")
        .map_err(|error| format!("cannot read the bazel-rs binary: {error}"))?;
    let digest = blake3::hash(&bytes);
    Ok(digest.as_bytes()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The `@platforms//cpu` value name of the host (the default target
/// platform's CPU constraint, `@platforms//host` detection).
pub fn host_platform_cpu() -> &'static str {
    match std::env::consts::ARCH {
        "x86" => "x86_32",
        "powerpc64" => "ppc64le",
        "powerpc" => "ppc32",
        arch => arch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context {
        Context {
            workspace: PathBuf::from("/ws"),
            cache: PathBuf::from("/ws/bazel-rs-out"),
            output_base: PathBuf::from("/ws/bazel-rs-out/worktrees/ws-0123456789abcdef"),
            mnemonic: "k8-fastbuild".to_owned(),
            compilation_mode: "fastbuild".to_owned(),
            target_cpu: Some("x86_64".to_owned()),
            defines: Vec::new(),
            client_action_env: Vec::new(),
            client_test_env: Vec::new(),
            starlark_semantics: Vec::new(),
            memory_limit_mb: Some(65536),
            repository_cache: None,
        }
    }

    fn output(context: &Context, options: &Options, keys: &[&str]) -> (String, Outcome) {
        let keys: Vec<String> = keys.iter().map(|key| (*key).to_owned()).collect();
        let mut out = Vec::new();
        let outcome = run(context, options, &keys, &mut out).unwrap();
        (String::from_utf8(out).unwrap(), outcome)
    }

    #[test]
    fn pretty_print_bytes_follows_string_utilities() {
        assert_eq!(pretty_print_bytes(9_999), "9999B");
        assert_eq!(pretty_print_bytes(10_000), "10KB");
        assert_eq!(pretty_print_bytes(83_000_000), "83MB");
        assert_eq!(pretty_print_bytes(68_719_476_736), "68719MB");
        assert_eq!(pretty_print_bytes(100_000_000_000), "100GB");
    }

    #[test]
    fn single_and_multiple_keys() {
        let context = context();
        let options = Options::default();
        let (single, _) = output(&context, &options, &["bazel-bin"]);
        assert_eq!(
            single,
            "/ws/bazel-rs-out/worktrees/ws-0123456789abcdef/execroot/_main/bazel-out/k8-fastbuild/bin\n"
        );
        let (several, _) = output(&context, &options, &["workspace", "workspace"]);
        assert_eq!(several, "workspace: /ws\nworkspace: /ws\n");
    }

    #[test]
    fn unknown_keys_are_listed_once_with_suggestions() {
        let (printed, outcome) = output(
            &context(),
            &Options::default(),
            &["workspace", "bazel-binn", "foo", "foo"],
        );
        assert_eq!(printed, "workspace: /ws\n");
        assert_eq!(
            outcome.unknown.as_deref(),
            Some("unknown key(s): 'bazel-binn' (did you mean 'bazel-bin'?), 'foo'")
        );
    }

    #[test]
    fn make_variables_join_the_sorted_items() {
        let mut context = context();
        context.defines = vec![
            ("FOO".to_owned(), "bar".to_owned()),
            ("COMPILATION_MODE".to_owned(), "x".to_owned()),
        ];
        let options = Options {
            show_make_env: true,
            ..Options::default()
        };
        let names: Vec<String> = items(&context, &options)
            .unwrap()
            .into_iter()
            .filter(|item| !item.hidden)
            .map(|item| item.name)
            .collect();
        assert_eq!(
            &names[..6],
            [
                "BINDIR",
                "COMPILATION_MODE",
                "FOO",
                "GENDIR",
                "TARGET_CPU",
                "bazel-bin"
            ]
        );
        let (printed, _) = output(&context, &options, &["COMPILATION_MODE"]);
        assert_eq!(printed, "x\n");
        context.target_cpu = None;
        assert!(items(&context, &options).is_err());
        assert!(items(&context, &Options::default()).is_ok());
    }

    #[test]
    fn response_proto_prints_nothing() {
        let options = Options {
            response_proto: true,
            ..Options::default()
        };
        assert_eq!(output(&context(), &options, &[]).0, "");
    }
}
