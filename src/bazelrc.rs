//! Bazel 9.3 rc-file evaluation: the client's startup-option split, rc-file
//! discovery and parsing, and the server's rc structuring and `--config`
//! expansion.
//!
//! Pinned source: `/tmp/opencode/bazel-9.3-reference` (e89755b, Release 9.3.0).
//! Client paths are relative to `src/main/cpp/`, server paths to
//! `src/main/java/com/google/devtools/`.
//!
//! * [`split_command_line`][]: OptionProcessor::SplitCommandLine
//!   (option_processor.cc:80-172) over the startup flags of startup_options.cc:121-171
//!   and bazel_startup_options.cc:29-39.
//! * [`read_rc_files`][]: OptionProcessor::GetRcFiles (option_processor.cc:374-490):
//!   system rc, `%workspace%/.bazelrc`, `~/.bazelrc`, `$BAZELRC`, then
//!   `--bazelrc` values up to `/dev/null` (blaze_util.cc:82-114), deduplicated
//!   by canonical path, each parsed by RcFile::ParseFile (rc_file.cc:85-249),
//!   with the duplicate-rc (option_processor.cc:307-340) and lost-rc
//!   (:203-243, 342-354, 467-487) warnings.
//! * [`process_startup_options`][]: StartupOptions::ProcessArgs
//!   (startup_options.cc:419-445) over `startup` and `startup:linux` rc options
//!   (option_processor.cc:603-656) then the command line, and
//!   BazelStartupOptions::MaybeLogStartupOptionWarnings (bazel_startup_options.cc:56-77).
//! * [`expand`][]: BlazeOptionHandler.structureRcOptionsAndConfigs
//!   (build/lib/runtime/BlazeOptionHandler.java:663-742), parseRcOptions
//!   (:199-251), the command line (:303-308) and ConfigExpander.expandConfigOptions
//!   (build/lib/runtime/ConfigExpander.java:90-303), producing the option
//!   units in parse order (for Bazel's error order) and in priority order (an
//!   expansion takes the priority of its `--config`, OptionsParserImpl.java:611-620),
//!   and the residue in parse-call order (OptionsParser.java:724-731).
//!
//! Only `--config`, `--announce_rc` and `--enable_platform_specific_config`
//! are evaluated here; every other option is handed, with its rc file:line,
//! to the existing native option parsers, which reject what they do not model.
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use crate::command_options::{COMMAND_OPTIONS, COMMANDS};

/// BAZEL_SYSTEM_BAZELRC_PATH for Bazel on Linux (src/main/cpp/BUILD).
pub const SYSTEM_BAZELRC: &str = "/etc/bazel.bazelrc";

/// The product name in client messages (StartupOptions::GetLowercaseProductName).
const PRODUCT: &str = "bazel";

/// Nullary startup flags (startup_options.cc:121-147, bazel_startup_options.cc:35-37).
const NULLARY_STARTUP: &[&str] = &[
    "batch",
    "batch_cpu_scheduling",
    "block_for_lock",
    "quiet",
    "client_debug",
    "preemptible",
    "fatal_event_bus_exceptions",
    "host_jvm_debug",
    "autodetect_server_javabase",
    "idle_server_tasks",
    "shutdown_on_low_sys_mem",
    "ignore_all_rc_files",
    "unlimit_coredumps",
    "write_command_log",
    "windows_enable_symlinks",
    "experimental_remote_repo_contents_cache",
    "experimental_run_in_user_cgroup",
    "home_rc",
    "system_rc",
    "workspace_rc",
];

/// RegisterNullaryStartupFlagNoRc flags: forbidden in rc files
/// (startup_options.cc:45-50, 216-224).
const NO_RC_STARTUP: &[&str] = &[
    "ignore_all_rc_files",
    "home_rc",
    "system_rc",
    "workspace_rc",
];

/// Unary startup flags (startup_options.cc:148-166, bazel_startup_options.cc:38).
const UNARY_STARTUP: &[&str] = &[
    "command_port",
    "connect_timeout_secs",
    "local_startup_timeout_secs",
    "digest_function",
    "unix_digest_hash_attribute_name",
    "server_javabase",
    "host_jvm_args",
    "host_jvm_profile",
    "invocation_policy",
    "io_nice_level",
    "install_base",
    "macos_qos_class",
    "max_idle_secs",
    "output_base",
    "output_user_root",
    "server_jvm_out",
    "failure_detail_out",
    "experimental_cgroup_parent",
    "extra_classpath",
    "bazelrc",
];

/// The command line split into startup options, command and arguments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandLine {
    /// Bazel startup options, unary ones normalized to `--name=value`.
    pub startup_args: Vec<String>,
    /// Native `--cache-dir=PATH` (not a Bazel option).
    pub cache_dir: Option<String>,
    pub command: Option<String>,
    pub command_args: Vec<String>,
}

/// blaze_util.cc:134-137.
fn is_arg(arg: &str) -> bool {
    arg.starts_with('-') && arg != "--help" && arg != "-help" && arg != "-h"
}

fn startup_flag_name(arg: &str) -> &str {
    arg.split_once('=').map_or(arg, |(name, _)| name)
}

fn is_nullary_startup(flag: &str) -> bool {
    flag.strip_prefix("--").is_some_and(|name| {
        NULLARY_STARTUP.contains(&name)
            || name
                .strip_prefix("no")
                .is_some_and(|name| NULLARY_STARTUP.contains(&name))
    })
}

fn is_unary_startup(flag: &str) -> bool {
    flag.strip_prefix("--")
        .is_some_and(|name| UNARY_STARTUP.contains(&name))
}

/// StartupOptions::MaybeCheckValidNullary (startup_options.cc:184-201).
fn check_valid_nullary(arg: &str) -> Result<bool, String> {
    match arg.split_once('=') {
        None => Ok(is_nullary_startup(arg)),
        Some((flag, _)) if is_nullary_startup(flag) => Err(format!(
            "In argument '{arg}': option '{flag}' does not take a value."
        )),
        Some(_) => Ok(false),
    }
}

/// OptionProcessor::SplitCommandLine (option_processor.cc:80-172). The native
/// `--cache-dir=PATH` and the native `--version` command are recognized too.
pub fn split_command_line(args: Vec<String>) -> Result<CommandLine, String> {
    let mut line = CommandLine::default();
    let mut args = args.into_iter().peekable();
    while let Some(arg) = args.next_if(|arg| is_arg(arg)) {
        if arg == "--version" {
            line.command = Some(arg);
            line.command_args = args.collect();
            return Ok(line);
        }
        if let Some(value) = arg.strip_prefix("--cache-dir=") {
            if value.is_empty() {
                return Err("--cache-dir requires a nonempty path".into());
            }
            line.cache_dir = Some(value.to_owned());
            continue;
        }
        if check_valid_nullary(&arg)? {
            line.startup_args.push(arg);
        } else if is_unary_startup(startup_flag_name(&arg)) {
            if arg.contains('=') {
                line.startup_args.push(arg);
            } else {
                let Some(value) = args.next() else {
                    return Err(format!(
                        "Startup option '{arg}' expects a value.\nUsage: '{arg}=somevalue' or '{arg} somevalue'.\n  For more info, run '{PRODUCT} help startup_options'."
                    ));
                };
                line.startup_args.push(format!("{arg}={value}"));
            }
        } else {
            return Err(format!(
                "Unknown startup option: '{arg}'.\n  For more info, run '{PRODUCT} help startup_options'."
            ));
        }
    }
    if let Some(command) = args.next() {
        if command.is_empty() {
            return Err(format!(
                "Command cannot be the empty string.\n  For more info, run '{PRODUCT} help'."
            ));
        }
        line.command = Some(command);
    }
    line.command_args = args.collect();
    Ok(line)
}

/// GetUnaryOption (blaze_util.cc:53-65).
fn unary_option<'a>(arg: &'a str, next: Option<&'a str>, key: &str) -> Option<&'a str> {
    let rest = arg.strip_prefix(key)?;
    if let Some(value) = rest.strip_prefix('=') {
        Some(value)
    } else if rest.is_empty() {
        next
    } else {
        None
    }
}

/// SearchNullaryOption (blaze_util.cc:116-132); `--name=value` was already
/// rejected by [`split_command_line`].
pub fn search_nullary(args: &[String], name: &str, default: bool) -> bool {
    let mut result = default;
    for arg in args {
        if arg == "--" {
            break;
        }
        if *arg == format!("--{name}") {
            result = true;
        } else if *arg == format!("--no{name}") {
            result = false;
        }
    }
    result
}

/// GetAllUnaryOptionValues (blaze_util.cc:82-114).
fn all_unary_values(args: &[String], key: &str, ignore_after: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--" {
            return values;
        }
        let next = &args[(index + 1).min(args.len() - 1)];
        let rest = args[index].strip_prefix(key);
        let (value, from_next) = match rest {
            Some(rest) if rest.starts_with('=') => (Some(&rest[1..]), false),
            Some("") => (Some(next.as_str()), true),
            _ => (None, false),
        };
        if let Some(value) = value {
            values.push(value.to_owned());
            if value == ignore_after {
                break;
            }
        }
        if from_next {
            index += 1;
        }
        index += 1;
    }
    values
}

/// The process environment the client consults.
#[derive(Clone, Debug)]
pub struct ClientEnv {
    pub cwd: String,
    /// The workspace directory (empty outside a workspace).
    pub workspace: String,
    /// `$HOME` (GetHomeDir, blaze_util_posix.cc:235).
    pub home: String,
    /// `$BAZELRC`.
    pub bazelrc_env: String,
    /// argv[0] (FindRcAlongsideBinary, option_processor.cc:274-285).
    pub path_to_binary: String,
    /// BAZEL_SYSTEM_BAZELRC_PATH.
    pub system_rc: String,
    /// The client's terminal options (option_processor.cc:658-673).
    pub isatty: bool,
    pub terminal_columns: i64,
    pub emacs: bool,
}

/// blaze_util::JoinPath (util/path.cc:26-50).
fn join_path(a: &str, b: &str) -> String {
    if a.is_empty() {
        return b.to_owned();
    }
    match (a.ends_with('/'), b.starts_with('/')) {
        (true, true) => format!("{a}{}", &b[1..]),
        (true, false) | (false, true) => format!("{a}{b}"),
        (false, false) => format!("{a}/{b}"),
    }
}

/// blaze_util::MakeAbsolute (util/path_posix.cc:67-73).
fn make_absolute(cwd: &str, path: &str) -> String {
    if path.starts_with('/') || path.is_empty() {
        path.to_owned()
    } else {
        join_path(cwd, path)
    }
}

/// blaze::AbsolutePathFromFlag (blaze_util.cc:139-147).
fn absolute_path_from_flag(env: &ClientEnv, value: &str) -> String {
    if value.is_empty() {
        env.cwd.clone()
    } else if let Some(rest) = value.strip_prefix('~') {
        join_path(&env.home, rest)
    } else {
        make_absolute(&env.cwd, value)
    }
}

/// blaze_util::ResolveEnvvars (util/path_posix.cc:75-95).
fn resolve_envvars(path: &str) -> Result<String, String> {
    let mut result = path.to_owned();
    let mut start = 0;
    while let Some(found) = result[start..].find("${") {
        let begin = start + found;
        let Some(end) = result[begin + 1..].find('}').map(|end| begin + 1 + end) else {
            return Err(format!(
                "ResolveEnvvars({path}): incomplete variable at position {begin}"
            ));
        };
        let value = std::env::var(&result[begin + 2..end]).unwrap_or_default();
        result.replace_range(begin..=end, &value);
        start = begin + value.len();
    }
    Ok(result)
}

/// blaze_util::MakeCanonical (util/file_posix.cc:378-387): realpath, empty on failure.
fn canonical(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    std::fs::canonicalize(path)
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned))
        .unwrap_or_default()
}

/// blaze_util::CanReadFile (util/file_posix.cc:403-405).
fn can_read_file(path: &str) -> bool {
    !Path::new(path).is_dir() && std::fs::File::open(path).is_ok()
}

/// strerror text of an I/O error (GetLastErrorString).
fn error_string(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(index) if text.ends_with(')') => text[..index].to_owned(),
        _ => text,
    }
}

/// One option of an rc file (rc_file.h:27-34), with the line it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RcOption {
    pub command: String,
    pub option: String,
    /// Canonical path of the (possibly imported) rc file.
    pub source: String,
    /// 1-based physical line of the option's first byte.
    pub line: usize,
}

/// A top-level rc file with its imports expanded in place (rc_file.h:36-100).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RcFile {
    pub canonical_source_paths: Vec<String>,
    /// Options in order of appearance.
    pub options: Vec<RcOption>,
}

enum ParseError {
    Unreadable(String),
    Invalid(String),
}

impl ParseError {
    fn text(self) -> String {
        match self {
            ParseError::Unreadable(text) | ParseError::Invalid(text) => text,
        }
    }
}

/// SemVer::Parse (sem_ver.cc:30-44): (major, minor, patch, prerelease).
fn sem_ver(text: &str) -> Option<(i32, i32, i32, String)> {
    let (core, build) = match text.split_once('+') {
        Some((core, build)) => (core, Some(build)),
        None => (text, None),
    };
    if let Some(build) = build
        && !build.split('.').all(|part| {
            !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return None;
    }
    let (core, prerelease) = match core.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (core, None),
    };
    let number = |part: &str| {
        let valid = part == "0"
            || (!part.is_empty()
                && !part.starts_with('0')
                && part.bytes().all(|b| b.is_ascii_digit()));
        valid.then(|| part.parse::<i32>().ok()).flatten()
    };
    let parts = core.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        return None;
    }
    let (major, minor, patch) = (number(parts[0])?, number(parts[1])?, number(parts[2])?);
    if let Some(pre) = prerelease {
        let identifier = |part: &str| {
            if part.is_empty() {
                return false;
            }
            if part.bytes().all(|b| b.is_ascii_digit()) {
                return part == "0" || !part.starts_with('0');
            }
            part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        };
        if !pre.split('.').all(identifier) {
            return None;
        }
    }
    Some((
        major,
        minor,
        patch,
        prerelease.unwrap_or_default().to_owned(),
    ))
}

/// SemVer::Compare (sem_ver.cc:56-130).
fn compare_sem_ver(x: &(i32, i32, i32, String), y: &(i32, i32, i32, String)) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let core = (x.0, x.1, x.2).cmp(&(y.0, y.1, y.2));
    if core != Ordering::Equal {
        return core;
    }
    let (a, b) = (&x.3, &y.3);
    if a == b {
        return Ordering::Equal;
    }
    if a.is_empty() {
        return Ordering::Greater;
    }
    if b.is_empty() {
        return Ordering::Less;
    }
    // absl::SimpleAtoi: optional sign, decimal digits.
    let atoi = |part: &str| part.parse::<i32>().ok();
    let (xs, ys) = (
        a.split('.').collect::<Vec<_>>(),
        b.split('.').collect::<Vec<_>>(),
    );
    let mut index = 0;
    while index != xs.len() && index != ys.len() {
        let (px, py) = (xs[index], ys[index]);
        index += 1;
        if px != py {
            return match (atoi(px), atoi(py)) {
                (Some(ix), Some(iy)) => {
                    if ix < iy {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => px.cmp(py),
            };
        }
    }
    if index == xs.len() {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

/// BazelVersionMatchesCondition (rc_file.cc:270-333).
fn version_matches(
    build: &(i32, i32, i32, String),
    op: &str,
    version: &str,
) -> Result<bool, String> {
    use std::cmp::Ordering;
    if op == "~" {
        let dots = version.matches('.').count();
        let full = match dots {
            0 => format!("{version}.0.0"),
            1 => format!("{version}.0"),
            _ => version.to_owned(),
        };
        let Some(compare) = sem_ver(&full) else {
            return Err(format!(
                "Could not parse the tilde range version '{version}' as a valid semantic version."
            ));
        };
        let upper = if dots == 0 {
            (compare.0 + 1, 0, 0, String::new())
        } else {
            (compare.0, compare.1 + 1, 0, String::new())
        };
        return Ok(compare_sem_ver(build, &compare) != Ordering::Less
            && compare_sem_ver(build, &upper) == Ordering::Less);
    }
    let Some(compare) = sem_ver(version) else {
        return Err(format!(
            "Could not parse version '{version}' as a valid semantic version."
        ));
    };
    let order = compare_sem_ver(build, &compare);
    Ok(match op {
        "<" => order == Ordering::Less,
        "<=" => order != Ordering::Greater,
        ">" => order == Ordering::Greater,
        ">=" => order != Ordering::Less,
        "==" => order == Ordering::Equal,
        _ => order != Ordering::Equal,
    })
}

/// kBazelVersionCmpOp `(<=?|>=?|==|!=|~)(\S+)` as a full match (rc_file.cc:60).
fn version_condition(text: &str) -> Option<(&str, &str)> {
    let op_len = if ["<=", ">=", "==", "!="]
        .iter()
        .any(|op| text.starts_with(op))
    {
        2
    } else if text.starts_with('<') || text.starts_with('>') || text.starts_with('~') {
        1
    } else {
        return None;
    };
    let version = &text[op_len..];
    let whitespace = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c');
    (!version.is_empty() && !version.contains(whitespace)).then(|| (&text[..op_len], version))
}

/// ascii_isspace (util/strings.h:41-48).
fn is_space(byte: u8) -> bool {
    matches!(byte, 9..=13 | 32)
}

/// kSeparator " \n\t\r" for strchr, which also matches NUL (util/strings.cc:43).
fn is_separator(byte: u8) -> bool {
    matches!(byte, b' ' | b'\n' | b'\t' | b'\r' | 0)
}

/// blaze_util::Tokenize with `#` comments (util/strings.cc:181-242): words
/// with the index of their first byte.
fn tokenize(line: &[u8]) -> Vec<(Vec<u8>, usize)> {
    let mut words = Vec::new();
    let mut index = 0;
    while index < line.len() {
        while index < line.len() && is_separator(line[index]) {
            index += 1;
        }
        if index < line.len() && line[index] == b'#' {
            break;
        }
        let start = index;
        let mut output = Vec::new();
        let mut quote = 0u8;
        while index < line.len() && (quote != 0 || !is_separator(line[index])) {
            if line[index] == b'\\' {
                index += 1;
                if index == line.len() {
                    break;
                }
                output.push(line[index]);
                index += 1;
                continue;
            }
            if quote != 0 {
                if line[index] == quote {
                    quote = 0;
                } else {
                    output.push(line[index]);
                }
                index += 1;
            } else if line[index] == b'#' {
                index = line.len();
                break;
            } else if line[index] == b'\'' || line[index] == b'"' {
                quote = line[index];
                index += 1;
            } else {
                output.push(line[index]);
                index += 1;
            }
        }
        if !output.is_empty() {
            words.push((output, start));
        }
    }
    words
}

/// blaze_util::Replace (util/strings.cc:142-151) of `old` by nothing, keeping
/// the physical line of every remaining byte.
fn remove_all(contents: &mut Vec<u8>, lines: &mut Vec<usize>, old: &[u8]) {
    let mut out = Vec::with_capacity(contents.len());
    let mut out_lines = Vec::with_capacity(lines.len());
    let mut index = 0;
    while index < contents.len() {
        if contents[index..].starts_with(old) {
            index += old.len();
        } else {
            out.push(contents[index]);
            out_lines.push(lines[index]);
            index += 1;
        }
    }
    *contents = out;
    *lines = out_lines;
}

/// The logical lines of an rc file (rc_file.cc:106-112): continuation
/// removal, `\n` split and StripWhitespace, with each byte's physical line.
fn logical_lines(raw: &[u8]) -> Vec<(Vec<u8>, Vec<usize>)> {
    let mut contents = raw.to_vec();
    let mut line = 1;
    let mut lines = Vec::with_capacity(raw.len());
    for &byte in raw {
        lines.push(line);
        if byte == b'\n' {
            line += 1;
        }
    }
    remove_all(&mut contents, &mut lines, b"\\\r\n");
    remove_all(&mut contents, &mut lines, b"\\\n");
    let mut result = Vec::new();
    let mut start = 0;
    for end in (0..=contents.len()).filter(|&i| i == contents.len() || contents[i] == b'\n') {
        let (mut first, mut last) = (start, end);
        while first < last && is_space(contents[first]) {
            first += 1;
        }
        while last > first && is_space(contents[last - 1]) {
            last -= 1;
        }
        result.push((contents[first..last].to_vec(), lines[first..last].to_vec()));
        start = end + 1;
    }
    result
}

fn utf8(bytes: Vec<u8>, file: &str, line: usize) -> Result<String, ParseError> {
    String::from_utf8(bytes).map_err(|_| {
        ParseError::Invalid(format!(
            "rc file {file}:{line} contains non-UTF-8 text; the native client supports UTF-8 rc files only"
        ))
    })
}

struct RcParser<'a> {
    workspace: &'a str,
    build_label: (i32, i32, i32, String),
    file: RcFile,
}

impl RcParser<'_> {
    /// RcFile::ParseFile (rc_file.cc:85-249).
    fn parse_file(&mut self, filename: &str, stack: &mut Vec<String>) -> Result<(), ParseError> {
        let contents = if Path::new(filename).is_dir() {
            // open(O_RDONLY) succeeds on a directory and ReadFrom fails
            // without setting the error message (util/file_posix.cc:267-279).
            Err(String::new())
        } else {
            std::fs::read(filename).map_err(|error| error_string(&error))
        };
        let contents = contents.map_err(|message| {
            ParseError::Unreadable(format!(
                "Unexpected error reading config file '{filename}': {message}"
            ))
        })?;
        let canonical_filename = canonical(filename);
        self.file
            .canonical_source_paths
            .push(canonical_filename.clone());
        for (line, line_numbers) in logical_lines(&contents) {
            if line.is_empty() {
                continue;
            }
            let words = tokenize(&line);
            if words.is_empty() {
                continue;
            }
            let number = |index: usize| line_numbers[index];
            let command = utf8(words[0].0.clone(), &canonical_filename, number(0))?;
            if !matches!(
                command.as_str(),
                "import" | "try-import" | "try-import-if-bazel-version"
            ) {
                for (word, start) in &words[1..] {
                    let option = utf8(word.clone(), &canonical_filename, number(*start))?;
                    self.file.options.push(RcOption {
                        command: command.clone(),
                        option,
                        source: canonical_filename.clone(),
                        line: number(*start),
                    });
                }
                continue;
            }
            let text = String::from_utf8_lossy(&line).into_owned();
            let expected = if command == "try-import-if-bazel-version" {
                3
            } else {
                2
            };
            if words.len() != expected {
                return Err(ParseError::Invalid(format!(
                    "Invalid import declaration in config file '{canonical_filename}': '{text}'"
                )));
            }
            let mut import = utf8(
                words[expected - 1].0.clone(),
                &canonical_filename,
                number(0),
            )?;
            if command == "try-import-if-bazel-version" {
                let condition = String::from_utf8_lossy(&words[1].0).into_owned();
                let Some((op, version)) = version_condition(&condition) else {
                    return Err(ParseError::Invalid(format!(
                        "Invalid version condition in config file '{canonical_filename}': '{text}'. Condition '{condition}'. A valid condition is one of the following 7 comparison operators ('<', '<=', '>', '>=', '==', '!=', '~') followed by a semantic version."
                    )));
                };
                match version_matches(&self.build_label, op, version) {
                    Err(error) => {
                        return Err(ParseError::Invalid(format!(
                            "Invalid import declaration in config file '{canonical_filename}': '{text}'. {error}"
                        )));
                    }
                    Ok(false) => continue,
                    Ok(true) => {}
                }
            }
            if let Some(rest) = import.strip_prefix("%workspace%/") {
                // WorkspaceLayout::ResolveWorkspaceRelativeRcFilePath
                // (workspace_layout.cc:74-85).
                let resolved = join_path(self.workspace, rest);
                if std::fs::metadata(&resolved).is_err() {
                    if command == "import" {
                        return Err(ParseError::Invalid(format!(
                            "Nonexistent path in import declaration in config file '{canonical_filename}': '{text}' (are you in your source checkout/WORKSPACE?)"
                        )));
                    }
                    continue;
                }
                import = resolved;
            }
            if stack.contains(&import) {
                let mut lines = String::new();
                for imported in stack.iter() {
                    lines.push_str(&format!("  {imported}\n"));
                }
                lines.push_str(&format!("  {import}\n"));
                return Err(ParseError::Invalid(format!(
                    "Import loop detected:\n{lines}"
                )));
            }
            stack.push(import.clone());
            match self.parse_file(&import, stack) {
                Ok(()) => {}
                Err(ParseError::Unreadable(_)) if command != "import" => {}
                Err(error) => return Err(error),
            }
            stack.pop();
        }
        Ok(())
    }
}

/// RcFile::Parse (rc_file.cc:62-74) of one top-level rc file.
pub fn parse_rc_file(path: &str, workspace: &str) -> Result<RcFile, String> {
    let build_label = sem_ver(crate::COMPATIBILITY_VERSION).expect("semantic version");
    let mut parser = RcParser {
        workspace,
        build_label,
        file: RcFile::default(),
    };
    parser
        .parse_file(path, &mut vec![path.to_owned()])
        .map_err(ParseError::text)?;
    Ok(parser.file)
}

/// OptionProcessor::GetRcFiles (option_processor.cc:374-490). Warnings are
/// appended to `warnings` as the client logs them.
pub fn read_rc_files(
    line: &CommandLine,
    env: &ClientEnv,
    warnings: &mut Vec<String>,
) -> Result<Vec<RcFile>, String> {
    let startup = &line.startup_args;
    let mut candidates = Vec::new();
    if search_nullary(startup, "system_rc", true) {
        candidates.push(make_absolute(&env.cwd, &resolve_envvars(&env.system_rc)?));
    }
    if !env.workspace.is_empty() && search_nullary(startup, "workspace_rc", true) {
        candidates.push(join_path(&env.workspace, ".bazelrc"));
    }
    if search_nullary(startup, "home_rc", true) && !env.home.is_empty() {
        candidates.push(join_path(&env.home, ".bazelrc"));
    }
    let explicit = split_bazelrc_env(&env.bazelrc_env)
        .into_iter()
        .chain(all_unary_values(startup, "--bazelrc", "/dev/null"));
    for rc in explicit {
        let absolute = absolute_path_from_flag(env, &rc);
        if !can_read_file(&absolute) {
            return Err(format!("Error: Unable to read .bazelrc file '{absolute}'."));
        }
        candidates.push(absolute);
    }
    let mut read = BTreeSet::new();
    let mut result = Vec::new();
    for path in dedupe(&candidates) {
        let parsed = parse_rc_file(&path, &env.workspace)?;
        duplicate_warnings(&read, &parsed.canonical_source_paths, warnings);
        read.extend(parsed.canonical_source_paths.iter().cloned());
        result.push(parsed);
    }
    let lost = lost_files(&old_rc_paths(line, env), &read);
    if !lost.is_empty() {
        warnings.push(format!(
            "The following rc files are no longer being read, please transfer their contents or import their path into one of the standard rc files:\n{}",
            lost.join("\n")
        ));
    }
    Ok(result)
}

/// blaze_util::Split(GetEnv("BAZELRC"), ',') (util/strings.cc:79-108).
fn split_bazelrc_env(value: &str) -> Vec<String> {
    value
        .split(',')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// internal::DedupeBlazercPaths (option_processor.cc:248-263).
fn dedupe(paths: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for path in paths {
        let canonical_path = canonical(path);
        if !canonical_path.is_empty() && seen.insert(canonical_path) {
            result.push(path.clone());
        }
    }
    result
}

/// internal::WarnAboutDuplicateRcFiles (option_processor.cc:307-340).
fn duplicate_warnings(read: &BTreeSet<String>, loaded: &[String], warnings: &mut Vec<String>) {
    let top = &loaded[0];
    for rc in loaded.iter().collect::<BTreeSet<_>>() {
        if read.contains(rc) {
            if rc == top {
                warnings.push(format!("Duplicate rc file: {rc} is read multiple times, it is a standard rc file location but must have been unnecessarily imported earlier."));
            } else {
                warnings.push(format!(
                    "Duplicate rc file: {rc} is read multiple times, most recently imported from {top}"
                ));
            }
        }
        if loaded.iter().filter(|other| *other == rc).count() > 1 {
            warnings.push(format!(
                "Duplicate rc file: {rc} is imported multiple times from {top}"
            ));
        }
    }
}

/// internal::GetOldRcPaths (option_processor.cc:203-243).
fn old_rc_paths(line: &CommandLine, env: &ClientEnv) -> BTreeSet<String> {
    let binary = if env.path_to_binary.starts_with('/') {
        env.path_to_binary.clone()
    } else {
        join_path(&env.cwd, &env.path_to_binary)
    };
    let base = env
        .path_to_binary
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    let binary_rc = format!("{binary}.{base}rc");
    let system = make_absolute(
        &env.cwd,
        &resolve_envvars(&env.system_rc).unwrap_or_default(),
    );
    let mut candidates = vec![
        join_path(&env.workspace, "tools/bazel.rc"),
        if can_read_file(&binary_rc) {
            binary_rc
        } else {
            String::new()
        },
        if can_read_file(&system) {
            system
        } else {
            String::new()
        },
    ];
    let values = all_unary_values(&line.startup_args, "--bazelrc", "/dev/null");
    if values.is_empty() {
        let workspace_rc = join_path(&env.workspace, ".bazelrc");
        if can_read_file(&workspace_rc) {
            candidates.push(workspace_rc);
        } else if !env.home.is_empty() {
            let home_rc = join_path(&env.home, ".bazelrc");
            if can_read_file(&home_rc) {
                candidates.push(home_rc);
            }
        }
    } else {
        for value in values {
            let absolute = absolute_path_from_flag(env, &value);
            if can_read_file(&absolute) {
                candidates.push(absolute);
            }
        }
    }
    dedupe(&candidates).into_iter().collect()
}

/// internal::GetLostFiles (option_processor.cc:342-354).
fn lost_files(old: &BTreeSet<String>, read: &BTreeSet<String>) -> Vec<String> {
    old.iter()
        .filter(|path| {
            let canonical_path = canonical(path);
            !canonical_path.is_empty() && !read.contains(&canonical_path)
        })
        .cloned()
        .collect()
}

/// The modelled startup option values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartupState {
    pub ignore_all_rc_files: bool,
}

/// StartupOptions::ProcessArgs (startup_options.cc:419-445) over the rc
/// `startup` and `startup:linux` options (option_processor.cc:603-656) and
/// the command line. Only the rc-file flags are modelled; every other valid
/// startup option is an explicit failure naming where it came from. On a
/// failure, `provenance` receives the client's
/// PrintStartupOptionsProvenanceMessage lines for the flags processed so far
/// (option_processor.cc:566-601, blaze.cc:1265-1277).
pub fn process_startup_options(
    rc_files: &[RcFile],
    line: &CommandLine,
    warnings: &mut Vec<String>,
    provenance: &mut Vec<String>,
) -> Result<StartupState, String> {
    let mut original = Vec::new();
    let result = process_startup_flags(rc_files, line, warnings, &mut original);
    if result.is_err() {
        // Consecutive flags of one source are grouped; command-line flags
        // (source "") are not printed.
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        for (source, value) in original {
            match groups.last_mut() {
                Some((last, values)) if *last == source => values.push(value),
                _ => groups.push((source, vec![value])),
            }
        }
        for (source, values) in groups {
            if !source.is_empty() {
                provenance.push(format!(
                    "Reading 'startup' options from {source}: {}",
                    values.join(" ")
                ));
            }
        }
    }
    result
}

fn process_startup_flags(
    rc_files: &[RcFile],
    line: &CommandLine,
    warnings: &mut Vec<String>,
    original: &mut Vec<(String, String)>,
) -> Result<StartupState, String> {
    let mut flags: Vec<(Option<(String, usize)>, String)> = Vec::new();
    for key in ["startup", "startup:linux"] {
        for rc in rc_files {
            for option in rc.options.iter().filter(|option| option.command == key) {
                flags.push((
                    Some((option.source.clone(), option.line)),
                    option.option.clone(),
                ));
            }
        }
    }
    flags.extend(line.startup_args.iter().map(|arg| (None, arg.clone())));
    let mut state = StartupState::default();
    let mut user_bazelrc = String::new();
    let mut explicit = BTreeSet::new();
    let mut index = 0;
    while index < flags.len() {
        let (source, arg) = &flags[index];
        let next = flags
            .get(index + 1)
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty());
        index += 1;
        let source_path = source
            .as_ref()
            .map_or_else(String::new, |(file, _)| file.clone());
        // ProcessArgs stores each flag, `--name=value` when space separated,
        // before propagating its error (startup_options.cc:425-441).
        let space_separated = !check_valid_nullary(arg).unwrap_or(false)
            && next.is_some()
            && UNARY_STARTUP.iter().any(|name| *arg == format!("--{name}"));
        original.push((
            source_path,
            match (space_separated, next) {
                (true, Some(next)) => format!("{arg}={next}"),
                _ => arg.clone(),
            },
        ));
        let unsupported = |arg: &str| match source {
            Some((file, line)) => format!(
                "unsupported startup option {arg:?} from rc file {file}:{line}; startup options are never silently ignored"
            ),
            None => format!(
                "unsupported startup option {arg:?}; the native client models only --[no]system_rc, --[no]workspace_rc, --[no]home_rc, --ignore_all_rc_files and --bazelrc"
            ),
        };
        if check_valid_nullary(arg)? {
            let enabled = !arg.starts_with("--no");
            let key = &arg[if enabled { 2 } else { 4 }..];
            if NO_RC_STARTUP.contains(&key) && source.is_some() {
                return Err(format!("Can't specify {arg} in the .bazelrc file."));
            }
            match key {
                "ignore_all_rc_files" => state.ignore_all_rc_files = enabled,
                "home_rc" | "system_rc" | "workspace_rc" => {
                    if enabled {
                        explicit.insert(key.to_owned());
                    } else {
                        explicit.remove(key);
                    }
                }
                _ => return Err(unsupported(arg)),
            }
            continue;
        }
        if let (Some((file, line)), Some(name)) = (source, native_only_option(arg)) {
            return Err(native_only_error(
                name,
                &format!(" from rc file {file}:{line}"),
            ));
        }
        let matched = UNARY_STARTUP.iter().find_map(|name| {
            let key = format!("--{name}");
            unary_option(arg, next, &key).map(|value| (*name, value))
        });
        let Some((name, value)) = matched else {
            return Err(format!(
                "Unknown startup option: '{arg}'.\n  For more info, run '{PRODUCT} help startup_options'."
            ));
        };
        let from_next = !arg.contains('=');
        if name == "bazelrc" {
            if source.is_some() {
                return Err("Can't specify --bazelrc in the RC file.".to_owned());
            }
            user_bazelrc = value.to_owned();
        } else {
            let form = if from_next {
                format!("{arg}={value}")
            } else {
                arg.clone()
            };
            return Err(unsupported(&form));
        }
        if from_next {
            index += 1;
        }
    }
    if state.ignore_all_rc_files {
        if !user_bazelrc.is_empty() {
            warnings
                .push("Value of --bazelrc is ignored, since --ignore_all_rc_files is on.".into());
        }
        for key in ["home_rc", "system_rc", "workspace_rc"] {
            if explicit.contains(key) {
                warnings.push(format!(
                    "Explicit value of --{key} is ignored, since --ignore_all_rc_files is on."
                ));
            }
        }
    }
    Ok(state)
}

/// One rc option as the client passes it to the server
/// (OptionProcessor::GetBlazercOptions, option_processor.cc:658-690).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlazercOption {
    /// Canonical rc path, or `client`.
    pub source: String,
    pub command: String,
    pub option: String,
    pub line: usize,
}

/// The client's terminal options and every non-`startup` rc option. Within a
/// top-level rc file the client groups options by command (a hash map,
/// rc_file.h:69-71); the per-command order, the only order the server
/// observes, is the order of appearance.
pub fn blazerc_options(env: &ClientEnv, rc_files: &[RcFile]) -> Vec<BlazercOption> {
    let client = |option: String| BlazercOption {
        source: "client".to_owned(),
        command: "common".to_owned(),
        option,
        line: 0,
    };
    let mut result = vec![
        client(format!("--isatty={}", u8::from(env.isatty))),
        client(format!("--terminal_columns={}", env.terminal_columns)),
    ];
    if env.emacs {
        result.push(client("--emacs".to_owned()));
    }
    for rc in rc_files {
        let mut commands: Vec<&str> = Vec::new();
        for option in &rc.options {
            if option.command != "startup" && !commands.contains(&option.command.as_str()) {
                commands.push(&option.command);
            }
        }
        for command in commands {
            for option in rc.options.iter().filter(|option| option.command == command) {
                result.push(BlazercOption {
                    source: option.source.clone(),
                    command: option.command.clone(),
                    option: option.option.clone(),
                    line: option.line,
                });
            }
        }
    }
    result
}

/// Where an option came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    CommandLine,
    Rc {
        file: String,
        line: usize,
        /// The rc command key (`common`, `build`, `build:NAME`).
        key: String,
    },
}

/// One argument and its origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arg {
    pub text: String,
    pub origin: Origin,
}

impl Arg {
    /// ` from rc file FILE:LINE ('KEY' options)` for rc options, else empty.
    pub fn origin_suffix(&self) -> String {
        match &self.origin {
            Origin::CommandLine => String::new(),
            Origin::Rc { file, line, key } => {
                if key.contains(':') {
                    format!(" from rc file {file}:{line} (config definition '{key}')")
                } else {
                    format!(" from rc file {file}:{line} ('{key}' options)")
                }
            }
        }
    }

    pub fn from_rc(&self) -> bool {
        matches!(self.origin, Origin::Rc { .. })
    }
}

/// An option and its separate value, if the option took the next argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    pub option: Arg,
    pub value: Option<Arg>,
}

/// What Bazel had reported when a parse step ran: notes and stored warnings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    pub notes: usize,
    /// The note of the chunk being parsed, whose "Ignored" suffix is added
    /// only after its parse.
    pub open_note: Option<usize>,
    pub warnings: usize,
}

/// One parse-order step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Unit(Unit),
    /// An OptionsParsingException raised by rc evaluation itself.
    Fail(String),
}

/// An rc note (BlazeOptionHandler.rcfileNotes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub text: String,
    pub suffix: Option<String>,
}

/// The evaluated rc options of one command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expanded {
    /// Native option units in Bazel's parse order, ending at the first failure.
    pub steps: Vec<(Step, Position)>,
    /// Native option units in priority order (valid when no step fails).
    pub priority: Vec<Unit>,
    /// Residue (targets, subcommands) in parse-call order.
    pub residue: Vec<Arg>,
    pub notes: Vec<Note>,
    /// Stored warning events, in order.
    pub warnings: Vec<String>,
    /// Final `--announce_rc`.
    pub announce: bool,
}

impl Expanded {
    /// The notes Bazel prints when `error` ends the invocation early
    /// (BlazeCommandDispatcher.replayEarlyExitEvents, :923-948): those
    /// visible at `position` that contain the invalid argument of an
    /// `ARG :: MESSAGE` error.
    pub fn early_exit_notes(&self, error: &str, position: Position) -> Vec<String> {
        let Some((bad, _)) = error.split_once(" :: ") else {
            return Vec::new();
        };
        self.notes[..position.notes.min(self.notes.len())]
            .iter()
            .enumerate()
            .map(
                |(index, note)| match (&note.suffix, Some(index) == position.open_note) {
                    (Some(suffix), false) => format!("{}{suffix}", note.text),
                    _ => note.text.clone(),
                },
            )
            .filter(|note| note.contains(bad))
            .collect()
    }

    /// The notes `--announce_rc` prints.
    pub fn all_notes(&self) -> Vec<String> {
        self.notes
            .iter()
            .map(|note| format!("{}{}", note.text, note.suffix.as_deref().unwrap_or("")))
            .collect()
    }
}

/// Environment-assignment options whose values may carry secrets. rc notes
/// show `--NAME=KEY=<redacted>` instead of Bazel's verbatim echo, keeping the
/// native rule that such values are never printed (docs/BAZELRC.md).
const REDACTED_OPTIONS: &[&str] = &["repo_env", "action_env", "host_action_env", "test_env"];

fn redacted_value(value: &str) -> String {
    match value.split_once('=') {
        Some((key, _)) => format!("{key}=<redacted>"),
        None => value.to_owned(),
    }
}

/// `--NAME=KEY=VALUE` as `--NAME=KEY=<redacted>` for [`REDACTED_OPTIONS`].
fn redacted_arg(arg: &str) -> Option<String> {
    REDACTED_OPTIONS.iter().find_map(|name| {
        let value = arg
            .strip_prefix("--")?
            .strip_prefix(name)?
            .strip_prefix('=')?;
        Some(format!("--{name}={}", redacted_value(value)))
    })
}

fn is_split_redacted(arg: &str) -> bool {
    REDACTED_OPTIONS
        .iter()
        .any(|name| arg.strip_prefix("--") == Some(name))
}

/// Note args with [`REDACTED_OPTIONS`] values redacted, also in the
/// `--NAME KEY=VALUE` form.
fn redacted_args<'a>(args: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut split = false;
    for arg in args {
        out.push(if split {
            redacted_value(arg)
        } else {
            redacted_arg(arg).unwrap_or_else(|| arg.to_owned())
        });
        split = is_split_redacted(arg);
    }
    out
}

/// An ignored-option form (`ARG` or `ARG VALUE`) with its value redacted.
fn redacted_form(form: &str) -> String {
    match form.split_once(' ') {
        Some((arg, value)) if is_split_redacted(arg) => format!("{arg} {}", redacted_value(value)),
        _ => redacted_arg(form).unwrap_or_else(|| form.to_owned()),
    }
}

/// A native-only option (not a Bazel 9.3 option): genuine Bazel reports it as
/// unrecognized in an rc file, so the native client rejects it there and
/// accepts it only on the command line.
pub fn native_only_option(arg: &str) -> Option<&'static str> {
    ["--experimental-builtin-modules", "--cache-dir"]
        .into_iter()
        .find(|name| {
            arg == *name
                || arg
                    .strip_prefix(name)
                    .is_some_and(|rest| rest.starts_with('='))
        })
}

/// The error for a native-only option from an rc file.
pub fn native_only_error(name: &str, origin: &str) -> String {
    format!(
        "native-only option {name}{origin} is not a Bazel option and is rejected in rc files (genuine Bazel 9.3 reports it as unrecognized); pass it on the command line"
    )
}

/// Command bit in [`COMMAND_OPTIONS`] masks.
fn command_bit(command: &str) -> u8 {
    match command {
        "build" => 1,
        "query" => 2,
        "mod" => 4,
        _ => 8,
    }
}

/// The args of each rc command key of the current chunk's rc file.
type ChunkKeys = Vec<(String, Vec<(String, usize)>)>;

type Row = (&'static str, &'static str, char, bool, bool, u8);

fn by_name(name: &str) -> Option<&'static Row> {
    COMMAND_OPTIONS
        .iter()
        .find(|row| row.0 == name || (!row.1.is_empty() && row.1 == name))
}

fn by_abbrev(abbrev: char) -> Option<&'static Row> {
    COMMAND_OPTIONS.iter().find(|row| row.2 == abbrev)
}

/// An rc chunk (RcChunkOfArgs.java): consecutive args of one rc file.
#[derive(Clone, Debug)]
struct Chunk {
    rc: String,
    args: Vec<(String, usize)>,
}

enum Item {
    Native(Unit),
    Config(String, Vec<Item>),
    Announce(bool),
    Platform(bool, Vec<Item>),
    Fail(String),
}

struct Scan {
    items: Vec<Item>,
    ignored: Vec<String>,
    residue: Vec<Arg>,
}

/// BooleanConverter (common/options/Converters.java:46-60).
fn boolean(form: &str, value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "t" | "y" => Ok(true),
        "false" | "0" | "no" | "f" | "n" => Ok(false),
        lower => Err(format!(
            "While parsing option {form}: '{lower}' is not a boolean"
        )),
    }
}

/// OptionsParserImpl.parse/identifyOptionAndPossibleArgument
/// (common/options/OptionsParserImpl.java:491-569, 749-857) over one parse
/// call's args, with per-arg fallback (BlazeOptionHandler.java:219-245):
/// splits options from residue using the command's option data, ignores
/// `common` options of other commands, and evaluates the rc options.
fn scan(args: &[(Arg, bool)], bit: u8) -> Scan {
    let mut scan = Scan {
        items: Vec::new(),
        ignored: Vec::new(),
        residue: Vec::new(),
    };
    let mut index = 0;
    while index < args.len() {
        let (arg, fallback) = &args[index];
        index += 1;
        let text = arg.text.as_str();
        let native = |value: Option<Arg>| {
            Item::Native(Unit {
                option: arg.clone(),
                value,
            })
        };
        if !text.starts_with('-') {
            scan.residue.push(arg.clone());
            continue;
        }
        if text.starts_with("-//") || text.starts_with("-@") {
            scan.items.push(native(None));
            continue;
        }
        if text == "--" {
            scan.residue
                .extend(args[index..].iter().map(|(arg, _)| arg.clone()));
            break;
        }
        if crate::configuration::STARLARK_SKIPPED_PREFIXES
            .iter()
            .any(|prefix| text.starts_with(prefix))
        {
            scan.items.push(native(None));
            continue;
        }
        let lookup = |row: Option<&'static Row>| {
            row.and_then(|row| {
                if row.5 & bit != 0 {
                    Some((row, false))
                } else if *fallback && row.5 != 0 {
                    Some((row, true))
                } else {
                    None
                }
            })
        };
        let chars = text.chars().collect::<Vec<_>>();
        let (found, inline, boolean_value) = if chars.len() == 2 {
            (lookup(by_abbrev(chars[1])), None, true)
        } else if chars.len() == 3 && chars[2] == '-' {
            (lookup(by_abbrev(chars[1])), None, false)
        } else if let Some(rest) = text.strip_prefix("--") {
            let (name, inline) = match rest.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (rest, None),
            };
            if name.trim().is_empty() {
                scan.items.push(native(None));
                continue;
            }
            match lookup(by_name(name)) {
                Some(found) => (Some(found), inline, true),
                None => match name
                    .strip_prefix("no")
                    .and_then(|name| lookup(by_name(name)))
                {
                    Some(found) => {
                        if !found.0.3 {
                            scan.items.push(Item::Fail(format!(
                                "{text} :: Illegal use of 'no' prefix on non-boolean option: {text}"
                            )));
                            return scan;
                        }
                        if inline.is_some() {
                            scan.items.push(Item::Fail(format!(
                                "{text} :: Unexpected value after boolean option: {text}"
                            )));
                            return scan;
                        }
                        (Some(found), Some("0"), false)
                    }
                    None => (None, inline, true),
                },
            }
        } else {
            (None, None, true)
        };
        let Some((row, from_fallback)) = found else {
            scan.items.push(native(None));
            continue;
        };
        let mut value = None;
        if inline.is_none() && !row.3 && row.4 {
            match args.get(index) {
                Some((next, _)) => {
                    value = Some(next.clone());
                    index += 1;
                }
                None => {
                    scan.items
                        .push(Item::Fail(format!("Expected value after {text}")));
                    return scan;
                }
            }
        }
        if from_fallback {
            scan.ignored.push(match &value {
                Some(value) => format!("{text} {}", value.text),
                None => text.to_owned(),
            });
            continue;
        }
        let form = match &value {
            Some(value) => format!("{text} {}", value.text),
            None => text.to_owned(),
        };
        match row.0 {
            "config" => {
                let value = inline
                    .map(str::to_owned)
                    .or_else(|| value.map(|value| value.text))
                    .unwrap_or_default();
                scan.items.push(Item::Config(value, Vec::new()));
            }
            "announce_rc" | "enable_platform_specific_config" => {
                let parsed = match inline {
                    Some("0") if !boolean_value => Ok(false),
                    Some(inline) => boolean(&form, inline),
                    None => Ok(boolean_value),
                };
                match parsed {
                    Err(error) => {
                        scan.items.push(Item::Fail(error));
                        return scan;
                    }
                    Ok(value) if row.0 == "announce_rc" => scan.items.push(Item::Announce(value)),
                    Ok(value) => scan.items.push(Item::Platform(value, Vec::new())),
                }
            }
            // The client's terminal options are not user options.
            _ => scan.items.push(native(value)),
        }
    }
    scan
}

/// ConfigExpander.getExpansion (ConfigExpander.java:225-303).
fn get_expansion(
    chunks: &HashMap<String, Vec<Chunk>>,
    groups: &[&str],
    ancestors: &[String],
    config: &str,
    longest: &mut Vec<String>,
    notes: &mut Vec<Note>,
) -> Result<Vec<(Arg, bool)>, String> {
    let mut expansion = Vec::new();
    let mut found = false;
    for group in groups {
        let key = format!("{group}:{config}");
        for chunk in chunks.get(&key).into_iter().flatten() {
            found = true;
            notes.push(Note {
                text: format!(
                    "Found applicable config definition {key} in file {}: {}",
                    chunk.rc,
                    redacted_args(chunk.args.iter().map(|(arg, _)| arg.as_str())).join(" ")
                ),
                suffix: None,
            });
            for (text, line) in &chunk.args {
                expansion.push((
                    Arg {
                        text: text.clone(),
                        origin: Origin::Rc {
                            file: chunk.rc.clone(),
                            line: *line,
                            key: key.clone(),
                        },
                    },
                    *group == "common",
                ));
                if text.starts_with("--config") {
                    let Some(equals) = text.find('=') else {
                        return Err(format!(
                            "In file {}, the definition of config {config} expands to another config that either has no value or is not in the form --config=value. For recursive config definitions, please do not provide the value in a separate token, such as in the form '--config value'.",
                            chunk.rc
                        ));
                    };
                    let nested = &text[equals + 1..];
                    if ancestors.iter().any(|ancestor| ancestor == nested) {
                        return Err(format!(
                            "Config expansion has a cycle: config value {nested} expands to itself, see inheritance chain [{}]",
                            ancestors.join(", ")
                        ));
                    }
                    let mut extended = ancestors.to_vec();
                    extended.push(nested.to_owned());
                    if extended.len() > longest.len() {
                        *longest = extended.clone();
                    }
                    expansion.extend(get_expansion(
                        chunks, groups, &extended, nested, longest, notes,
                    )?);
                }
            }
        }
    }
    if !found {
        return Err(format!(
            "Config value '{config}' is not defined in any .rc file"
        ));
    }
    Ok(expansion)
}

struct Expander<'a> {
    command: &'a str,
    bit: u8,
    out: Expanded,
    failed: bool,
}

impl Expander<'_> {
    fn position(&self, open_note: Option<usize>) -> Position {
        Position {
            notes: self.out.notes.len(),
            open_note,
            warnings: self.out.warnings.len(),
        }
    }

    fn fail(&mut self, error: String, open_note: Option<usize>) {
        let position = self.position(open_note);
        self.out.steps.push((Step::Fail(error), position));
        self.failed = true;
    }

    /// Records the parse steps of one parse call; stops at a failure.
    fn record(&mut self, scan: &Scan, open_note: Option<usize>) {
        fn walk(this: &mut Expander<'_>, items: &[Item], open_note: Option<usize>) {
            for item in items {
                if this.failed {
                    return;
                }
                match item {
                    Item::Native(unit) => {
                        let position = this.position(open_note);
                        this.out.steps.push((Step::Unit(unit.clone()), position));
                    }
                    Item::Fail(error) => this.fail(error.clone(), open_note),
                    Item::Config(_, children) | Item::Platform(_, children) => {
                        walk(this, children, open_note)
                    }
                    Item::Announce(_) => {}
                }
            }
        }
        walk(self, &scan.items, open_note);
        self.out.residue.extend(scan.residue.iter().cloned());
    }

    fn ignored_note(&mut self, ignored: &[String]) {
        if !ignored.is_empty() {
            self.out.notes.push(Note {
                text: format!(
                    "Ignored as unsupported by '{}': {}",
                    self.command,
                    ignored
                        .iter()
                        .map(|form| redacted_form(form))
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
                suffix: None,
            });
        }
    }

    /// Expands one config value at a parse step; `None` after a failure.
    fn expand_config(
        &mut self,
        chunks: &HashMap<String, Vec<Chunk>>,
        groups: &[&str],
        config: &str,
    ) -> Option<Vec<Item>> {
        let mut longest = Vec::new();
        let expansion = get_expansion(
            chunks,
            groups,
            &[config.to_owned()],
            config,
            &mut longest,
            &mut self.out.notes,
        );
        let expansion = match expansion {
            Ok(expansion) => expansion,
            Err(error) => {
                self.fail(error, None);
                return None;
            }
        };
        if longest.len() >= 10 {
            self.out.warnings.push(format!(
                "There is a recursive chain of configs {} configs long: [{}]. This seems excessive, and might be hiding errors.",
                longest.len(),
                longest.join(", ")
            ));
        }
        let scanned = scan(&expansion, self.bit);
        self.record(&scanned, None);
        if self.failed {
            return None;
        }
        self.ignored_note(&scanned.ignored);
        Some(scanned.items)
    }
}

/// BlazeOptionHandler.parseArgsAndConfigs for `command` (BlazeOptionHandler.java:258-323).
/// `platform` is ConfigExpander.getPlatformName (`linux`).
pub fn expand(
    command: &str,
    blazerc: &[BlazercOption],
    command_line: &[String],
    platform: &str,
) -> Expanded {
    let mut this = Expander {
        command,
        bit: command_bit(command),
        out: Expanded::default(),
        failed: false,
    };
    // structureRcOptionsAndConfigs (:663-742).
    let mut chunks: HashMap<String, Vec<Chunk>> = HashMap::new();
    let mut last: Option<(String, ChunkKeys)> = None;
    let flush = |last: &mut Option<(String, ChunkKeys)>,
                 chunks: &mut HashMap<String, Vec<Chunk>>| {
        if let Some((rc, keys)) = last.take() {
            for (key, args) in keys {
                chunks.entry(key).or_default().push(Chunk {
                    rc: rc.clone(),
                    args,
                });
            }
        }
    };
    for option in blazerc {
        if (option.option.starts_with("--flag_alias=") || option.option == "--flag_alias")
            && !["build", "common", "always"].contains(&option.command.as_str())
        {
            this.fail(
                format!(
                    "{}: \"{} {}\" disallowed. --flag_alias only supports these commands: build, common, always",
                    option.source, option.command, option.option
                ),
                None,
            );
            return this.out;
        }
        let base = match option.command.find(':') {
            Some(index) if index > 0 => &option.command[..index],
            _ => option.command.as_str(),
        };
        if !COMMANDS.contains(&base) && !["always", "common", "startup"].contains(&base) {
            this.out.warnings.push(format!(
                "while reading option defaults file '{}':\n  invalid command name '{}'.",
                option.source, option.command
            ));
            continue;
        }
        if last.as_ref().is_none_or(|(rc, _)| *rc != option.source) {
            flush(&mut last, &mut chunks);
            last = Some((option.source.clone(), Vec::new()));
        }
        let keys = &mut last.as_mut().expect("chunk").1;
        let arg = (option.option.clone(), option.line);
        match keys.iter_mut().find(|(key, _)| *key == option.command) {
            Some((_, args)) => args.push(arg),
            None => keys.push((option.command.clone(), vec![arg])),
        }
    }
    flush(&mut last, &mut chunks);

    // getCommandNamesToParse (:607-621): none of build/query/mod inherits.
    let groups = ["always", "common", command];
    let mut items: Vec<Item> = Vec::new();
    // parseRcOptions (:199-251).
    for group in groups {
        for chunk in chunks.get(group).cloned().unwrap_or_default() {
            let inherited = if group == command { "" } else { "Inherited " };
            let source = if chunk.rc == "client" {
                "Options provided by the client".to_owned()
            } else {
                format!("Reading rc options for '{command}' from {}", chunk.rc)
            };
            let note = this.out.notes.len();
            this.out.notes.push(Note {
                text: format!(
                    "{source}:\n  {inherited}'{group}' options: {}",
                    redacted_args(chunk.args.iter().map(|(arg, _)| arg.as_str())).join(" ")
                ),
                suffix: None,
            });
            if chunk.rc == "client" {
                // --isatty/--terminal_columns/--emacs: launcher options.
                continue;
            }
            let args = chunk
                .args
                .iter()
                .map(|(text, line)| {
                    (
                        Arg {
                            text: text.clone(),
                            origin: Origin::Rc {
                                file: chunk.rc.clone(),
                                line: *line,
                                key: group.to_owned(),
                            },
                        },
                        group == "common",
                    )
                })
                .collect::<Vec<_>>();
            let scanned = scan(&args, this.bit);
            this.record(&scanned, Some(note));
            if this.failed {
                return this.out;
            }
            if !scanned.ignored.is_empty() {
                this.out.notes[note].suffix = Some(format!(
                    "\n  Ignored as unsupported by '{command}': {}",
                    scanned
                        .ignored
                        .iter()
                        .map(|form| redacted_form(form))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
            }
            items.extend(scanned.items);
        }
    }
    // The command line (:303-308).
    let args = command_line
        .iter()
        .map(|text| {
            (
                Arg {
                    text: text.clone(),
                    origin: Origin::CommandLine,
                },
                false,
            )
        })
        .collect::<Vec<_>>();
    let scanned = scan(&args, this.bit);
    this.record(&scanned, None);
    if this.failed {
        return this.out;
    }
    items.extend(scanned.items);

    // expandConfigOptions (ConfigExpander.java:100-132): the top-level
    // --config instances in priority order.
    for item in items.iter_mut() {
        if let Item::Config(config, children) = item {
            let config = config.clone();
            match this.expand_config(&chunks, &groups, &config) {
                Some(expansion) => *children = expansion,
                None => return this.out,
            }
        }
    }
    // --enable_platform_specific_config (:134-157).
    fn platform_value(items: &[Item], value: &mut bool) {
        for item in items {
            match item {
                Item::Platform(enabled, children) => {
                    *value = *enabled;
                    platform_value(children, value);
                }
                Item::Config(_, children) => platform_value(children, value),
                _ => {}
            }
        }
    }
    let mut enabled = false;
    platform_value(&items, &mut enabled);
    if enabled
        && groups
            .iter()
            .any(|group| chunks.contains_key(&format!("{group}:{platform}")))
    {
        let Some(expansion) = this.expand_config(&chunks, &groups, platform) else {
            return this.out;
        };
        fn last_platform(items: &mut [Item]) -> Option<&mut Vec<Item>> {
            let mut found = None;
            for item in items.iter_mut() {
                match item {
                    Item::Platform(_, children) => found = Some(children),
                    Item::Config(_, children) => {
                        if let Some(nested) = last_platform(children) {
                            found = Some(nested);
                        }
                    }
                    _ => {}
                }
            }
            found
        }
        if let Some(children) = last_platform(&mut items) {
            *children = expansion;
        }
    }
    // Flatten in priority order.
    fn flatten(items: Vec<Item>, out: &mut Expanded, configs: &mut Vec<String>) {
        for item in items {
            match item {
                Item::Native(unit) => out.priority.push(unit),
                Item::Config(config, children) => {
                    configs.push(config);
                    flatten(children, out, configs);
                }
                Item::Announce(value) => out.announce = value,
                Item::Platform(_, children) => flatten(children, out, configs),
                Item::Fail(_) => unreachable!("failures end the expansion"),
            }
        }
    }
    let mut configs = Vec::new();
    flatten(items, &mut this.out, &mut configs);
    let mut seen = BTreeSet::new();
    let mut duplicates: Vec<String> = Vec::new();
    for config in configs {
        if !seen.insert(config.clone()) && !duplicates.contains(&config) {
            duplicates.push(config);
        }
    }
    if !duplicates.is_empty() {
        this.out.warnings.push(format!(
            "The following configs were expanded more than once: [{}]. For repeatable flags, repeats are counted twice and may lead to unexpected behavior.",
            duplicates.join(", ")
        ));
    }
    this.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        tokenize(line.as_bytes())
            .into_iter()
            .map(|(word, _)| String::from_utf8(word).unwrap())
            .collect()
    }

    #[test]
    fn tokenize_quotes_escapes_and_comments() {
        assert_eq!(
            words("build --a 'b c' \"d#e\" f#g h"),
            ["build", "--a", "b c", "d#e", "f"]
        );
        assert_eq!(words("build --x=a\\ b # c"), ["build", "--x=a b"]);
        assert_eq!(words("# only"), Vec::<String>::new());
        assert_eq!(words("build --a='' x"), ["build", "--a=", "x"]);
        assert_eq!(words("build '' x"), ["build", "x"]);
        assert_eq!(words("build \"unterminated x"), ["build", "unterminated x"]);
    }

    #[test]
    fn logical_lines_join_continuations_and_keep_physical_lines() {
        let lines =
            logical_lines(b"build \\\n  --a\n\n # c \\\nbuild --hidden\r\n\tquery --b \x0b\n");
        let text = lines
            .iter()
            .map(|(line, _)| String::from_utf8(line.clone()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            text,
            ["build   --a", "", "# c build --hidden", "query --b", ""]
        );
        assert_eq!(lines[0].1[8], 2);
        assert_eq!(lines[3].1[0], 6);
    }

    #[test]
    fn sem_ver_and_conditions() {
        let build = sem_ver("9.3.0").unwrap();
        assert!(version_matches(&build, ">=", "9.0.0").unwrap());
        assert!(!version_matches(&build, "<", "9.3.0").unwrap());
        assert!(version_matches(&build, ">", "9.3.0-rc1").unwrap());
        assert!(version_matches(&build, "~", "9").unwrap());
        assert!(version_matches(&build, "~", "9.3").unwrap());
        assert!(!version_matches(&build, "~", "9.2").unwrap());
        assert!(version_matches(&build, "!=", "8.0.0").unwrap());
        assert!(version_matches(&build, "==", "9.3.0+meta").unwrap());
        assert!(version_matches(&build, "<", "9.x").is_err());
        assert_eq!(version_condition(">=9.0.0"), Some((">=", "9.0.0")));
        assert_eq!(version_condition("=9"), None);
        assert_eq!(version_condition("<"), None);
        assert!(sem_ver("09.1.0").is_none());
        assert!(sem_ver("1.2").is_none());
    }

    #[test]
    fn split_command_line_follows_the_client() {
        let split =
            |args: &[&str]| split_command_line(args.iter().map(|a| a.to_string()).collect());
        let line = split(&[
            "--nohome_rc",
            "--bazelrc",
            "x.rc",
            "--cache-dir=c",
            "build",
            "--x",
        ])
        .unwrap();
        assert_eq!(line.startup_args, ["--nohome_rc", "--bazelrc=x.rc"]);
        assert_eq!(line.cache_dir.as_deref(), Some("c"));
        assert_eq!(line.command.as_deref(), Some("build"));
        assert_eq!(line.command_args, ["--x"]);
        assert_eq!(
            split(&["--home_rc=1", "build"]).unwrap_err(),
            "In argument '--home_rc=1': option '--home_rc' does not take a value."
        );
        assert_eq!(
            split(&["--bogus", "build"]).unwrap_err(),
            "Unknown startup option: '--bogus'.\n  For more info, run 'bazel help startup_options'."
        );
        assert!(
            split(&["--bazelrc"])
                .unwrap_err()
                .starts_with("Startup option '--bazelrc' expects a value.")
        );
        assert_eq!(
            split(&["--help"]).unwrap().command.as_deref(),
            Some("--help")
        );
        assert_eq!(
            split(&["--version"]).unwrap().command.as_deref(),
            Some("--version")
        );
    }

    #[test]
    fn unary_values_stop_after_dev_null() {
        let args = ["--bazelrc=a", "--bazelrc=/dev/null", "--bazelrc=b"].map(String::from);
        assert_eq!(
            all_unary_values(&args, "--bazelrc", "/dev/null"),
            ["a", "/dev/null"]
        );
        assert!(!search_nullary(
            &["--home_rc".into(), "--nohome_rc".into()],
            "home_rc",
            true
        ));
    }

    #[test]
    fn startup_failures_report_rc_provenance() {
        let option = |text: &str, line: usize| RcOption {
            command: "startup".into(),
            option: text.into(),
            source: "/w/.bazelrc".into(),
            line,
        };
        let rc = RcFile {
            canonical_source_paths: vec!["/w/.bazelrc".into()],
            options: vec![
                option("--output_base", 1),
                option("/x", 1),
                option("--nohome_rc", 2),
            ],
        };
        let (mut warnings, mut provenance) = (Vec::new(), Vec::new());
        let error = process_startup_options(
            &[rc],
            &CommandLine::default(),
            &mut warnings,
            &mut provenance,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "unsupported startup option \"--output_base=/x\" from rc file /w/.bazelrc:1; startup options are never silently ignored"
        );
        assert_eq!(
            provenance,
            ["Reading 'startup' options from /w/.bazelrc: --output_base=/x"]
        );
        let (mut warnings, mut provenance) = (Vec::new(), Vec::new());
        let line = CommandLine {
            startup_args: vec!["--home_rc".into()],
            ..CommandLine::default()
        };
        assert!(process_startup_options(&[], &line, &mut warnings, &mut provenance).is_ok());
        assert!(provenance.is_empty());
    }

    #[test]
    fn environment_values_are_redacted_in_notes() {
        assert_eq!(
            redacted_args([
                "--repo_env=K=secret",
                "--action_env",
                "A=b",
                "--repo_env=INHERITED",
                "--define=a=b"
            ]),
            [
                "--repo_env=K=<redacted>",
                "--action_env",
                "A=<redacted>",
                "--repo_env=INHERITED",
                "--define=a=b"
            ]
        );
        assert_eq!(redacted_form("--test_env T=v"), "--test_env T=<redacted>");
        assert_eq!(
            redacted_form("--host_action_env=H=v"),
            "--host_action_env=H=<redacted>"
        );
        let out = expand(
            "query",
            &[
                rc("common", "--test_env=TOKEN=secret", 1),
                rc("build:x", "--repo_env=K=v", 2),
            ],
            &[],
            "linux",
        );
        let notes = out.all_notes().join("\n");
        assert!(
            notes.contains("Inherited 'common' options: --test_env=TOKEN=<redacted>"),
            "{notes}"
        );
        assert!(
            notes.contains("Ignored as unsupported by 'query': --test_env=TOKEN=<redacted>"),
            "{notes}"
        );
        assert!(!notes.contains("secret"), "{notes}");
        let out = expand(
            "build",
            &[rc("build:x", "--repo_env", 2), rc("build:x", "K=v", 2)],
            &["--config=x".into()],
            "linux",
        );
        assert!(
            out.all_notes()
                .iter()
                .any(|note| note.ends_with("build:x in file /w/.bazelrc: --repo_env K=<redacted>"))
        );
    }

    #[test]
    fn native_only_options() {
        assert_eq!(
            native_only_option("--experimental-builtin-modules"),
            Some("--experimental-builtin-modules")
        );
        assert_eq!(
            native_only_option("--experimental-builtin-modules=false"),
            Some("--experimental-builtin-modules")
        );
        assert_eq!(native_only_option("--cache-dir=x"), Some("--cache-dir"));
        assert_eq!(native_only_option("--cache-dirs=x"), None);
        let rc = RcFile {
            canonical_source_paths: vec!["/w/.bazelrc".into()],
            options: vec![RcOption {
                command: "startup".into(),
                option: "--cache-dir=c".into(),
                source: "/w/.bazelrc".into(),
                line: 3,
            }],
        };
        let error = process_startup_options(
            &[rc],
            &CommandLine::default(),
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            error.starts_with(
                "native-only option --cache-dir from rc file /w/.bazelrc:3 is not a Bazel option"
            ),
            "{error}"
        );
    }

    fn rc(command: &str, option: &str, line: usize) -> BlazercOption {
        BlazercOption {
            source: "/w/.bazelrc".into(),
            command: command.into(),
            option: option.into(),
            line,
        }
    }

    fn texts(units: &[Unit]) -> Vec<String> {
        units
            .iter()
            .flat_map(|unit| std::iter::once(&unit.option).chain(unit.value.as_ref()))
            .map(|arg| arg.text.clone())
            .collect()
    }

    #[test]
    fn expansion_orders_groups_and_configs_in_place() {
        let options = [
            rc("build", "--define=a=1", 1),
            rc("common", "--test_output=errors", 2),
            rc("common", "--define=c=1", 3),
            rc("build:x", "--define=x=1", 4),
            rc("build:x", "--config=y", 4),
            rc("common:y", "--define=y=1", 5),
            rc("common:y", "--test_output", 5),
            rc("common:y", "all", 5),
            rc("build", "//rc:target", 6),
            rc("bulid", "--a", 7),
            rc("common", "--announce_rc", 8),
        ];
        let out = expand(
            "build",
            &options,
            &["--config=x".into(), "//:t".into(), "--define=z=1".into()],
            "linux",
        );
        assert!(
            out.steps
                .iter()
                .all(|(step, _)| matches!(step, Step::Unit(_)))
        );
        assert_eq!(
            texts(&out.priority),
            [
                "--test_output=errors",
                "--define=c=1",
                "--define=a=1",
                "--define=x=1",
                "--define=y=1",
                "--test_output",
                "all",
                "--define=z=1"
            ]
        );
        let residue = out
            .residue
            .iter()
            .map(|arg| arg.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(residue, ["//rc:target", "//:t"]);
        assert!(out.announce);
        assert_eq!(
            out.warnings,
            ["while reading option defaults file '/w/.bazelrc':\n  invalid command name 'bulid'."]
        );
        let query = expand("query", &options, &[], "linux");
        assert_eq!(texts(&query.priority), ["--define=c=1"]);
        assert_eq!(
            query.all_notes()[0],
            "Reading rc options for 'query' from /w/.bazelrc:\n  Inherited 'common' options: --test_output=errors --define=c=1 --announce_rc\n  Ignored as unsupported by 'query': --test_output=errors"
        );
    }

    #[test]
    fn config_errors_are_bazel_messages() {
        let fail = |options: &[BlazercOption], args: &[&str]| {
            let out = expand(
                "build",
                options,
                &args.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                "linux",
            );
            match out.steps.last() {
                Some((Step::Fail(error), _)) => error.clone(),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            fail(&[], &["--config=nope"]),
            "Config value 'nope' is not defined in any .rc file"
        );
        assert_eq!(
            fail(
                &[
                    rc("build:a", "--config=b", 1),
                    rc("build:b", "--config=a", 2)
                ],
                &["--config=a"]
            ),
            "Config expansion has a cycle: config value a expands to itself, see inheritance chain [a, b]"
        );
        assert!(
            fail(
                &[rc("build:a", "--config", 1), rc("build:a", "b", 1)],
                &["--config", "a"]
            )
            .starts_with("In file /w/.bazelrc, the definition of config a expands")
        );
        assert_eq!(
            fail(&[], &["--noconfig"]),
            "--noconfig :: Illegal use of 'no' prefix on non-boolean option: --noconfig"
        );
        assert_eq!(
            fail(&[], &["--announce_rc=maybe"]),
            "While parsing option --announce_rc=maybe: 'maybe' is not a boolean"
        );
        assert_eq!(
            fail(&[rc("common", "--test_output", 1)], &[]),
            "Expected value after --test_output"
        );
        assert!(
            fail(&[rc("test", "--flag_alias=a=//:b", 1)], &[])
                .ends_with("--flag_alias only supports these commands: build, common, always")
        );
    }

    #[test]
    fn duplicate_and_platform_configs() {
        let options = [
            rc("build:a", "--define=a=1", 1),
            rc("build:linux", "--define=l=1", 2),
            rc("common", "--enable_platform_specific_config", 3),
        ];
        let out = expand(
            "build",
            &options,
            &["--config=a".into(), "--config=a".into()],
            "linux",
        );
        assert_eq!(
            texts(&out.priority),
            ["--define=l=1", "--define=a=1", "--define=a=1"]
        );
        assert_eq!(
            out.warnings,
            [
                "The following configs were expanded more than once: [a]. For repeatable flags, repeats are counted twice and may lead to unexpected behavior."
            ]
        );
    }

    #[test]
    fn early_exit_notes_contain_the_bad_option() {
        let options = [rc("build", "--bogus", 1), rc("common", "--define=a=1", 2)];
        let out = expand("build", &options, &[], "linux");
        let (step, position) = out
            .steps
            .iter()
            .find(|(step, _)| matches!(step, Step::Unit(unit) if unit.option.text == "--bogus"))
            .unwrap();
        assert!(matches!(step, Step::Unit(_)));
        assert_eq!(
            out.early_exit_notes("--bogus :: Unrecognized option: --bogus", *position),
            ["Reading rc options for 'build' from /w/.bazelrc:\n  'build' options: --bogus"]
        );
        assert!(out.early_exit_notes("plain error", *position).is_empty());
    }
}
