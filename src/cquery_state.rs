//! Binary configured-graph snapshots. The default early lookup preserves local
//! replay and saved diagnostics. An explicit native opt-in selects current
//! ordinary loading in a separate analysis namespace; the paths never mix.
use crate::engine::cquery::Graph;
use crate::engine::root_checkpoint::{Budget, RootCut};
use crate::snapshot::{self, format, key, store};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

pub const CURRENT_LOADING_ENV: &str = "BAZEL_RS_CQUERY_CURRENT_LOADING";
pub const ROOT_CHECKPOINT_ENV: &str = "BAZEL_RS_CQUERY_ROOT_CHECKPOINT";

pub fn root_checkpoint_budget() -> Result<Option<Budget>, String> {
    // snapshot-unrecorded: native admission/profile controls, not semantic inputs.
    let enabled = parse_current_loading(std::env::var_os(ROOT_CHECKPOINT_ENV).as_deref())
        .map_err(|error| error.replace(CURRENT_LOADING_ENV, ROOT_CHECKPOINT_ENV))?;
    if !enabled {
        return Ok(None);
    }
    let lower = |name: &str, ceiling: u64| -> Result<u64, String> {
        // snapshot-unrecorded: lower-only lookup-overlap admission; never a
        // semantic input or authority to choose a different root/demand.
        let Some(value) = std::env::var_os(name) else {
            return Ok(ceiling);
        };
        value
            .to_str()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value <= ceiling)
            .ok_or_else(|| format!("{name} must be an integer from 0 through {ceiling}"))
    };
    let defaults = Budget::default();
    Ok(Some(Budget {
        millis: lower("BAZEL_RS_CQUERY_ROOT_MAX_MS", defaults.millis)?,
        nodes: lower("BAZEL_RS_CQUERY_ROOT_MAX_NODES", defaults.nodes as u64)? as usize,
        rss_mb: lower("BAZEL_RS_CQUERY_ROOT_MAX_RSS_MB", defaults.rss_mb)?,
    }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Profile {
    Default,
    Patterns,
    Root,
}

pub fn current_loading_enabled() -> Result<bool, String> {
    // snapshot-unrecorded: native profile selector, encoded in the cquery key.
    parse_current_loading(std::env::var_os(CURRENT_LOADING_ENV).as_deref())
}

fn parse_current_loading(value: Option<&std::ffi::OsStr>) -> Result<bool, String> {
    match value.and_then(std::ffi::OsStr::to_str) {
        None if value.is_none() => Ok(false),
        Some("") | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(value) if value.eq_ignore_ascii_case("off") => Ok(false),
        Some(value) if value.eq_ignore_ascii_case("on") => Ok(true),
        _ => Err(format!(
            "{CURRENT_LOADING_ENV} must be 1/on or 0/off (unset or empty means off)"
        )),
    }
}

#[path = "cquery_events.rs"]
mod event_phases;
#[path = "cquery_history.rs"]
mod history;
pub(crate) use event_phases::PrefixAgreement;

pub struct Session {
    cache: PathBuf,
    key: key::Key,
    capture: Option<snapshot::capture::Capture>,
    verify: Option<(Graph, store::Reader)>,
    prefix: Option<snapshot::capture::Captured>,
    replay: snapshot::hit::ReplayInputs,
    verify_mode: bool,
    current_loading: bool,
    prefix_boundary_owned: bool,
    verify_suffix: Option<Vec<u8>>,
    profile: Profile,
    root_cut: Option<RootCut>,
}

pub struct Started {
    pub hit: Option<Graph>,
    pub session: Option<Session>,
}

fn presentation(option: &crate::bazelrc::Unit) -> bool {
    let name = option.option.text.split('=').next().unwrap_or_default();
    // `--output=files` and `--output=build` read facts the other formats'
    // graphs do not record (Engine::set_cquery_files/set_cquery_build): each
    // keys its own analysis.
    if name == "--output" {
        let value = option
            .option
            .text
            .split_once('=')
            .map(|(_, value)| value)
            .or_else(|| option.value.as_ref().map(|value| value.text.as_str()));
        return !matches!(value, Some("files" | "build" | "transitions"));
    }
    matches!(
        name,
        "--output_file"
            | "--query_file"
            | "--transitions"
            | "--universe_scope"
            | "--implicit_deps"
            | "--noimplicit_deps"
            | "--tool_deps"
            | "--notool_deps"
            | "--include_aspects"
            | "--noinclude_aspects"
            | "--starlark:expr"
            | "--starlark:file"
            | "--graph:node_limit"
            | "--graph:factored"
            | "--nograph:factored"
    ) || name.starts_with("--proto:")
        || name.starts_with("--noproto:")
}

/// Installed default: lookup before any current package loading. A miss opens
/// the normal recording window, exactly as the original cquery checkpoint did.
pub fn start(
    inputs: key::Inputs<'_>,
    replay: snapshot::hit::ReplayInputs,
    publication_repositories: crate::repository_publication::Factory<'_>,
) -> Result<Started, String> {
    let Some((key, mode)) = analysis_key(&inputs, Profile::Default) else {
        return Ok(Started {
            hit: None,
            session: None,
        });
    };
    let found = read(
        inputs.cache,
        &key,
        &replay,
        Validation::Ordinary(publication_repositories),
    )?;
    let verify = match found {
        Some((graph, reader, events)) if mode != snapshot::Mode::Verify => {
            use std::io::Write;
            std::io::stderr()
                .lock()
                .write_all(&crate::ui::filtered(&events))
                .map_err(|e| e.to_string())?;
            reader.touch();
            explain("hit");
            return Ok(Started {
                hit: Some(graph),
                session: None,
            });
        }
        Some((graph, reader, _)) => Some((graph, reader)),
        None => None,
    };
    Ok(Started {
        hit: None,
        session: open_session(inputs.cache, key, replay, mode, Profile::Default, verify),
    })
}

/// Explicit experimental profile: open recording but do not inspect history
/// until ordinary loading has sealed its results. Never fall back to replay.
pub fn begin(
    inputs: key::Inputs<'_>,
    replay: snapshot::hit::ReplayInputs,
) -> Result<Option<Session>, String> {
    let Some((key, mode)) = analysis_key(&inputs, Profile::Patterns) else {
        return Ok(None);
    };
    Ok(open_session(
        inputs.cache,
        key,
        replay,
        mode,
        Profile::Patterns,
        None,
    ))
}

/// Root mode opens the same observation/capture window, but performs NO pattern
/// lookup. Capture remains prefix-owned while configured phase1 is live.
pub fn begin_root(
    inputs: key::Inputs<'_>,
    replay: snapshot::hit::ReplayInputs,
) -> Result<Option<Session>, String> {
    let Some((key, mode)) = analysis_key(&inputs, Profile::Root) else {
        return Ok(None);
    };
    Ok(open_session(
        inputs.cache,
        key,
        replay,
        mode,
        Profile::Root,
        None,
    ))
}

fn analysis_key(inputs: &key::Inputs<'_>, profile: Profile) -> Option<(key::Key, snapshot::Mode)> {
    let mode = snapshot::Mode::from_env();
    if matches!(mode, snapshot::Mode::Off | snapshot::Mode::Record) || key::test_hook().is_some() {
        return None;
    }
    let _ = std::fs::create_dir_all(store::directory(inputs.cache));
    let Ok(binary) = snapshot::exe_id::binary_id(&store::root(inputs.cache)) else {
        return None;
    };
    let options = inputs
        .options
        .iter()
        .filter(|unit| !presentation(unit))
        .cloned()
        .collect::<Vec<_>>();
    let filtered = key::Inputs {
        command: match profile {
            Profile::Root => "cquery-graph-registered-root-v1",
            Profile::Patterns => "cquery-graph-current-loading-v2",
            Profile::Default => "cquery-graph-v2",
        },
        options: &options,
        ..*inputs
    };
    let key = key::compute(&filtered, binary);
    Some((key, mode))
}

fn open_session(
    cache: &Path,
    key: key::Key,
    replay: snapshot::hit::ReplayInputs,
    mode: snapshot::Mode,
    profile: Profile,
    verify: Option<(Graph, store::Reader)>,
) -> Option<Session> {
    if !crate::observed_fs::open_window() {
        return None;
    }
    snapshot::replay::install();
    let capture = snapshot::capture::start(snapshot::capture::Mode::Tee).ok();
    Some(Session {
        cache: cache.to_owned(),
        key,
        capture,
        verify,
        prefix: None,
        replay,
        verify_mode: mode == snapshot::Mode::Verify,
        current_loading: profile != Profile::Default,
        profile,
        root_cut: None,
        prefix_boundary_owned: false,
        verify_suffix: None,
    })
}

#[derive(Clone, Copy)]
enum Validation<'a> {
    Ordinary(crate::repository_publication::Factory<'a>),
    Current(
        &'a crate::repository::CurrentRefresh,
        &'a [u8],
        Option<RootCut>,
    ),
}

fn read(
    cache: &Path,
    key: &key::Key,
    replay: &snapshot::hit::ReplayInputs,
    validation: Validation<'_>,
) -> Result<Option<(Graph, store::Reader, Vec<u8>)>, String> {
    let mut reason = "no configured graph snapshot".to_owned();
    for candidate in history::candidates(cache, key) {
        let result = candidate.and_then(|reader| {
            match validation {
                Validation::Ordinary(factory) => read_ordinary_candidate(&reader, replay, factory),
                Validation::Current(current, prefix, cut) => {
                    read_candidate(&reader, replay, current, prefix, cut)
                }
            }
            .map(|(graph, events)| (graph, reader, events))
        });
        match result {
            Ok(selected) => return Ok(Some(selected)),
            Err(error) if error == snapshot::replay::CANCELLED => return Err(error),
            Err(error) if error.starts_with(crate::repository_publication::COMMIT_ERROR) => {
                return Err(error);
            }
            Err(error) => reason = error,
        }
        // Candidate INPUTS, replay heaps, and any corrupt graph are dropped
        // before probing the next generation. No diagnostics have been emitted.
    }
    explain(&format!("miss: {reason}"));
    Ok(None)
}

fn read_candidate(
    reader: &store::Reader,
    replay: &snapshot::hit::ReplayInputs,
    current: &crate::repository::CurrentRefresh,
    current_prefix: &[u8],
    root_cut: Option<RootCut>,
) -> Result<(Graph, Vec<u8>), String> {
    let _version = history::section(reader, format::Section::Stats)?;
    let mut inputs = format::decode_inputs(&history::section(reader, format::Section::Inputs)?)?;
    if inputs.workspace_status {
        return Err("configured graph recorded workspace status execution".into());
    }
    let events = format::decode_events(&history::section(reader, format::Section::Events)?)?;
    let phases = match root_cut {
        Some(cut) => event_phases::decode_root(&events, cut)?,
        None => event_phases::decode(&events)?,
    };
    let agreement = phases.agree(current_prefix)?;
    drop(inputs.tables.take());
    let environment = snapshot::validate::Environment::new(&replay.environment);
    let adoption = current.prepare(
        &inputs.replay,
        &mut inputs.recording,
        &environment,
        &agreement,
    )?;
    let graph = decode_graph(reader)?;
    current.accept(
        &inputs.replay,
        &inputs.recording,
        adoption,
        &environment,
        &agreement,
    )?;
    Ok((graph, phases.suffix.to_vec()))
}

/// The installed ordinary validation/replay/adoption contract. This function is
/// reached only by the early default checkpoint, never after a current prefix.
fn read_ordinary_candidate(
    reader: &store::Reader,
    replay: &snapshot::hit::ReplayInputs,
    publication_repositories: crate::repository_publication::Factory<'_>,
) -> Result<(Graph, Vec<u8>), String> {
    let _version = history::section(reader, format::Section::Stats)?;
    let mut inputs = format::decode_inputs(&history::section(reader, format::Section::Inputs)?)?;
    if inputs.workspace_status {
        return Err("configured graph recorded workspace status execution".into());
    }
    let environment = snapshot::validate::Environment::new(&replay.environment);
    let strict = snapshot::validate::validate_for_hit(&inputs.recording, &environment);
    let adoption = if strict.is_err()
        || crate::repository_publication::needs_view_adoption(&inputs.recording)
    {
        if !inputs.replay.is_empty() {
            return Err(
                "historical publication adoption with local repository replay is unsupported"
                    .into(),
            );
        }
        drop(inputs.tables.take());
        let prepared = crate::repository_publication::Prepared::prepare(
            publication_repositories,
            &mut inputs.recording,
            &environment,
        )?;
        Some((prepared, inputs.recording))
    } else {
        if !inputs.replay.is_empty() {
            let cancel = AtomicBool::new(false);
            snapshot::hit::replay_records(inputs.tables.take(), replay, &inputs.replay, &cancel)?;
            snapshot::hit::check_local_trees(&inputs.replay, &cancel, false)?;
        }
        None
    };
    let events = format::decode_events(&history::section(reader, format::Section::Events)?)?;
    let graph = decode_graph(reader)?;
    if let Some((prepared, recording)) = adoption {
        prepared.commit(publication_repositories, &recording, &environment)?;
    }
    Ok((graph, events))
}

fn decode_graph(reader: &store::Reader) -> Result<Graph, String> {
    let bytes = history::section(reader, format::Section::Plan)?;
    let graph: Graph =
        postcard::from_bytes(&bytes).map_err(|e| format!("configured graph decode: {e}"))?;
    for node in &graph.nodes {
        if node
            .configuration
            .is_some_and(|id| id as usize >= graph.configurations.len())
            || node
                .edges
                .iter()
                .any(|edge| edge.to as usize >= graph.nodes.len())
        {
            return Err("invalid configured graph index".into());
        }
    }
    if graph
        .patterns
        .values()
        .flatten()
        .any(|id| *id as usize >= graph.nodes.len())
    {
        return Err("invalid configured graph pattern index".into());
    }
    Ok(graph)
}

impl Session {
    pub fn lookup_root(
        &mut self,
        current: &crate::repository::CurrentRefresh,
        cut: RootCut,
    ) -> Result<Option<Graph>, String> {
        if self.profile != Profile::Root || self.root_cut.is_some() {
            return Err("root checkpoint does not belong to this session".into());
        }
        self.root_cut = Some(cut);
        self.lookup(current)
    }
    /// The current command has completed its ordinary loading prefix. Only
    /// now can historical metadata be inspected. Current output has already
    /// been forwarded once. It must match the explicitly recorded prefix; only
    /// a selected candidate's suffix is replayed after complete acceptance.
    pub fn lookup(
        &mut self,
        current: &crate::repository::CurrentRefresh,
    ) -> Result<Option<Graph>, String> {
        if !self.current_loading {
            return Err("ordinary cquery session cannot enter current-loading lookup".into());
        }
        if self.profile == Profile::Root && self.root_cut.is_none() {
            return Err("root mode cannot look up at the pattern boundary".into());
        }
        if self.prefix.is_some() {
            return Err("current-loading lookup already performed".into());
        }
        let prefix = self
            .capture
            .take()
            .map(|capture| capture.stop(std::time::Duration::from_secs(2)))
            .unwrap_or_default();
        self.prefix_boundary_owned =
            current.event_boundary_owned() && event_phases::complete(&prefix);
        self.prefix = Some(prefix);
        self.capture = snapshot::capture::start(snapshot::capture::Mode::Tee).ok();
        if !self.prefix_boundary_owned || !current.eligible() || self.capture.is_none() {
            explain("miss: current loading event boundary or result is not eligible");
            return Ok(None);
        }
        // Lookup is a distinct transient phase, never a saved analysis suffix.
        let found = read(
            &self.cache,
            &self.key,
            &self.replay,
            Validation::Current(
                current,
                &self.prefix.as_ref().expect("prefix captured").bytes,
                self.root_cut,
            ),
        );
        let lookup = self
            .capture
            .take()
            .expect("lookup capture")
            .stop(std::time::Duration::from_secs(2));
        if !event_phases::complete(&lookup) || !lookup.bytes.is_empty() {
            self.prefix_boundary_owned = false;
            if matches!(&found, Ok(Some(_))) {
                // Adoption may have completed: terminal, never a partial-commit
                // miss or a replay of saved analysis diagnostics.
                return Err(format!(
                    "{}unaccounted output during current-loading acceptance",
                    crate::repository_publication::COMMIT_ERROR
                ));
            }
        }
        let found = found?;
        match found {
            Some((graph, reader, suffix)) if self.verify_mode => {
                self.verify = Some((graph, reader));
                self.verify_suffix = Some(suffix);
                self.capture = snapshot::capture::start(snapshot::capture::Mode::Tee).ok();
                Ok(None)
            }
            Some((graph, reader, suffix)) => {
                crate::observed_fs::close_window();
                // Prefix is already visible. No capture is active now, so the
                // accepted suffix cannot be recorded/emitted as current work.
                use std::io::Write;
                std::io::stderr()
                    .lock()
                    .write_all(&crate::ui::filtered(&suffix))
                    .map_err(|error| error.to_string())?;
                reader.touch();
                explain("hit");
                Ok(Some(graph))
            }
            None => {
                self.capture = snapshot::capture::start(snapshot::capture::Mode::Tee).ok();
                Ok(None)
            }
        }
    }

    pub fn finish(
        mut self,
        graph: &Graph,
        registry: &crate::registry_session::SessionHandle,
        event_sources_quiescent: bool,
    ) -> Result<(), String> {
        // Args callbacks and other action-only values do not belong to this
        // payload. Analysis failures have already aborted before this point.
        crate::observed_fs::settle_plan_taints([]);
        crate::observed_fs::close_window();
        let window = self
            .capture
            .take()
            .map(|capture| capture.stop(std::time::Duration::from_secs(2)));
        if let Some((expected, reader)) = &self.verify
            && expected != graph
        {
            history::remove(&self.cache, reader)?;
            return Err(
                "configured graph snapshot verification mismatch; stale generation invalidated"
                    .into(),
            );
        }
        if let (Some(expected), Some((_, reader))) = (&self.verify_suffix, &self.verify)
            && (!self.prefix_boundary_owned
                || !event_sources_quiescent
                || !window.as_ref().is_some_and(|window| {
                    event_phases::complete(window) && window.bytes == *expected
                }))
        {
            history::remove(&self.cache, reader)?;
            return Err("configured graph snapshot diagnostic phase verification mismatch; stale generation invalidated".into());
        }
        let save = || -> Result<(), String> {
            let window = window.ok_or("stderr capture unavailable")?;
            if !window.complete || window.overflowed || window.ended_early {
                return Err("incomplete stderr capture".into());
            }
            let events = if self.current_loading {
                let prefix = self
                    .prefix
                    .as_ref()
                    .ok_or("current-loading prefix capture unavailable")?;
                if !self.prefix_boundary_owned
                    || !event_sources_quiescent
                    || !event_phases::complete(prefix)
                {
                    return Err("current-loading event phase ownership is incomplete".into());
                }
                let bytes = match self.root_cut {
                    Some(cut) => event_phases::encode_root(&prefix.bytes, &window.bytes, cut)?,
                    None => event_phases::encode(&prefix.bytes, &window.bytes)?,
                };
                format::encode_events(&bytes)
            } else {
                format::encode_events(&window.bytes)
            };
            if let Some(registry) = registry.session()
                && (registry.updated_lockfile().is_some()
                    || registry.updated_hidden_lockfile().is_some())
            {
                return Err("lockfile write pending".into());
            }
            // The hidden lockfile, read unobserved, is recorded by the
            // entries this cquery used (crate::registry_session).
            if let Some(registry) = registry.session() {
                registry.note_hidden_lockfile();
            }
            let recorder = crate::observed_fs::recorder().ok_or("no input observations")?;
            if !recorder.taints().is_empty() {
                return Err(format!("tainted analysis: {:?}", recorder.taints()));
            }
            let capture = snapshot::replay::global().ok_or("no repository replay recorder")?;
            let records = capture.records();
            let tables = capture.tables();
            if !records.is_empty() && tables.is_none() {
                return Err("repository replay lacks pinned tables".into());
            }
            let recording = snapshot::validate::Policy::current().select_owned(recorder.take());
            crate::perf_mark("cquery.snapshot_inputs_begin");
            let inputs = format::encode_inputs(&recording, &records, tables.as_ref(), false);
            crate::perf_mark("cquery.snapshot_encode_begin");
            // A bounded frame buffer, never a second full uncompressed graph.
            let plan = encode_graph(graph)?;
            crate::perf_mark("cquery.snapshot_compressed");
            history::save(
                &self.cache,
                &self.key,
                inputs,
                events,
                plan,
                self.verify.as_ref().map(|(_, reader)| reader),
            )?;
            crate::perf_mark("cquery.snapshot_saved");
            explain("saved");
            Ok(())
        };
        if let Err(error) = save() {
            explain(&format!("not saved: {error}"));
        }
        Ok(())
    }
}

/// Incremental postcard -> existing compressed PLAN framing. Fixed boundaries
/// preserve the exact decompressed representation (and compression bytes), so
/// readers and validators need no special streaming format or migration.
fn encode_graph(value: &impl serde::Serialize) -> Result<Vec<u8>, String> {
    use std::io::Write;
    const FRAME: usize = 4 << 20;
    struct Frames {
        pending: Vec<u8>,
        parts: Vec<Vec<(usize, Vec<u8>)>>,
        total: usize,
        stored: u64,
    }
    impl Frames {
        fn emit(&mut self) -> std::io::Result<()> {
            if !self.pending.is_empty() {
                let part = std::mem::replace(&mut self.pending, Vec::with_capacity(FRAME));
                let compressed = format::compress_part(part);
                let size = compressed
                    .iter()
                    .map(|(_, bytes)| 8 + bytes.len() as u64)
                    .sum::<u64>();
                self.stored = self
                    .stored
                    .checked_add(size)
                    .filter(|size| *size <= history::MAX_PLAN)
                    .ok_or_else(|| {
                        std::io::Error::other("configured graph exceeds stored size limit")
                    })?;
                self.parts.push(compressed);
            }
            Ok(())
        }
    }
    impl Write for Frames {
        fn write(&mut self, mut bytes: &[u8]) -> std::io::Result<usize> {
            let length = bytes.len();
            self.total = self
                .total
                .checked_add(length)
                .filter(|total| *total <= history::MAX_RAW)
                .ok_or_else(|| std::io::Error::other("configured graph exceeds raw size limit"))?;
            while !bytes.is_empty() {
                let count = bytes.len().min(FRAME - self.pending.len());
                self.pending.extend_from_slice(&bytes[..count]);
                bytes = &bytes[count..];
                if self.pending.len() == FRAME {
                    self.emit()?;
                }
            }
            Ok(length)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut frames = Frames {
        pending: Vec::with_capacity(FRAME),
        parts: Vec::new(),
        total: 0,
        stored: 12,
    };
    // Buffer postcard's scalar writes without adding a graph-sized allocation.
    let mut writer = std::io::BufWriter::with_capacity(64 << 10, &mut frames);
    postcard::to_io(value, &mut writer).map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())?;
    drop(writer);
    frames.emit().map_err(|error| error.to_string())?;
    Ok(format::assemble_compressed(frames.parts))
}

impl Drop for Session {
    fn drop(&mut self) {
        crate::observed_fs::close_window();
        if let Some(capture) = self.capture.take() {
            capture.stop(std::time::Duration::from_secs(2));
        }
    }
}

fn explain(text: &str) {
    if snapshot::explain() {
        snapshot::diag::line_fmt(format_args!("bazel-rs-cquery-snapshot: {text}"));
    }
}

fn configuration_path(cache: &Path) -> PathBuf {
    store::root(cache).join("cquery-configurations.bin")
}

/// The last successful configured analysis's actual configuration table, with
/// framing, binary identity and an integrity checksum. No JSON cache on disk.
pub fn save_configurations(cache: &Path, graph: &Graph) -> Result<(), String> {
    use std::io::Write;
    let root = store::root(cache);
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let binary = snapshot::exe_id::binary_id(&root).map_err(|e| e.to_string())?;
    let bytes = postcard::to_allocvec(&graph.configurations).map_err(|e| e.to_string())?;
    let mut file = tempfile::NamedTempFile::new_in(&root).map_err(|e| e.to_string())?;
    file.write_all(b"BZRCQCF1")
        .and_then(|()| file.write_all(&binary))
        .and_then(|()| file.write_all(&format::checksum(&bytes)))
        .and_then(|()| file.write_all(&bytes))
        .map_err(|e| e.to_string())?;
    file.persist(configuration_path(cache))
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn dump_configurations(cache: &Path) -> Result<(), String> {
    config_command(cache, true, true, &[])
}

fn read_configurations(cache: &Path) -> Result<Vec<crate::cquery_proto::Configuration>, String> {
    let root = store::root(cache);
    let binary = snapshot::exe_id::binary_id(&root).map_err(|e| e.to_string())?;
    // snapshot-unrecorded: binary command state, never an analysis input.
    let bytes = std::fs::read(configuration_path(cache))
        .map_err(|e| format!("no successful cquery configuration state: {e}"))?;
    if bytes.len() < 56
        || &bytes[..8] != b"BZRCQCF1"
        || bytes[8..40] != binary
        || bytes[40..56] != format::checksum(&bytes[56..])
    {
        return Err("invalid or incompatible configured-query state".into());
    }
    let mut configurations: Vec<crate::cquery_proto::Configuration> =
        postcard::from_bytes(&bytes[56..]).map_err(|e| e.to_string())?;
    // ConfigCommand.forOutput: ordered by configuration hash.
    configurations.sort_by(|a, b| a.checksum.cmp(&b.checksum));
    Ok(configurations)
}

/// A configuration's fragment options as name -> sorted option map
/// (FragmentOptionsForOutput).
fn option_maps(
    configuration: &crate::cquery_proto::Configuration,
) -> Vec<(String, std::collections::BTreeMap<String, String>)> {
    configuration
        .fragment_options
        .iter()
        .map(|fragment| {
            (
                fragment.name.clone(),
                fragment
                    .options
                    .iter()
                    .filter_map(|o| Some((o.name.clone()?, o.value.clone()?)))
                    .collect(),
            )
        })
        .collect()
}

fn configuration_json(configuration: &crate::cquery_proto::Configuration) -> serde_json::Value {
    serde_json::json!({
        "skyKey": format!("BuildConfigurationKey[{}]", configuration.checksum),
        "configHash": configuration.checksum,
        "mnemonic": configuration.mnemonic,
        "isExec": configuration.is_tool,
        "fragments": configuration.fragments.iter().map(|f| serde_json::json!({"name": f.name, "fragmentOptions": f.fragment_option_names})).collect::<Vec<_>>(),
        "fragmentOptions": option_maps(configuration).into_iter().map(|(name, options)| serde_json::json!({"name": name, "options": options})).collect::<Vec<_>>(),
    })
}

/// TextOutputFormatter.writeConfiguration.
fn configuration_text(configuration: &crate::cquery_proto::Configuration) -> String {
    let mut out = format!(
        "BuildConfigurationValue {}:\nSkyframe Key: BuildConfigurationKey[{}]\nFragments: ",
        configuration.checksum, configuration.checksum
    );
    for fragment in &configuration.fragments {
        out.push_str(&format!(
            "{}: [{}], ",
            fragment.name,
            fragment.fragment_option_names.join(",")
        ));
    }
    out.push('\n');
    for (name, options) in option_maps(configuration) {
        out.push_str(&format!("FragmentOptions {name} {{\n"));
        for (option, value) in options {
            out.push_str(&format!("  {option}: {value}\n"));
        }
        out.push_str("}\n");
    }
    out
}

fn find_configuration<'a>(
    configurations: &'a [crate::cquery_proto::Configuration],
    prefix: &str,
) -> Result<&'a crate::cquery_proto::Configuration, String> {
    let matches: Vec<_> = configurations
        .iter()
        .filter(|c| c.checksum.starts_with(prefix))
        .collect();
    match matches.as_slice() {
        [] => Err(format!("No configuration found with ID prefix {prefix}")),
        [only] => Ok(only),
        _ => Err(format!(
            "Configuration identifier '{prefix}' is ambiguous.\n'{prefix}' is a prefix of multiple configurations:\n {}\n\nUse a sufficient prefix to uniquely identify one configuration.",
            matches
                .iter()
                .map(|c| c.checksum.as_str())
                .collect::<Vec<_>>()
                .join("\n ")
        )),
    }
}

/// `config [--output=text|json] [--dump_all] [ID [ID2]]` (ConfigCommand)
/// over the configurations of the last successful native cquery.
pub fn config_command(
    cache: &Path,
    json: bool,
    dump_all: bool,
    ids: &[String],
) -> Result<(), String> {
    use std::io::Write;
    if ids.len() > 2 {
        return Err("Too many config ids.".into());
    }
    let configurations = read_configurations(cache)?;
    let mut out = String::new();
    match ids {
        [] if dump_all => {
            if json {
                let values: Vec<_> = configurations.iter().map(configuration_json).collect();
                out = serde_json::to_string(&values).map_err(|e| e.to_string())?;
                out.push('\n');
            } else {
                configurations
                    .iter()
                    .for_each(|c| out.push_str(&configuration_text(c)));
            }
        }
        [] => {
            if json {
                let ids: Vec<_> = configurations.iter().map(|c| c.checksum.as_str()).collect();
                out = serde_json::to_string(&serde_json::json!({"configuration-IDs": ids}))
                    .map_err(|e| e.to_string())?;
                out.push('\n');
            } else {
                out.push_str("Available configurations:\n");
                for configuration in &configurations {
                    out.push_str(&format!(
                        "{} {}{}\n",
                        configuration.checksum,
                        configuration.mnemonic,
                        if configuration.is_tool { " (exec)" } else { "" }
                    ));
                }
            }
        }
        [id] => {
            eprintln!("INFO: Displaying config with id {id}");
            let configuration = find_configuration(&configurations, id)?;
            if json {
                out = serde_json::to_string(&configuration_json(configuration))
                    .map_err(|e| e.to_string())?;
                out.push('\n');
            } else {
                out = configuration_text(configuration);
            }
        }
        [first, second] => {
            eprintln!("INFO: Displaying diff between configs {first} and {second}");
            let one = option_maps(find_configuration(&configurations, first)?);
            let two = option_maps(find_configuration(&configurations, second)?);
            let names: std::collections::BTreeSet<&String> =
                one.iter().chain(two.iter()).map(|(name, _)| name).collect();
            let mut diffs = Vec::new();
            for name in names {
                let empty = std::collections::BTreeMap::new();
                let a = one
                    .iter()
                    .find(|(n, _)| n == name)
                    .map_or(&empty, |(_, o)| o);
                let b = two
                    .iter()
                    .find(|(n, _)| n == name)
                    .map_or(&empty, |(_, o)| o);
                let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
                let changed: Vec<(String, String, String)> = keys
                    .into_iter()
                    .filter(|key| a.get(*key) != b.get(*key))
                    .map(|key| {
                        let show = |v: Option<&String>| v.cloned().unwrap_or_else(|| "null".into());
                        (key.clone(), show(a.get(key)), show(b.get(key)))
                    })
                    .collect();
                if !changed.is_empty() {
                    diffs.push((name.clone(), changed));
                }
            }
            if json {
                let value = serde_json::json!({
                    "configHash1": first,
                    "configHash2": second,
                    "fragmentsDiff": diffs.iter().map(|(name, changed)| serde_json::json!({
                        "name": name,
                        "optionsDiff": changed.iter().map(|(k, a, b)| (k.clone(), serde_json::json!({"first": a, "second": b}))).collect::<serde_json::Map<_, _>>(),
                    })).collect::<Vec<_>>(),
                });
                out = serde_json::to_string(&value).map_err(|e| e.to_string())?;
                out.push('\n');
            } else {
                out.push_str(&format!(
                    "Displaying diff between configs {first} and {second}\n"
                ));
                for (name, changed) in diffs {
                    out.push_str(&format!("FragmentOptions {name} {{\n"));
                    for (key, a, b) in changed {
                        out.push_str(&format!("  {key}: {a}, {b}\n"));
                    }
                    out.push_str("}\n");
                }
            }
        }
        _ => unreachable!("checked above"),
    }
    std::io::stdout()
        .lock()
        .write_all(out.as_bytes())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_loading_profile_is_explicit_and_rejects_invalid_values() {
        use std::ffi::OsStr;
        assert!(!parse_current_loading(None).unwrap());
        for value in ["", "0", "off", "OFF"] {
            assert!(!parse_current_loading(Some(OsStr::new(value))).unwrap());
        }
        for value in ["1", "on", "ON"] {
            assert!(parse_current_loading(Some(OsStr::new(value))).unwrap());
        }
        for value in ["true", "yes", "2", " on ", "auto"] {
            assert!(parse_current_loading(Some(OsStr::new(value))).is_err());
        }
        use std::os::unix::ffi::OsStrExt;
        assert!(parse_current_loading(Some(OsStr::from_bytes(b"\xff"))).is_err());
    }
    #[test]
    fn incremental_graph_encoding_has_identical_frames_and_roundtrips() {
        // Scalar writes, large writes and boundaries inside one string are all
        // exercised, including a final partial frame and the empty sequence.
        for text in [String::new(), "abc".repeat((4 << 20) + 1)] {
            let value = vec![(text, 19u64, Some(vec![1u32, 2, 99]))];
            let raw = postcard::to_allocvec(&value).unwrap();
            let streamed = encode_graph(&value).unwrap();
            assert_eq!(streamed, format::compress_plan(&raw));
            assert_eq!(format::decompress_plan(streamed).unwrap(), raw);
        }
    }

    #[test]
    fn late_invalid_graph_indices_do_not_adopt_a_proved_publication() {
        let fixture = crate::repository_publication::tests::Fixture::new();
        let before = fixture.state();
        let graph = Graph {
            nodes: Vec::new(),
            configurations: Vec::new(),
            top_checksum: String::new(),
            patterns: [("//:bad".to_owned(), vec![1])].into_iter().collect(),
            repository_mapping: Default::default(),
            provider_sets: Vec::new(),
            build_options: Vec::new(),
            fact_errors: Default::default(),
            files_recorded: false,
            build_blocks: Default::default(),
            build_recorded: false,
            transitions: Default::default(),
            transitions_recorded: false,
            fragments: Default::default(),
            fragments_recorded: false,
        };
        let key = key::Key {
            g: [13; 32],
            binary: [29; 32],
        };
        history::save(
            fixture.temp.path(),
            &key,
            format::encode_inputs(&fixture.a, &[], None, false),
            format::encode_events(&event_phases::encode(b"", b"").unwrap()),
            encode_graph(&graph).unwrap(),
            None,
        )
        .unwrap();
        let reader = history::candidates(fixture.temp.path(), &key)
            .into_iter()
            .next()
            .unwrap()
            .unwrap();
        let replay = snapshot::hit::ReplayInputs {
            options: Default::default(),
            environment: Vec::new(),
            downloads_enabled: false,
        };
        let _window = crate::observed_fs::tests_support::record_this_thread();
        let repositories = std::sync::Arc::new(fixture.repositories().unwrap());
        repositories.enable_current_loading();
        repositories.validate_root_module().unwrap();
        let current = crate::repository::CurrentRefresh::seal(repositories, true);
        let error = read_candidate(&reader, &replay, &current, b"", None).unwrap_err();
        assert!(
            error.contains("invalid configured graph pattern index"),
            "{error}"
        );
        assert_eq!(fixture.state(), before);
    }
}
