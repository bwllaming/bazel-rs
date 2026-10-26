//! Binary configured-graph snapshots. Shares the observed-input validator and
//! repository replay invariants with build snapshots, never their action payload.
use crate::engine::cquery::Graph;
use crate::snapshot::{self, format, key, store};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

pub struct Session {
    cache: PathBuf,
    key: key::Key,
    capture: Option<snapshot::capture::Capture>,
    verify: Option<Graph>,
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

pub fn start(inputs: key::Inputs<'_>, replay: snapshot::hit::ReplayInputs) -> Started {
    let miss = || Started {
        hit: None,
        session: None,
    };
    let mode = snapshot::Mode::from_env();
    if matches!(mode, snapshot::Mode::Off | snapshot::Mode::Record) || key::test_hook().is_some() {
        return miss();
    }
    let _ = std::fs::create_dir_all(store::directory(inputs.cache));
    let Ok(binary) = snapshot::exe_id::binary_id(&store::root(inputs.cache)) else {
        return miss();
    };
    let options = inputs
        .options
        .iter()
        .filter(|unit| !presentation(unit))
        .cloned()
        .collect::<Vec<_>>();
    let filtered = key::Inputs {
        command: "cquery-graph-v1",
        options: &options,
        ..inputs
    };
    let key = key::compute(&filtered, binary);
    let found = read(inputs.cache, &key, &replay);
    let verify = match found {
        Ok(graph) if mode != snapshot::Mode::Verify => {
            explain("hit");
            return Started {
                hit: Some(graph),
                session: None,
            };
        }
        Ok(graph) => Some(graph),
        Err(error) => {
            explain(&format!("miss: {error}"));
            None
        }
    };
    if !crate::observed_fs::open_window() {
        return miss();
    }
    snapshot::replay::install();
    let capture = snapshot::capture::start(snapshot::capture::Mode::Tee).ok();
    Started {
        hit: None,
        session: Some(Session {
            cache: inputs.cache.to_owned(),
            key,
            capture,
            verify,
        }),
    }
}

fn read(
    cache: &Path,
    key: &key::Key,
    replay: &snapshot::hit::ReplayInputs,
) -> Result<Graph, String> {
    let reader = store::Reader::open(cache, key)?.ok_or("no configured graph snapshot")?;
    let inputs = format::decode_inputs(&reader.section(format::Section::Inputs)?)?;
    if inputs.workspace_status {
        return Err("configured graph recorded workspace status execution".into());
    }
    let environment = snapshot::validate::Environment::new(&replay.environment);
    snapshot::validate::validate_for_hit(&inputs.recording, &environment)?;
    if !inputs.replay.is_empty() {
        let cancel = AtomicBool::new(false);
        snapshot::hit::replay_records(inputs.tables, replay, &inputs.replay, &cancel)?;
        snapshot::hit::check_local_trees(&inputs.replay, &cancel, false)?;
    }
    let bytes = reader.section(format::Section::Plan)?;
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
    let events = format::decode_events(&reader.section(format::Section::Events)?)?;
    use std::io::Write;
    std::io::stderr()
        .lock()
        .write_all(&crate::ui::filtered(&events))
        .map_err(|e| e.to_string())?;
    reader.touch();
    Ok(graph)
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
        if let Some(expected) = self.verify.take()
            && expected != *graph
        {
            let _ = store::remove(&self.cache, &self.key);
            return Err(
                "configured graph snapshot verification mismatch; stale snapshot removed".into(),
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
            let inputs = format::encode_inputs(&recording, &records, tables.as_ref(), false);
            // Once, not a round-trip or a JSON intermediate. Compression drops
            // its uncompressed buffer before the atomic store hand-off.
            let bytes = postcard::to_allocvec(graph).map_err(|e| e.to_string())?;
            let plan = format::compress_plan(&bytes);
            drop(bytes);
            store::write_in_background(
                &self.cache,
                &self.key,
                vec![
                    (format::Section::Inputs, inputs),
                    (
                        format::Section::Events,
                        format::encode_events(&window.bytes),
                    ),
                    (format::Section::Plan, plan),
                ],
            )
            .commit()?;
            explain("saved");
            Ok(())
        };
        if let Err(error) = save() {
            explain(&format!("not saved: {error}"));
        }
        Ok(())
    }
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
