//! Binary configured-graph snapshots. Shares the observed-input validator and
//! repository replay invariants with build snapshots, never their action payload.
use crate::engine::cquery::Graph;
use crate::snapshot::{self, format, key, store};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

#[path = "cquery_history.rs"]
mod history;

pub struct Session {
    cache: PathBuf,
    key: key::Key,
    capture: Option<snapshot::capture::Capture>,
    verify: Option<(Graph, store::Reader)>,
}

pub struct Started {
    pub hit: Option<Graph>,
    pub session: Option<Session>,
}

fn presentation(option: &crate::bazelrc::Unit) -> bool {
    let name = option.option.text.split('=').next().unwrap_or_default();
    matches!(
        name,
        "--output"
            | "--output_file"
            | "--query_file"
            | "--transitions"
            | "--universe_scope"
            | "--implicit_deps"
            | "--noimplicit_deps"
            | "--tool_deps"
            | "--notool_deps"
            | "--include_aspects"
            | "--noinclude_aspects"
    ) || name.starts_with("--proto:")
        || name.starts_with("--noproto:")
}

pub fn start(
    inputs: key::Inputs<'_>,
    replay: snapshot::hit::ReplayInputs,
) -> Result<Started, String> {
    let miss = || Started {
        hit: None,
        session: None,
    };
    let mode = snapshot::Mode::from_env();
    if matches!(mode, snapshot::Mode::Off | snapshot::Mode::Record) || key::test_hook().is_some() {
        return Ok(miss());
    }
    let _ = std::fs::create_dir_all(store::directory(inputs.cache));
    let Ok(binary) = snapshot::exe_id::binary_id(&store::root(inputs.cache)) else {
        return Ok(miss());
    };
    let options = inputs
        .options
        .iter()
        .filter(|unit| !presentation(unit))
        .cloned()
        .collect::<Vec<_>>();
    let filtered = key::Inputs {
        command: "cquery-graph-v2",
        options: &options,
        ..inputs
    };
    let key = key::compute(&filtered, binary);
    let found = read(inputs.cache, &key, &replay)?;
    let verify = match found {
        Some((graph, reader, events)) if mode != snapshot::Mode::Verify => {
            use std::io::Write;
            // Output errors are command failures, never candidate misses.
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
    if !crate::observed_fs::open_window() {
        return Ok(miss());
    }
    snapshot::replay::install();
    let capture = snapshot::capture::start(snapshot::capture::Mode::Tee).ok();
    Ok(Started {
        hit: None,
        session: Some(Session {
            cache: inputs.cache.to_owned(),
            key,
            capture,
            verify,
        }),
    })
}

fn read(
    cache: &Path,
    key: &key::Key,
    replay: &snapshot::hit::ReplayInputs,
) -> Result<Option<(Graph, store::Reader, Vec<u8>)>, String> {
    let mut reason = "no configured graph snapshot".to_owned();
    for candidate in history::candidates(cache, key) {
        let result = candidate.and_then(|reader| {
            read_candidate(&reader, replay).map(|(graph, events)| (graph, reader, events))
        });
        match result {
            Ok(selected) => return Ok(Some(selected)),
            Err(error) if error == snapshot::replay::CANCELLED => return Err(error),
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
) -> Result<(Graph, Vec<u8>), String> {
    let _version = history::section(reader, format::Section::Stats)?;
    let mut inputs = format::decode_inputs(&history::section(reader, format::Section::Inputs)?)?;
    if inputs.workspace_status {
        return Err("configured graph recorded workspace status execution".into());
    }
    let environment = snapshot::validate::Environment::new(&replay.environment);
    snapshot::validate::validate_for_hit(&inputs.recording, &environment)?;
    if !inputs.replay.is_empty() {
        let cancel = AtomicBool::new(false);
        snapshot::hit::replay_records(inputs.tables.take(), replay, &inputs.replay, &cancel)?;
        snapshot::hit::check_local_trees(&inputs.replay, &cancel, false)?;
    }
    drop(inputs);
    let events = format::decode_events(&history::section(reader, format::Section::Events)?)?;
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
    Ok((graph, events))
}

impl Session {
    pub fn finish(
        mut self,
        graph: &Graph,
        registry: &crate::registry_session::SessionHandle,
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
        let save = || -> Result<(), String> {
            let window = window.ok_or("stderr capture unavailable")?;
            if !window.complete || window.overflowed || window.ended_early {
                return Err("incomplete stderr capture".into());
            }
            if let Some(registry) = registry.session()
                && (registry.updated_lockfile().is_some()
                    || registry.updated_hidden_lockfile().is_some())
            {
                return Err("lockfile write pending".into());
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
                format::encode_events(&window.bytes),
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
    let configurations: Vec<crate::cquery_proto::Configuration> =
        postcard::from_bytes(&bytes[56..]).map_err(|e| e.to_string())?;
    let values: Vec<_> = configurations.into_iter().map(|c| serde_json::json!({
        "skyKey": format!("BuildConfigurationKey[{}]", c.checksum), "configHash": c.checksum,
        "mnemonic": c.mnemonic, "isExec": c.is_tool,
        "fragments": c.fragments.iter().map(|f| serde_json::json!({"name":f.name, "fragmentOptions": f.fragment_option_names})).collect::<Vec<_>>(),
        "fragmentOptions": c.fragment_options.iter().map(|f| serde_json::json!({"name":f.name, "options": f.options.iter().filter_map(|o| Some((o.name.clone()?, o.value.clone()?))).collect::<std::collections::BTreeMap<_,_>>()})).collect::<Vec<_>>()
    })).collect();
    serde_json::to_writer(std::io::stdout().lock(), &values).map_err(|e| e.to_string())?;
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
