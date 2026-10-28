//! Checkout/revert reuse through the real CLI, with tiny owned workspaces.
#![allow(clippy::disallowed_methods)]
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/temp_root.rs");

const BUILD: &str = r#"
load(":defs.bzl", "custom")
platform(name = "platform")
custom(name = "top", deps = [":a"], tool = ":tool")
filegroup(name = "a", srcs = ["input"])
filegroup(name = "b", srcs = ["other"])
filegroup(name = "tool", srcs = ["input"])
genrule(name = "gen", outs = ["generated"], cmd = "touch GEN-RAN; touch $@")
"#;

struct Workspace {
    _temp: tempfile::TempDir,
    root: PathBuf,
    cache: PathBuf,
}
impl Workspace {
    fn new() -> Self {
        let temp = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let root = temp.path().join("ws");
        fs::create_dir(&root).unwrap();
        let ws = Self {
            root,
            cache: temp.path().join("cache"),
            _temp: temp,
        };
        ws.write("MODULE.bazel", "module(name='history')\n");
        ws.write("BUILD.bazel", BUILD);
        ws.write("leaf.bzl", "VALUE = 'one'\n");
        ws.write("defs.bzl", r#"
load(":leaf.bzl", "VALUE")
print("history-note-" + VALUE)
def _impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.run_shell(inputs = [ctx.info_file], outputs = [out], command = "touch ACTION-RAN; touch " + out.path)
    return [DefaultInfo(files = depset([out]))]
custom = rule(implementation = _impl, attrs = {"deps": attr.label_list(), "message": attr.string(default = VALUE), "tool": attr.label(cfg = "exec", allow_files = True)})
"#);
        ws.write("input", "input\n");
        ws.write("other", "other\n");
        ws
    }
    fn write(&self, name: &str, text: &str) {
        let p = self.root.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_bazel-rs"));
        c.current_dir(&self.root)
            .env("BAZEL_RS_SNAPSHOT", "on")
            .env("BAZEL_RS_SNAPSHOT_EXPLAIN", "1")
            .env_remove("BAZEL_RS_DIGEST_MEMO")
            .arg("--ignore_all_rc_files")
            .arg(format!("--cache-dir={}", self.cache.display()));
        c
    }
    fn query_args(&self, c: &mut Command, args: &[&str]) {
        c.args([
            "cquery",
            "--experimental-builtin-modules=false",
            "--lockfile_mode=off",
            "--platforms=//:platform",
            "--host_platform=//:platform",
        ])
        .arg(format!(
            "--workspace_status_command=touch {}",
            self.root.join("STATUS-RAN").display()
        ))
        .args(args);
    }
    fn query(&self, args: &[&str]) -> Output {
        let mut c = self.command();
        self.query_args(&mut c, args);
        let out = c.output().unwrap();
        good(&out);
        out
    }
    fn config(&self) -> Vec<u8> {
        let out = self
            .command()
            .args(["config", "--output=json", "--dump_all"])
            .output()
            .unwrap();
        good(&out);
        out.stdout
    }
    fn snapshots(&self) -> Vec<PathBuf> {
        fn visit(p: &Path, found: &mut Vec<PathBuf>) {
            if let Ok(entries) = fs::read_dir(p) {
                for e in entries.flatten() {
                    if e.file_type().unwrap().is_dir() {
                        visit(&e.path(), found);
                    } else if e.path().extension().is_some_and(|s| s == "snap") {
                        found.push(e.path());
                    }
                }
            }
        }
        let mut found = Vec::new();
        visit(&self.cache, &mut found);
        found.sort();
        found
    }
    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(&self.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "-c",
                "user.name=History test",
                "-c",
                "user.email=history@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        good(&out);
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    fn sentinels_absent(&self) {
        for name in ["ACTION-RAN", "GEN-RAN", "STATUS-RAN"] {
            assert!(!self.root.join(name).exists(), "{name}");
        }
    }
}
fn good(out: &Output) {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn state(out: &Output, state: &str) {
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains(&format!("bazel-rs-cquery-snapshot: {state}")),
        "{text}"
    );
}
fn hit_eq(out: &Output, expected: &Output) {
    state(out, "hit");
    assert_eq!(out.stdout, expected.stdout);
}
const PROTO: &[&str] = &["--output=proto", "deps(//:top)"];

#[test]
fn real_git_checkouts_reuse_both_graphs_and_only_selected_diagnostics() {
    let ws = Workspace::new();
    ws.git(&["init", "-q"]);
    ws.git(&["add", "."]);
    ws.git(&["commit", "-qm", "A"]);
    let a = ws.git(&["rev-parse", "HEAD"]);
    ws.write("leaf.bzl", "VALUE = 'two'\n");
    ws.git(&["commit", "-qam", "B"]);
    let b = ws.git(&["rev-parse", "HEAD"]);
    ws.git(&["checkout", "-q", &a]);
    let old = fs::metadata(ws.root.join("leaf.bzl")).unwrap();
    let first = ws.query(PROTO);
    state(&first, "miss");
    state(&first, "saved");
    hit_eq(&ws.query(PROTO), &first);
    ws.git(&["checkout", "-q", &b]);
    let second = ws.query(PROTO);
    state(&second, "miss");
    state(&second, "saved");
    hit_eq(&ws.query(PROTO), &second);
    assert_ne!(first.stdout, second.stdout);
    for (revision, expected, note, absent) in
        [(&a, &first, "one", "two"), (&b, &second, "two", "one")]
    {
        ws.git(&["checkout", "-q", revision]);
        let out = ws.query(PROTO);
        hit_eq(&out, expected);
        let text = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            text.matches(&format!("history-note-{note}")).count(),
            1,
            "{text}"
        );
        assert!(!text.contains(&format!("history-note-{absent}")), "{text}");
    }
    let new = fs::metadata(ws.root.join("leaf.bzl")).unwrap();
    assert_ne!(
        (old.ino(), old.ctime(), old.ctime_nsec()),
        (new.ino(), new.ctime(), new.ctime_nsec())
    );
    assert_eq!(ws.snapshots().len(), 2);
    ws.sentinels_absent();
}

#[test]
fn ignore_bytes_are_validated_even_when_graph_is_identical() {
    let ws = Workspace::new();
    ws.write(".bazelignore", "unused-a\n");
    let a = ws.query(PROTO);
    ws.write(".bazelignore", "unused-b\n");
    let b = ws.query(PROTO);
    state(&b, "miss");
    assert_eq!(a.stdout, b.stdout);
    ws.write(".bazelignore", "unused-a\n");
    hit_eq(&ws.query(PROTO), &a);
    ws.write(".bazelignore", "unused-b\n");
    hit_eq(&ws.query(PROTO), &b);
    assert_eq!(ws.snapshots().len(), 2);
}

#[test]
fn ignored_package_membership_never_uses_the_other_revisions_graph() {
    let ws = Workspace::new();
    ws.write(
        "hidden/BUILD.bazel",
        "filegroup(name='visible_when_not_ignored')\n",
    );
    ws.write(".bazelignore", "hidden\n");
    let a = ws.query(&["//..."]);
    ws.write(".bazelignore", "");
    let b = ws.query(&["//..."]);
    state(&b, "miss");
    assert!(!String::from_utf8_lossy(&a.stdout).contains("//hidden:"));
    assert!(String::from_utf8_lossy(&b.stdout).contains("//hidden:visible_when_not_ignored"));
    ws.write(".bazelignore", "hidden\n");
    hit_eq(&ws.query(&["//..."]), &a);
    ws.write(".bazelignore", "");
    hit_eq(&ws.query(&["//..."]), &b);
    ws.sentinels_absent();
}

#[test]
fn metadata_only_replacement_hits_but_same_size_restored_mtime_edit_misses() {
    let ws = Workspace::new();
    let a = ws.query(PROTO);
    let p = ws.root.join("leaf.bzl");
    let old = fs::metadata(&p).unwrap();
    let temp = ws.root.join("replacement");
    fs::write(&temp, fs::read(&p).unwrap()).unwrap();
    fs::rename(&temp, &p).unwrap();
    assert_ne!(old.ino(), fs::metadata(&p).unwrap().ino());
    hit_eq(&ws.query(PROTO), &a);
    ws.write("leaf.bzl", "VALUE = 'two'\n");
    fs::File::open(&p)
        .unwrap()
        .set_modified(old.modified().unwrap())
        .unwrap();
    assert_eq!(old.len(), fs::metadata(&p).unwrap().len());
    let b = ws.query(PROTO);
    state(&b, "miss");
    assert_ne!(a.stdout, b.stdout);
    ws.write("leaf.bzl", "VALUE = 'one'\n");
    hit_eq(&ws.query(PROTO), &a);
    ws.write("leaf.bzl", "VALUE = 'two'\n");
    hit_eq(&ws.query(PROTO), &b);
}

#[test]
fn build_module_source_presence_glob_directory_and_symlink_observations_remain_live() {
    for kind in ["build", "module", "source", "glob", "directory", "symlink"] {
        let ws = Workspace::new();
        let args: &[&str] = if kind == "directory" {
            &["--output=proto", "//..."]
        } else {
            PROTO
        };
        if kind == "glob" {
            ws.write(
                "BUILD.bazel",
                "platform(name='platform')\nfilegroup(name='top', srcs=glob(['*.txt'], allow_empty=True))\n",
            );
        }
        if kind == "symlink" {
            ws.write("one.bzl", "VALUE = 'one'\n");
            ws.write("two.bzl", "VALUE = 'two'\n");
            fs::remove_file(ws.root.join("leaf.bzl")).unwrap();
            std::os::unix::fs::symlink("one.bzl", ws.root.join("leaf.bzl")).unwrap();
        }
        let change = |b: bool| match kind {
            "build" => ws.write(
                "BUILD.bazel",
                &if b {
                    BUILD.replace("deps = [\":a\"]", "deps = [\":b\"]")
                } else {
                    BUILD.to_owned()
                },
            ),
            "module" => ws.write(
                "MODULE.bazel",
                if b {
                    "module(name='other_history')\n"
                } else {
                    "module(name='history')\n"
                },
            ),
            "source" => {
                if b {
                    fs::remove_file(ws.root.join("input")).unwrap()
                } else {
                    ws.write("input", "input\n")
                }
            }
            "glob" => {
                if b {
                    ws.write("added.txt", "x")
                } else {
                    fs::remove_file(ws.root.join("added.txt")).unwrap()
                }
            }
            "directory" => {
                if b {
                    ws.write("newpkg/BUILD.bazel", "filegroup(name='new')\n")
                } else {
                    fs::remove_dir_all(ws.root.join("newpkg")).unwrap()
                }
            }
            "symlink" => {
                fs::remove_file(ws.root.join("leaf.bzl")).unwrap();
                std::os::unix::fs::symlink(
                    if b { "two.bzl" } else { "one.bzl" },
                    ws.root.join("leaf.bzl"),
                )
                .unwrap();
            }
            _ => unreachable!(),
        };
        let a = ws.query(args);
        state(&a, "saved");
        change(true);
        let b = ws.query(args);
        state(&b, "miss");
        state(&b, "saved");
        change(false);
        hit_eq(&ws.query(args), &a);
        change(true);
        hit_eq(&ws.query(args), &b);
    }
}

#[test]
fn verification_deduplicates_and_configuration_dump_follows_selected_graph() {
    let ws = Workspace::new();
    ws.write("BUILD.bazel", &BUILD.replace(", tool = \":tool\"", ""));
    let a = ws.query(PROTO);
    let ca = ws.config();
    ws.write("BUILD.bazel", BUILD);
    let b = ws.query(PROTO);
    let cb = ws.config();
    assert_ne!(ca, cb);
    for (build, expected, config) in [
        (BUILD.replace(", tool = \":tool\"", ""), &a, &ca),
        (BUILD.into(), &b, &cb),
    ] {
        ws.write("BUILD.bazel", &build);
        hit_eq(&ws.query(PROTO), expected);
        assert_eq!(&ws.config(), config);
        let mut c = ws.command();
        ws.query_args(&mut c, PROTO);
        c.env("BAZEL_RS_SNAPSHOT", "verify");
        let verified = c.output().unwrap();
        good(&verified);
        assert_eq!(verified.stdout, expected.stdout);
        assert_eq!(
            String::from_utf8_lossy(&verified.stderr)
                .matches("history-note-one")
                .count(),
            1
        );
        assert_eq!(ws.snapshots().len(), 2);
    }
}

#[test]
fn third_version_evicts_lru_and_failed_analysis_preserves_history() {
    let ws = Workspace::new();
    let a = ws.query(PROTO);
    ws.write("leaf.bzl", "VALUE = 'two'\n");
    let b = ws.query(PROTO);
    ws.write("leaf.bzl", "VALUE = 'one'\n");
    hit_eq(&ws.query(PROTO), &a);
    ws.write("leaf.bzl", "VALUE = 'three'\n");
    let c = ws.query(PROTO);
    state(&c, "saved");
    assert_eq!(ws.snapshots().len(), 2);
    ws.write("leaf.bzl", "fail('history-failed-analysis')\n");
    let mut command = ws.command();
    ws.query_args(&mut command, PROTO);
    let failed = command.output().unwrap();
    assert!(!failed.status.success());
    assert_eq!(ws.snapshots().len(), 2);
    ws.write("leaf.bzl", "VALUE = 'one'\n");
    hit_eq(&ws.query(PROTO), &a);
    ws.write("leaf.bzl", "VALUE = 'three'\n");
    hit_eq(&ws.query(PROTO), &c);
    ws.write("leaf.bzl", "VALUE = 'two'\n");
    let evicted = ws.query(PROTO);
    state(&evicted, "miss");
    assert_eq!(evicted.stdout, b.stdout);
}

#[test]
fn corrupt_newest_falls_back_without_diagnostics_and_both_missing_or_corrupt_miss() {
    use bazel_rs::snapshot::{
        format::{self, Section},
        key::Key,
    };
    let ws = Workspace::new();
    let a = ws.query(PROTO);
    let original = ws.snapshots().pop().unwrap();
    let bytes = fs::read(&original).unwrap();
    let header = format::Header::decode(&bytes, bytes.len() as u64).unwrap();
    let other = original.with_file_name(
        original
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .replace("cq0", "cq1"),
    );
    for damage in [
        "inputs",
        "status",
        "events",
        "plan",
        "index",
        "header",
        "truncated",
    ] {
        let mut sections: Vec<_> = [
            Section::Inputs,
            Section::Events,
            Section::Plan,
            Section::Stats,
        ]
        .into_iter()
        .map(|k| {
            let s = header.slot(k).unwrap();
            (
                k,
                bytes[s.offset as usize..(s.offset + s.len) as usize].to_vec(),
            )
        })
        .collect();
        match damage {
            "inputs" => sections[0].1 = vec![255],
            "status" => {
                let inputs = format::decode_inputs(&sections[0].1).unwrap();
                sections[0].1 = format::encode_inputs(
                    &inputs.recording,
                    &inputs.replay,
                    inputs.tables.as_ref(),
                    true,
                );
            }
            "events" => sections[1].1 = vec![255],
            "plan" => sections[2].1 = format::compress_plan(&[255]),
            "index" => {
                let raw = format::decompress_plan(sections[2].1.clone()).unwrap();
                let mut graph: bazel_rs::engine::cquery::Graph =
                    postcard::from_bytes(&raw).unwrap();
                graph.patterns.insert("//:top".into(), vec![u32::MAX]);
                sections[2].1 = format::compress_plan(&postcard::to_allocvec(&graph).unwrap());
            }
            _ => {}
        }
        let borrowed: Vec<_> = sections.iter().map(|(k, b)| (*k, b.as_slice())).collect();
        let h = format::Header::new(
            &Key {
                g: header.key,
                binary: header.binary,
            },
            header.created_ns + 1,
            &borrowed,
        );
        let mut damaged = h.encode().to_vec();
        for (_, s) in sections {
            damaged.extend(s);
        }
        if damage == "header" {
            damaged[0] ^= 1;
        }
        if damage == "truncated" {
            damaged.truncate(7);
        }
        fs::write(&other, damaged).unwrap();
        let out = ws.query(PROTO);
        hit_eq(&out, &a);
        assert_eq!(
            String::from_utf8_lossy(&out.stderr)
                .matches("history-note-one")
                .count(),
            1
        );
    }
    fs::write(&original, b"corrupt").unwrap();
    let miss = ws.query(PROTO);
    state(&miss, "miss");
    assert_eq!(miss.stdout, a.stdout);
    for path in ws.snapshots() {
        fs::remove_file(path).unwrap();
    }
    let absent = ws.query(PROTO);
    state(&absent, "miss");
    assert_eq!(absent.stdout, a.stdout);
}

#[test]
fn universe_cwd_output_base_separate_but_presentation_shares_and_output_errors_are_terminal() {
    let ws = Workspace::new();
    ws.write("sub/BUILD.bazel", "filegroup(name='sub')\n");
    let a = ws.query(PROTO);
    state(&ws.query(&["--output=label_kind", "//:top"]), "hit");
    state(&ws.query(&["deps(//:a)"]), "miss");
    let mut other = ws.command();
    other.current_dir(ws.root.join("sub"));
    ws.query_args(&mut other, PROTO);
    let out = other.output().unwrap();
    good(&out);
    state(&out, "miss");
    let mut other = ws.command();
    other.arg(format!(
        "--output_base={}",
        ws._temp.path().join("other-output-base").display()
    ));
    ws.query_args(&mut other, PROTO);
    let out = other.output().unwrap();
    good(&out);
    state(&out, "miss");
    hit_eq(&ws.query(PROTO), &a);
    let mut failed = ws.command();
    ws.query_args(
        &mut failed,
        &[
            "--output_file=missing/targets.pb",
            "--output=proto",
            "deps(//:top)",
        ],
    );
    let out = failed.output().unwrap();
    assert!(!out.status.success());
    state(&out, "hit");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("snapshot: miss"));
}

#[test]
fn repository_environment_and_replayed_inputs_are_checked_for_each_candidate() {
    for execute in [false, true] {
        let ws = Workspace::new();
        ws.write(
            "MODULE.bazel",
            "module(name='history')\nr=use_repo_rule('//:repo.bzl','repo')\nr(name='external')\n",
        );
        let selector = ws._temp.path().join("selector");
        let value = if execute {
            format!(
                "ctx.execute(['/bin/cat', {:?}]).stdout.strip()",
                selector.to_str().unwrap()
            )
        } else {
            "ctx.os.environ.get('HISTORY_NAME', 'one')".into()
        };
        ws.write(
            "repo.bzl",
            &format!(
                r#"def _impl(ctx):
    ctx.file('BUILD.bazel', 'filegroup(name = "' + {value} + '")')
repo = repository_rule(implementation = _impl, environ = ['HISTORY_NAME'], local = True)
"#
            ),
        );
        let query = |name| {
            fs::write(&selector, name).unwrap();
            let mut c = ws.command();
            c.env("HISTORY_NAME", if execute { "constant" } else { name });
            ws.query_args(&mut c, &["--output=proto", "@external//:all"]);
            let out = c.output().unwrap();
            good(&out);
            out
        };
        let a = query("one");
        state(&a, "saved");
        hit_eq(&query("one"), &a);
        let b = query("two");
        state(&b, "miss");
        state(&b, "saved");
        assert_ne!(a.stdout, b.stdout);
        hit_eq(&query("two"), &b);
        let restored = query("one");
        hit_eq(&restored, &a);
        // The repository store must still have the publication as well as
        // reproduce its execution identity; no stale graph may bypass either.
        hit_eq(&query("one"), &a);
        let restored = query("two");
        hit_eq(&restored, &b);
        hit_eq(&query("two"), &b);
        // A valid replay identity alone is insufficient: corrupt all retained
        // publications and require analysis/repair, never a graph hit.
        let mut roots = std::collections::BTreeSet::new();
        for snapshot in ws.snapshots() {
            use bazel_rs::snapshot::format;
            let bytes = fs::read(snapshot).unwrap();
            let header = format::Header::decode(&bytes, bytes.len() as u64).unwrap();
            let slot = header.slot(format::Section::Inputs).unwrap();
            let inputs = format::decode_inputs(
                &bytes[slot.offset as usize..(slot.offset + slot.len) as usize],
            )
            .unwrap();
            roots.extend(inputs.replay.into_iter().map(|record| record.root));
        }
        assert!(!roots.is_empty(), "fixture must exercise local replay");
        for root in roots {
            fs::write(root.join("BUILD.bazel"), "filegroup(name='tampered')\n").unwrap();
        }
        let repaired = query("two");
        state(&repaired, "miss");
        assert_eq!(repaired.stdout, b.stdout);
        ws.sentinels_absent();
    }
}
