//! Direct configured edge ownership, not merely transitive node equality.
#![allow(clippy::disallowed_methods)]
use bazel_rs::{
    configuration::{Configuration, NativeOptions},
    cquery_proto::{Target, Transitions},
    engine::{
        Engine,
        cquery::{Graph, Options},
    },
    label::Label,
    reapi::{self, Value},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::{Command, Output},
};

const ROOTS: &str = "set(//:requester //:twin //:plain //:via_alias //:via_decorated)";
const DEPS: &str = "deps(set(//:requester //:twin //:plain //:via_alias //:via_decorated))";
const FLAGS: &[&str] = &[
    "--experimental-builtin-modules=false",
    "--lockfile_mode=off",
    "--platforms=//:platform",
    "--host_platform=//:platform",
    "--define=branch=one",
    "--define=aspect=one",
    "--extra_toolchains=//:tc",
];
const FILTERS: &[&[&str]] = &[
    &[],
    &["--noinclude_aspects"],
    &["--noimplicit_deps"],
    &["--notool_deps"],
    &["--aspects=//:defs.bzl%closure"],
    &[],
    &["--notool_deps"],
    &["--notool_deps", "--universe_scope=//:exec_requester"],
];

struct Workspace(tempfile::TempDir);
impl Workspace {
    fn new() -> Self {
        let ws = Self(tempfile::tempdir().unwrap());
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/cquery_aspect_edges/workspace");
        for entry in fs::read_dir(fixture).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), ws.0.path().join(entry.file_name())).unwrap();
        }
        ws
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        cmd.current_dir(self.0.path())
            .env("BAZEL_RS_SNAPSHOT", "on")
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .args(["--ignore_all_rc_files", "--cache-dir=cache", "cquery"])
            .args(FLAGS)
            .args(args);
        cmd
    }
    fn ok(&self, args: &[&str]) -> Output {
        success(self.command(args).output().unwrap())
    }
    fn graph(&self) -> Graph {
        let platform = Label::parse_canonical("//:platform").unwrap();
        let mut engine = Engine::new(self.0.path().to_owned());
        engine.set_configuration(Configuration::new(
            NativeOptions {
                platforms: vec![platform.clone()],
                host_platform: platform,
                define: vec![
                    ("branch".into(), "one".into()),
                    ("aspect".into(), "one".into()),
                ],
                extra_toolchains: vec!["//:tc".into()],
                ..NativeOptions::default()
            },
            BTreeMap::new(),
        ));
        engine
            .cquery_graph(
                &[
                    "//:requester",
                    "//:twin",
                    "//:plain",
                    "//:via_alias",
                    "//:via_decorated",
                ]
                .map(str::to_owned),
                "",
            )
            .unwrap()
    }
}
fn success(out: Output) -> Output {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}
fn expected(text: &str, case: usize) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| {
            let (tuple, cases) = line.rsplit_once(' ').unwrap();
            cases
                .contains(char::from(b'0' + case as u8))
                .then(|| tuple.to_owned())
        })
        .collect()
}
fn expected_edges(case: usize) -> BTreeSet<String> {
    expected(
        if case < 5 {
            include_str!("fixtures/cquery_aspect_edges/edges.txt")
        } else {
            include_str!("fixtures/cquery_aspect_edges/exec-edges.txt")
        },
        case,
    )
}
fn expected_nodes(case: usize) -> BTreeSet<String> {
    expected(
        if case < 5 {
            include_str!("fixtures/cquery_aspect_edges/nodes.txt")
        } else {
            include_str!("fixtures/cquery_aspect_edges/exec-nodes.txt")
        },
        case,
    )
}
fn graph_config(graph: &Graph, node: usize) -> &str {
    let node = &graph.nodes[node];
    if matches!(&node.target, Target::Rule(rule) if rule.rule_class == "platform") {
        "platform"
    } else {
        node.configuration.map_or("null", |id| {
            if graph.configurations[id as usize].is_tool {
                "exec"
            } else {
                "target"
            }
        })
    }
}

#[test]
fn typed_graph_preserves_requesters_required_paths_aliases_and_toolchains() {
    let ws = Workspace::new();
    let graph = ws.graph();
    // The full graph also stores independently analyzed, unreachable nodes.
    // Pin every edge of every reachable base/requester instead of equating just
    // the closures. No base may absorb an aspect-only witness/tool/condition.
    let expected = expected_edges(0);
    let owners: BTreeSet<_> = expected
        .iter()
        .map(|edge| edge.split_whitespace().next().unwrap())
        .collect();
    let mut actual = BTreeSet::new();
    for (id, node) in graph.nodes.iter().enumerate() {
        if !owners.contains(node.label()) {
            continue;
        }
        for edge in &node.edges {
            let target = &graph.nodes[edge.to as usize];
            actual.insert(format!(
                "{} {} {} {}",
                node.label(),
                graph_config(&graph, id),
                target.label(),
                graph_config(&graph, edge.to as usize)
            ));
            if edge.aspect {
                assert!(edge.implicit);
                assert!(
                    edge.attribute.is_empty(),
                    "aspect projection must not invent rule attrs"
                );
            }
        }
    }
    assert_eq!(actual, expected);
    let bytes = postcard::to_stdvec(&graph).unwrap();
    assert_eq!(graph, postcard::from_bytes::<Graph>(&bytes).unwrap());
    // Both formatters consume this same typed graph.
    for (case, flags) in FILTERS[..4].iter().enumerate() {
        let mut options = Options::default();
        options.common.output = "proto".into();
        options.writer.transitions = Transitions::Lite;
        for flag in *flags {
            options.parse(flag, None).unwrap();
        }
        let mut bytes = Vec::new();
        graph
            .write(&options.expression(DEPS).unwrap(), &options, "", &mut bytes)
            .unwrap();
        assert_wire(&bytes, false, case);
    }
}

// Field numbers below are pinned from Bazel 9.3 analysis_v2.proto/build.proto,
// independently checked with fixtures/cquery_proto/schema.pb during capture.
fn messages(bytes: &[u8], number: u64) -> Vec<&[u8]> {
    reapi::fields(bytes)
        .unwrap()
        .into_iter()
        .filter_map(|(n, value)| match value {
            Value::Bytes(bytes) if n == number => Some(bytes),
            _ => None,
        })
        .collect()
}
fn text(bytes: &[u8], number: u64) -> String {
    String::from_utf8(
        messages(bytes, number)
            .first()
            .copied()
            .unwrap_or_default()
            .to_vec(),
    )
    .unwrap()
}
fn number(bytes: &[u8], field: u64) -> u64 {
    reapi::fields(bytes)
        .unwrap()
        .into_iter()
        .find_map(|(n, value)| match value {
            Value::Varint(value) if n == field => Some(value),
            _ => None,
        })
        .unwrap_or(0)
}
fn frames(mut bytes: &[u8]) -> Vec<&[u8]> {
    let mut result = Vec::new();
    while !bytes.is_empty() {
        let (mut length, mut shift) = (0, 0);
        loop {
            let byte = bytes[0];
            bytes = &bytes[1..];
            length |= usize::from(byte & 127) << shift;
            if byte < 128 {
                break;
            }
            shift += 7;
            assert!(shift < 64);
        }
        let (frame, rest) = bytes.split_at(length);
        result.push(frame);
        bytes = rest;
    }
    result
}
fn assert_wire(bytes: &[u8], streamed: bool, case: usize) {
    let frames = if streamed { frames(bytes) } else { vec![bytes] };
    let configs: BTreeMap<_, _> = frames
        .iter()
        .flat_map(|f| messages(f, 2))
        .map(|c| (number(c, 1), number(c, 5) != 0))
        .collect();
    let targets: Vec<_> = frames.iter().flat_map(|f| messages(f, 1)).collect();
    let platforms: BTreeSet<_> = targets
        .iter()
        .flat_map(|t| messages(t, 1))
        .flat_map(|t| messages(t, 2))
        .filter(|r| text(r, 2) == "platform")
        .map(|r| text(r, 1))
        .collect();
    let config = |label: &str, id| {
        if platforms.contains(label) {
            "platform"
        } else if id == 0 {
            "null"
        } else if configs[&id] {
            "exec"
        } else {
            "target"
        }
    };
    let (mut nodes, mut edges) = (BTreeSet::new(), BTreeSet::new());
    for entry in targets {
        let target = messages(entry, 1)[0];
        let kind = number(target, 1);
        let payload = messages(target, if kind == 1 { 2 } else { 3 })[0];
        let label = text(payload, 1);
        let cfg = config(&label, number(entry, 3));
        nodes.insert(format!("{label} {cfg} {kind}"));
        if kind == 1 {
            for dep in messages(payload, 15) {
                let to = text(dep, 1);
                let to_cfg = config(&to, number(dep, 3));
                if to_cfg == "null" {
                    assert!(messages(dep, 2).is_empty());
                }
                edges.insert(format!("{label} {cfg} {to} {to_cfg}"));
            }
        }
    }
    assert_eq!(nodes, expected_nodes(case), "case {case} nodes");
    assert_eq!(
        edges,
        expected_edges(case),
        "case {case} direct configured edges"
    );
}

#[test]
fn proto_filters_and_formatted_deps_share_complete_snapshot_capture() {
    let ws = Workspace::new();
    // First capture with aspects HIDDEN. Later enabling aspects must hit the
    // same complete graph, never a presentation-dependent analysis snapshot.
    let first = ws.ok(&["--noinclude_aspects", DEPS]);
    assert!(String::from_utf8_lossy(&first.stderr).contains("snapshot: saved"));
    for (case, filters) in FILTERS.iter().enumerate() {
        let expression = match case {
            4 | 7 => "deps(//:branch)",
            5 | 6 => "deps(//:exec_requester)",
            _ => DEPS,
        };
        for format in ["--output=proto", "--output=streamed_proto"] {
            let mut args = vec![format, "--transitions=lite"];
            args.extend_from_slice(filters);
            args.push(expression);
            let out = ws.ok(&args);
            assert_wire(&out.stdout, format.ends_with("streamed_proto"), case);
            if !(4..=5).contains(&case) {
                assert!(String::from_utf8_lossy(&out.stderr).contains("snapshot: hit"));
            }
        }
        let mut args = filters.to_vec();
        args.push(expression);
        let out = ws.ok(&args);
        let labels: BTreeSet<_> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|line| line.split_whitespace().next().unwrap().to_owned())
            .collect();
        let expected: BTreeSet<_> = expected_nodes(case)
            .iter()
            .map(|node| node.split_whitespace().next().unwrap().to_owned())
            .collect();
        assert_eq!(labels, expected);
    }
    let verified = success(
        ws.command(&["--output=proto", "--transitions=lite", DEPS])
            .env("BAZEL_RS_SNAPSHOT", "verify")
            .output()
            .unwrap(),
    );
    assert_wire(&verified.stdout, false, 0);
    let roots = ws.ok(&[ROOTS]);
    assert!(String::from_utf8_lossy(&roots.stderr).contains("snapshot: hit"));
}
