//! Bounded native DAG execution over the action cache.
//!
//! Graph structure is checked before any action (or cache I/O). A failed or
//! panicking worker stops new scheduling; all in-flight workers are joined.
//! Cancellation does not kill processes: an already-running command, including
//! one blocked forever, must finish before this call can return. The action
//! executor's unsandboxed-command and cache trust limitations still apply.

use crate::action::{self, Action, ActionResult};
use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;

#[derive(Clone, Debug)]
pub struct Dependency {
    pub action_id: String,
    /// Index in the producer's declared outputs, not sorted output order.
    pub output_index: usize,
    pub destination: PathBuf,
}

#[derive(Clone, Debug)]
pub struct PlannedAction {
    /// Inputs here are filesystem sources; generated inputs go in dependencies.
    pub action: crate::action::Action,
    pub dependencies: Vec<Dependency>,
}

struct Graph {
    nodes: BTreeMap<String, PlannedAction>,
    remaining: BTreeMap<String, usize>,
    consumers: BTreeMap<String, Vec<String>>,
    ready: BTreeSet<String>,
}

fn validate(nodes: Vec<PlannedAction>, jobs: usize) -> Result<Graph, String> {
    if jobs == 0 {
        return Err("scheduler jobs must be greater than zero".into());
    }
    let mut indexed = BTreeMap::new();
    for node in nodes {
        let id = node.action.id.clone();
        if indexed.insert(id.clone(), node).is_some() {
            return Err(format!("action '{id}': duplicate action id"));
        }
    }
    let mut remaining = BTreeMap::new();
    let mut consumers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, node) in &indexed {
        let mut synthetic = node.action.clone();
        for dependency in &node.dependencies {
            let producer = indexed.get(&dependency.action_id).ok_or_else(|| {
                format!(
                    "action '{id}': unknown dependency '{}'",
                    dependency.action_id
                )
            })?;
            if dependency.output_index >= producer.action.outputs.len() {
                return Err(format!(
                    "action '{id}': dependency '{}' output index {} is out of range ({} outputs)",
                    dependency.action_id,
                    dependency.output_index,
                    producer.action.outputs.len()
                ));
            }
            // validate_structure inspects destinations only, never these paths.
            synthetic.inputs.push((
                PathBuf::from("__scheduler_generated_source__"),
                dependency.destination.clone(),
            ));
            consumers
                .entry(dependency.action_id.clone())
                .or_default()
                .push(id.clone());
        }
        action::validate_structure(&synthetic).map_err(|e| format!("action '{id}': {e}"))?;
        remaining.insert(id.clone(), node.dependencies.len());
    }
    let ready: BTreeSet<_> = remaining
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(id, _)| id.clone())
        .collect();
    // Kahn's algorithm avoids recursion even for very deep generated graphs.
    // Multiple outputs from one producer are counted/decremented separately.
    let mut pending = remaining.clone();
    let mut visit = ready.clone();
    let mut visited = 0;
    while let Some(id) = visit.pop_first() {
        visited += 1;
        if let Some(children) = consumers.get(&id) {
            for child in children {
                let count = pending.get_mut(child).expect("known consumer");
                *count -= 1;
                if *count == 0 {
                    visit.insert(child.clone());
                }
            }
        }
    }
    if visited != indexed.len() {
        let blocked: Vec<_> = pending
            .iter()
            .filter(|(_, count)| **count != 0)
            .map(|(id, _)| format!("'{id}'"))
            .collect();
        return Err(format!(
            "dependency cycle: actions in or blocked by a cycle: {}",
            blocked.join(", ")
        ));
    }
    Ok(Graph {
        nodes: indexed,
        remaining,
        consumers,
        ready,
    })
}

/// Execute a fully validated graph with at most `jobs` concurrent actions.
///
/// Results are keyed deterministically by action id. Generated inputs use the
/// producer's actual published paths and declaration-order output indices.
/// Duplicate input destinations follow `action::validate_structure` semantics:
/// their contents/modes must agree at execution time. Source existence and
/// contents are likewise checked by execution, not by structural validation.
/// Failures include action ids; failures from all in-flight workers are reported.
/// No new work starts after failure is observed; running processes are not killed.
pub fn execute_graph(
    cache: &Path,
    nodes: Vec<PlannedAction>,
    jobs: usize,
) -> Result<BTreeMap<String, crate::action::ActionResult>, String> {
    execute_with(cache, nodes, jobs, &action::execute)
}

// The worker seam allows concurrency/lifecycle tests without sleeps or shell
// timing assumptions. Production always delegates to action::execute unchanged,
// retaining its atomic per-key fs2 cache locking.
fn execute_with<F>(
    cache: &Path,
    nodes: Vec<PlannedAction>,
    jobs: usize,
    execute: &F,
) -> Result<BTreeMap<String, ActionResult>, String>
where
    F: Fn(&Path, &Action) -> Result<ActionResult, String> + Sync,
{
    let mut graph = validate(nodes, jobs)?;
    let failed = AtomicBool::new(false);
    let mut results: BTreeMap<String, ActionResult> = BTreeMap::new();
    let mut errors = BTreeMap::new();
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel();
        let mut running = BTreeMap::new();
        loop {
            while running.len() < jobs && !failed.load(Ordering::Acquire) {
                let Some(id) = graph.ready.pop_first() else {
                    break;
                };
                let mut node = graph.nodes.remove(&id).expect("ready node exists");
                for dependency in node.dependencies {
                    let producer = &results[&dependency.action_id];
                    node.action.inputs.push((
                        producer.outputs[dependency.output_index].clone(),
                        dependency.destination,
                    ));
                }
                let worker_sender = sender.clone();
                let worker_id = id.clone();
                let failed = &failed;
                let spawn = thread::Builder::new().spawn_scoped(scope, move || {
                    // A failure can arrive while preparing or spawning this
                    // worker. Do not invoke another action in that case.
                    if failed.load(Ordering::Acquire) {
                        let _ = worker_sender.send((worker_id, None));
                        return;
                    }
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        let result = execute(cache, &node.action)?;
                        if result.outputs.len() != node.action.outputs.len() {
                            return Err(format!(
                                "executor returned {} outputs, expected {}",
                                result.outputs.len(),
                                node.action.outputs.len()
                            ));
                        }
                        Ok(result)
                    }))
                    .unwrap_or_else(|payload| {
                        let message = payload
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| payload.downcast_ref::<&str>().copied())
                            .unwrap_or("non-string panic payload");
                        Err(format!("worker panicked: {message}"))
                    });
                    if result.is_err() {
                        // Publish cancellation before sending, so another ready
                        // node cannot slip through behind a queued failure.
                        failed.store(true, Ordering::Release);
                    }
                    let _ = worker_sender.send((worker_id, Some(result)));
                });
                match spawn {
                    Ok(handle) => {
                        running.insert(id, handle);
                    }
                    Err(error) => {
                        failed.store(true, Ordering::Release);
                        errors.insert(id, format!("cannot spawn worker: {error}"));
                    }
                }
            }
            if running.is_empty() {
                break;
            }
            // Every spawned worker sends one completion, including caught
            // panics and workers skipped on cancellation. Join promptly rather
            // than retaining one OS thread handle per node until scope exit.
            let (id, outcome) = receiver.recv().expect("workers send completion");
            let handle = running.remove(&id).expect("completion from running worker");
            handle.join().expect("worker panics are caught");
            match outcome {
                Some(Ok(result)) => {
                    results.insert(id.clone(), result);
                    if let Some(children) = graph.consumers.get(&id) {
                        for child in children {
                            let count = graph.remaining.get_mut(child).expect("known consumer");
                            *count -= 1;
                            if *count == 0 {
                                graph.ready.insert(child.clone());
                            }
                        }
                    }
                }
                Some(Err(error)) => {
                    errors.insert(id, error);
                }
                None => {}
            }
        }
    });
    if errors.is_empty() {
        Ok(results)
    } else {
        Err(errors
            .into_iter()
            .map(|(id, error)| format!("action '{id}': {error}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::time::Duration;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
    const TIMEOUT: Duration = Duration::from_secs(20);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            loop {
                let path = std::env::temp_dir().join(format!(
                    "native-scheduler-test-{}-{}",
                    std::process::id(),
                    NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(fs::canonicalize(path).unwrap()),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("{e}"),
                }
            }
        }

        fn cache(&self) -> PathBuf {
            self.0.join("cache")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn node(id: &str) -> PlannedAction {
        PlannedAction {
            action: Action {
                id: id.into(),
                command: "printf ok > out".into(),
                inputs: vec![],
                outputs: vec!["out".into()],
                write: None,
                program: None,
                arguments: Vec::new(),
                environment: BTreeMap::new(),
                shell_strict: true,
            },
            dependencies: vec![],
        }
    }

    fn dependency(id: &str, index: usize, destination: &str) -> Dependency {
        Dependency {
            action_id: id.into(),
            output_index: index,
            destination: destination.into(),
        }
    }

    fn success(action: &Action) -> Result<ActionResult, String> {
        Ok(ActionResult {
            outputs: action.outputs.clone(),
            cache_hit: false,
        })
    }

    #[test]
    fn native_write_executable_feeds_direct_process() {
        let fixture = Fixture::new();
        let mut producer = node("producer");
        producer.action.command.clear();
        producer.action.write = Some(action::WriteAction {
            content: b"#!/bin/sh\nfalse\nprintf '%s:%s' \"$1\" \"$VALUE\" > out\n".to_vec(),
            executable: true,
        });
        let mut consumer = node("consumer");
        consumer.action.command.clear();
        consumer.action.program = Some("./generated tool".into());
        consumer.action.arguments = vec!["literal $1".into()];
        consumer
            .action
            .environment
            .insert("VALUE".into(), "雪".into());
        consumer.dependencies = vec![dependency("producer", 0, "generated tool")];
        let graph = vec![consumer, producer];
        let first = execute_graph(&fixture.cache(), graph.clone(), 2).unwrap();
        assert_eq!(
            fs::read(&first["consumer"].outputs[0]).unwrap(),
            "literal $1:雪".as_bytes()
        );
        assert!(first.values().all(|r| !r.cache_hit));
        assert!(
            execute_graph(&fixture.cache(), graph, 2)
                .unwrap()
                .values()
                .all(|r| r.cache_hit)
        );
    }

    #[test]
    fn invalid_direct_program_rejected_before_any_action() {
        let mut direct = node("direct");
        direct.action.program = Some("/bin/true".into());
        invalid(vec![direct.clone()], 2, "empty shell command");
        direct.action.command.clear();
        direct.action.program = Some(String::new());
        invalid(vec![direct], 2, "direct program must be nonempty");
    }

    #[test]
    fn native_write_producer_feeds_shell_arguments_and_environment() {
        let fixture = Fixture::new();
        let mut producer = node("producer");
        producer.action.command.clear();
        producer.action.write = Some(action::WriteAction {
            content: b"native\0bytes".to_vec(),
            executable: false,
        });
        let mut consumer = node("consumer");
        consumer.dependencies = vec![dependency("producer", 0, "generated")];
        consumer.action.command =
            "cat generated > out; printf '%s:%s' \"$1\" \"$VALUE\" >> out".into();
        consumer.action.arguments = vec!["literal $1".into()];
        consumer
            .action
            .environment
            .insert("VALUE".into(), "雪".into());
        let graph = vec![consumer, producer];
        let first = execute_graph(&fixture.cache(), graph.clone(), 2).unwrap();
        assert!(first.values().all(|r| !r.cache_hit));
        assert_eq!(
            fs::read(&first["consumer"].outputs[0]).unwrap(),
            "native\0bytesliteral $1:雪".as_bytes()
        );
        assert!(
            execute_graph(&fixture.cache(), graph, 2)
                .unwrap()
                .values()
                .all(|r| r.cache_hit)
        );
    }

    #[test]
    fn write_generated_inputs_and_invalid_shell_values_rejected_before_any_action() {
        let mut write = node("write");
        write.action.command.clear();
        write.action.write = Some(action::WriteAction {
            content: vec![0],
            executable: false,
        });
        write.dependencies = vec![dependency("producer", 0, "generated")];
        invalid(vec![node("producer"), write], 2, "write action");
        let mut shell = node("shell");
        shell.action.arguments.push("bad\0argument".into());
        invalid(vec![shell.clone()], 2, "argument contains a NUL");
        shell.action.arguments.clear();
        shell
            .action
            .environment
            .insert("bad=key".into(), "value".into());
        invalid(vec![shell], 2, "environment key");
    }

    #[test]
    fn chain_fanout_fanin_and_repeated_graph_cache_hits() {
        let fixture = Fixture::new();
        let source = fixture.0.join("source");
        fs::write(&source, "hello").unwrap();
        let mut root = node("root");
        root.action.inputs.push((source, "src".into()));
        // Deliberately reverse lexical order to test declaration-order indices.
        root.action.outputs = vec!["z".into(), "a".into()];
        root.action.command = "cat src > z; printf second > a".into();
        let mut left = node("left");
        left.dependencies = vec![dependency("root", 0, "generated")];
        left.action.command = "cat generated > out".into();
        let mut right = node("right");
        right.dependencies = vec![
            dependency("root", 1, "first"),
            dependency("root", 0, "second"),
        ];
        right.action.command = "cat first second > out".into();
        let mut end = node("end");
        end.dependencies = vec![dependency("left", 0, "l"), dependency("right", 0, "r")];
        end.action.command = "cat l r > out".into();
        let graph = vec![end, right, root, left];
        let first = execute_graph(&fixture.cache(), graph.clone(), 3).unwrap();
        assert_eq!(
            first.keys().map(String::as_str).collect::<Vec<_>>(),
            ["end", "left", "right", "root"]
        );
        assert!(first.values().all(|r| !r.cache_hit));
        assert_eq!(
            fs::read(&first["end"].outputs[0]).unwrap(),
            b"hellosecondhello"
        );
        let second = execute_graph(&fixture.cache(), graph, 2).unwrap();
        assert!(second.values().all(|r| r.cache_hit));
        for (id, result) in first {
            assert_eq!(result.outputs, second[&id].outputs);
        }
    }

    fn invalid(graph: Vec<PlannedAction>, jobs: usize, expected: &str) {
        let fixture = Fixture::new();
        let marker = fixture.0.join("ran");
        let mut independent = node("000-independent");
        independent.action.command = format!("touch '{}'; printf ok > out", marker.display());
        let mut graph = graph;
        graph.insert(0, independent);
        let error = execute_graph(&fixture.cache(), graph, jobs).unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert!(
            !fixture.cache().exists(),
            "validation created cache: {error}"
        );
        assert!(
            !marker.exists(),
            "validation ran independent action: {error}"
        );
    }

    #[test]
    fn validates_ids_edges_indices_cycles_and_jobs_before_any_action() {
        invalid(vec![node("duplicate"), node("duplicate")], 4, "duplicate");
        let mut a = node("a");
        a.dependencies.push(dependency("missing", 0, "input"));
        invalid(
            vec![a.clone()],
            4,
            "action 'a': unknown dependency 'missing'",
        );
        a.dependencies = vec![dependency("b", 1, "input")];
        invalid(vec![a.clone(), node("b")], 4, "output index 1");
        a.dependencies = vec![dependency("a", 0, "input")];
        invalid(vec![a.clone()], 4, "cycle");
        a.dependencies = vec![dependency("b", 0, "input")];
        let mut b = node("b");
        b.dependencies = vec![dependency("a", 0, "input")];
        invalid(vec![a, b], 4, "'a', 'b'");
        invalid(vec![node("a")], 0, "jobs must be greater than zero");
    }

    #[test]
    fn validates_complete_generated_and_source_namespaces_before_any_action() {
        for destination in ["/absolute", "../escape", "a//b", "", "out", "out/child"] {
            let mut consumer = node("consumer");
            consumer.dependencies = vec![dependency("producer", 0, destination)];
            invalid(vec![node("producer"), consumer], 4, "action 'consumer'");
        }
        let mut consumer = node("consumer");
        consumer
            .action
            .inputs
            .push(("does-not-exist".into(), "src".into()));
        consumer.dependencies = vec![dependency("producer", 0, "src/child")];
        invalid(
            vec![node("producer"), consumer.clone()],
            4,
            "overlapping input",
        );
        consumer.action.inputs.clear();
        consumer.dependencies = vec![
            dependency("producer", 0, "src"),
            dependency("producer", 0, "src/child"),
        ];
        invalid(vec![node("producer"), consumer], 4, "overlapping input");
        let mut malformed = node("malformed");
        malformed.action.outputs = vec!["parent".into(), "parent/child".into()];
        invalid(vec![malformed.clone()], 4, "action 'malformed'");
        malformed.action.outputs = vec!["../escape".into()];
        invalid(vec![malformed.clone()], 4, "unsafe mapped path");
        malformed.action.outputs.clear();
        malformed.action.command = "true\0".into();
        invalid(vec![malformed], 4, "NUL");
    }

    #[test]
    fn independent_workers_overlap_and_never_exceed_jobs() {
        // Controller-driven waves prove overlap without measuring wall time.
        // Timeouts only prevent a broken scheduler from hanging the test suite.
        for jobs in [1, 2, 4] {
            let active = AtomicUsize::new(0);
            let maximum = AtomicUsize::new(0);
            let (started_tx, started_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let release_rx = Mutex::new(release_rx);
            let worker = |_: &Path, action: &Action| {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(count, Ordering::SeqCst);
                started_tx.send(action.id.clone()).unwrap();
                release_rx.lock().unwrap().recv_timeout(TIMEOUT).unwrap();
                active.fetch_sub(1, Ordering::SeqCst);
                success(action)
            };
            thread::scope(|scope| {
                let handle = scope.spawn(|| {
                    let graph = (0..12).map(|i| node(&format!("node-{i:02}"))).collect();
                    execute_with(Path::new("unused"), graph, jobs, &worker)
                });
                for _ in 0..12 / jobs {
                    for _ in 0..jobs {
                        started_rx.recv_timeout(TIMEOUT).unwrap();
                    }
                    assert_eq!(active.load(Ordering::SeqCst), jobs);
                    for _ in 0..jobs {
                        release_tx.send(()).unwrap();
                    }
                }
                assert_eq!(handle.join().unwrap().unwrap().len(), 12);
            });
            assert_eq!(maximum.load(Ordering::SeqCst), jobs);
            assert_eq!(active.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn failed_dependency_and_queued_independent_action_never_start() {
        let fixture = Fixture::new();
        let marker = fixture.0.join("dependent-ran");
        let mut bad = node("a-fail");
        bad.action.command = "echo intentional-failure >&2; exit 7".into();
        let mut dependent = node("dependent");
        dependent.dependencies = vec![dependency("a-fail", 0, "input")];
        dependent.action.command = format!("touch '{}'; cat input > out", marker.display());
        let mut queued = node("z-queued");
        queued.action.command = dependent.action.command.clone();
        let error = execute_graph(&fixture.cache(), vec![dependent, queued, bad], 1).unwrap_err();
        assert!(error.contains("a-fail") && error.contains("intentional-failure"));
        assert!(!marker.exists());
    }

    #[test]
    fn failure_waits_for_running_actions_and_collects_their_errors() {
        let (started_tx, started_rx) = mpsc::channel();
        let (fail_tx, fail_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let fail_rx = Mutex::new(fail_rx);
        let finish_rx = Mutex::new(finish_rx);
        let finished = AtomicBool::new(false);
        let worker = |_: &Path, action: &Action| {
            started_tx.send(action.id.clone()).unwrap();
            match action.id.as_str() {
                "a-fail" => {
                    fail_rx.lock().unwrap().recv_timeout(TIMEOUT).unwrap();
                    Err("first failure".into())
                }
                "b-running" => {
                    finish_rx.lock().unwrap().recv_timeout(TIMEOUT).unwrap();
                    finished.store(true, Ordering::SeqCst);
                    Err("in-flight failure".into())
                }
                _ => panic!("unexpected new action after failure"),
            }
        };
        thread::scope(|scope| {
            let (done_tx, done_rx) = mpsc::channel();
            let worker = &worker;
            let finished = &finished;
            let handle = scope.spawn(move || {
                let mut dependent = node("dependent");
                dependent.dependencies = vec![dependency("a-fail", 0, "input")];
                let result = execute_with(
                    Path::new("unused"),
                    vec![
                        node("a-fail"),
                        node("b-running"),
                        dependent,
                        node("z-queued"),
                    ],
                    2,
                    worker,
                );
                assert!(
                    finished.load(Ordering::SeqCst),
                    "returned before in-flight work ended"
                );
                done_tx.send(()).unwrap();
                result
            });
            let started: BTreeSet<_> = (0..2)
                .map(|_| started_rx.recv_timeout(TIMEOUT).unwrap())
                .collect();
            assert_eq!(
                started,
                BTreeSet::from(["a-fail".into(), "b-running".into()])
            );
            fail_tx.send(()).unwrap();
            assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
            finish_tx.send(()).unwrap();
            let error = handle.join().unwrap().unwrap_err();
            assert!(error.contains("action 'a-fail': first failure"), "{error}");
            assert!(
                error.contains("action 'b-running': in-flight failure"),
                "{error}"
            );
            assert!(started_rx.try_recv().is_err());
        });
    }

    #[test]
    fn panicking_worker_returns_actionable_error_without_hanging() {
        for string_payload in [true, false] {
            let calls = AtomicUsize::new(0);
            let worker = |_: &Path, _: &Action| -> Result<ActionResult, String> {
                calls.fetch_add(1, Ordering::SeqCst);
                if string_payload {
                    panic!("intentional panic");
                }
                std::panic::panic_any(42_u32);
            };
            let mut dependent = node("dependent");
            dependent.dependencies = vec![dependency("a-panic", 0, "input")];
            let error = execute_with(
                Path::new("unused"),
                vec![dependent, node("a-panic"), node("z-queued")],
                1,
                &worker,
            )
            .unwrap_err();
            assert!(
                error.contains("action 'a-panic': worker panicked"),
                "{error}"
            );
            assert!(error.contains(if string_payload {
                "intentional panic"
            } else {
                "non-string panic payload"
            }));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn empty_graph_does_not_create_cache_and_zero_jobs_is_still_invalid() {
        let fixture = Fixture::new();
        assert!(
            execute_graph(&fixture.cache(), vec![], 2)
                .unwrap()
                .is_empty()
        );
        assert!(execute_graph(&fixture.cache(), vec![], 0).is_err());
        assert!(!fixture.cache().exists());
    }

    #[test]
    fn duplicate_generated_destinations_use_existing_action_content_rules() {
        let fixture = Fixture::new();
        let mut consumer = node("consumer");
        consumer.dependencies = vec![
            dependency("producer", 0, "input"),
            dependency("producer", 0, "input"),
        ];
        consumer.action.command = "cat input > out".into();
        let result = execute_graph(&fixture.cache(), vec![consumer, node("producer")], 2).unwrap();
        assert_eq!(fs::read(&result["consumer"].outputs[0]).unwrap(), b"ok");
    }
}
