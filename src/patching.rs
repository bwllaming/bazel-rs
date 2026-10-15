//! Bounded native UTF-8 unified text patches, not full PatchUtil parity.
//!
//! Sections run in input order, hunks in reverse source order. Matching uses
//! exact context with nearest-offset search (earlier wins ties), never fuzz.
//! Java-style CR/LF/CRLF input lines become LF-terminated output lines. Empty
//! patches are no-ops. Git mail preambles and ordinary index records work;
//! rename/copy, binary, combined diffs, quoted Git paths, and all explicit Git
//! mode changes (including new/deleted file mode) are explicitly unsupported.
//! Creation/deletion instead use ordinary /dev/null unified headers.
//!
//! Paths are checked before and after stripping, even for skipped sections.
//! This intentionally rejects more paths than upstream. Limits: 16 MiB per
//! patch or file, 100000 lines/hunks, 256 MiB tree working set, and 100 million
//! context-line comparisons per section. Non-UTF-8 and NUL text are unsupported.
//!
//! Tree roots and their ancestors must be private and trusted against concurrent
//! mutation. All affected paths and contents are preflighted before any writes;
//! this does NOT publish a repository or promise rollback on filesystem errors.
//! The caller must discard its staging tree on failure. Like Bazel's PatchUtil
//! (`applyPatchToFile`, PatchUtil.java:200-260: read the old file following
//! links, `oldFile.delete()`, then write a fresh file), a patched path that is a
//! symlink or a hard-linked file is read through the link and then replaced by
//! a new regular file: the referent and other hard-link names are never
//! written. Native reads follow only links resolving inside the tree; a leaf
//! link escaping it, symlinked ancestor directories and special files are
//! rejected. Existing permissions (of the referent) become 0755 if
//! owner-executable, otherwise 0644; new files are 0644, directories 0755.
//! File-to-directory transitions within one patch are unsupported.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_LINES: usize = 100_000;
const MAX_TREE_BYTES: usize = 256 * 1024 * 1024;
const MAX_COMPARISONS: usize = 100_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch {
    sections: Vec<Section>,
    strip: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Section {
    old: Option<String>,
    new: Option<String>,
    hunks: Vec<Hunk>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Hunk {
    position: usize,
    old: Vec<String>,
    new: Vec<String>,
}

fn text_lines(bytes: &[u8]) -> Result<Vec<&str>, String> {
    if bytes.len() > MAX_BYTES {
        return Err("patch/text exceeds 16 MiB limit".into());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "unsupported non-UTF-8 patch/text")?;
    if text.contains('\0') {
        return Err("unsupported NUL/binary patch/text".into());
    }
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if matches!(bytes[i], b'\r' | b'\n') {
            lines.push(&text[start..i]);
            if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                i += 1;
            }
            start = i + 1;
            if lines.len() > MAX_LINES {
                return Err("patch/text exceeds 100000 line limit".into());
            }
        }
        i += 1;
    }
    if start < bytes.len() {
        lines.push(&text[start..]);
    }
    if lines.len() > MAX_LINES {
        return Err("patch/text exceeds 100000 line limit".into());
    }
    Ok(lines)
}

fn safe_path(path: &str) -> Result<Vec<&str>, String> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(['\\', '\0', ':', '"'])
        || path.split('/').any(|part| part == "..")
    {
        return Err(format!("unsafe or unsupported patch path: {path:?}"));
    }
    let parts: Vec<_> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.is_empty() {
        return Err("empty normalized patch path".into());
    }
    Ok(parts)
}

fn filename(raw: &str, strip: usize) -> Result<Option<String>, String> {
    let path = raw.split('\t').next().unwrap_or("").trim();
    if path == "/dev/null" {
        return Ok(None);
    }
    // Strip lexical slash-separated components first. Normalizing away `./`
    // or empty components first can redirect a patch to a different file.
    safe_path(path)?;
    let stripped = if strip == 0 {
        path
    } else {
        path.match_indices('/')
            .nth(strip - 1)
            .map_or("", |(at, _)| &path[at + 1..])
    };
    if stripped.is_empty() {
        return Err(format!(
            "cannot determine patch file name with strip = {strip}: {path:?}"
        ));
    }
    Ok(Some(safe_path(stripped)?.join("/")))
}

fn range(raw: &str, sign: char) -> Result<(usize, usize), String> {
    let raw = raw.strip_prefix(sign).ok_or("malformed hunk range")?;
    let (start, count) = raw.split_once(',').unwrap_or((raw, "1"));
    let number = |s: &str| -> Result<usize, String> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err("malformed hunk range number".into());
        }
        s.parse().map_err(|_| "hunk range number overflow".into())
    };
    let (start, count) = (number(start)?, number(count)?);
    if count > MAX_LINES || start > i32::MAX as usize || start.checked_add(count).is_none() {
        return Err("hunk range exceeds supported limits".into());
    }
    if start == 0 && count != 0 {
        return Err("hunk start zero requires count zero".into());
    }
    Ok((start, count))
}

fn hunk(lines: &[&str], i: &mut usize) -> Result<(Hunk, usize, usize), String> {
    let header = lines[*i];
    let rest = header.strip_prefix("@@").ok_or("malformed hunk header")?;
    let end = rest.find("@@").ok_or("malformed hunk header")?;
    let fields: Vec<_> = rest[..end].split_whitespace().collect();
    if fields.len() != 2
        || !rest.starts_with(char::is_whitespace)
        || !rest[..end].ends_with(char::is_whitespace)
    {
        return Err("malformed hunk header".into());
    }
    let (old_start, old_count) = range(fields[0], '-')?;
    let (new_start, new_count) = range(fields[1], '+')?;
    let mut result = Hunk {
        position: if old_count == 0 {
            old_start
        } else {
            old_start - 1
        },
        old: Vec::new(),
        new: Vec::new(),
    };
    *i += 1;
    let mut marker_allowed = false;
    loop {
        if lines.get(*i) == Some(&"\\ No newline at end of file") {
            if !marker_allowed {
                return Err("misplaced no-newline marker".into());
            }
            marker_allowed = false;
            *i += 1;
            continue;
        }
        if result.old.len() == old_count && result.new.len() == new_count {
            break;
        }
        let line = *lines
            .get(*i)
            .ok_or("truncated hunk: expecting more lines")?;
        let kind = line.as_bytes().first().copied().unwrap_or(b' ');
        // Slice only ASCII prefixes, never an arbitrary first UTF-8 byte.
        let content = if line.is_empty() {
            ""
        } else if matches!(kind, b' ' | b'-' | b'+') {
            &line[1..]
        } else {
            return Err("malformed/truncated hunk body".into());
        };
        match kind {
            b' ' => {
                result.old.push(content.into());
                result.new.push(content.into());
            }
            b'-' => result.old.push(content.into()),
            b'+' => result.new.push(content.into()),
            _ => unreachable!(),
        }
        if result.old.len() > old_count || result.new.len() > new_count {
            return Err("hunk line count overflow".into());
        }
        marker_allowed = true;
        *i += 1;
    }
    Ok((result, new_start, new_count))
}

fn metadata(line: &str) -> Result<(), String> {
    if [
        "rename ",
        "copy ",
        "similarity index ",
        "dissimilarity index ",
        "old mode ",
        "new mode ",
        "new file mode ",
        "deleted file mode ",
        "GIT binary patch",
        "Binary files ",
        "diff --cc ",
        "diff --combined ",
        "*** ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix) || line == prefix.trim_end())
    {
        return Err(format!(
            "unsupported Git rename/copy/mode/binary/combined patch metadata: {line}"
        ));
    }
    if line == "index" {
        return Err("malformed Git index record".into());
    }
    if let Some(index) = line.strip_prefix("index ") {
        let fields: Vec<_> = index.split_whitespace().collect();
        if fields.is_empty() || fields.len() > 2 {
            return Err("malformed Git index record".into());
        }
        let (old, new) = fields[0]
            .split_once("..")
            .ok_or("malformed Git index hashes")?;
        if [old, new]
            .iter()
            .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err("malformed Git index hashes".into());
        }
        if fields.len() == 2 && !matches!(fields[1], "100644" | "100755") {
            return Err("unsupported Git index mode (only 100644/100755)".into());
        }
    }
    Ok(())
}

/// Parse and validate every section before any single-file filtering.
pub fn parse(bytes: &[u8], strip: usize) -> Result<Patch, String> {
    let lines = text_lines(bytes)?;
    let mut patch = Patch {
        sections: Vec::new(),
        strip,
    };
    let mut i = 0;
    let mut git_pending = false;
    let mut after_hunk = false;
    while i < lines.len() {
        let line = lines[i];
        metadata(line)?;
        if line == "diff --git" {
            return Err("malformed Git header".into());
        }
        if let Some(paths) = line.strip_prefix("diff --git ") {
            if git_pending {
                return Err("Git diff has no supported ---/+++ section".into());
            }
            let paths: Vec<_> = paths.split_whitespace().collect();
            if paths.len() != 2 {
                return Err("unsupported quoted or malformed Git paths".into());
            }
            for path in paths {
                if filename(path, strip)?.is_none() {
                    return Err("invalid /dev/null Git preamble path".into());
                }
            }
            git_pending = true;
            after_hunk = false;
            i += 1;
        } else if let Some(old) = line.strip_prefix("--- ") {
            if after_hunk {
                return Err("adjacent plain patch sections require an explicit separator".into());
            }
            let old = filename(old, strip)?;
            i += 1;
            let new = lines
                .get(i)
                .and_then(|l| l.strip_prefix("+++ "))
                .ok_or("patch requires paired ---/+++ file headers")?;
            let new = filename(new, strip)?;
            if old.is_none() && new.is_none() {
                return Err("patch has two /dev/null paths".into());
            }
            i += 1;
            let mut section = Section {
                old,
                new,
                hunks: Vec::new(),
            };
            let mut old_end = 0;
            let mut new_end = 0;
            while lines.get(i).is_some_and(|l| l.starts_with("@@")) {
                let (hunk, new_start, new_count) = hunk(&lines, &mut i)?;
                let new_position = if new_count == 0 {
                    new_start
                } else {
                    new_start - 1
                };
                if !section.hunks.is_empty() && (hunk.position < old_end || new_position < new_end)
                {
                    return Err("unsupported overlapping or unordered hunks".into());
                }
                old_end = hunk.position + hunk.old.len();
                new_end = new_position + hunk.new.len();
                if section.old.is_none() && (!hunk.old.is_empty() || hunk.position != 0) {
                    return Err("creation /dev/null side must have range -0,0".into());
                }
                if section.new.is_none() && (!hunk.new.is_empty() || new_start != 0) {
                    return Err("deletion /dev/null side must have range +0,0".into());
                }
                section.hunks.push(hunk);
            }
            if section.hunks.is_empty() {
                return Err("unified patch section has no hunk".into());
            }
            if (section.old.is_none() || section.new.is_none()) && section.hunks.len() != 1 {
                return Err("unsupported multiple-hunk creation/deletion".into());
            }
            patch.sections.push(section);
            git_pending = false;
            after_hunk = true;
        } else {
            if line.starts_with("@@") || line.starts_with("+++ ") || line.starts_with('\\') {
                return Err("malformed or orphaned hunk/header/marker".into());
            }
            if after_hunk
                && !matches!(line, "--" | "-- ")
                && (line.starts_with('+') || line.starts_with('-') || line.starts_with(' '))
            {
                return Err("hunk line count overflow or orphaned body line".into());
            }
            if !line.is_empty() && !line.starts_with("index ") {
                after_hunk = false;
            }
            i += 1;
        }
    }
    if git_pending {
        return Err("Git diff has no supported ---/+++ section".into());
    }
    Ok(patch)
}

impl Section {
    fn apply(&self, original: &[u8]) -> Result<Vec<u8>, String> {
        let mut lines: Vec<String> = text_lines(original)?
            .into_iter()
            .map(str::to_owned)
            .collect();
        let mut comparisons = 0usize;
        let mut output_size: usize = lines.iter().map(|l| l.len() + 1).sum();
        for hunk in self.hunks.iter().rev() {
            let mut best = None;
            if hunk.old.len() <= lines.len() {
                let end = lines.len() - hunk.old.len();
                if hunk.old.is_empty() {
                    if hunk.position <= end {
                        best = Some(hunk.position);
                    }
                } else {
                    let mut matches = |position: usize| -> Result<bool, String> {
                        for (actual, expected) in lines[position..].iter().zip(&hunk.old) {
                            comparisons += 1;
                            if comparisons > MAX_COMPARISONS {
                                return Err("patch context search exceeds comparison limit".into());
                            }
                            if actual != expected {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    };
                    if hunk.position <= end && matches(hunk.position)? {
                        best = Some(hunk.position);
                    } else {
                        // Search actual nearest offsets, skipping invalid indices;
                        // equal distances always try the earlier position first.
                        let mut left = hunk.position.min(end + 1);
                        let mut right = hunk.position.saturating_add(1).min(end + 1);
                        while left > 0 || right <= end {
                            let position = if left > 0
                                && (right > end
                                    || (left - 1).abs_diff(hunk.position)
                                        <= right.abs_diff(hunk.position))
                            {
                                left -= 1;
                                left
                            } else {
                                let position = right;
                                right += 1;
                                position
                            };
                            if matches(position)? {
                                best = Some(position);
                                break;
                            }
                        }
                    }
                }
            }
            let position =
                best.ok_or("patch requires unsupported fuzz or has mismatched context")?;
            let count = lines.len() - hunk.old.len() + hunk.new.len();
            if count > MAX_LINES {
                return Err("patched text exceeds 100000 line limit".into());
            }
            output_size -= hunk.old.iter().map(|l| l.len() + 1).sum::<usize>();
            output_size += hunk.new.iter().map(|l| l.len() + 1).sum::<usize>();
            if output_size > MAX_BYTES {
                return Err("patched text exceeds 16 MiB limit".into());
            }
            lines.splice(
                position..position + hunk.old.len(),
                hunk.new.iter().cloned(),
            );
        }
        let mut output = Vec::new();
        for line in lines {
            output.extend_from_slice(line.as_bytes());
            output.push(b'\n');
        }
        Ok(output)
    }
}

impl Patch {
    /// Only equal, root-level MODULE.bazel header pairs are applied. Creation,
    /// deletion, nested modules, and source files are skipped without reading
    /// their content. With no relevant sections, preserve original bytes exactly.
    pub fn apply_module(&self, original: &[u8]) -> Result<Vec<u8>, String> {
        let mut output = original.to_vec();
        for section in &self.sections {
            if self.strip == 0
                && section.old.as_deref() == Some("a/MODULE.bazel")
                && section.new.as_deref() == Some("b/MODULE.bazel")
            {
                return Err(
                    "patch contains a/b prefixes; did you forget to set patch_strip = 1?".into(),
                );
            }
            if section.old.as_deref() == Some("MODULE.bazel")
                && section.new.as_deref() == Some("MODULE.bazel")
            {
                output = section.apply(&output)?;
            }
        }
        Ok(output)
    }

    /// Preflight a complete patch, then mutate a caller-owned private stage.
    /// On any error the caller must discard the stage, never publish it.
    pub fn apply_tree(&self, root: &Path) -> Result<(), String> {
        check_root(root)?;
        let mut state: BTreeMap<String, Option<Contents>> = BTreeMap::new();
        // Include BOTH paths, even when the existing old file is preferred.
        for section in &self.sections {
            for path in [&section.old, &section.new].into_iter().flatten() {
                if !state.contains_key(path) {
                    state.insert(path.clone(), read_checked(root, path)?);
                    working_set(&state)?;
                }
            }
        }
        let mut changed = BTreeMap::new();
        for section in &self.sections {
            let old = section.old.as_ref().and_then(|p| state[p].as_ref());
            let new = section.new.as_ref().and_then(|p| state[p].as_ref());
            let (target, input, mode) = match (&section.old, old, new) {
                (None, _, Some(_)) => return Err("patch creation target already exists".into()),
                (None, _, None) => (
                    section.new.as_ref().expect("validated creation path"),
                    &[][..],
                    0o644,
                ),
                (Some(path), Some(contents), _) => (path, contents.bytes.as_slice(), contents.mode),
                (Some(_), None, Some(contents)) => (
                    section.new.as_ref().expect("existing new path"),
                    contents.bytes.as_slice(),
                    contents.mode,
                ),
                _ => return Err("cannot find file to patch".into()),
            };
            let bytes = section.apply(input)?;
            let contents = if section.new.is_none() {
                if !bytes.is_empty() {
                    return Err("deletion patch did not remove all content".into());
                }
                None
            } else {
                Some(Contents { bytes, mode })
            };
            state.insert(target.clone(), contents);
            changed.insert(target.clone(), ());
            working_set(&state)?;
        }
        for path in changed.keys() {
            let mut parent = Path::new(path).parent();
            while let Some(p) = parent {
                if state
                    .get(p.to_str().ok_or("non-UTF-8 patch parent")?)
                    .is_some_and(Option::is_some)
                {
                    return Err("patch output has a file/directory conflict".into());
                }
                parent = p.parent();
            }
        }
        // No mutation occurred before this point. I/O failures below require
        // caller rollback; no attempt is made to publish individual files.
        for path in changed.keys() {
            let destination = root.join(path);
            // `oldFile.delete()` before writing: an existing file, hard-link name
            // or symlink at the path is removed, never written through.
            if let Some(contents) = &state[path] {
                create_parents(root, Path::new(path))?;
                remove_leaf(&destination)?;
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&destination)
                    .map_err(|e| format!("write patch file {destination:?}: {e}"))?;
                file.write_all(&contents.bytes)
                    .map_err(|e| format!("write patch content: {e}"))?;
                set_mode(&destination, contents.mode)?;
            } else {
                remove_leaf(&destination)?;
            }
        }
        Ok(())
    }
}

struct Contents {
    bytes: Vec<u8>,
    mode: u32,
}

fn working_set(state: &BTreeMap<String, Option<Contents>>) -> Result<(), String> {
    if state
        .values()
        .flatten()
        .map(|c| c.bytes.len())
        .sum::<usize>()
        > MAX_TREE_BYTES
    {
        return Err("patch tree exceeds 256 MiB working-set limit".into());
    }
    Ok(())
}

fn directory(path: &Path) -> Result<(), String> {
    let meta =
        fs::symlink_metadata(path).map_err(|e| format!("inspect patch directory {path:?}: {e}"))?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(format!("unsafe patch directory/symlink: {path:?}"));
    }
    Ok(())
}

fn check_root(root: &Path) -> Result<(), String> {
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(root)
    };
    for ancestor in absolute.ancestors() {
        directory(ancestor)?;
    }
    Ok(())
}

fn read_checked(root: &Path, path: &str) -> Result<Option<Contents>, String> {
    let parts = safe_path(path)?;
    let mut current = root.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        current.push(part);
        let meta = match fs::symlink_metadata(&current) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("inspect patch path {current:?}: {e}")),
        };
        if meta.file_type().is_symlink() && i + 1 != parts.len() {
            return Err(format!("unsafe patch symlink: {current:?}"));
        }
        if i + 1 != parts.len() {
            if !meta.is_dir() {
                return Err("patch ancestor is not a directory".into());
            }
            continue;
        }
        let (current, meta) = if meta.file_type().is_symlink() {
            // A leaf link is read through (`oldFile.exists()`/`readFile` follow
            // it) and later replaced by the patched regular file.
            match crate::source_links::resolve_in_root(root, Path::new(path))
                .map_err(|e| format!("unsafe patch symlink {current:?}: {e}"))?
            {
                crate::source_links::Resolution::Missing(_) => return Ok(None),
                crate::source_links::Resolution::Resolved(referent) => {
                    let meta = fs::symlink_metadata(&referent)
                        .map_err(|e| format!("inspect patch link referent {referent:?}: {e}"))?;
                    (referent, meta)
                }
            }
        } else {
            (current, meta)
        };
        if !meta.is_file() {
            return Err("patch target is not a regular file".into());
        }
        let mut mode = 0o644;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o100 != 0 {
                mode = 0o755;
            }
        }
        if meta.len() > MAX_BYTES as u64 {
            return Err("patch input exceeds 16 MiB".into());
        }
        let mut bytes = Vec::new();
        File::open(&current)
            .map_err(|e| format!("read patch input: {e}"))?
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > MAX_BYTES {
            return Err("patch input exceeds 16 MiB".into());
        }
        return Ok(Some(Contents { bytes, mode }));
    }
    unreachable!("nonempty safe path")
}

/// Remove a regular file or symlink at `path` without following it.
fn remove_leaf(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("inspect patch file {path:?}: {e}")),
        Ok(meta) if meta.is_dir() => Err(format!("patch target is a directory: {path:?}")),
        Ok(_) => fs::remove_file(path).map_err(|e| format!("delete patch file: {e}")),
    }
}

fn create_parents(root: &Path, path: &Path) -> Result<(), String> {
    let mut current = root.to_path_buf();
    if let Some(parent) = path.parent() {
        for part in parent.components() {
            current.push(part);
            match fs::symlink_metadata(&current) {
                Ok(_) => directory(&current)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&current).map_err(|e| format!("create patch directory: {e}"))?;
                    set_mode(&current, 0o755)?;
                }
                Err(e) => return Err(format!("inspect patch parent: {e}")),
            }
        }
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn patch(body: &str) -> Patch {
        parse(body.as_bytes(), 0).unwrap()
    }
    fn module(body: &str) -> Patch {
        patch(&format!("--- MODULE.bazel\n+++ MODULE.bazel\n{body}"))
    }
    fn change(path: &str, from: &str, to: &str) -> String {
        format!("diff --git {path} {path}\n--- {path}\n+++ {path}\n@@ -1 +1 @@\n-{from}\n+{to}\n")
    }

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let base = std::env::var_os("TMPDIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp/opencode"));
            let path = base.join(format!(
                "patching-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            set_mode(&path, 0o700).unwrap();
            Self(path)
        }
        fn write(&self, path: &str, bytes: &[u8]) {
            fs::write(self.0.join(path), bytes).unwrap();
        }
        fn read(&self, path: &str) -> Vec<u8> {
            fs::read(self.0.join(path)).unwrap()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn module_add_dependency() {
        let p = module(
            "@@ -1 +1,2 @@ function context\n module(name = \"a\")\n+bazel_dep(name = \"b\", version = \"1\")\n",
        );
        assert_eq!(
            p.apply_module(b"module(name = \"a\")\n").unwrap(),
            b"module(name = \"a\")\nbazel_dep(name = \"b\", version = \"1\")\n"
        );
    }

    #[test]
    fn irrelevant_sections_preserve_arbitrary_original() {
        let p = patch(
            &(change("source", "not here", "new") + &change("nested/MODULE.bazel", "x", "y")),
        );
        assert_eq!(
            p.apply_module(b"\xff\0\rno newline").unwrap(),
            b"\xff\0\rno newline"
        );
    }

    #[test]
    fn empty_and_mail_only_are_noops() {
        for input in ["", "Subject: hello\n\n---\n file | 1 +\n"] {
            assert_eq!(patch(input).apply_module(b"x").unwrap(), b"x");
        }
    }

    #[test]
    fn ordered_sections() {
        let p = patch(&(change("MODULE.bazel", "a", "b") + &change("MODULE.bazel", "b", "c")));
        assert_eq!(p.apply_module(b"a").unwrap(), b"c\n");
    }

    #[test]
    fn reverse_hunks_preserve_source_positions() {
        let p = module("@@ -1 +1,2 @@\n-a\n+A\n+extra\n@@ -3 +4 @@\n-c\n+C\n");
        assert_eq!(p.apply_module(b"a\nb\nc\n").unwrap(), b"A\nextra\nb\nC\n");
        // Reverse order is observable: the later hunk can match an offset
        // location which an earlier hunk would otherwise have changed.
        let p = module("@@ -1 +1 @@\n-a\n+b\n@@ -5 +5 @@\n-a\n+c\n");
        assert_eq!(p.apply_module(b"a\na\n").unwrap(), b"b\nc\n");
    }

    #[test]
    fn nearest_offset_earlier_tie_and_nominal_priority() {
        let p = module("@@ -3 +3 @@\n-a\n+b\n");
        assert_eq!(p.apply_module(b"x\na\nx\na\n").unwrap(), b"x\nb\nx\na\n");
        assert_eq!(p.apply_module(b"a\nx\na\nx\n").unwrap(), b"a\nx\nb\nx\n");
        assert_eq!(p.apply_module(b"x\nx\nx\na\n").unwrap(), b"x\nx\nx\nb\n");
    }

    #[test]
    fn mismatches_report_unsupported_fuzz() {
        let p = module("@@ -1,2 +1,2 @@\n context\n-old\n+new\n");
        assert!(
            p.apply_module(b"different\nold\n")
                .unwrap_err()
                .contains("unsupported fuzz")
        );
    }

    #[test]
    fn line_endings_and_no_final_newline() {
        let p = patch("--- MODULE.bazel\r\n+++ MODULE.bazel\r@@ -1,3 +1,3 @@\n a\r\n-b\r+B\n c");
        assert_eq!(p.apply_module(b"a\r\nb\rc").unwrap(), b"a\nB\nc\n");
    }

    #[test]
    fn empty_context_and_no_newline_markers() {
        let p = module(
            "@@ -1,2 +1,2 @@\n\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n",
        );
        assert_eq!(p.apply_module(b"\nold").unwrap(), b"\nnew\n");
        assert!(parse(b"\\ No newline at end of file\n", 0).is_err());
        assert!(
            parse(
                b"--- f\n+++ f\n@@ -1 +1 @@\n\\ No newline at end of file\n-a\n+b",
                0
            )
            .is_err()
        );
    }

    #[test]
    fn root_module_creation_and_deletion_are_skipped() {
        for input in [
            "--- /dev/null\n+++ MODULE.bazel\n@@ -0,0 +1 @@\n+new\n",
            "--- MODULE.bazel\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n",
        ] {
            assert_eq!(
                patch(input).apply_module(b"untouched").unwrap(),
                b"untouched"
            );
        }
    }

    #[test]
    fn strip_zero_hint_and_strip_one_success() {
        let input = b"diff --git a/MODULE.bazel b/MODULE.bazel\nindex abcd..ef12 100644\n--- a/MODULE.bazel\n+++ b/MODULE.bazel\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(
            parse(input, 0)
                .unwrap()
                .apply_module(b"a")
                .unwrap_err()
                .contains("patch_strip = 1")
        );
        assert_eq!(parse(input, 1).unwrap().apply_module(b"a").unwrap(), b"b\n");
    }

    #[test]
    fn paths_timestamps_and_dev_null_before_strip() {
        let p = parse(
            b"--- a/./MODULE.bazel\told time\n+++ b/./MODULE.bazel \tnew time\n@@ -1 +1 @@\n-a\n+b",
            1,
        )
        .unwrap();
        assert_eq!(p.apply_module(b"a").unwrap(), b"b\n");
        assert!(parse(b"--- /dev/null\n+++ a/new\n@@ -0,0 +1 @@\n+x", 1).is_ok());
        assert!(parse(b"--- a\n+++ a\n@@ -1 +1 @@\n-a\n+b", 1).is_err());
    }

    #[test]
    fn reviewer_strip_counts_lexical_components_before_normalizing() {
        let input = b"--- ./a/MODULE.bazel\n+++ ./b/MODULE.bazel\n@@ -1 +1 @@\n-old\n+new\n";
        assert_eq!(
            parse(input, 1).unwrap().apply_module(b"old\n").unwrap(),
            b"old\n"
        );
        let input = b"--- a//MODULE.bazel\n+++ b//MODULE.bazel\n@@ -1 +1 @@\n-old\n+new\n";
        assert_eq!(
            parse(input, 2).unwrap().apply_module(b"old\n").unwrap(),
            b"new\n"
        );
        assert!(
            parse(input, 1).is_err(),
            "stripping into an absolute path must fail"
        );
    }

    #[test]
    fn adjacent_plain_sections_are_rejected_instead_of_diverging_from_bazel() {
        let bytes = b"--- data\n+++ data\n@@ -1 +1 @@\n-old\n+new\n--- MODULE.bazel\n+++ MODULE.bazel\n@@ -1 +1 @@\n-old\n+new\n";
        assert!(parse(bytes, 0).unwrap_err().contains("explicit separator"));
    }

    #[test]
    fn reviewer_dot_prefix_does_not_redirect_tree_patch_to_root_file() {
        let root = Temp::new();
        fs::create_dir(root.0.join("a")).unwrap();
        root.write("file", b"old\n");
        root.write("a/file", b"old\n");
        let bytes = b"--- ./a/file\n+++ ./b/file\n@@ -1 +1 @@\n-old\n+new\n";
        parse(bytes, 1).unwrap().apply_tree(&root.0).unwrap();
        assert_eq!(root.read("file"), b"old\n");
        assert_eq!(root.read("a/file"), b"new\n");
        assert!(!root.0.join("b/file").exists());
    }

    #[test]
    fn unsafe_paths_fail_even_in_skipped_sections_before_strip() {
        for name in [
            "../source",
            "prefix/../source",
            "/absolute/source",
            "C:/source",
            "a\\source",
            "a/../../source",
            "./",
        ] {
            for strip in [0, 1] {
                assert!(
                    parse(change(name, "a", "b").as_bytes(), strip).is_err(),
                    "{name}"
                );
            }
        }
        assert!(
            parse(
                b"diff --git ../f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b",
                1
            )
            .is_err()
        );
    }

    #[test]
    fn counts_and_truncation_fail_for_skipped_source() {
        for body in [
            "@@ -1,2 +1 @@\n-a\n+b\n",
            "@@ -1 +1 @@\n-a\n+b\n+c\n",
            "@@ -1 +1 @@\n-a\n",
            "@@ -1,0 +1 @@\n-a\n+b\n",
            "@@ -0 +1 @@\n-a\n+b\n",
            "@@ nonsense @@\n",
            "@@ -1 +1 @\n",
            "@@ -1,100001 +1 @@\n",
            "@@ -99999999999999999999999 +1 @@\n",
            "@@ -1 +1 @@\né\n",
            "@@ -1 +1 @@\n-a\n+b\n@@ broken\n",
        ] {
            assert!(
                parse(format!("--- source\n+++ source\n{body}").as_bytes(), 0).is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn header_pairs_and_null_counts_required() {
        for input in [
            "--- f\n",
            "+++ f\n",
            "--- f\n+++ f\n",
            "@@ -1 +1 @@\n-a\n+b\n",
            "--- /dev/null\n+++ /dev/null\n@@ -0,0 +0,0 @@\n",
            "--- /dev/null\n+++ f\n@@ -1 +1 @@\n-a\n+b\n",
            "--- f\n+++ /dev/null\n@@ -1 +1 @@\n-a\n+b\n",
            "--- /dev/null\n+++ f\n@@ -1,0 +1 @@\n+b\n",
        ] {
            assert!(parse(input.as_bytes(), 0).is_err(), "{input}");
        }
    }

    #[test]
    fn reject_invalid_utf8_nul_binary_and_mode_metadata() {
        assert!(parse(b"\xff", 0).unwrap_err().contains("non-UTF-8"));
        assert!(parse(b"x\0y", 0).unwrap_err().contains("NUL"));
        for line in [
            "GIT binary patch",
            "Binary files a/f and b/f differ",
            "rename from source",
            "copy to f",
            "old mode 100644",
            "new mode 100755",
            "new file mode 100644",
            "deleted file mode 100644",
            "index ab..cd 120000",
            "index ab..cd 160000",
            "index ab..cd nonsense",
            "index nonsense",
            "diff --cc source",
            "diff --combined source",
            "similarity index 100%",
            "*** source",
        ] {
            let input = format!("{line}\n{}", change("source", "a", "b"));
            assert!(parse(input.as_bytes(), 0).is_err(), "{line}");
        }
        assert!(module("@@ -1 +1 @@\n-a\n+b").apply_module(b"\xff").is_err());
    }

    #[test]
    fn git_mail_with_blank_context_and_footer() {
        let input = "From 123 Mon Sep 17\nSubject: change\n\n---\n f | 2 +-\n\ndiff --git a/MODULE.bazel b/MODULE.bazel\nindex abc..def 100755\n--- a/MODULE.bazel\n+++ b/MODULE.bazel\n@@ -1,2 +1,2 @@\n\n-a\n+b\n--\n2.18.0\n";
        assert_eq!(
            parse(input.as_bytes(), 1)
                .unwrap()
                .apply_module(b"\na")
                .unwrap(),
            b"\nb\n"
        );
        assert!(parse(b"diff --git a/f b/f\nindex abc..def 100644\n", 1).is_err());
    }

    #[test]
    fn bounded_inputs() {
        assert!(parse(&vec![b'x'; MAX_BYTES + 1], 0).is_err());
        assert!(parse(&vec![b'\n'; MAX_LINES + 1], 0).is_err());
    }

    #[test]
    fn insertion_zero_counts_and_empty_output() {
        assert_eq!(
            module("@@ -1,0 +2 @@\n+b\n").apply_module(b"a\nc").unwrap(),
            b"a\nb\nc\n"
        );
        assert_eq!(
            module("@@ -0,0 +1 @@\n+a\n").apply_module(b"").unwrap(),
            b"a\n"
        );
        assert_eq!(
            module("@@ -1 +0,0 @@\n-a\n").apply_module(b"a").unwrap(),
            b""
        );
    }

    #[test]
    fn tree_create_modify_delete_and_ordered_sections() {
        let t = Temp::new();
        t.write("keep", b"old");
        t.write("delete", b"gone\n");
        let input = format!(
            "{}{}diff --git nested/new nested/new\n--- /dev/null\n+++ nested/new\n@@ -0,0 +1 @@\n+born\ndiff --git delete delete\n--- delete\n+++ /dev/null\n@@ -1 +0,0 @@\n-gone\n",
            change("keep", "old", "middle"),
            change("keep", "middle", "new")
        );
        patch(&input).apply_tree(&t.0).unwrap();
        assert_eq!(t.read("keep"), b"new\n");
        assert_eq!(t.read("nested/new"), b"born\n");
        assert!(!t.0.join("delete").exists());
    }

    #[test]
    fn tree_module_creation_and_deletion_are_applied() {
        let t = Temp::new();
        patch("--- /dev/null\n+++ MODULE.bazel\n@@ -0,0 +1 @@\n+a\n")
            .apply_tree(&t.0)
            .unwrap();
        assert_eq!(t.read("MODULE.bazel"), b"a\n");
        patch("--- MODULE.bazel\n+++ /dev/null\n@@ -1 +0,0 @@\n-a\n")
            .apply_tree(&t.0)
            .unwrap();
        assert!(!t.0.join("MODULE.bazel").exists());
    }

    #[test]
    fn tree_prefers_existing_old_and_falls_back_to_new() {
        let t = Temp::new();
        t.write("old", b"a");
        t.write("new", b"other");
        let p = patch("--- old\n+++ new\n@@ -1 +1 @@\n-a\n+b");
        p.apply_tree(&t.0).unwrap();
        assert_eq!(t.read("old"), b"b\n");
        assert_eq!(t.read("new"), b"other");
        fs::remove_file(t.0.join("old")).unwrap();
        t.write("new", b"a");
        p.apply_tree(&t.0).unwrap();
        assert_eq!(t.read("new"), b"b\n");
    }

    #[test]
    fn preflight_content_failure_makes_no_files_or_parents() {
        let t = Temp::new();
        t.write("existing", b"actual");
        let p = patch(
            &("--- /dev/null\n+++ parent/new\n@@ -0,0 +1 @@\n+new\n".to_owned()
                + &change("existing", "wrong", "replacement")),
        );
        assert!(p.apply_tree(&t.0).is_err());
        assert!(!t.0.join("parent").exists());
        assert_eq!(t.read("existing"), b"actual");
    }

    #[test]
    fn preflight_directory_conflicts_make_no_changes() {
        let t = Temp::new();
        t.write("existing", b"a");
        let input = format!(
            "{}diff --git parent parent\n--- /dev/null\n+++ parent\n@@ -0,0 +1 @@\n+x\ndiff --git parent/new parent/new\n--- /dev/null\n+++ parent/new\n@@ -0,0 +1 @@\n+y\n",
            change("existing", "a", "b")
        );
        assert!(patch(&input).apply_tree(&t.0).is_err());
        assert_eq!(t.read("existing"), b"a");
        assert!(!t.0.join("parent").exists());
    }

    #[test]
    fn creation_conflict_and_incomplete_deletion_fail_preflight() {
        let t = Temp::new();
        t.write("f", b"a\nb\n");
        assert!(
            patch("--- /dev/null\n+++ f\n@@ -0,0 +1 @@\n+x\n")
                .apply_tree(&t.0)
                .is_err()
        );
        assert!(
            patch("--- f\n+++ /dev/null\n@@ -1 +0,0 @@\n-a\n")
                .apply_tree(&t.0)
                .is_err()
        );
        assert_eq!(t.read("f"), b"a\nb\n");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_final_ancestor_root_and_unused_new_path_are_rejected() {
        use std::os::unix::fs::symlink;
        let t = Temp::new();
        let outside = Temp::new();
        outside.write("f", b"a");
        symlink(outside.0.join("f"), t.0.join("link")).unwrap();
        symlink(&outside.0, t.0.join("dir")).unwrap();
        t.write("safe", b"a");
        for path in ["link", "dir/f"] {
            let p = patch(&(change("safe", "a", "b") + &change(path, "a", "b")));
            assert!(p.apply_tree(&t.0).is_err());
        }
        assert!(
            patch(&change("f", "a", "b"))
                .apply_tree(&t.0.join("dir"))
                .is_err()
        );
        assert!(
            patch("--- safe\n+++ link\n@@ -1 +1 @@\n-a\n+b")
                .apply_tree(&t.0)
                .is_err()
        );
        assert!(
            patch("--- link\n+++ /dev/null\n@@ -1 +0,0 @@\n-a")
                .apply_tree(&t.0)
                .is_err()
        );
        assert_eq!(t.read("safe"), b"a");
        assert_eq!(outside.read("f"), b"a");
    }

    #[cfg(unix)]
    #[test]
    fn hardlinks_are_broken_like_bazel_and_modes_normalized() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let t = Temp::new();
        t.write("f", b"a");
        fs::hard_link(t.0.join("f"), t.0.join("link")).unwrap();
        // PatchUtil deletes the old name and writes a fresh file: the other
        // hard-link name keeps the original bytes.
        patch(&change("f", "a", "b")).apply_tree(&t.0).unwrap();
        assert_eq!(t.read("f"), b"b\n");
        assert_eq!(t.read("link"), b"a");
        assert_eq!(fs::metadata(t.0.join("f")).unwrap().nlink(), 1);
        assert_eq!(fs::metadata(t.0.join("link")).unwrap().nlink(), 1);
        fs::remove_file(t.0.join("link")).unwrap();
        t.write("f", b"a");
        set_mode(&t.0.join("f"), 0o4751).unwrap();
        let input = format!(
            "{}diff --git nested/new nested/new\n--- /dev/null\n+++ nested/new\n@@ -0,0 +1 @@\n+x\n",
            change("f", "a", "b")
        );
        patch(&input).apply_tree(&t.0).unwrap();
        for (path, mode) in [("f", 0o755), ("nested", 0o755), ("nested/new", 0o644)] {
            assert_eq!(
                fs::metadata(t.0.join(path)).unwrap().permissions().mode() & 0o7777,
                mode
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn internal_leaf_links_are_read_through_and_replaced_never_written_through() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let t = Temp::new();
        fs::create_dir(t.0.join("data")).unwrap();
        t.write("data/real.txt", b"real\n");
        set_mode(&t.0.join("data/real.txt"), 0o755).unwrap();
        symlink("data/real.txt", t.0.join("rel.txt")).unwrap();
        symlink("missing", t.0.join("dangling")).unwrap();
        patch(&change("rel.txt", "real", "patched"))
            .apply_tree(&t.0)
            .unwrap();
        let meta = fs::symlink_metadata(t.0.join("rel.txt")).unwrap();
        assert!(meta.is_file(), "the link itself is replaced");
        assert_eq!(meta.permissions().mode() & 0o777, 0o755, "referent mode");
        assert_eq!(t.read("rel.txt"), b"patched\n");
        assert_eq!(t.read("data/real.txt"), b"real\n", "referent unchanged");
        // Deleting a link removes the link, not its referent.
        symlink("data/real.txt", t.0.join("gone.txt")).unwrap();
        patch("--- gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-real")
            .apply_tree(&t.0)
            .unwrap();
        assert!(fs::symlink_metadata(t.0.join("gone.txt")).is_err());
        assert_eq!(t.read("data/real.txt"), b"real\n");
        // A dangling leaf is a missing file to patch.
        assert!(
            patch(&change("dangling", "a", "b"))
                .apply_tree(&t.0)
                .unwrap_err()
                .contains("cannot find file to patch")
        );
        assert_eq!(
            fs::read_link(t.0.join("dangling")).unwrap(),
            Path::new("missing")
        );
    }
}
