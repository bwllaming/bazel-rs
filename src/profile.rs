//! `--profile=<file>`: a Chrome trace JSON profile (docs/PROFILE.md) in
//! the layout of Bazel's JsonTraceFileWriter (profiler/JsonTraceFileWriter
//! .java): `{"otherData": {...}, "traceEvents": [...]}` with `M` thread
//! metadata, `i` build phase markers (ProfilePhase descriptions), `X`
//! spans with microsecond `ts`/`dur`, and the critical path as the fake
//! thread 0. Gzipped when the file name ends in `.gz`. chrome://tracing,
//! Perfetto and the usual Bazel profile tools read it.
//!
//! Off by default: without `--profile` (or `--generate_json_trace_profile`)
//! every hook is one relaxed atomic load. While recording, each thread
//! appends to its own buffer (an uncontended mutex per thread, registered
//! once); the file is written when the command ends ([`finish`]).
//!
//! The same recorder keeps the finished actions' spans and dependencies
//! for the critical path while a build event file is written (crate::bep:
//! BuildToolLogs' `critical path`, BuildMetrics' timing).

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The profiling options of build/test/run (CommonCommandOptions).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// `--profile`.
    pub path: Option<String>,
    /// `--[no]generate_json_trace_profile`: `None` is Bazel's `auto`, which
    /// natively writes no profile unless `--profile` names one.
    pub generate: Option<bool>,
    /// `--experimental_profile_include_target_label`.
    pub include_target_label: bool,
    /// `--experimental_profile_include_primary_output`.
    pub include_primary_output: bool,
}

const BOOLEAN_NOOPS: &[&str] = &[
    "slim_profile",
    "experimental_slim_json_profile",
    "record_full_profiler_data",
    "experimental_announce_profile_path",
    "experimental_collect_load_average_in_profiler",
    "experimental_collect_system_network_usage",
    "experimental_collect_resource_estimation",
    "experimental_collect_pressure_stall_indicators",
    "experimental_collect_skyframe_counts_in_profiler",
    "experimental_collect_worker_data_in_profiler",
    "experimental_profile_include_target_configuration",
    "experimental_run_bep_event_include_residue",
    "experimental_command_profile",
];

const STRING_NOOPS: &[&str] = &[
    "experimental_profile_additional_tasks",
    "memory_profile",
    "memory_profile_stable_heap_parameters",
    "experimental_profile_cpu_usage",
];

impl Options {
    /// One option: `Some(Ok(used_next))` when it is a profiling option.
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Option<Result<bool, String>> {
        use crate::configuration::boolean_option;
        if let Some(value) = boolean_option(arg, "generate_json_trace_profile")
            .or_else(|| boolean_option(arg, "experimental_generate_json_trace_profile"))
        {
            return Some(value.map(|value| {
                self.generate = Some(value);
                false
            }));
        }
        if let Some(value) = boolean_option(arg, "experimental_profile_include_target_label") {
            return Some(value.map(|value| {
                self.include_target_label = value;
                false
            }));
        }
        if let Some(value) = boolean_option(arg, "experimental_profile_include_primary_output") {
            return Some(value.map(|value| {
                self.include_primary_output = value;
                false
            }));
        }
        for name in BOOLEAN_NOOPS {
            if let Some(value) = boolean_option(arg, name) {
                return Some(value.map(|_| false));
            }
        }
        for name in std::iter::once(&"profile").chain(STRING_NOOPS) {
            let flag = format!("--{name}");
            let (value, used) = if arg == flag {
                match next {
                    Some(value) => (value, true),
                    None => return Some(Err(format!("{flag} requires a value"))),
                }
            } else if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
                (value, false)
            } else {
                continue;
            };
            if *name == "profile" {
                self.path = (!value.is_empty()).then(|| value.to_owned());
            }
            return Some(Ok(used));
        }
        None
    }

    /// The profile file to write, if any: `--profile` (relative to the
    /// working directory), else with an explicit
    /// `--generate_json_trace_profile` `<output base>/command-<id>.profile.gz`.
    pub fn file(&self, cwd: &Path, output_base: &Path) -> Option<PathBuf> {
        if self.generate == Some(false) && self.path.is_none() {
            return None;
        }
        match &self.path {
            Some(path) => Some(cwd.join(path)),
            None if self.generate == Some(true) => Some(output_base.join(format!(
                "command-{}.profile.gz",
                crate::bep::invocation_id()
            ))),
            None => None,
        }
    }
}

/// One `X` span.
struct Span {
    category: &'static str,
    name: String,
    start: Duration,
    duration: Duration,
    args: Vec<(&'static str, String)>,
}

/// One thread's buffer.
struct ThreadBuffer {
    tid: u64,
    name: String,
    spans: Vec<Span>,
}

/// One finished action, for the critical path.
struct ActionNode {
    id: String,
    description: String,
    start: Duration,
    end: Duration,
    dependencies: Vec<String>,
}

struct Profile {
    path: Option<PathBuf>,
    output_base: PathBuf,
    include_target_label: bool,
    include_primary_output: bool,
    threads: Vec<Arc<Mutex<ThreadBuffer>>>,
}

/// A profile file is requested.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Phase marks and action nodes are recorded (a profile or a build event
/// file).
static RECORDING: AtomicBool = AtomicBool::new(false);
/// Thread ids: 1 is the main thread, the others count from 2.
static NEXT_TID: AtomicU64 = AtomicU64::new(2);
static PROFILE: Mutex<Option<Profile>> = Mutex::new(None);
static MARKS: Mutex<Vec<(String, Duration)>> = Mutex::new(Vec::new());
static ACTIONS: Mutex<Vec<ActionNode>> = Mutex::new(Vec::new());

thread_local! {
    static LOCAL: RefCell<Option<Arc<Mutex<ThreadBuffer>>>> = const { RefCell::new(None) };
}

/// Whether a profile file is being recorded.
#[inline]
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Whether phase marks and actions are recorded.
#[inline]
pub fn recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

/// Record phase marks and actions (for crate::bep) without a profile file.
pub fn enable_recording() {
    RECORDING.store(true, Ordering::Relaxed);
}

/// Start recording a profile to `path` (`output_base`: the profile's
/// `output_base`).
pub fn install(path: PathBuf, output_base: PathBuf, options: &Options) {
    *PROFILE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Profile {
        path: Some(path),
        output_base,
        include_target_label: options.include_target_label,
        include_primary_output: options.include_primary_output,
        threads: Vec::new(),
    });
    ACTIVE.store(true, Ordering::Relaxed);
    RECORDING.store(true, Ordering::Relaxed);
}

fn since_start(instant: Instant) -> Duration {
    instant.saturating_duration_since(crate::process_clock().0)
}

/// A crate::perf_mark: a phase marker (and the build event timing).
pub fn mark(name: &str) {
    if !recording() {
        return;
    }
    let at = since_start(Instant::now());
    MARKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((name.to_owned(), at));
}

/// The time between the first marks `from` and `to`, in milliseconds (0
/// when either is missing).
pub fn phase_millis(from: &str, to: &str) -> i64 {
    let marks = MARKS.lock().unwrap_or_else(|e| e.into_inner());
    let find = |name: &str| {
        marks
            .iter()
            .find(|(mark, _)| mark == name)
            .map(|(_, at)| *at)
    };
    match (find(from), find(to)) {
        (Some(from), Some(to)) => to.saturating_sub(from).as_millis() as i64,
        _ => 0,
    }
}

/// This thread's buffer, registered on first use.
fn with_buffer(f: impl FnOnce(&mut ThreadBuffer)) {
    LOCAL.with(|local| {
        let mut local = local.borrow_mut();
        let buffer = local.get_or_insert_with(|| {
            let thread = std::thread::current();
            let tid = if thread.name() == Some("main") {
                1
            } else {
                NEXT_TID.fetch_add(1, Ordering::Relaxed)
            };
            let name = thread
                .name()
                .map_or_else(|| format!("worker-{tid}"), str::to_owned);
            let buffer = Arc::new(Mutex::new(ThreadBuffer {
                tid,
                name,
                spans: Vec::new(),
            }));
            if let Some(profile) = PROFILE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                profile.threads.push(buffer.clone());
            }
            buffer
        });
        f(&mut buffer.lock().unwrap_or_else(|e| e.into_inner()));
    });
}

/// A span on this thread (any category), recorded only with a profile.
pub fn span(
    category: &'static str,
    name: impl Into<String>,
    start: Instant,
    args: Vec<(&'static str, String)>,
) {
    if !active() {
        return;
    }
    let end = Instant::now();
    let span = Span {
        category,
        name: name.into(),
        start: since_start(start),
        duration: end.saturating_duration_since(start),
        args,
    };
    with_buffer(|buffer| buffer.spans.push(span));
}

/// An action's mnemonic as Bazel names it: the spawn's, else the native
/// write's or transform's Bazel counterpart (FileWrite, SymlinkTree,
/// SourceSymlinkManifest, RepoMappingManifest, TemplateExpand, Symlink).
pub fn mnemonic(action: &crate::action::Action) -> &str {
    use crate::action::Transform;
    if !action.extra.mnemonic.is_empty() {
        return &action.extra.mnemonic;
    }
    if action.id.ends_with("#repo_mapping") {
        return "RepoMappingManifest";
    }
    match &action.extra.transform {
        Some(Transform::RunfilesManifest { .. }) => "SourceSymlinkManifest",
        Some(Transform::RunfilesTree { .. }) => "SymlinkTree",
        Some(Transform::Symlink { .. }) | Some(Transform::UnresolvedSymlink { .. }) => "Symlink",
        Some(Transform::Template { .. }) | Some(Transform::TemplateKeyed { .. }) => {
            "TemplateExpand"
        }
        Some(Transform::BuildInfo { .. }) => "BuildInfo",
        Some(Transform::WorkspaceStatus { .. }) => "BazelWorkspaceStatusAction",
        Some(Transform::Unsupported { .. }) => "Action",
        None if action.write.is_some() => "FileWrite",
        None => "Action",
    }
}

/// The progress-message-like description of an action: its mnemonic and
/// primary output (`Genrule bazel-out/k8-fastbuild/bin/pkg/a.txt`).
pub fn describe(action: &crate::action::Action) -> String {
    let output = action
        .outputs
        .first()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    format!("{} {output}", mnemonic(action))
}

/// A scheduler worker finished an action that started at `start`: its
/// `action processing` span and its critical path node.
pub fn action_done(
    action: &crate::action::Action,
    dependencies: Vec<String>,
    start: Instant,
    cache_hit: bool,
) {
    if !recording() {
        return;
    }
    let end = Instant::now();
    let description = describe(action);
    if active() {
        let (label, output) = PROFILE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or((false, false), |profile| {
                (profile.include_target_label, profile.include_primary_output)
            });
        let mut args = vec![("mnemonic", mnemonic(action).to_owned())];
        if label {
            let owner = action.id.split('#').next().unwrap_or(&action.id);
            args.push(("target", owner.to_owned()));
        }
        if output && let Some(primary) = action.outputs.first() {
            args.push(("out", primary.display().to_string()));
        }
        let span = Span {
            category: "action processing",
            name: description.clone(),
            start: since_start(start),
            duration: end.saturating_duration_since(start),
            args,
        };
        with_buffer(|buffer| buffer.spans.push(span));
    }
    if !cache_hit {
        ACTIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(ActionNode {
                id: action.id.clone(),
                description,
                start: since_start(start),
                end: since_start(end),
                dependencies,
            });
    }
}

/// The critical path (CriticalPathComputer): the chain of executed actions
/// whose durations add up to the most, first action first, as
/// (description, duration).
pub fn critical_path() -> Vec<(String, Duration)> {
    critical_path_spans()
        .into_iter()
        .map(|(_, duration, description)| (description, duration))
        .collect()
}

/// The critical path's spans (start, duration, description).
fn critical_path_spans() -> Vec<(Duration, Duration, String)> {
    let actions = ACTIONS.lock().unwrap_or_else(|e| e.into_inner());
    let index: HashMap<&str, usize> = actions
        .iter()
        .enumerate()
        .map(|(position, action)| (action.id.as_str(), position))
        .collect();
    // Dependencies finish before their consumers start: in end order.
    let mut order: Vec<usize> = (0..actions.len()).collect();
    order.sort_by_key(|&position| actions[position].end);
    let mut best: Vec<(Duration, Option<usize>)> = vec![(Duration::ZERO, None); actions.len()];
    for &position in &order {
        let action = &actions[position];
        let own = action.end.saturating_sub(action.start);
        let mut longest = (Duration::ZERO, None);
        for dependency in &action.dependencies {
            if let Some(&dependency) = index.get(dependency.as_str())
                && best[dependency].0 > longest.0
            {
                longest = (best[dependency].0, Some(dependency));
            }
        }
        best[position] = (own + longest.0, longest.1);
    }
    let Some(mut current) = (0..actions.len()).max_by_key(|&position| best[position].0) else {
        return Vec::new();
    };
    let mut path = Vec::new();
    loop {
        let action = &actions[current];
        path.push((
            action.start,
            action.end.saturating_sub(action.start),
            action.description.clone(),
        ));
        match best[current].1 {
            Some(previous) => current = previous,
            None => break,
        }
    }
    path.reverse();
    path
}

/// This process's user and system CPU time, its finished children's
/// included (/proc/self/stat utime, stime, cutime, cstime).
pub fn cpu_times() -> (Duration, Duration) {
    // snapshot-unrecorded: this process's CPU times for the build metrics.
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return (Duration::ZERO, Duration::ZERO);
    };
    // The fields after the parenthesized command name.
    let fields: Vec<&str> = stat
        .rsplit_once(')')
        .map(|(_, rest)| rest.split_whitespace().collect())
        .unwrap_or_default();
    let ticks = |index: usize| {
        fields
            .get(index)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
    };
    // Fields 14-17 of proc(5); `fields[0]` is field 3.
    let per_tick = Duration::from_millis(10);
    (
        per_tick * (ticks(11) + ticks(13)) as u32,
        per_tick * (ticks(12) + ticks(14)) as u32,
    )
}

/// The ProfilePhase description of a crate::perf_mark name.
fn phase(mark: &str) -> Option<&'static str> {
    Some(match mark {
        "select.start" => "Evaluate target patterns",
        "plan.start" | "snapshot.hit" => "Load and analyze dependencies",
        "plan.end" => "Prepare for build",
        "execute.start" => "Build artifacts",
        "execute.end" => "Complete build",
        _ => return None,
    })
}

fn json_string(text: &str) -> String {
    let mut out = String::new();
    crate::bep::proto::json_string(&mut out, text);
    out
}

fn micros(duration: Duration) -> u128 {
    duration.as_micros()
}

/// Write the profile (when one is recorded). Idempotent.
pub fn finish() {
    if !active() {
        return;
    }
    ACTIVE.store(false, Ordering::Relaxed);
    let Some(profile) = PROFILE.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    let Some(path) = profile.path.clone() else {
        return;
    };
    if let Err(error) = write(&profile, &path) {
        crate::note!(
            "WARNING: Failed to write the profile {}: {error}",
            path.display()
        );
    }
}

fn write(profile: &Profile, path: &Path) -> std::io::Result<()> {
    let (_, start) = crate::process_clock();
    let start_millis = start
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let mut out = String::new();
    out.push_str("{\"otherData\":{");
    out.push_str(&format!(
        "\"bazel_version\":{},\"build_id\":{},\"output_base\":{},\"date\":{},\"profile_start_ts\":{start_millis}",
        json_string(&format!(
            "bazel-rs {} (Bazel {} compatible)",
            env!("CARGO_PKG_VERSION"),
            crate::COMPATIBILITY_VERSION
        )),
        json_string(crate::bep::invocation_id()),
        json_string(&profile.output_base.display().to_string()),
        json_string(&crate::bep::proto::rfc3339(
            start_millis.div_euclid(1000),
            (start_millis.rem_euclid(1000) * 1_000_000) as i32
        )),
    ));
    out.push_str("},\"traceEvents\":[\n");
    let mut events: Vec<String> = Vec::new();
    let metadata = |tid: u64, name: &str, sort: i64| {
        [
            format!(
                "{{\"name\":\"thread_name\",\"ph\":\"M\",\"pid\":1,\"tid\":{tid},\"args\":{{\"name\":{}}}}}",
                json_string(name)
            ),
            format!(
                "{{\"name\":\"thread_sort_index\",\"ph\":\"M\",\"pid\":1,\"tid\":{tid},\"args\":{{\"sort_index\":{sort}}}}}"
            ),
        ]
    };
    events.extend(metadata(0, "Critical Path", 0));
    // The main thread is the first that recorded (tid 1) or, if none did,
    // a bare lane for the phase markers.
    let main_tid = 1;
    let threads: Vec<Arc<Mutex<ThreadBuffer>>> = profile.threads.clone();
    let has_main = threads.iter().any(|buffer| {
        buffer
            .lock()
            .map(|buffer| buffer.tid == main_tid)
            .unwrap_or(false)
    });
    if !has_main {
        events.extend(metadata(main_tid, "main", 1_000_000));
    }
    events.push(format!(
        "{{\"cat\":\"build phase marker\",\"name\":\"Launch Blaze\",\"ph\":\"X\",\"ts\":0,\"dur\":0,\"pid\":1,\"tid\":{main_tid}}}"
    ));
    events.push(format!(
        "{{\"cat\":\"build phase marker\",\"name\":\"Initialize command\",\"ph\":\"i\",\"ts\":0,\"pid\":1,\"tid\":{main_tid}}}"
    ));
    let marks = MARKS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut seen_phases: Vec<&str> = Vec::new();
    for (mark, at) in &marks {
        if let Some(description) = phase(mark)
            && !seen_phases.contains(&description)
        {
            seen_phases.push(description);
            events.push(format!(
                "{{\"cat\":\"build phase marker\",\"name\":{},\"ph\":\"i\",\"ts\":{},\"pid\":1,\"tid\":{main_tid}}}",
                json_string(description),
                micros(*at)
            ));
        }
    }
    // The phases as spans on the main thread (the markers above are
    // instants): target patterns, analysis, execution.
    let find = |name: &str| {
        marks
            .iter()
            .find(|(mark, _)| mark == name)
            .map(|(_, at)| *at)
    };
    for (from, to, name) in [
        ("select.start", "select.end", "evaluate target patterns"),
        ("plan.start", "plan.end", "analysis"),
        ("execute.start", "execute.end", "execution"),
    ] {
        if let (Some(from), Some(to)) = (find(from), find(to)) {
            events.push(format!(
                "{{\"cat\":\"general information\",\"name\":{},\"ph\":\"X\",\"ts\":{},\"dur\":{},\"pid\":1,\"tid\":{main_tid}}}",
                json_string(name),
                micros(from),
                micros(to.saturating_sub(from))
            ));
        }
    }
    let mut sort_index: BTreeMap<u64, i64> = BTreeMap::new();
    for buffer in &threads {
        let buffer = buffer.lock().unwrap_or_else(|e| e.into_inner());
        let sort = if buffer.tid == main_tid {
            1_000_000
        } else {
            10_000 + buffer.tid as i64
        };
        sort_index.insert(buffer.tid, sort);
        events.extend(metadata(buffer.tid, &buffer.name, sort));
        for span in &buffer.spans {
            let mut args = String::new();
            if !span.args.is_empty() {
                args.push_str(",\"args\":{");
                for (position, (key, value)) in span.args.iter().enumerate() {
                    if position > 0 {
                        args.push(',');
                    }
                    args.push_str(&format!("{}:{}", json_string(key), json_string(value)));
                }
                args.push('}');
            }
            events.push(format!(
                "{{\"cat\":{},\"name\":{},\"ph\":\"X\",\"ts\":{},\"dur\":{},\"pid\":1{args},\"tid\":{}}}",
                json_string(span.category),
                json_string(&span.name),
                micros(span.start),
                micros(span.duration),
                buffer.tid
            ));
        }
    }
    for (start, duration, description) in critical_path_spans() {
        events.push(format!(
            "{{\"cat\":\"critical path component\",\"name\":{},\"ph\":\"X\",\"ts\":{},\"dur\":{},\"pid\":1,\"args\":{{\"tid\":{main_tid}}},\"tid\":0}}",
            json_string(&format!("action '{description}'")),
            micros(start),
            micros(duration)
        ));
    }
    out.push_str(&events.join(",\n"));
    out.push_str("\n]}\n");
    // snapshot-unrecorded: the profile file (a write).
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    if path.extension().is_some_and(|extension| extension == "gz") {
        let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        encoder.write_all(out.as_bytes())?;
        encoder.finish()?.flush()
    } else {
        let mut file = file;
        file.write_all(out.as_bytes())?;
        file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_and_choose_the_file() {
        let mut options = Options::default();
        assert_eq!(options.parse("--profile=/tmp/p.gz", None), Some(Ok(false)));
        assert_eq!(
            options.parse("--experimental_profile_include_target_label", None),
            Some(Ok(false))
        );
        assert_eq!(options.parse("--noslim_profile", None), Some(Ok(false)));
        assert_eq!(options.parse("--jobs=1", None), None);
        assert!(options.include_target_label);
        assert_eq!(
            options.file(Path::new("/w"), Path::new("/o")),
            Some(PathBuf::from("/tmp/p.gz"))
        );
        let mut generated = Options::default();
        assert_eq!(generated.file(Path::new("/w"), Path::new("/o")), None);
        generated.parse("--generate_json_trace_profile", None);
        assert!(
            generated
                .file(Path::new("/w"), Path::new("/o"))
                .unwrap()
                .starts_with("/o")
        );
        let mut relative = Options::default();
        relative.parse("--profile", Some("p.json"));
        assert_eq!(
            relative.file(Path::new("/w"), Path::new("/o")),
            Some(PathBuf::from("/w/p.json"))
        );
    }
}
