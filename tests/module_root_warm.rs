//! Module roots fetched ahead of their sequential demand
//! (docs/PARALLEL_ANALYSIS.md, "Module roots fetched ahead";
//! src/module_root_warm.rs): the registry module roots a file's `load()`
//! statements (transitively) and the registered toolchain patterns name are
//! published into the repository store by background workers, while the
//! sequential demand still materializes each root itself. These tests pin
//! that nothing it shows depends on that: cold builds at
//! `--loading_phase_threads` 1 (no warm-up), 8, 32, 8 with jitter and 8 with
//! the warm-up switched off are byte-identical in exit code, stdout, stderr
//! (`.bzl` and BUILD prints in sequential order), lockfile and the snapshot
//! recording, and the warm-up did run (transitively, and for a registered
//! toolchain's package).

#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)
use base64::Engine;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");
include!("support/console.rs");

/// Every module's files (besides MODULE.bazel) and its bazel_dep.
type Module = (
    &'static str,
    &'static str,
    Vec<(&'static str, &'static str)>,
);

fn modules() -> Vec<Module> {
    vec![
        (
            "a",
            "bazel_dep(name = 'c', version = '1')\n",
            vec![
                ("BUILD.bazel", ""),
                (
                    "defs.bzl",
                    "load('@c//:c.bzl', 'C')\nprint('a defs, ' + C)\nA = 'a'\n",
                ),
            ],
        ),
        (
            "b",
            "",
            vec![
                ("BUILD.bazel", ""),
                ("defs.bzl", "print('b defs')\nB = 'b'\n"),
            ],
        ),
        (
            "c",
            "",
            vec![
                ("BUILD.bazel", ""),
                ("c.bzl", "print('c top-level')\nC = 'c'\n"),
            ],
        ),
        (
            "d",
            "bazel_dep(name = 'e', version = '1')\n",
            vec![
                ("BUILD.bazel", ""),
                (
                    "tc/BUILD.bazel",
                    "load('@e//:e.bzl', 'impl')\nprint('d tc BUILD')\nimpl(name = 'impl')\ntoolchain(name = 'tc', toolchain_type = '@@//:type', toolchain = ':impl')\n",
                ),
            ],
        ),
        (
            "e",
            "",
            vec![
                ("BUILD.bazel", ""),
                (
                    "e.bzl",
                    "print('e top-level')\ndef _impl(ctx):\n    return [platform_common.ToolchainInfo(tag = 'e')]\nimpl = rule(implementation = _impl)\n",
                ),
            ],
        ),
    ]
}

fn module_file(module: &str, deps: &str) -> String {
    format!("module(name = '{module}', version = '1')\n{deps}")
}

fn archive(module: &str, deps: &str, files: &[(&str, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let module_text = module_file(module, deps);
    let mut entries = vec![("MODULE.bazel", module_text.as_str())];
    entries.extend(files.iter().copied());
    for (name, data) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_mode(0o644);
        header.set_size(data.len() as u64);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_path(format!("{module}-1/{name}")).unwrap();
        header.set_cksum();
        builder.append(&header, data.as_bytes()).unwrap();
    }
    let tar = builder.into_inner().unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, &tar).unwrap();
    encoder.finish().unwrap()
}

struct Fixture(tempfile::TempDir);

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timeline: String,
    lockfile: Option<String>,
    records: Vec<String>,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self(
            tempfile::Builder::new()
                .prefix("module-root-warm-")
                .tempdir()
                .unwrap(),
        );
        for (module, deps, files) in modules() {
            let bytes = archive(module, deps, &files);
            let path = fixture.path().join(format!("{module}-1.tar.gz"));
            fs::write(&path, &bytes).unwrap();
            fixture.write(
                &format!("registry/modules/{module}/1/MODULE.bazel"),
                &module_file(module, deps),
            );
            let source = serde_json::json!({
                "url": format!("file://{}", path.display()),
                "integrity": format!(
                    "sha256-{}",
                    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&bytes))
                ),
                "strip_prefix": format!("{module}-1"),
            });
            fixture.write(
                &format!("registry/modules/{module}/1/source.json"),
                &source.to_string(),
            );
        }
        fixture.write(
            "MODULE.bazel",
            "module(name = 'main')\nbazel_dep(name = 'a', version = '1')\nbazel_dep(name = 'b', version = '1')\nbazel_dep(name = 'd', version = '1')\nregister_toolchains('@d//tc:all')\n",
        );
        fixture.write(
            "BUILD.bazel",
            "load('@a//:defs.bzl', 'A')\nload('@b//:defs.bzl', 'B')\nload('//:rules.bzl', 'u')\nprint('main BUILD ' + A + B)\ntoolchain_type(name = 'type', visibility = ['//visibility:public'])\nplatform(name = 'p')\nu(name = 'u')\n",
        );
        fixture.write(
            "rules.bzl",
            "def _u(ctx):\n    print('u sees ' + ctx.toolchains['//:type'].tag)\n    return []\nu = rule(implementation = _u, toolchains = ['//:type'])\n",
        );
        fixture
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A cold `build //:u` (fresh cache and lockfile) recording the
    /// analysis snapshot.
    fn cold(&self, threads: usize, env: &[(&str, &str)]) -> Run {
        let _ = fs::remove_dir_all(self.path().join("cache"));
        let _ = fs::remove_file(self.path().join("MODULE.bazel.lock"));
        let dump = self.path().join("dump.txt");
        let _ = fs::remove_file(&dump);
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        command
            .arg("--ignore_all_rc_files")
            .arg("--cache-dir=cache")
            .arg("build")
            .arg(format!(
                "--registry=file://{}",
                self.path().join("registry").display()
            ))
            .arg(format!("--loading_phase_threads={threads}"))
            .arg("--lockfile_mode=update")
            .arg("--platforms=//:p")
            .arg("--host_platform=//:p")
            .arg("//:u")
            .current_dir(self.path())
            .env_remove("BAZEL_RS_TIMING")
            .env_remove("BAZEL_RS_TIMING_TIMELINE")
            .env_remove("BAZEL_RS_LOADING_JITTER")
            .env_remove("BAZEL_RS_DEBUG_NO_ROOT_WARM")
            .env("BAZEL_RS_SNAPSHOT", "record")
            .env("BAZEL_RS_SNAPSHOT_DUMP", &dump);
        for (key, value) in env {
            command.env(key, value);
        }
        let output: Output = command.explicit_modules().output().unwrap();
        let text = String::from_utf8_lossy(&output.stderr).into_owned();
        let (timeline, stderr): (Vec<&str>, Vec<&str>) = text
            .split_inclusive('\n')
            .partition(|line| line.starts_with("PERF") || line.starts_with("bazel-rs-snapshot:"));
        Run {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: without_elapsed(&stderr.concat()),
            timeline: timeline.concat(),
            lockfile: fs::read_to_string(self.path().join("MODULE.bazel.lock")).ok(),
            records: records(&dump),
        }
    }
}

/// The snapshot recording's observations, environment reads and taints (as
/// tests/parallel_fetch.rs compares them: a set; stat keys and the racy
/// flag depend on when files were written).
fn records(dump: &Path) -> Vec<String> {
    let text = fs::read_to_string(dump).unwrap_or_default();
    let mut records: Vec<String> = text
        .lines()
        .filter(|line| {
            (line.starts_with("OBS ") || line.starts_with("ENV ") || line.starts_with("TAINT "))
                // The jitter hook taints the recording it runs in.
                && !line.starts_with("TAINT test-hook ")
        })
        .map(|line| {
            line.split(' ')
                .filter(|word| !word.starts_with("key:") && *word != "racy")
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    records.sort();
    records
}

fn assert_same(reference: &Run, other: &Run, what: &str) {
    assert_eq!(other.code, reference.code, "{what}: exit\n{}", other.stderr);
    assert_eq!(other.stdout, reference.stdout, "{what}: stdout");
    assert_eq!(other.stderr, reference.stderr, "{what}: stderr");
    assert_eq!(other.lockfile, reference.lockfile, "{what}: lockfile");
    assert_eq!(
        other.records, reference.records,
        "{what}: snapshot recording"
    );
}

#[test]
fn warmed_module_roots_change_nothing_the_build_shows() {
    let fixture = Fixture::new();
    let reference = fixture.cold(1, &[]);
    assert_eq!(reference.code, Some(0), "{}", reference.stderr);
    // Prints in sequential order: a's load of c first, then b, then main.
    let mut at = 0;
    for line in [
        "c top-level",
        "a defs, c",
        "b defs",
        "main BUILD ab",
        "e top-level",
        "d tc BUILD",
        "u sees e",
    ] {
        let found = reference.stderr[at..]
            .find(line)
            .unwrap_or_else(|| panic!("{line} after {at}: {}", reference.stderr));
        at += found + line.len();
    }
    assert!(
        reference
            .records
            .iter()
            .any(|line| line.starts_with("TAINT ")),
        "a cold materialization taints: {:?}",
        reference.records
    );
    for (threads, env) in [
        (8, vec![]),
        (32, vec![]),
        (8, vec![("BAZEL_RS_LOADING_JITTER", "30")]),
        (8, vec![("BAZEL_RS_DEBUG_NO_ROOT_WARM", "1")]),
    ] {
        let other = fixture.cold(threads, &env);
        assert_same(&reference, &other, &format!("threads={threads} {env:?}"));
    }
    // The warm-up ran: the main BUILD's loads, a's load of c
    // (transitively), the registered toolchain's package and its load.
    let traced = fixture.cold(8, &[("BAZEL_RS_TIMING_TIMELINE", "1")]);
    assert_same(&reference, &traced, "threads=8 with the timeline");
    for module in ["a+", "b+", "c+", "d+", "e+"] {
        assert!(
            traced
                .timeline
                .lines()
                .any(|line| line.contains(" module-root-warm ") && line.ends_with(module)),
            "{module} warmed:\n{}",
            traced.timeline
        );
    }
    let sequential = fixture.cold(1, &[("BAZEL_RS_TIMING_TIMELINE", "1")]);
    assert!(
        !sequential.timeline.contains(" module-root-warm "),
        "one loading thread is the sequential engine:\n{}",
        sequential.timeline
    );
}

/// A module whose archive is missing fails the build exactly as without
/// the warm-up (the warm-up's failure is dropped; the demand reports it).
#[test]
fn a_failed_warm_up_leaves_the_demand_its_own_error() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.path().join("c-1.tar.gz")).unwrap();
    let reference = fixture.cold(1, &[]);
    assert_ne!(reference.code, Some(0), "{}", reference.stderr);
    for threads in [8, 32] {
        let other = fixture.cold(threads, &[]);
        assert_same(&reference, &other, &format!("threads={threads}"));
    }
}
