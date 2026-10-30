//! The changed files between two revisions, classified for the scoped
//! analysis of `bazel-rs affected` (docs/AFFECTED_TARGETS.md §8).
//!
//! A change is either *local* — inside a main-repository package, read only
//! by the loading phase (BUILD files, `.bzl` files) and by actions (sources)
//! — or *global*: it can change the configuration, the module graph or a
//! repository's contents (MODULE.bazel and its includes, the lockfile, rc
//! files, `.bazelversion`, `.bazelignore`, files outside every package,
//! files a module extension or repository rule is given or records, the
//! `.bzl` files of module extensions and repository rules, well-known
//! package-manager manifests). A global change means a full analysis of both
//! revisions; local changes are written as the scope input
//! (crate::engine's `affected_scope`) together with the *roots*: the
//! main-repository labels the configuration names outside the loading graph
//! (flag values, registered toolchains and execution platforms, labels in
//! MODULE.bazel, labels in `.bzl` files defining transitions).

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

/// Package-manager manifests a module extension or repository rule
/// conventionally reads by path (a safety net next to the labels
/// MODULE.bazel names).
const MANIFESTS: &[&str] = &[
    "package.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "package-lock.json",
    "yarn.lock",
    ".npmrc",
    "go.mod",
    "go.sum",
    "go.work",
    "go.work.sum",
    "Cargo.toml",
    "Cargo.lock",
    "Gemfile.lock",
    "poetry.lock",
    "pyproject.toml",
    "uv.lock",
    "maven_install.json",
];

/// Bazel's own configuration files.
fn bazel_file(name: &str) -> bool {
    matches!(
        name,
        "MODULE.bazel"
            | "MODULE.bazel.lock"
            | "WORKSPACE"
            | "WORKSPACE.bazel"
            | "WORKSPACE.bzlmod"
            | "REPO.bazel"
            | ".bazelversion"
            | ".bazelignore"
            | ".bazeliskrc"
    ) || name.ends_with(".MODULE.bazel")
        || name.ends_with(".bazelrc")
        || name == ".bazelrc"
}

fn manifest(name: &str) -> bool {
    MANIFESTS.contains(&name) || (name.starts_with("requirements") && name.ends_with(".txt"))
}

fn is_build_file(path: &str) -> bool {
    matches!(path.rsplit('/').next(), Some("BUILD" | "BUILD.bazel"))
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Runs git with the arguments, returning its stdout.
type GitFn<'a> = dyn Fn(&[&str]) -> Result<Vec<u8>, String> + 'a;

/// One revision's tree (workspace-relative paths).
struct Tree {
    files: HashSet<String>,
    build_dirs: HashSet<String>,
    /// Symbolic links and their workspace-relative targets.
    symlinks: Vec<(String, String)>,
    ignored: Vec<String>,
}

/// `path` with `.` and `..` resolved (`None`: it leaves the workspace).
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

impl Tree {
    fn read(git: &GitFn<'_>, rev: &str, below: &str) -> Result<Self, String> {
        let listing = git(&["ls-tree", "-r", "-z", "--full-tree", rev])?;
        let mut tree = Tree {
            files: HashSet::new(),
            build_dirs: HashSet::new(),
            symlinks: Vec::new(),
            ignored: Vec::new(),
        };
        let mut links = Vec::new();
        for entry in listing.split(|byte| *byte == 0) {
            let entry = String::from_utf8_lossy(entry);
            let Some((meta, path)) = entry.split_once('\t') else {
                continue;
            };
            let Some(path) = strip_below(path, below) else {
                continue;
            };
            if is_build_file(&path) {
                tree.build_dirs.insert(parent(&path).to_owned());
            }
            if meta.starts_with("120000") {
                links.push((
                    path.clone(),
                    meta.rsplit(' ').next().unwrap_or("").to_owned(),
                ));
            }
            tree.files.insert(path);
        }
        for (path, blob) in links {
            let target = String::from_utf8_lossy(&git(&["cat-file", "blob", &blob])?).into_owned();
            if target.starts_with('/') {
                continue;
            }
            if let Some(resolved) = normalize(&format!("{}/{target}", parent(&path))) {
                tree.symlinks.push((path, resolved));
            }
        }
        if tree.files.contains(".bazelignore") {
            let text = git(&[
                "show",
                &format!("{rev}:{}", join_below(below, ".bazelignore")),
            ])?;
            tree.ignored = String::from_utf8_lossy(&text)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(|line| line.trim_end_matches('/').to_owned())
                .collect();
        }
        Ok(tree)
    }

    fn ignored(&self, path: &str) -> bool {
        self.ignored
            .iter()
            .any(|entry| path == entry || path.starts_with(&format!("{entry}/")))
    }

    /// The package of `path` (the nearest directory above it holding a
    /// BUILD file; `None`: no package, or an ignored directory).
    fn package_of(&self, path: &str) -> Option<String> {
        if self.ignored(path) {
            return None;
        }
        let mut dir = parent(path);
        loop {
            if self.build_dirs.contains(dir) {
                return Some(dir.to_owned());
            }
            if dir.is_empty() {
                return None;
            }
            dir = parent(dir);
        }
    }
}

fn strip_below(path: &str, below: &str) -> Option<String> {
    if below.is_empty() {
        return Some(path.to_owned());
    }
    path.strip_prefix(below)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(str::to_owned)
}

fn join_below(below: &str, path: &str) -> String {
    if below.is_empty() {
        path.to_owned()
    } else {
        format!("{below}/{path}")
    }
}

/// The string literals of Starlark (or rc) text that are main-repository
/// labels: `//p:n`, `@//p:n`, `@@//p:n` (normalized to `//p:n`), and in
/// MODULE.bazel (`relative`) also `:n`.
fn label_literals(text: &str, relative: bool) -> Vec<String> {
    let mut labels = Vec::new();
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let quote = bytes[at];
        if quote != b'"' && quote != b'\'' {
            at += 1;
            continue;
        }
        let Some(end) = text[at + 1..].find(quote as char) else {
            break;
        };
        let literal = &text[at + 1..at + 1 + end];
        at += end + 2;
        if literal.contains('\n') {
            continue;
        }
        if let Some(label) = main_label(literal) {
            labels.push(label);
        } else if relative && literal.starts_with(':') && literal.len() > 1 {
            labels.push(format!("//{literal}"));
        }
    }
    labels
}

/// `text` as a main-repository label (`//p:n`), if it is one.
fn main_label(text: &str) -> Option<String> {
    let rest = text
        .strip_prefix("@@//")
        .or_else(|| text.strip_prefix("@//"))
        .or_else(|| text.strip_prefix("//"))?;
    if rest
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\'')
    {
        return None;
    }
    Some(format!("//{rest}"))
}

/// The workspace path a label names (`//a/b:c/d` → `a/b/c/d`, `//a/b` →
/// `a/b/b`); `None` for a pattern.
fn label_path(label: &str) -> Option<String> {
    let rest = label.strip_prefix("//")?;
    if rest.ends_with("...") || rest.ends_with(":all") || rest.ends_with(":*") {
        return None;
    }
    let (package, name) = match rest.split_once(':') {
        Some((package, name)) => (package, name),
        None => (rest, rest.rsplit('/').next().unwrap_or(rest)),
    };
    Some(if package.is_empty() {
        name.to_owned()
    } else {
        format!("{package}/{name}")
    })
}

/// The `load()`ed main-repository `.bzl` files of a `.bzl` file at `path`.
fn bzl_loads(text: &str, path: &str) -> Vec<String> {
    let mut loads = Vec::new();
    for (index, _) in text.match_indices("load(") {
        // Only a call at the start of a line.
        if index > 0 && text.as_bytes()[index - 1] != b'\n' {
            continue;
        }
        let rest = &text[index + 5..];
        let rest = rest.trim_start();
        let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            continue;
        };
        let Some(end) = rest[1..].find(quote) else {
            continue;
        };
        let literal = &rest[1..1 + end];
        if let Some(label) = main_label(literal) {
            loads.extend(label_path(&label));
        } else if let Some(name) = literal.strip_prefix(':') {
            let dir = parent(path);
            loads.push(if dir.is_empty() {
                name.to_owned()
            } else {
                format!("{dir}/{name}")
            });
        }
    }
    loads
}

/// The rc lines a `command` (`build` or `test`) of `bazel-rs affected`
/// reads at a revision (`text` reads a workspace file): `startup`,
/// `common`, `always`, `build` (and `test`) lines, unconfigured or of a
/// config they or `forwarded` enable (`--config`, transitively; the host
/// platform configs of `--enable_platform_specific_config` always), with
/// `%workspace%` imports inlined in place. Two revisions whose lines are
/// equal configure the command alike: their rc difference is no change.
fn effective_rc(
    text: &dyn Fn(&str) -> Option<String>,
    command: &str,
    forwarded: &[String],
) -> Vec<String> {
    fn lines(
        text: &dyn Fn(&str) -> Option<String>,
        path: &str,
        depth: usize,
        out: &mut Vec<(String, String, String)>,
    ) {
        let Some(content) = text(path) else {
            out.push(("missing".to_owned(), String::new(), path.to_owned()));
            return;
        };
        let joined = content.replace("\\\n", " ");
        for line in joined.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (head, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let rest = rest.trim();
            if matches!(head, "import" | "try-import") {
                match rest.strip_prefix("%workspace%/") {
                    Some(path) if depth < 16 => lines(text, path, depth + 1, out),
                    _ => out.push((head.to_owned(), String::new(), rest.to_owned())),
                }
                continue;
            }
            let (command, config) = head.split_once(':').unwrap_or((head, ""));
            out.push((command.to_owned(), config.to_owned(), rest.to_owned()));
        }
    }
    let mut entries = Vec::new();
    lines(text, ".bazelrc", 0, &mut entries);
    let commands = [
        "startup",
        "common",
        "always",
        "build",
        command,
        "import",
        "try-import",
        "missing",
    ];
    let configs_of = |options: &str| -> Vec<String> {
        let words: Vec<&str> = options.split_whitespace().collect();
        let mut configs = Vec::new();
        for (at, word) in words.iter().enumerate() {
            if let Some(config) = word.strip_prefix("--config=") {
                configs.push(config.to_owned());
            } else if *word == "--config"
                && let Some(config) = words.get(at + 1)
            {
                configs.push((*config).to_owned());
            }
        }
        configs
    };
    let mut configs: HashSet<String> = ["", "linux", "macos", "windows", "freebsd", "openbsd"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    configs.extend(configs_of(&forwarded.join(" ")));
    loop {
        let before = configs.len();
        for (command, config, rest) in &entries {
            if commands.contains(&command.as_str()) && configs.contains(config) {
                configs.extend(configs_of(rest));
            }
        }
        if configs.len() == before {
            break;
        }
    }
    entries
        .into_iter()
        .filter(|(command, config, _)| {
            commands.contains(&command.as_str()) && configs.contains(config)
        })
        .map(|(command, config, rest)| format!("{command}:{config} {rest}"))
        .collect()
}

/// The scope input of a local change, or the reasons it is global.
pub struct Classified {
    /// Why a full analysis is needed (empty: scoped).
    pub global: Vec<String>,
    /// The scope file's body (without its header and mode).
    pub spec: String,
    /// Changed files, packages, `.bzl` files and roots (for the report).
    pub counts: (usize, usize, usize, usize),
}

/// Classifies the changes between `base` and `head` (commits) of the
/// repository at `top`, whose workspace is `below` it; `forwarded` are the
/// command's options (their labels are roots).
pub fn classify(
    top: &Path,
    below: &Path,
    base: &str,
    head: &str,
    command: &str,
    forwarded: &[String],
) -> Result<Classified, String> {
    let git = |args: &[&str]| -> Result<Vec<u8>, String> {
        let output = crate::spawn_lock::output(
            std::process::Command::new("git")
                .arg("-C")
                .arg(top)
                .args(args),
        )
        .map_err(|error| format!("cannot run git: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(output.stdout)
    };
    let below = below.to_string_lossy().trim_end_matches('/').to_owned();
    let show = |rev: &str, path: &str| -> Option<String> {
        git(&["show", &format!("{rev}:{}", join_below(&below, path))])
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    };
    let diff = git(&["diff", "--name-status", "-z", "--no-renames", base, head])?;
    let mut changes: Vec<(String, String)> = Vec::new();
    let mut fields = diff.split(|byte| *byte == 0);
    while let Some(status) = fields.next() {
        if status.is_empty() {
            continue;
        }
        let Some(path) = fields.next() else { break };
        changes.push((
            String::from_utf8_lossy(status).into_owned(),
            String::from_utf8_lossy(path).into_owned(),
        ));
    }
    let revs = [base, head];
    let trees = [
        Tree::read(&git, base, &below)?,
        Tree::read(&git, head, &below)?,
    ];
    let mut global = Vec::new();
    // What the configuration and the repositories see, at both revisions.
    let mut repository_files: HashSet<String> = HashSet::new();
    let mut repository_dirs: Vec<String> = Vec::new();
    let mut roots: BTreeSet<String> = BTreeSet::new();
    let mut rc_paths: HashSet<String> = HashSet::new();
    for (rev, tree) in revs.iter().zip(&trees) {
        // MODULE.bazel and its includes: every label is a root (registered
        // toolchains, nogo, ...) and every file it names a repository input.
        let mut module_files = vec!["MODULE.bazel".to_owned()];
        module_files.extend(
            tree.files
                .iter()
                .filter(|path| path.ends_with(".MODULE.bazel"))
                .cloned(),
        );
        let mut extension_bzl: Vec<String> = Vec::new();
        for file in &module_files {
            let Some(text) = show(rev, file) else {
                continue;
            };
            for label in label_literals(&text, true) {
                if let Some(path) = label_path(&label) {
                    if path.ends_with(".bzl") {
                        extension_bzl.push(path.clone());
                    }
                    repository_files.insert(path);
                }
                roots.insert(label);
            }
        }
        // The lockfile's recorded inputs (`FILE:@@//p:n`, `DIRENTS:@@//d`).
        if let Some(text) = show(rev, "MODULE.bazel.lock") {
            for (index, _) in text.match_indices("@@//") {
                let rest = &text[index..];
                let end = rest
                    .find(|c: char| c == '"' || c == '%' || c.is_whitespace())
                    .unwrap_or(rest.len());
                if let Some(label) = main_label(&rest[..end])
                    && let Some(path) = label_path(&label)
                {
                    if path.ends_with(".bzl") {
                        extension_bzl.push(path.clone());
                    }
                    repository_dirs.push(path.clone());
                    repository_files.insert(path);
                }
            }
        }
        // The module extensions' and repository rules' .bzl files and
        // their loads: repository code (a change is global); their labels
        // are repository inputs.
        let mut seen = HashSet::new();
        while let Some(path) = extension_bzl.pop() {
            if !seen.insert(path.clone()) {
                continue;
            }
            repository_files.insert(path.clone());
            let Some(text) = show(rev, &path) else {
                continue;
            };
            for label in label_literals(&text, false) {
                repository_files.extend(label_path(&label));
                roots.insert(label);
            }
            extension_bzl.extend(bzl_loads(&text, &path));
        }
        // rc files: the labels of every option are roots.
        let mut rc_files = vec![".bazelrc".to_owned()];
        let mut index = 0;
        while index < rc_files.len() {
            let file = rc_files[index].clone();
            index += 1;
            let Some(text) = show(rev, &file) else {
                continue;
            };
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('#') {
                    continue;
                }
                let mut words = line.split_whitespace();
                if let Some("import" | "try-import") = words.next() {
                    if let Some(path) = words.next()
                        && let Some(path) = path.strip_prefix("%workspace%/")
                    {
                        rc_paths.insert(path.to_owned());
                        rc_files.push(path.to_owned());
                    }
                    continue;
                }
                roots.extend(option_labels(line));
            }
        }
        // .bzl files defining transitions: their labels can become
        // configuration values (`--platforms`, label flags).
        if let Ok(listing) = git(&[
            "grep",
            "-l",
            "-z",
            "-F",
            "transition(",
            rev,
            "--",
            &join_below(&below, "*.bzl"),
        ]) {
            for entry in listing.split(|byte| *byte == 0) {
                let entry = String::from_utf8_lossy(entry);
                let Some((_, path)) = entry.split_once(':') else {
                    continue;
                };
                let Some(path) = strip_below(path, &below) else {
                    continue;
                };
                if let Some(text) = show(rev, &path) {
                    roots.extend(label_literals(&text, false));
                }
            }
        }
    }
    for option in forwarded {
        roots.extend(option_labels(option));
    }
    // rc files: a change the command does not see is no change.
    rc_paths.insert(".bazelrc".to_owned());
    let same_rc = {
        let base_rc = effective_rc(&|path| show(base, path), command, forwarded);
        let head_rc = effective_rc(&|path| show(head, path), command, forwarded);
        base_rc == head_rc
    };
    // The changes.
    let mut packages: BTreeSet<String> = BTreeSet::new();
    let mut bzl: BTreeSet<String> = BTreeSet::new();
    let mut files: BTreeSet<String> = BTreeSet::new();
    for (status, path) in &changes {
        let Some(path) = strip_below(path, &below) else {
            global.push(format!("{path}: outside the workspace"));
            continue;
        };
        let name = path.rsplit('/').next().unwrap_or(&path);
        if rc_paths.contains(&path) {
            if !same_rc {
                global.push(format!("{path}: changes the command's rc options"));
                continue;
            }
            // Read by no rule unless a package holds it: a plain file then.
            if trees.iter().all(|tree| tree.package_of(&path).is_none()) {
                continue;
            }
        } else if bazel_file(name) {
            global.push(format!("{path}: a Bazel configuration file"));
            continue;
        }
        if manifest(name) {
            global.push(format!("{path}: a package-manager manifest"));
            continue;
        }
        if repository_files.contains(&path)
            || repository_dirs
                .iter()
                .any(|dir| path.starts_with(&format!("{dir}/")))
        {
            global.push(format!(
                "{path}: read by a module extension or repository rule"
            ));
            continue;
        }
        let homes: Vec<Option<String>> = trees.iter().map(|tree| tree.package_of(&path)).collect();
        if homes.iter().all(Option::is_none) {
            global.push(format!("{path}: in no package"));
            continue;
        }
        // A file only one revision has, or a BUILD file: the package's
        // globs or rules may differ.
        let added_or_removed = !status.starts_with('M');
        for home in homes.iter().flatten() {
            if added_or_removed || is_build_file(&path) {
                packages.insert(home.clone());
            }
        }
        if is_build_file(&path) && added_or_removed {
            // A subpackage appearing or disappearing changes its parent's
            // globs; a BUILD file at the root has no parent.
            let dir = parent(&path);
            if !dir.is_empty() {
                for tree in &trees {
                    if let Some(home) = tree.package_of(&format!("{dir}/x")) {
                        packages.insert(home);
                    }
                    if let Some(home) = tree.package_of(dir) {
                        packages.insert(home);
                    }
                }
            }
        }
        if path.ends_with(".bzl") {
            bzl.insert(path.clone());
        }
        files.insert(path.clone());
    }
    // A symbolic link to a changed file (or to a directory holding one)
    // changed too.
    for tree in &trees {
        for (link, target) in &tree.symlinks {
            for file in files.clone() {
                if file == *target {
                    files.insert(link.clone());
                } else if let Some(rest) = file.strip_prefix(&format!("{target}/")) {
                    files.insert(format!("{link}/{rest}"));
                }
            }
        }
    }
    let mut spec = String::new();
    for package in &packages {
        spec.push_str(&format!("package\t{package}\n"));
    }
    for path in &bzl {
        spec.push_str(&format!("bzl\t{path}\n"));
    }
    for path in &files {
        spec.push_str(&format!("file\t{path}\n"));
    }
    for root in &roots {
        spec.push_str(&format!("root\t{root}\n"));
    }
    Ok(Classified {
        global,
        spec,
        counts: (files.len(), packages.len(), bzl.len(), roots.len()),
    })
}

/// The main-repository labels in an option or rc line (`--platforms=//p`,
/// `--//flag:x=1`, `--extra_toolchains=//a:b,//c:d`).
fn option_labels(line: &str) -> Vec<String> {
    line.split(|c: char| c.is_whitespace() || matches!(c, '=' | ',' | '"' | '\''))
        .filter_map(|word| {
            let word = word.trim_start_matches("--no").trim_start_matches("--");
            main_label(word)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_literals_and_paths() {
        assert_eq!(
            label_literals(
                "x = \"//a/b:c.txt\"\ny = '@@//d'\nz = \":e\"\nurl = \"https://x//y\"\n",
                true
            ),
            ["//a/b:c.txt", "//d", "//:e"]
        );
        assert_eq!(label_literals("z = \":e\"", false), Vec::<String>::new());
        assert_eq!(label_path("//a/b:c/d").as_deref(), Some("a/b/c/d"));
        assert_eq!(label_path("//a/b").as_deref(), Some("a/b/b"));
        assert_eq!(label_path("//:x").as_deref(), Some("x"));
        assert_eq!(label_path("//a/..."), None);
        assert_eq!(label_path("//a:all"), None);
    }

    #[test]
    fn option_labels_skip_urls() {
        assert_eq!(
            option_labels("build --platforms=//platforms:x --remote_cache=https://h/x"),
            ["//platforms:x"]
        );
        assert_eq!(
            option_labels("build --//flags:f=1 --@r//s:t=//src:u"),
            ["//flags:f", "//src:u"]
        );
        assert_eq!(
            option_labels("--extra_toolchains=//a:b,//c:all"),
            ["//a:b", "//c:all"]
        );
    }

    #[test]
    fn loads_of_a_bzl_file() {
        assert_eq!(
            bzl_loads(
                "load(\"//a:b.bzl\", \"x\")\nload(':c.bzl', 'y')\nload(\"@r//:d.bzl\", \"z\")\n",
                "e/f.bzl"
            ),
            ["a/b.bzl", "e/c.bzl"]
        );
    }

    #[test]
    fn effective_rc_lines() {
        let files = |base: &'static str, extra: &'static str| {
            move |path: &str| match path {
                ".bazelrc" => Some(base.to_owned()),
                "tools/ci.bazelrc" => Some(extra.to_owned()),
                _ => None,
            }
        };
        let rc = "build --a\\\n  --b\n# c\ntest:replay --x\nbuild:ci --y\nbuild --config=ci\nimport %workspace%/tools/ci.bazelrc\n";
        let lines = effective_rc(&files(rc, "test --z\n"), "test", &[]);
        assert_eq!(
            lines,
            [
                "build: --a   --b",
                "build:ci --y",
                "build: --config=ci",
                "test: --z"
            ]
        );
        // An unused config's line is no difference; a used one is.
        let more = format!("{rc}test:other --w\n");
        let more: &'static str = Box::leak(more.into_boxed_str());
        assert_eq!(effective_rc(&files(more, "test --z\n"), "test", &[]), lines);
        assert_ne!(
            effective_rc(
                &files(more, "test --z\n"),
                "test",
                &["--config=other".to_owned()]
            ),
            lines
        );
        // `build` does not read `test` lines.
        assert_eq!(
            effective_rc(&files(rc, "test --z\n"), "build", &[]).len(),
            3
        );
    }

    #[test]
    fn classification_helpers() {
        assert!(bazel_file("MODULE.bazel"));
        assert!(bazel_file("go.MODULE.bazel"));
        assert!(bazel_file("user.bazelrc"));
        assert!(!bazel_file("BUILD.bazel"));
        assert!(manifest("requirements_lock.txt"));
        assert!(manifest("go.mod"));
        assert!(!manifest("main.go"));
        assert_eq!(normalize("a/b/../c").as_deref(), Some("a/c"));
        assert_eq!(normalize("../x"), None);
    }
}
