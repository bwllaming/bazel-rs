//! N28d/N29 through the Cargo-built CLI: glob() follows symlinks like Bazel
//! (main repository and a local_jdk-shaped generated repository), and
//! repository rules / module extension tags accept
//! attr.label_keyed_string_dict (rules_oci shape).
#![cfg(unix)]
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Fixture {
    workspace: tempfile::TempDir,
    /// A host directory outside every repository (a JDK install).
    host: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let f = Self {
            workspace: tempfile::Builder::new()
                .prefix("glob-links-ws-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
            host: tempfile::Builder::new()
                .prefix("glob-links-host-")
                .tempdir_in("/tmp/opencode")
                .unwrap(),
        };
        f.file("MODULE.bazel", "module(name='main')\n");
        f
    }

    fn path(&self) -> &Path {
        self.workspace.path()
    }

    fn file(&self, name: &str, data: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    fn host_file(&self, name: &str, data: &str) -> PathBuf {
        let path = fs::canonicalize(self.host.path()).unwrap().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, data).unwrap();
        path
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new("timeout")
            .args([
                "600s",
                env!("CARGO_BIN_EXE_bazel-rs"),
                "--ignore_all_rc_files",
                "--cache-dir=cache",
            ])
            .args(args)
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn outputs(output: &Output) -> Vec<(String, PathBuf)> {
    assert!(output.status.success(), "{}", stderr(output));
    stdout(output)
        .lines()
        .map(|line| {
            let (label, path) = line.split_once('\t').expect("native build output path");
            (label.to_owned(), PathBuf::from(path))
        })
        .collect()
}

fn failed(output: &Output, marker: &str) {
    assert!(!output.status.success(), "{}", stdout(output));
    assert!(!stderr(output).contains("panicked"), "{}", stderr(output));
    assert!(stderr(output).contains(marker), "{}", stderr(output));
}

#[test]
fn main_repository_globs_follow_file_directory_and_dangling_links() {
    let f = Fixture::new();
    f.file("p/real/a.txt", "a\n");
    f.file("p/real/deep/b.txt", "b\n");
    f.file("p/c.txt", "c\n");
    f.file(
        "p/BUILD.bazel",
        "genrule(name='list', srcs=glob(['**/*.txt']), outs=['list.out'], cmd='echo $(SRCS) > $@')\nfilegroup(name='all', srcs=glob(['**/*.txt']))\ngenrule(name='cat', srcs=[':all'], outs=['cat.out'], cmd='cat $(SRCS) > $@')\n",
    );
    symlink("c.txt", f.path().join("p/alias.txt")).unwrap();
    symlink("real", f.path().join("p/dirlink")).unwrap();
    symlink("missing.txt", f.path().join("p/dangling.txt")).unwrap();
    let listed = outputs(&f.run(&["build", "//p:list"]));
    assert_eq!(
        fs::read_to_string(&listed[0].1).unwrap(),
        "p/alias.txt p/c.txt p/dirlink/a.txt p/dirlink/deep/b.txt p/real/a.txt p/real/deep/b.txt\n"
    );
    // Sources through the links are buildable and read the targets' bytes.
    let built = outputs(&f.run(&["build", "//p:cat"]));
    assert_eq!(
        fs::read_to_string(&built[0].1).unwrap(),
        "c\nc\na\nb\na\nb\n"
    );
}

#[test]
fn main_repository_symlink_cycles_and_expansions_are_bazel_errors() {
    let f = Fixture::new();
    f.file(
        "cyc/BUILD.bazel",
        "filegroup(name='g', srcs=glob(['*'], exclude=['loop']))\n",
    );
    symlink("loop", f.path().join("cyc/loop")).unwrap();
    let root = fs::canonicalize(f.path()).unwrap();
    failed(
        &f.run(&["query", "//cyc:g"]),
        &format!(
            "no such package 'cyc': error globbing [*] - [loop] op=FILES: [unix_jni.cc:382] {0}/cyc/loop (Too many levels of symbolic links)",
            root.display()
        ),
    );
    f.file("up/BUILD.bazel", "filegroup(name='g', srcs=glob(['**']))\n");
    f.file("up/x.txt", "x\n");
    symlink(".", f.path().join("up/self")).unwrap();
    failed(
        &f.run(&["query", "//up:g"]),
        &format!(
            "no such package 'up': Symlink issue while evaluating globs: Infinite symlink expansion: {0}/up/self- > {0}/up",
            root.display()
        ),
    );
    // Main-repository links to host paths stay an explicit native restriction.
    let host = f.host_file("dir/h.txt", "h\n");
    f.file(
        "hostlink/BUILD.bazel",
        "filegroup(name='g', srcs=glob(['**']))\n",
    );
    symlink(host.parent().unwrap(), f.path().join("hostlink/d")).unwrap();
    failed(
        &f.run(&["query", "//hostlink:g"]),
        "outside its repository root",
    );
}

const LOCAL_JDK: &str = r#"
def _impl(ctx):
    java_home = ctx.attr.java_home
    for entry in ["bin", "lib", "release"]:
        ctx.symlink(java_home + "/" + entry, entry)
    ctx.file("BUILD.bazel", """
genrule(name = "list", srcs = glob(["lib/**", "release"], allow_empty = True), outs = ["list.txt"], cmd = "echo $(SRCS) > $@")
filegroup(name = "jdk-bin", srcs = glob(["bin/**"], exclude = ["**/*%*/**"]))
filegroup(name = "jdk-lib", srcs = glob(["lib/**", "release"], allow_empty = True, exclude = ["lib/missioncontrol/**"]))
filegroup(name = "jre", srcs = glob(["jre/bin/**", "jre/lib/**"], allow_empty = True))
filegroup(name = "java", srcs = [glob(["bin/java.exe", "bin/java"], allow_empty = True)[0]])
genrule(name = "copy", srcs = [":java", ":jdk-lib"], outs = ["copy.txt"], cmd = "cat $(SRCS) > $@")
""")
local_java_repository = repository_rule(_impl, attrs = {"java_home": attr.string()}, local = True)
"#;

#[test]
fn generated_local_jdk_repository_globs_and_builds_through_host_links() {
    let f = Fixture::new();
    let java = f.host_file("jdk/bin/java", "#!java\n");
    f.host_file("jdk/lib/modules", "modules\n");
    f.host_file("jdk/release", "JAVA_VERSION=\"25\"\n");
    let home = java.parent().unwrap().parent().unwrap().to_path_buf();
    // The JDK's dangling lib/src.zip (a link to an uninstalled source zip).
    symlink(home.join("lib/missing-src.zip"), home.join("lib/src.zip")).unwrap();
    f.file("jdk.bzl", LOCAL_JDK);
    f.file(
        "MODULE.bazel",
        &format!(
            "module(name='main')\nlocal_java_repository = use_repo_rule('//:jdk.bzl', 'local_java_repository')\nlocal_java_repository(name='local_jdk', java_home='{}')\n",
            home.display()
        ),
    );
    f.file("BUILD.bazel", "");
    let query = f.run(&["query", "@local_jdk//:all"]);
    assert!(query.status.success(), "{}", stderr(&query));
    assert_eq!(
        stdout(&query),
        "@local_jdk//:copy\n@local_jdk//:java\n@local_jdk//:jdk-bin\n@local_jdk//:jdk-lib\n@local_jdk//:jre\n@local_jdk//:list\n"
    );
    // The dangling lib/src.zip is silently skipped (Bazel tolerates it).
    let listed = outputs(&f.run(&["build", "@local_jdk//:list"]));
    assert_eq!(
        fs::read_to_string(&listed[0].1).unwrap(),
        "external/+local_java_repository+local_jdk/lib/modules external/+local_java_repository+local_jdk/release\n"
    );
    // Source artifacts read the host targets' bytes; nothing is written there.
    let built = outputs(&f.run(&["build", "@local_jdk//:copy"]));
    assert_eq!(
        fs::read_to_string(&built[0].1).unwrap(),
        "#!java\nmodules\nJAVA_VERSION=\"25\"\n"
    );
    assert_eq!(fs::read_to_string(&java).unwrap(), "#!java\n");
}

const OCI: &str = r#"
def _alias_impl(rctx):
    platforms = rctx.attr.platforms
    lines = ["%s=%s:%s" % (str(k), v, type(k)) for k, v in platforms.items()]
    frozen = "mutable"
    rctx.file("BUILD.bazel", "exports_files(['data.txt'])\n")
    rctx.file("data.txt", "\n".join(lines) + "\n")

oci_alias = repository_rule(_alias_impl, attrs = {"platforms": attr.label_keyed_string_dict()})

def _ext_impl(mctx):
    for mod in mctx.modules:
        for tag in mod.tags.toolchains:
            platforms = dict(tag.platforms)
            platforms[Label("//:linux_arm64")] = "linux/arm64"
            oci_alias(name = tag.name, platforms = platforms)

oci = module_extension(_ext_impl, tag_classes = {"toolchains": tag_class(attrs = {
    "name": attr.string(),
    "platforms": attr.label_keyed_string_dict(),
})})
"#;

#[test]
fn rules_oci_shaped_label_keyed_string_dict_repository_attributes() {
    let f = Fixture::new();
    f.file("oci.bzl", OCI);
    f.file("BUILD.bazel", "");
    f.file(
        "MODULE.bazel",
        "module(name='main')\noci = use_extension('//:oci.bzl', 'oci')\noci.toolchains(name='oci_crane_toolchains', platforms={'//:linux_amd64': 'linux/amd64'})\nuse_repo(oci, 'oci_crane_toolchains')\noci_alias = use_repo_rule('//:oci.bzl', 'oci_alias')\noci_alias(name='direct', platforms={':linux_amd64': 'linux/amd64', '@//:linux_s390x': 'linux/s390x'})\n",
    );
    let ext = outputs(&f.run(&["build", "@oci_crane_toolchains//:data.txt"]));
    assert_eq!(
        fs::read_to_string(&ext[0].1).unwrap(),
        "@@//:linux_amd64=linux/amd64:Label\n@@//:linux_arm64=linux/arm64:Label\n"
    );
    let direct = outputs(&f.run(&["build", "@direct//:data.txt"]));
    assert_eq!(
        fs::read_to_string(&direct[0].1).unwrap(),
        "@@//:linux_amd64=linux/amd64:Label\n@@//:linux_s390x=linux/s390x:Label\n"
    );
    // Keys converting to the same label: Bazel's LabelKeyedDictType text.
    f.file(
        "MODULE.bazel",
        "module(name='main')\noci_alias = use_repo_rule('//:oci.bzl', 'oci_alias')\noci_alias(name='direct', platforms={'//:a': 'x', '@//:a': 'y'})\n",
    );
    failed(
        &f.run(&["build", "@direct//:data.txt"]),
        "in call to 'oci_alias' repo rule with name 'direct', duplicate labels in attribute 'platforms': //:a (as [\"//:a\", \"@//:a\"])",
    );
    // The attribute value is frozen.
    f.file(
        "MODULE.bazel",
        "module(name='main')\noci_alias = use_repo_rule('//:oci.bzl', 'oci_alias')\noci_alias(name='direct', platforms={'//:a': 'x'})\n",
    );
    f.file(
        "oci.bzl",
        &OCI.replace(
            "    frozen = \"mutable\"\n",
            "    platforms[Label(\"//:b\")] = \"y\"\n",
        ),
    );
    failed(&f.run(&["build", "@direct//:data.txt"]), "frozen");
}

#[test]
fn label_keyed_string_dict_module_errors_use_bazel_texts() {
    let f = Fixture::new();
    f.file("oci.bzl", OCI);
    f.file("BUILD.bazel", "");
    for (platforms, expected) in [
        (
            "{1: 'x'}",
            "in call to 'oci_alias' repo rule with name 'direct', expected value of type 'string' for dict key element, but got 1 (int)",
        ),
        (
            "{'//:a:b': 'x'}",
            "in call to 'oci_alias' repo rule with name 'direct', invalid label '//:a:b' in dict key element: invalid target name 'a:b': target names may not contain ':'",
        ),
        (
            "{'@nope//:a': 'x'}",
            "in call to 'oci_alias' repo rule with name 'direct', no repository visible as '@nope' to the root module, but referenced by label '@nope//:a' in attribute 'platforms'",
        ),
    ] {
        f.file(
            "MODULE.bazel",
            &format!(
                "module(name='main')\noci_alias = use_repo_rule('//:oci.bzl', 'oci_alias')\noci_alias(name='direct', platforms={platforms})\n"
            ),
        );
        failed(&f.run(&["build", "@direct//:data.txt"]), expected);
    }
    // Labels are ordered like Label.compareTo (sorted() of dict keys).
    f.file(
        "MODULE.bazel",
        "module(name='main')\noci_alias = use_repo_rule('//:oci.bzl', 'oci_alias')\noci_alias(name='direct', platforms={'//:b': 'x', '//:a': 'y'})\n",
    );
    f.file(
        "oci.bzl",
        &OCI.replace(
            "    frozen = \"mutable\"\n",
            "    lines = [str(k) for k in sorted(platforms.keys())]\n",
        ),
    );
    let sorted = outputs(&f.run(&["build", "@direct//:data.txt"]));
    assert_eq!(
        fs::read_to_string(&sorted[0].1).unwrap(),
        "@@//:a\n@@//:b\n"
    );
}

/// Reference row `dlink_dirs0`: `glob(['*'], exclude_directories = 0)` returns
/// a directory link and the directory itself; Bazel accepts both as genrule
/// sources. They are input-file targets natively too (N27); only their use
/// as action inputs is the explicit native gap, named for every label of the
/// directory (the link and the directory resolve to one path).
#[test]
fn directories_reached_through_links_are_file_targets_with_the_action_input_gap() {
    let f = Fixture::new();
    f.file("p/real/r.txt", "r\n");
    f.file("p/f.txt", "f\n");
    f.file(
        "p/BUILD.bazel",
        "genrule(name='dump', srcs=glob(['*'], exclude_directories=0), outs=['o'], cmd='ls > $@')\nfilegroup(name='fg', srcs=glob(['*'], exclude_directories=0))\n",
    );
    symlink("real", f.path().join("p/d")).unwrap();
    // As file targets they analyze (no `source is not a regular file`).
    let query = f.run(&["build", "//p:fg"]);
    assert!(
        !stderr(&query).contains("source is not a regular file"),
        "{}",
        stderr(&query)
    );
    let output = f.run(&["build", "//p:dump"]);
    failed(
        &output,
        "//p:dump: source directory '//p:d' as an action input is not supported by the native profile\n//p:dump: source directory '//p:real' as an action input is not supported by the native profile",
    );
}

/// A repository rule that extracts an archive (N32 streaming xz decode) and
/// then links a host directory into its output: N28d's link policy applies
/// to the extracted tree like to any repository-rule output. glob() follows
/// the archive's in-repository file and directory links and, because the
/// repository is generated by a repository rule, the host link (reads only).
const EXTRACTED: &str = r#"
def _impl(ctx):
    ctx.extract(ctx.attr.src, stripPrefix = "top")
    ctx.symlink(ctx.attr.host, "host")
extracted = repository_rule(_impl, attrs = {"src": attr.label(), "host": attr.string()})
"#;

fn extracted_archive() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let entries: &[(&str, tar::EntryType, &str, &str)] = &[
        ("top/", tar::EntryType::Directory, "", ""),
        (
            "top/BUILD.bazel",
            tar::EntryType::Regular,
            "",
            "genrule(name = 'list', srcs = glob(['**/*.txt']), outs = ['list.out'], cmd = 'echo $(SRCS) > $@')\ngenrule(name = 'cat', srcs = ['link.txt', 'linkdir/b.txt', 'host/h.txt'], outs = ['cat.out'], cmd = 'cat $(SRCS) > $@')\n",
        ),
        ("top/a.txt", tar::EntryType::Regular, "", "a\n"),
        ("top/dir/", tar::EntryType::Directory, "", ""),
        ("top/dir/b.txt", tar::EntryType::Regular, "", "b\n"),
        ("top/link.txt", tar::EntryType::Symlink, "a.txt", ""),
        ("top/linkdir", tar::EntryType::Symlink, "dir", ""),
    ];
    for (name, kind, link, data) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_mode(if *kind == tar::EntryType::Directory {
            0o755
        } else {
            0o644
        });
        header.set_size(data.len() as u64);
        header.set_entry_type(*kind);
        header.set_path(name).unwrap();
        if !link.is_empty() {
            header.set_link_name(link).unwrap();
        }
        header.set_cksum();
        builder.append(&header, data.as_bytes()).unwrap();
    }
    let tar = builder.into_inner().unwrap();
    let mut writer =
        lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::default()).unwrap();
    std::io::Write::write_all(&mut writer, &tar).unwrap();
    writer.finish().unwrap()
}

#[test]
fn extracted_archive_trees_follow_the_n28d_link_policy() {
    let f = Fixture::new();
    let host = f.host_file("share/h.txt", "host\n");
    fs::write(f.path().join("src.tar.xz"), extracted_archive()).unwrap();
    f.file("ext.bzl", EXTRACTED);
    f.file(
        "MODULE.bazel",
        &format!(
            "module(name='main')\nextracted = use_repo_rule('//:ext.bzl', 'extracted')\nextracted(name='x', src='//:src.tar.xz', host='{}')\n",
            host.parent().unwrap().display()
        ),
    );
    f.file("BUILD.bazel", "exports_files(['src.tar.xz'])\n");
    let listed = outputs(&f.run(&["build", "@x//:list"]));
    assert_eq!(
        fs::read_to_string(&listed[0].1).unwrap(),
        "external/+extracted+x/a.txt external/+extracted+x/dir/b.txt external/+extracted+x/host/h.txt external/+extracted+x/link.txt external/+extracted+x/linkdir/b.txt\n"
    );
    let built = outputs(&f.run(&["build", "@x//:cat"]));
    assert_eq!(fs::read_to_string(&built[0].1).unwrap(), "a\nb\nhost\n");
    // Reads only: the host file is unchanged.
    assert_eq!(fs::read_to_string(&host).unwrap(), "host\n");
}
