//! The analysis-snapshot recording discipline (L2-W3, PLAN-L2 §1b "making
//! 'forgot to record X' fail safe", layer 1): loading, module resolution and
//! analysis observe the filesystem and the environment only through
//! `crate::observed_fs`, which records every observation of the planning
//! window so a snapshot can be re-validated without re-running planning.
//!
//! A production module of `src/` may not call `std::fs` readers
//! (`read`, `read_to_string`, `read_dir`, `metadata`, `symlink_metadata`,
//! `canonicalize`, `read_link`, `exists`), `File::open`, `OpenOptions`, the
//! path methods `exists`, `try_exists`, `metadata`, `symlink_metadata`,
//! `read_dir`, `canonicalize`, `read_link`, `is_file`, `is_dir`,
//! `is_symlink`, or `std::env::var*` — outside `#[cfg(test)]` code — unless
//! it is one of the execution-phase or storage modules below (they run after
//! planning, or record what they verify through observed_fs' semantic hooks)
//! or the single use carries a `// snapshot-unrecorded: <reason>` comment on
//! its line or the comment lines directly above it. `Metadata`/`FileType`
//! predicates (`metadata.is_dir()`, `kind.is_file()`,
//! `entry.file_type()?.is_symlink()`) are no filesystem calls; the check
//! recognizes them by their receiver (`metadata`, `meta`, `kind`, `ft`,
//! `file_type()`, `*_metadata`, `*_kind`, `*_type`) and flags any other
//! receiver.
//!
//! Like `tests/spawn_discipline.rs` this is a textual check: it strips
//! comments and string literals, skips `#[cfg(test)]` items (and files
//! declared as `#[cfg(test)] mod name;`), and scans the rest.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// Execution-phase and storage modules (PLAN-L2 §1b item 1), plus the
/// recorder itself (`observed_fs`, `snapshot/*`).
const ALLOWED_MODULES: &[&str] = &[
    "action",
    "sandbox",
    // Persistent workers: their working directories and logs (execution).
    "worker",
    "scheduler",
    "run_command",
    "spawn_lock",
    "archive",
    "materializer",
    "repository_store",
    // The repositories' directories (slots) of the store: locks, links,
    // markers, copies; never an input of planning (the snapshot names
    // trees only, docs/REPOSITORY_SLOTS.md).
    "repository_slots",
    "builtin_tools",
    "patching",
    "repository_ctx",
    // The shared `--repository_cache`: content-addressed blobs verified
    // against their digests, like the native download store; a miss is a
    // network fetch, which taints (docs/REPOSITORY_CACHE.md).
    "repository_cache",
    "download_proxy",
    "credentials",
    "remote_cache",
    "reapi",
    "memory_limit",
    "bazelrc",
    "main",
    "workspace_status",
    "runfiles_tree",
    "lib",
    "observed_fs",
    // The execution root's output tree, written after execution.
    "output_tree",
    // Directory stamps of published trees and of the output tree, checked
    // by the executor's cache checks and the output tree update.
    "dir_stamps",
    // `info` reads no planning input (it never plans).
    "info",
    // The output base lock (taken before the recording window) and
    // `clean` (never plans).
    "output_base",
    // Bootstrap before the recording window: the git layout of the
    // workspace decides the cache root, which is part of the snapshot key
    // (docs/WORKTREES.md).
    "worktree",
    // The cache's garbage collection (docs/CACHE_GC.md): the lock and the
    // last-use records around the command, the collection after it.
    "cache_gc",
    // The execution log (docs/PROFILE.md): digests of spawn outputs, read
    // by the executor after the spawn.
    "exec_log",
    // The profile (docs/PROFILE.md): this process's CPU times and the
    // profile file, written when the command ends.
    "profile", // The coverage output generator and the combined report
    // (docs/COVERAGE.md): tracefiles read inside a test action and after
    // the tests, never during planning.
    "coverage",
    // `affected` (docs/AFFECTED_TARGETS.md): the target digests hash the
    // planned actions' source inputs after planning (the recording window
    // is closed), and the orchestrator never plans itself.
    "affected",
];
const ALLOWED_PREFIXES: &[&str] = &[
    "test_",
    "git_",
    "extension_",
    "snapshot/",
    "action/",
    "cache_gc/",
    // The namespace sandbox (sandbox/namespaces.rs): the spawn's mount plan
    // and the worker's mounts, at execution.
    "sandbox/",
    // The build event files (docs/BEP.md): opened before the recording
    // window, the built outputs' digests read after execution.
    "bep/",
    // repository_ctx's submodules (its non-blocking downloads' transfers).
    "repository_ctx/",
    // `affected`'s submodules: the remote base digests (the orchestrator's
    // key, and a helper process that never plans).
    "affected/",
];

/// Diagnostics switches and test hooks: never inputs of planning (runs with
/// a test or debug hook are never saved, PLAN-L2 §1a).
const DIAGNOSTIC_ENV: &[&str] = &[
    "BAZEL_RS_TIMING",
    "BAZEL_RS_TIMING_MODULES",
    "BAZEL_RS_DUMP_ACTIONS",
    "BAZEL_RS_LOADING_JITTER",
    "BAZEL_RS_LOADING_NO_DEMAND",
    // The analysis evaluator's completion-order jitter (docs/EVALUATOR.md).
    "BAZEL_RS_EVAL_JITTER",
    // Integration-20 (taint TestHook, crate::snapshot::record): the
    // analysis self-checks of the memoized dependency views and the
    // artifact index (they only ever fail a command), and the persistent
    // digest table's switch (`off` bypasses it; every digest is the same).
    "BAZEL_RS_VERIFY_VIEWS",
    "BAZEL_RS_VERIFY_ARTIFACTS",
    "BAZEL_RS_DIGEST_MEMO",
    // perf-w0a: the retained-memory breakdown (stderr only).
    "BAZEL_RS_TIMING_ALLOC",
    // W6R: phase-1 prefetch off while the analysis snapshot records (a
    // diagnostics switch; every recording is the same with it).
    "BAZEL_RS_SNAPSHOT_PREFETCH",
];
const DIAGNOSTIC_ENV_PREFIXES: &[&str] = &["BAZEL_RS_DEBUG_"];

const ESCAPE: &str = "snapshot-unrecorded:";

fn allowed_module(module: &str) -> bool {
    ALLOWED_MODULES.contains(&module)
        || ALLOWED_PREFIXES
            .iter()
            .any(|prefix| module.starts_with(prefix))
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `text` with comments and the contents of string and char literals
/// replaced by spaces (newlines kept), so byte offsets and lines match.
fn strip(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for byte in &mut out[from..to] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        if rest.starts_with(b"//") {
            let end = rest
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |n| i + n);
            blank(&mut out, i, end);
            i = end;
        } else if rest.starts_with(b"/*") {
            let mut depth = 0usize;
            let mut j = i;
            while j < bytes.len() {
                if bytes[j..].starts_with(b"/*") {
                    depth += 1;
                    j += 2;
                } else if bytes[j..].starts_with(b"*/") {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            blank(&mut out, i, j);
            i = j;
        } else if (rest.starts_with(b"r\"") || rest.starts_with(b"r#") || rest.starts_with(b"br"))
            && (i == 0 || !is_ident(bytes[i - 1]))
            && raw_string_start(rest).is_some()
        {
            let (open, hashes) = raw_string_start(rest).unwrap();
            let mut close = vec![b'"'];
            close.extend(std::iter::repeat_n(b'#', hashes));
            let body = i + open;
            let end = bytes[body..]
                .windows(close.len())
                .position(|window| window == close.as_slice())
                .map_or(bytes.len(), |n| body + n + close.len());
            blank(&mut out, i, end);
            i = end;
        } else if bytes[i] == b'"' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'"' {
                j += if bytes[j] == b'\\' { 2 } else { 1 };
            }
            blank(&mut out, i + 1, j.min(bytes.len()));
            i = j + 1;
        } else if bytes[i] == b'\'' {
            // A char literal ('x', '\n', '\u{..}') or a lifetime ('a).
            let end = if rest.len() >= 3 && rest[1] == b'\\' {
                rest[2..]
                    .iter()
                    .position(|byte| *byte == b'\'')
                    .map(|n| i + 2 + n + 1)
            } else if rest.len() >= 3 && rest[2] == b'\'' {
                Some(i + 3)
            } else {
                // Multi-byte UTF-8 char literal or a lifetime.
                let close = rest[1..].iter().take(5).position(|byte| *byte == b'\'');
                match close {
                    Some(n) if !rest[1..=n].iter().all(|byte| is_ident(*byte)) => Some(i + n + 2),
                    _ => None,
                }
            };
            match end {
                Some(end) => {
                    blank(&mut out, i + 1, end - 1);
                    i = end;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    out
}

/// `r"`, `r#"`, `br"`, `br#"`: the opening length and the number of hashes.
fn raw_string_start(rest: &[u8]) -> Option<(usize, usize)> {
    let mut i = if rest.starts_with(b"br") { 2 } else { 1 };
    let mut hashes = 0;
    while rest.get(i) == Some(&b'#') {
        hashes += 1;
        i += 1;
    }
    (rest.get(i) == Some(&b'"')).then_some((i + 1, hashes))
}

/// Byte ranges of `#[cfg(test)]` / `#[cfg(all(test, ...))]` items.
fn test_ranges(code: &[u8]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let text = String::from_utf8_lossy(code);
    for marker in ["#[cfg(test)]", "#[cfg(all(test"] {
        let mut from = 0;
        while let Some(found) = text[from..].find(marker) {
            let start = from + found;
            // Past the attribute's closing `]`.
            let mut nesting = 0usize;
            let mut close = start + 1;
            while close < code.len() {
                match code[close] {
                    b'[' | b'(' => nesting += 1,
                    b']' | b')' => {
                        nesting -= 1;
                        if nesting == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                close += 1;
            }
            from = close + 1;
            // The item: up to its `;`/`,` (a declaration, a field, an
            // expression statement) or its closing `}` (a block item), at
            // bracket depth 0; a closer at depth 0 ends an enclosing block.
            let mut depth = 0usize;
            let mut i = from;
            let mut end = None;
            while i < code.len() {
                match code[i] {
                    b'{' | b'(' | b'[' => depth += 1,
                    b'}' | b')' | b']' if depth == 0 => {
                        end = Some(i);
                        break;
                    }
                    b'}' | b')' | b']' => {
                        depth -= 1;
                        if depth == 0 && code[i] == b'}' {
                            end = Some(i + 1);
                            break;
                        }
                    }
                    b';' | b',' if depth == 0 => {
                        end = Some(i + 1);
                        break;
                    }
                    _ => {}
                }
                i += 1;
            }
            ranges.push((start, end.unwrap_or(code.len())));
        }
    }
    ranges
}

/// Module files declared `#[cfg(test)] mod name;` (whole files of tests).
fn test_module_files(path: &Path, code: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(code);
    let mut files = Vec::new();
    for (start, end) in test_ranges(code) {
        let item = text[start..end].trim_end();
        if let Some(name) = item
            .strip_suffix(';')
            .and_then(|item| item.split_whitespace().last())
            && item.split_whitespace().any(|word| word == "mod")
        {
            let parent = path.with_extension("");
            files.push(parent.join(format!("{name}.rs")));
        }
    }
    files
}

/// Receivers of `.is_file()`/`.is_dir()`/`.is_symlink()` that are
/// `Metadata`/`FileType` values, not paths.
fn metadata_receiver(code: &[u8], dot: usize) -> bool {
    let mut end = dot;
    while end > 0 && code[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    if end > 0 && code[end - 1] == b')' {
        // A call: `x.file_type()` (a FileType) only.
        let mut depth = 0usize;
        let mut i = end;
        while i > 0 {
            i -= 1;
            match code[i] {
                b')' => depth += 1,
                b'(' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        let name_end = i;
        let mut name_start = name_end;
        while name_start > 0 && is_ident(code[name_start - 1]) {
            name_start -= 1;
        }
        return &code[name_start..name_end] == b"file_type";
    }
    let mut start = end;
    while start > 0 && is_ident(code[start - 1]) {
        start -= 1;
    }
    let name = String::from_utf8_lossy(&code[start..end]);
    matches!(name.as_ref(), "metadata" | "meta" | "kind" | "ft")
        || name.ends_with("_metadata")
        || name.ends_with("_kind")
        || name.ends_with("_type")
}

/// The string literal argument of an `env::var*(` call at `open` (the
/// offset of `(`), from the raw text.
fn env_literal(raw: &str, open: usize) -> Option<&str> {
    let rest = raw[open + 1..].trim_start();
    let rest = rest.strip_prefix('"')?;
    Some(&rest[..rest.find('"')?])
}

fn diagnostic_env(name: &str) -> bool {
    DIAGNOSTIC_ENV.contains(&name)
        || DIAGNOSTIC_ENV_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

const FS_FUNCTIONS: &[&str] = &[
    "read",
    "read_to_string",
    "read_dir",
    "metadata",
    "symlink_metadata",
    "canonicalize",
    "read_link",
    "exists",
];
const PATH_METHODS: &[&str] = &[
    "exists",
    "try_exists",
    "metadata",
    "symlink_metadata",
    "read_dir",
    "canonicalize",
    "read_link",
];
const PREDICATES: &[&str] = &["is_file", "is_dir", "is_symlink"];

/// Every unrecorded observation in one source text: `(line, what)`.
fn offenders(raw: &str) -> Vec<(usize, String)> {
    let code = strip(raw);
    let tests = test_ranges(&code);
    let in_test = |at: usize| {
        tests
            .iter()
            .any(|(start, end)| (*start..*end).contains(&at))
    };
    let lines: Vec<&str> = raw.split('\n').collect();
    let line_of = |at: usize| code[..at].iter().filter(|byte| **byte == b'\n').count();
    let escaped = |line: usize| {
        if lines[line].contains(ESCAPE) {
            return true;
        }
        let mut above = line;
        while above > 0 {
            above -= 1;
            let text = lines[above].trim_start();
            if !text.starts_with("//") {
                break;
            }
            if text.contains(ESCAPE) {
                return true;
            }
        }
        false
    };
    let mut found = Vec::new();
    let mut report = |at: usize, what: String| {
        if in_test(at) {
            return;
        }
        let line = line_of(at);
        if !escaped(line) {
            found.push((line + 1, what));
        }
    };
    let text = String::from_utf8_lossy(&code).into_owned();
    let occurrences = |needle: &str| -> Vec<usize> {
        let mut at = Vec::new();
        let mut from = 0;
        while let Some(found) = text[from..].find(needle) {
            at.push(from + found);
            from += found + needle.len();
        }
        at
    };
    let boundary = |at: usize| at == 0 || !is_ident(code[at - 1]);
    for function in FS_FUNCTIONS {
        for at in occurrences(&format!("fs::{function}(")) {
            if boundary(at) {
                report(at, format!("fs::{function}"));
            }
        }
    }
    for needle in ["File::open(", "OpenOptions::new("] {
        for at in occurrences(needle) {
            if boundary(at) {
                report(at, needle.trim_end_matches('(').to_owned());
            }
        }
    }
    for method in PATH_METHODS {
        for at in occurrences(&format!(".{method}()")) {
            report(at, format!(".{method}()"));
        }
        // Calls with arguments do not exist for these methods; a
        // `.metadata(` of another type would still be flagged above.
    }
    for predicate in PREDICATES {
        for at in occurrences(&format!(".{predicate}()")) {
            if !metadata_receiver(&code, at) {
                report(at, format!(".{predicate}() on a path"));
            }
        }
    }
    for function in ["var", "var_os", "vars", "vars_os"] {
        for at in occurrences(&format!("env::{function}(")) {
            if !boundary(at) {
                continue;
            }
            let open = at + "env::".len() + function.len();
            if env_literal(raw, open).is_some_and(diagnostic_env) {
                continue;
            }
            report(at, format!("env::{function}"));
        }
    }
    // Direct imports of the readers (`use std::fs::read_to_string;`).
    for at in occurrences("use std::fs::") {
        let end = text[at..].find(';').map_or(text.len(), |n| at + n);
        let imported = &text[at + "use std::fs::".len()..end];
        let names: BTreeSet<&str> = imported
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .collect();
        if FS_FUNCTIONS.iter().any(|name| names.contains(name)) {
            report(at, format!("use std::fs::{}", imported.trim()));
        }
    }
    for at in occurrences("use std::env::") {
        let end = text[at..].find(';').map_or(text.len(), |n| at + n);
        let imported = &text[at + "use std::env::".len()..end];
        let names: BTreeSet<&str> = imported
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .collect();
        if ["var", "var_os", "vars", "vars_os"]
            .iter()
            .any(|name| names.contains(name))
        {
            report(at, format!("use std::env::{}", imported.trim()));
        }
    }
    found.sort();
    found.dedup();
    found
}

fn sources(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn planning_reads_go_through_observed_fs() {
    let src = Path::new(ROOT).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    files.sort();
    let mut test_files = BTreeSet::new();
    let mut texts = Vec::new();
    for path in &files {
        let raw = fs::read_to_string(path).unwrap();
        test_files.extend(test_module_files(path, &strip(&raw)));
        texts.push((path.clone(), raw));
    }
    let mut problems = Vec::new();
    for (path, raw) in &texts {
        if test_files.contains(path) {
            continue;
        }
        let module = path
            .strip_prefix(&src)
            .unwrap()
            .with_extension("")
            .to_string_lossy()
            .into_owned();
        if allowed_module(&module) {
            continue;
        }
        for (line, what) in offenders(raw) {
            problems.push(format!("src/{module}.rs:{line}: {what}"));
        }
    }
    assert!(
        problems.is_empty(),
        "unrecorded filesystem/environment observations outside the allowlist \
         (route them through crate::observed_fs, or justify with `// {ESCAPE} <reason>`):\n{}",
        problems.join("\n")
    );
}

#[test]
fn every_allowlisted_module_exists() {
    // A renamed module must not silently keep (or lose) its exemption.
    let src = Path::new(ROOT).join("src");
    for module in ALLOWED_MODULES {
        assert!(
            src.join(format!("{module}.rs")).is_file(),
            "allowlisted module src/{module}.rs does not exist"
        );
    }
}

#[test]
fn the_scanner_finds_what_it_must() {
    let sample = r#"
use std::fs;
use std::fs::read_to_string;
fn production(path: &std::path::Path) {
    let _ = fs::read(path);
    let _ = std::fs::metadata(path);
    let _ = crate::observed_fs::metadata(path);
    let _ = path.exists();
    let _ = path.join("BUILD").is_file();
    let _ = metadata.is_dir() || kind.is_file() || entry.file_type().is_symlink();
    let _ = std::env::var_os("HOME");
    let _ = std::env::var_os("BAZEL_RS_TIMING");
    let _ = std::env::var("BAZEL_RS_DEBUG_PROVENANCE");
    // snapshot-unrecorded: documented exception
    let _ = std::fs::File::open(path);
    let _ = std::fs::read_link(path); // snapshot-unrecorded: same line
    let text = "fs::read(not code)"; // fs::read(comment)
    let _ = '"'; let _ = std::fs::canonicalize(path);
}
#[cfg(test)]
mod tests {
    fn helper(path: &std::path::Path) { let _ = std::fs::read(path); }
}
#[cfg(all(test, unix))]
fn unix_helper(path: &std::path::Path) { let _ = path.is_dir(); }
"#;
    let found: Vec<String> = offenders(sample)
        .into_iter()
        .map(|(line, what)| format!("{line}:{what}"))
        .collect();
    assert_eq!(
        found,
        [
            "3:use std::fs::read_to_string",
            "5:fs::read",
            "6:fs::metadata",
            "8:.exists()",
            "9:.is_file() on a path",
            "11:env::var_os",
            "18:fs::canonicalize",
        ]
    );
}
