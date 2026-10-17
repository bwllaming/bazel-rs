//! Bazel 9.3 native patch (`PatchUtil.java`) with java-diff-utils 4.12 hunks.
//!
//! This is a line-for-line port of
//! `bazel/repository/decompressor/PatchUtil.java` (pinned Bazel 9.3.0,
//! `src/main/java/com/google/devtools/build/lib/`) and of the java-diff-utils
//! 4.12 pieces it calls (`UnifiedDiffUtils.parseUnifiedDiff`,
//! `Patch.applyFuzzy`, `Chunk.verifyChunk`, `ChangeDelta.applyFuzzyToAt`):
//!
//! * The patch file and every patched file are read as UTF-8 text lines
//!   (Guava `CharSource.readLines`: `\n`, `\r\n` and `\r` terminate lines,
//!   malformed bytes decode to U+FFFD) and patched files are written back as
//!   UTF-8 with `\n` after every line (`FileSystemUtils.writeLinesAs`).
//! * Sections are applied one after the other while the file is scanned;
//!   hunks of one section are applied separately in reverse order with
//!   `applyFuzzy(…, 2)` (PatchUtil.java:221-235): context mismatches are
//!   retried ignoring up to two leading and trailing hunk lines at every
//!   offset, so a hunk may apply partially or not at all without an error.
//! * Git headers: `diff --git` file names, `rename from/to`, `new mode` and
//!   `new file mode` owner digits; other Git lines are ignored.
//!
//! Java runtime failures that Bazel catches (`IndexOutOfBoundsException`) are
//! reproduced with the JDK's message; failures that crash Bazel 9.3 (an
//! uncaught `IllegalArgumentException`/`NumberFormatException`/
//! `StringIndexOutOfBoundsException`, e.g. a one-line hunk that only matches
//! with fuzz 1) are explicit native errors ([`PatchError::Crash`]).
//!
//! Native safety profile of the on-disk tree ([`TreeFs`]): the root and its
//! ancestors must be real directories; symlinked ancestor directories inside
//! the tree are rejected (Bazel would write through them); a leaf symlink is
//! read through only when it resolves inside the tree (source_links) and is
//! replaced, never written through; created files are 0644 and modes are
//! normalized to 0644/0755 like the rest of the store. Like PatchUtil, patch and
//! target texts are read whole into memory (line lists); there are no
//! artificial size, line, working-set or comparison caps.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Cache-identity marker of the patch engine. Identities of patched trees
/// include it so caches written by the earlier exact-context engine are never
/// served; unpatched identities are unchanged.
pub const ENGINE: &str = "patchutil-java-diff-utils-4.12";

/// A decoded patch file plus its strip count (`PatchUtil.apply`'s arguments).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch {
    lines: Vec<String>,
    strip: i64,
}

/// How a patch application failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatchError {
    /// `PatchFailedException` (also wrapped `IndexOutOfBoundsException`).
    Failed(String),
    /// `IOException` while reading or writing files, or a native limit.
    Io(String),
    /// An exception that crashes genuine Bazel 9.3; reported explicitly.
    Crash(String),
}

impl fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(message) | Self::Io(message) => f.write_str(message),
            Self::Crash(message) => write!(
                f,
                "genuine Bazel 9.3 crashes applying this patch ({message}); the native profile rejects it explicitly"
            ),
        }
    }
}

impl From<PatchError> for String {
    fn from(error: PatchError) -> Self {
        error.to_string()
    }
}

type PResult<T> = Result<T, PatchError>;

fn failed<T>(message: impl Into<String>) -> PResult<T> {
    Err(PatchError::Failed(message.into()))
}

/// Guava `CharSource.readLines` over UTF-8 decoded bytes.
fn text_lines(bytes: &[u8]) -> PResult<Vec<String>> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    let mut pending = false;
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\r' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                lines.push(std::mem::take(&mut current));
                pending = false;
            }
            _ => {
                current.push(c);
                pending = true;
            }
        }
    }
    if pending {
        lines.push(current);
    }
    Ok(lines)
}

/// `FileSystemUtils.writeLinesAs(file, UTF_8, lines)`: `\n` after every line.
fn encode_lines(lines: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Decode a patch file. Every structural check happens while applying, as in
/// `PatchUtil.applyInternal`.
pub fn parse(bytes: &[u8], strip: usize) -> Result<Patch, String> {
    parse_signed(bytes, i64::try_from(strip).unwrap_or(i64::MAX)).map_err(String::from)
}

/// Like [`parse`], keeping a negative Starlark `strip` (it strips nothing but
/// appears in Bazel's messages).
pub fn parse_signed(bytes: &[u8], strip: i64) -> PResult<Patch> {
    Ok(Patch {
        lines: text_lines(bytes)?,
        strip,
    })
}

// ---------------------------------------------------------------------------
// java.util.ArrayList / AbstractList behaviour used by java-diff-utils.

enum Java {
    /// IndexOutOfBoundsException (caught by PatchUtil.applyPatchToFile).
    IndexOutOfBounds(String),
    /// Any other RuntimeException: crashes Bazel.
    Crash(String),
    Failed(String),
}

fn check_index(index: i64, length: usize) -> Result<usize, Java> {
    if index < 0 || index as u64 >= length as u64 {
        return Err(Java::IndexOutOfBounds(format!(
            "Index {index} out of bounds for length {length}"
        )));
    }
    Ok(index as usize)
}

fn sub_list(lines: &[String], from: i64, to: i64) -> Result<&[String], Java> {
    // AbstractList.subListRangeCheck.
    if from < 0 {
        return Err(Java::IndexOutOfBounds(format!("fromIndex = {from}")));
    }
    if to > lines.len() as i64 {
        return Err(Java::IndexOutOfBounds(format!("toIndex = {to}")));
    }
    if from > to {
        return Err(Java::Crash(format!(
            "java.lang.IllegalArgumentException: fromIndex({from}) > toIndex({to})"
        )));
    }
    Ok(&lines[from as usize..to as usize])
}

// ---------------------------------------------------------------------------
// java-diff-utils 4.12.

#[derive(Clone, Debug)]
struct Delta {
    /// Chunk position (0-based; -1 when the hunk header was never matched).
    position: i64,
    source: Vec<String>,
    target: Vec<String>,
}

/// The `UnifiedDiffUtils` hunk regex, matched against a whole string:
/// `^@@\s+-(?:(\d+)(?:,(\d+))?)\s+\+(?:(\d+)(?:,(\d+))?)\s+@@$`.
/// Returns the four groups (absent optional counts are None).
type HeaderGroups<'a> = (&'a str, Option<&'a str>, &'a str, Option<&'a str>);

fn chunk_header_groups(text: &str) -> Option<HeaderGroups<'_>> {
    fn java_space(b: u8) -> bool {
        matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
    }
    fn spaces(s: &str) -> Option<&str> {
        let rest = s.trim_start_matches(|c: char| c.is_ascii() && java_space(c as u8));
        (rest.len() < s.len()).then_some(rest)
    }
    fn digits(s: &str) -> Option<(&str, &str)> {
        let end = s.bytes().take_while(u8::is_ascii_digit).count();
        (end > 0).then(|| s.split_at(end))
    }
    fn range(s: &str) -> Option<(&str, Option<&str>, &str)> {
        let (start, rest) = digits(s)?;
        if let Some(after) = rest.strip_prefix(',')
            && let Some((count, rest)) = digits(after)
        {
            return Some((start, Some(count), rest));
        }
        Some((start, None, rest))
    }
    let rest = text.strip_prefix("@@")?;
    let rest = spaces(rest)?.strip_prefix('-')?;
    let (old_start, old_count, rest) = range(rest)?;
    let rest = spaces(rest)?.strip_prefix('+')?;
    let (new_start, new_count, rest) = range(rest)?;
    let rest = spaces(rest)?;
    // `$` also matches before one final line terminator; lines have none.
    (rest == "@@").then_some((old_start, old_count, new_start, new_count))
}

fn parse_int(text: &str) -> Result<i64, Java> {
    text.parse::<i32>().map(i64::from).map_err(|_| {
        Java::Crash(format!(
            "java.lang.NumberFormatException: For input string: \"{text}\""
        ))
    })
}

/// `UnifiedDiffUtils.parseUnifiedDiff` (4.12): every hunk becomes a
/// `ChangeDelta`; lines before the first `+++` are prelude.
fn parse_unified_diff(diff: &[String]) -> Result<Vec<Delta>, Java> {
    let mut in_prelude = true;
    let mut raw: Vec<(char, String)> = Vec::new();
    let mut deltas = Vec::new();
    let mut old_ln: i64 = 0;
    let mut new_ln: i64 = 0;
    fn flush(raw: &mut Vec<(char, String)>, deltas: &mut Vec<Delta>, old_ln: i64, new_ln: i64) {
        if raw.is_empty() {
            return;
        }
        let mut source = Vec::new();
        let mut target = Vec::new();
        for (tag, rest) in raw.drain(..) {
            if tag == ' ' || tag == '-' {
                source.push(rest.clone());
            }
            if tag == ' ' || tag == '+' {
                target.push(rest);
            }
        }
        let _ = new_ln;
        deltas.push(Delta {
            position: old_ln - 1,
            source,
            target,
        });
    }
    for line in diff {
        if in_prelude {
            if line.starts_with("+++") {
                in_prelude = false;
            }
            continue;
        }
        if let Some((old_start, _, new_start, _)) = chunk_header_groups(line) {
            flush(&mut raw, &mut deltas, old_ln, new_ln);
            old_ln = parse_int(old_start)?;
            new_ln = parse_int(new_start)?;
            if old_ln == 0 {
                old_ln = 1;
            }
            if new_ln == 0 {
                new_ln = 1;
            }
        } else if let Some(tag) = line.chars().next() {
            if matches!(tag, ' ' | '+' | '-') {
                raw.push((tag, line[1..].to_owned()));
            }
        } else {
            raw.push((' ', String::new()));
        }
    }
    flush(&mut raw, &mut deltas, old_ln, new_ln);
    // Patch.getDeltas(): stable sort by source position.
    deltas.sort_by_key(|delta| delta.position);
    Ok(deltas)
}

#[derive(PartialEq, Eq)]
enum Verify {
    Ok,
    PositionOutOfTarget,
    ContentDoesNotMatchTarget,
}

/// `Chunk.verifyChunk(target, fuzz, position)`.
fn verify_chunk(
    source: &[String],
    target: &[String],
    fuzz: i64,
    position: i64,
    comparisons: &mut usize,
) -> Result<Verify, Java> {
    let size = source.len() as i64;
    let last_index = size - fuzz;
    let last = position + size - 1;
    let length = target.len() as i64;
    if position + fuzz > length || last - fuzz > length {
        return Ok(Verify::PositionOutOfTarget);
    }
    let mut i = fuzz;
    while i < last_index {
        *comparisons += 1;
        let actual = &target[check_index(position + i, target.len())?];
        if *actual != source[i as usize] {
            return Ok(Verify::ContentDoesNotMatchTarget);
        }
        i += 1;
    }
    Ok(Verify::Ok)
}

/// `Patch.applyFuzzy(target, 2)` for the single-delta patches PatchUtil
/// builds (`lastPatchEnd` = -1, `lastPatchDelta` = 0).
fn apply_fuzzy(target: &mut Vec<String>, delta: &Delta) -> Result<(), Java> {
    const MAX_FUZZ: i64 = 2;
    const LAST_PATCH_END: i64 = -1;
    let default = delta.position;
    let size = delta.source.len() as i64;
    let mut comparisons = 0usize;
    for fuzz in 0..=MAX_FUZZ {
        // findPositionWithFuzz.
        let mut found = None;
        if verify_chunk(&delta.source, target, fuzz, default, &mut comparisons)? == Verify::Ok {
            found = Some(default);
        } else {
            let mut before_out = false;
            let mut after_out = false;
            let mut more: i64 = 0;
            loop {
                // findPositionWithFuzzAndMoreDelta.
                if !before_out && default - more + fuzz <= LAST_PATCH_END {
                    before_out = true;
                }
                if !after_out && (target.len() as i64) < default + more + size - fuzz {
                    after_out = true;
                }
                if !before_out
                    && verify_chunk(
                        &delta.source,
                        target,
                        fuzz,
                        default - more,
                        &mut comparisons,
                    )? == Verify::Ok
                {
                    found = Some(default - more);
                    break;
                }
                if !after_out
                    && verify_chunk(
                        &delta.source,
                        target,
                        fuzz,
                        default + more,
                        &mut comparisons,
                    )? == Verify::Ok
                {
                    found = Some(default + more);
                    break;
                }
                if before_out && after_out {
                    break;
                }
                more += 1;
            }
        }
        if let Some(position) = found {
            return apply_fuzzy_to_at(target, delta, fuzz, position);
        }
    }
    // CONFLICT_PRODUCES_EXCEPTION.
    Err(Java::Failed(
        "could not apply patch due to CONTENT_DOES_NOT_MATCH_TARGET".into(),
    ))
}

/// `ChangeDelta.applyFuzzyToAt`.
fn apply_fuzzy_to_at(
    target: &mut Vec<String>,
    delta: &Delta,
    fuzz: i64,
    position: i64,
) -> Result<(), Java> {
    let size = delta.source.len() as i64;
    let mut i = fuzz;
    while i < size - fuzz {
        let index = check_index(position + fuzz, target.len())?;
        target.remove(index);
        i += 1;
    }
    let lines = sub_list(&delta.target, fuzz, delta.target.len() as i64 - fuzz)?.to_vec();
    for (i, line) in (fuzz..).zip(lines) {
        let index = position + i;
        if index < 0 || index as u64 > target.len() as u64 {
            return Err(Java::IndexOutOfBounds(format!(
                "Index: {index}, Size: {}",
                target.len()
            )));
        }
        target.insert(index as usize, line);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// PatchUtil.java.

/// A file system view rooted at the patch output directory. Paths are
/// normalized, root-relative ('' is the directory itself).
pub trait PatchFs {
    /// `outputDirectory.getPathString()`.
    fn root_display(&self) -> String;
    /// `Path.exists()` (follows symlinks).
    fn exists(&mut self, path: &str) -> PResult<bool>;
    /// `FileSystemUtils.readLines(file, UTF_8)`.
    fn read_lines(&mut self, path: &str) -> PResult<Vec<String>>;
    /// `getFilePermissionValue`: 4 (0400) + 2 (0200) + 1 (0100).
    fn permission(&mut self, path: &str) -> PResult<i32>;
    /// `Path.delete()`.
    fn delete(&mut self, path: &str) -> PResult<()>;
    /// `writeLinesAs` (creating parents), then `setFilePermission` unless -1.
    fn write(&mut self, path: &str, lines: &[String], permission: i32) -> PResult<()>;

    fn display(&self, path: &str) -> String {
        let root = self.root_display();
        if path.is_empty() {
            root
        } else if root == "/" {
            format!("/{path}")
        } else {
            format!("{root}/{path}")
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineType {
    OldFile,
    NewFile,
    ChunkHead,
    ChunkAdd,
    ChunkDel,
    ChunkEql,
    GitHeader,
    RenameFrom,
    RenameTo,
    NewMode,
    NewFileMode,
    OtherGitLine,
    ChunkNoNewline,
    Unknown,
}

const GIT_LINE_PREFIXES: [&str; 11] = [
    "old mode ",
    "new mode ",
    "deleted file mode ",
    "new file mode ",
    "copy from ",
    "copy to ",
    "rename old ",
    "rename new ",
    "similarity index ",
    "dissimilarity index ",
    "index ",
];

/// PatchUtil.getLineType (java:107-158).
fn line_type(line: &str, reading_chunk: bool, git: bool) -> LineType {
    if line.starts_with('\\') {
        return LineType::ChunkNoNewline;
    }
    if reading_chunk {
        if line.starts_with('+') {
            return LineType::ChunkAdd;
        }
        if line.starts_with('-') {
            return LineType::ChunkDel;
        }
        if line.starts_with(' ') || line.is_empty() {
            return LineType::ChunkEql;
        }
    } else {
        if line.starts_with("--- ") {
            return LineType::OldFile;
        }
        if line.starts_with("+++ ") {
            return LineType::NewFile;
        }
        if line.starts_with("diff --git ") {
            return LineType::GitHeader;
        }
        if git {
            if line.starts_with("rename from ") {
                return LineType::RenameFrom;
            }
            if line.starts_with("rename to ") {
                return LineType::RenameTo;
            }
            if line.starts_with("new mode ") {
                return LineType::NewMode;
            }
            if line.starts_with("new file mode ") {
                return LineType::NewFileMode;
            }
            if GIT_LINE_PREFIXES.iter().any(|p| line.starts_with(p)) {
                return LineType::OtherGitLine;
            }
        }
    }
    if line.starts_with("@@")
        && line.rfind("@@") != Some(0)
        && let Some(pos) = line[2..].find("@@").map(|p| p + 2)
        && chunk_header_groups(&line[..pos + 2]).is_some()
    {
        return LineType::ChunkHead;
    }
    LineType::Unknown
}

#[derive(Clone, Copy)]
struct ChunkHeader {
    old_size: i64,
    new_size: i64,
}

#[derive(PartialEq, Eq)]
enum Check {
    Complete,
    Continue,
    Error,
}

impl ChunkHeader {
    fn new(header: &str) -> PResult<Self> {
        let Some((_, old, _, new)) = chunk_header_groups(header) else {
            return failed(format!("Wrong chunk header: {header}"));
        };
        let size = |group: Option<&str>| match group {
            None => Ok(1),
            Some(text) => parse_int(text).map_err(java_error),
        };
        Ok(Self {
            old_size: size(old)?,
            new_size: size(new)?,
        })
    }

    fn check(&self, old: i64, new: i64) -> Check {
        if old == self.old_size && new == self.new_size {
            Check::Complete
        } else if old <= self.old_size && new <= self.new_size {
            Check::Continue
        } else {
            Check::Error
        }
    }
}

fn java_error(error: Java) -> PatchError {
    match error {
        Java::IndexOutOfBounds(message) | Java::Failed(message) => PatchError::Failed(message),
        Java::Crash(message) => PatchError::Crash(message),
    }
}

/// PatchUtil.stripPath (java:270-279).
fn strip_path(path: &str, strip: i64) -> &str {
    let mut strip = strip;
    let mut pos = 0;
    let bytes = path.as_bytes();
    while pos < bytes.len() && strip > 0 {
        if bytes[pos] == b'/' {
            strip -= 1;
        }
        pos += 1;
    }
    &path[pos..]
}

/// Java `String.trim()`: strips chars <= U+0020 at both ends.
fn java_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c <= ' ')
}

/// PatchUtil.extractPath (java:286-307).
fn extract_path(line: &str, strip: i64, loc: usize) -> PResult<Option<String>> {
    let line = line.split('\t').next().unwrap_or("");
    if line.chars().count() > 4 {
        let path = java_trim(&line[4..]);
        if path == "/dev/null" {
            return Ok(None);
        }
        let path = strip_path(path, strip);
        if !path.is_empty() {
            return Ok(Some(path.to_owned()));
        }
    }
    failed(format!(
        "Cannot determine file name with strip = {strip} at line {loc}:\n{line}"
    ))
}

/// PatchUtil.getFilePath (java:309-323): `outputDirectory.getRelative(path)`
/// must stay below the output directory. Returns the normalized relative path.
fn file_path(fs: &dyn PatchFs, path: Option<&str>, loc: usize) -> PResult<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let root = fs.root_display();
    let root_parts: Vec<&str> = root.split('/').filter(|p| !p.is_empty()).collect();
    let mut parts: Vec<&str> = if path.starts_with('/') {
        Vec::new()
    } else {
        root_parts.clone()
    };
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.len() < root_parts.len() || parts[..root_parts.len()] != root_parts[..] {
        return failed(format!(
            "Cannot patch file outside of external repository ({root}), file path = \"{path}\" at line {loc}"
        ));
    }
    Ok(Some(parts[root_parts.len()..].join("/")))
}

fn check_patch_content_is_complete(
    content: &[String],
    header: Option<ChunkHeader>,
    old: i64,
    new: i64,
    loc: usize,
) -> PResult<()> {
    if !content.is_empty() {
        if content.len() < 2 || !content[0].starts_with("---") || !content[1].starts_with("+++") {
            return failed(format!(
                "The patch content must start with ---/+++ prelude lines at line {loc}."
            ));
        }
        let Some(header) = header else {
            return failed(format!(
                "Looks like a unified diff at line {loc}, but no patch chunk was found."
            ));
        };
        if header.check(old, new) == Check::Continue {
            return failed(format!(
                "Expecting more chunk line at line {}",
                loc + content.len()
            ));
        }
    }
    Ok(())
}

fn check_files_status_for_renaming(
    fs: &mut dyn PatchFs,
    old: &Option<String>,
    new: &Option<String>,
    old_str: &Option<String>,
    new_str: &Option<String>,
    loc: usize,
) -> PResult<()> {
    let old_error = match old {
        None => ", old file name is not specified".to_owned(),
        Some(path) if !fs.exists(path)? => format!(
            ", old file name ({}) doesn't exist",
            old_str.as_deref().unwrap_or("null")
        ),
        Some(_) => String::new(),
    };
    let new_error = match new {
        None => ", new file name is not specified".to_owned(),
        Some(path) if fs.exists(path)? => format!(
            ", new file name ({}) already exists",
            new_str.as_deref().unwrap_or("null")
        ),
        Some(_) => String::new(),
    };
    if !old_error.is_empty() || !new_error.is_empty() {
        return failed(format!(
            "Cannot rename file (near line {loc}){old_error}{new_error}."
        ));
    }
    Ok(())
}

fn check_files_status_for_patching(
    fs: &mut dyn PatchFs,
    deltas: &[Delta],
    old: &Option<String>,
    new: &Option<String>,
    old_str: &Option<String>,
    new_str: &Option<String>,
    loc: usize,
) -> PResult<()> {
    if old.is_none() && new.is_none() {
        return failed(format!(
            "Wrong patch format near line {loc}, neither new file or old file are specified."
        ));
    }
    let add_file = deltas.len() == 1 && deltas[0].source.is_empty();
    let old_exists = match old {
        Some(path) => fs.exists(path)?,
        None => false,
    };
    let new_exists = match new {
        Some(path) => fs.exists(path)?,
        None => false,
    };
    if !add_file && !old_exists && !new_exists {
        let old_error = match old {
            None => ", old file name is not specified".to_owned(),
            Some(_) => format!(
                ", old file name ({}) doesn't exist",
                old_str.as_deref().unwrap_or("null")
            ),
        };
        let new_error = match new {
            None => ", new file name is not specified".to_owned(),
            Some(_) => format!(
                ", new file name ({}) doesn't exist",
                new_str.as_deref().unwrap_or("null")
            ),
        };
        return failed(format!(
            "Cannot find file to patch (near line {loc}){old_error}{new_error}."
        ));
    }
    Ok(())
}

/// PatchUtil.applyPatchToFile (java:200-260).
fn apply_patch_to_file(
    fs: &mut dyn PatchFs,
    deltas: &[Delta],
    old: &Option<String>,
    new: &Option<String>,
    renaming: bool,
    mut permission: i32,
) -> PResult<()> {
    let old_exists = match old {
        Some(path) => fs.exists(path)?,
        None => false,
    };
    let input = if old_exists {
        old.clone()
    } else {
        match new {
            Some(path) if fs.exists(path)? => Some(path.clone()),
            _ => None,
        }
    };
    let mut content = match &input {
        None => Vec::new(),
        Some(path) => {
            let lines = fs.read_lines(path)?;
            if permission == -1 {
                permission = fs.permission(path)?;
            }
            lines
        }
    };
    let shown_old = old
        .as_ref()
        .map_or_else(|| "null".to_owned(), |path| fs.display(path));
    for delta in deltas.iter().rev() {
        apply_fuzzy(&mut content, delta).map_err(|error| match error {
            Java::IndexOutOfBounds(message) | Java::Failed(message) => PatchError::Failed(format!(
                "in patch applied to {shown_old}: {message}, error applying change near line {}",
                delta.position + 1
            )),
            Java::Crash(message) => PatchError::Crash(message),
        })?;
    }
    let output = if old_exists && !renaming {
        old.clone()
    } else {
        new.clone()
    };
    if let Some(path) = old {
        fs.delete(path)?;
    }
    let delete_file = new.is_none() && content.is_empty();
    if let Some(path) = output
        && !delete_file
    {
        fs.write(&path, &content, permission)?;
    }
    Ok(())
}

/// PatchUtil.applyInternal (java:448-660). `single_file` selects
/// `applyToSingleFile` (only sections whose old and new file are it).
fn apply_internal(
    lines: &[String],
    strip: i64,
    fs: &mut dyn PatchFs,
    single_file: Option<&str>,
) -> PResult<()> {
    let mut git = false;
    let mut rename_from = false;
    let mut rename_to = false;
    let mut reading_chunk = false;
    let mut content: Vec<String> = Vec::new();
    let mut header: Option<ChunkHeader> = None;
    let mut old_str: Option<String> = None;
    let mut new_str: Option<String> = None;
    let mut old: Option<String> = None;
    let mut new: Option<String> = None;
    let mut old_count: i64 = 0;
    let mut new_count: i64 = 0;
    let mut permission: i32 = -1;
    let end = "$".to_owned();
    for i in 0..=lines.len() {
        // An extra line makes sure the last chunk also gets applied.
        let line = lines.get(i).unwrap_or(&end);
        let kind = line_type(line, reading_chunk, git);
        match kind {
            LineType::OldFile => {
                content.push(line.clone());
                old_str = extract_path(line, strip, i + 1)?;
                old = file_path(fs, old_str.as_deref(), i + 1)?;
            }
            LineType::NewFile => {
                content.push(line.clone());
                new_str = extract_path(line, strip, i + 1)?;
                new = file_path(fs, new_str.as_deref(), i + 1)?;
            }
            LineType::NewMode | LineType::NewFileMode => {
                let index = if kind == LineType::NewMode { 12 } else { 17 };
                let Some(c) = line.encode_utf16().nth(index) else {
                    return Err(PatchError::Crash(format!(
                        "java.lang.StringIndexOutOfBoundsException at line {}",
                        i + 1
                    )));
                };
                if !(u16::from(b'0')..=u16::from(b'7')).contains(&c) {
                    return failed(format!("Wrong file mode format at line {}: {line}", i + 1));
                }
                permission = i32::from(c - u16::from(b'0'));
            }
            LineType::ChunkHead => {
                let pos = line[2..].find("@@").map_or(0, |p| p + 2);
                let text = &line[..pos + 2];
                content.push(text.to_owned());
                header = Some(ChunkHeader::new(text)?);
                old_count = 0;
                new_count = 0;
                reading_chunk = true;
            }
            LineType::ChunkAdd | LineType::ChunkDel | LineType::ChunkEql => {
                if kind != LineType::ChunkAdd {
                    old_count += 1;
                }
                if kind != LineType::ChunkDel {
                    new_count += 1;
                }
                content.push(line.clone());
                let checked = header
                    .as_ref()
                    .map_or(Check::Error, |h| h.check(old_count, new_count));
                if checked == Check::Complete {
                    reading_chunk = false;
                } else if checked == Check::Error {
                    let what = match kind {
                        LineType::ChunkAdd => "an added",
                        LineType::ChunkDel => "a deleted",
                        _ => "a context",
                    };
                    return failed(format!(
                        "Wrong chunk detected near line {}: {line}, does not expect {what} line here.",
                        i + 1
                    ));
                }
            }
            LineType::ChunkNoNewline => {
                if !content.is_empty() && header.is_some() {
                    content.push(line.clone());
                }
            }
            LineType::RenameFrom | LineType::RenameTo => {
                let (skip, slot_str, slot) = if kind == LineType::RenameFrom {
                    rename_from = true;
                    (12, &mut old_str, &mut old)
                } else {
                    rename_to = true;
                    (10, &mut new_str, &mut new)
                };
                if slot_str.is_none() {
                    let name = java_trim(&line[skip..]).to_owned();
                    if name.is_empty() {
                        return failed(format!(
                            "Cannot determine file name from line {}:\n{line}",
                            i + 1
                        ));
                    }
                    *slot = file_path(fs, Some(&name), i + 1)?;
                    *slot_str = Some(name);
                }
            }
            LineType::OtherGitLine => {}
            LineType::GitHeader | LineType::Unknown => {
                let renaming = git && rename_from && rename_to;
                if !content.is_empty() || renaming || permission != -1 {
                    let start = i + 1 - content.len();
                    check_patch_content_is_complete(&content, header, old_count, new_count, start)?;
                    if renaming {
                        if let Some(single) = single_file {
                            if new.as_deref() == Some(single) || old.as_deref() == Some(single) {
                                return failed(format!(
                                    "Renaming {} while applying patches to it as a single file is not supported.",
                                    fs.display(single)
                                ));
                            }
                        } else {
                            check_files_status_for_renaming(
                                fs, &old, &new, &old_str, &new_str, start,
                            )?;
                        }
                    }
                    if let Some(single) = single_file
                        && strip == 0
                        && old_str.as_deref() == Some(&format!("a/{single}"))
                        && new_str.as_deref() == Some(&format!("b/{single}"))
                    {
                        return failed(format!(
                            "error at line {start}: the patch file contains a/b prefixes, did you forget to set patch_strip = 1?"
                        ));
                    }
                    if single_file.is_none_or(|single| {
                        new.as_deref() == Some(single) && old.as_deref() == Some(single)
                    }) {
                        let deltas = parse_unified_diff(&content).map_err(java_error)?;
                        check_files_status_for_patching(
                            fs, &deltas, &old, &new, &old_str, &new_str, start,
                        )?;
                        apply_patch_to_file(fs, &deltas, &old, &new, renaming, permission)?;
                    }
                }
                content.clear();
                header = None;
                old_str = None;
                new_str = None;
                old = None;
                new = None;
                permission = -1;
                old_count = 0;
                new_count = 0;
                reading_chunk = false;
                git = kind == LineType::GitHeader;
                if git {
                    // File names from `diff --git a/x b/y` for patches
                    // without ---/+++ lines (Splitter.on(' ')).
                    let args: Vec<&str> = line.split(' ').collect();
                    if args.len() >= 4 {
                        let o = strip_path(args[2], strip).to_owned();
                        if !o.is_empty() {
                            old = file_path(fs, Some(&o), i + 1)?;
                        }
                        old_str = Some(o);
                        let n = strip_path(args[3], strip).to_owned();
                        if !n.is_empty() {
                            new = file_path(fs, Some(&n), i + 1)?;
                        }
                        new_str = Some(n);
                    }
                }
                rename_from = false;
                rename_to = false;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// File systems.

/// In-memory module file system (ModuleFileFunction.java:936-955:
/// `/module/MODULE.bazel` in an InMemoryFileSystem).
struct ModuleFs {
    files: BTreeMap<String, (Vec<String>, i32)>,
    original: Option<Vec<u8>>,
}

impl PatchFs for ModuleFs {
    fn root_display(&self) -> String {
        "/module".into()
    }
    fn exists(&mut self, path: &str) -> PResult<bool> {
        Ok(path.is_empty() || self.files.contains_key(path))
    }
    fn read_lines(&mut self, path: &str) -> PResult<Vec<String>> {
        match self.files.get(path) {
            Some((lines, _)) => Ok(lines.clone()),
            None => Err(PatchError::Io(format!(
                "{} (No such file or directory)",
                self.display(path)
            ))),
        }
    }
    fn permission(&mut self, path: &str) -> PResult<i32> {
        Ok(self
            .files
            .get(path)
            .map_or(6, |(_, permission)| *permission))
    }
    fn delete(&mut self, path: &str) -> PResult<()> {
        if self.files.remove(path).is_some() && path == "MODULE.bazel" {
            self.original = None;
        }
        Ok(())
    }
    fn write(&mut self, path: &str, lines: &[String], permission: i32) -> PResult<()> {
        if path == "MODULE.bazel" {
            self.original = None;
        }
        let permission = if permission == -1 { 6 } else { permission };
        self.files
            .insert(path.to_owned(), (lines.to_vec(), permission));
        Ok(())
    }
}

/// The on-disk tree below a caller-owned private stage (native safety profile
/// described in the module documentation).
pub struct TreeFs {
    root: PathBuf,
    display: String,
}

impl TreeFs {
    pub fn new(root: &Path) -> PResult<Self> {
        check_root(root)?;
        let root: PathBuf = root.components().collect();
        Ok(Self {
            display: root.to_string_lossy().into_owned(),
            root,
        })
    }

    /// The path with every existing ancestor inside the tree checked not to
    /// be a symlink (missing ancestors are allowed).
    fn checked(&self, path: &str) -> PResult<PathBuf> {
        let mut current = self.root.clone();
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        for (index, part) in parts.iter().enumerate() {
            current.push(part);
            if index + 1 == parts.len() {
                break;
            }
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(PatchError::Io(format!(
                        "unsafe patch symlink: {} (native patches never traverse symlinked directories)",
                        current.display()
                    )));
                }
                Ok(_) => {}
                Err(e)
                    if e.kind() == io::ErrorKind::NotFound
                        || e.kind() == io::ErrorKind::NotADirectory => {}
                Err(e) => {
                    return Err(PatchError::Io(format!(
                        "inspect patch path {}: {e}",
                        current.display()
                    )));
                }
            }
        }
        Ok(current)
    }

    /// Follow a leaf link inside the tree; `None` when nothing exists.
    fn resolved(&self, path: &str) -> PResult<Option<(PathBuf, fs::Metadata)>> {
        let current = self.checked(path)?;
        let meta = match fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    || e.kind() == io::ErrorKind::NotADirectory =>
            {
                return Ok(None);
            }
            Err(e) => {
                return Err(PatchError::Io(format!(
                    "inspect patch path {}: {e}",
                    current.display()
                )));
            }
        };
        if !meta.file_type().is_symlink() {
            return Ok(Some((current, meta)));
        }
        match crate::source_links::resolve_in_root(&self.root, Path::new(path))
            .map_err(|e| PatchError::Io(format!("unsafe patch symlink {current:?}: {e}")))?
        {
            crate::source_links::Resolution::Missing(_) => Ok(None),
            crate::source_links::Resolution::Resolved(referent) => {
                let meta = fs::metadata(&referent).map_err(|e| {
                    PatchError::Io(format!("inspect patch link referent {referent:?}: {e}"))
                })?;
                Ok(Some((referent, meta)))
            }
        }
    }
}

impl PatchFs for TreeFs {
    fn root_display(&self) -> String {
        self.display.clone()
    }

    fn exists(&mut self, path: &str) -> PResult<bool> {
        Ok(self.resolved(path)?.is_some())
    }

    fn read_lines(&mut self, path: &str) -> PResult<Vec<String>> {
        let shown = self.display(path);
        let Some((current, meta)) = self.resolved(path)? else {
            return Err(PatchError::Io(format!(
                "{shown} (No such file or directory)"
            )));
        };
        if meta.is_dir() {
            return Err(PatchError::Io(format!("{shown} (Is a directory)")));
        }
        if !meta.is_file() {
            return Err(PatchError::Io(format!(
                "patch target is not a regular file: {shown}"
            )));
        }
        let mut bytes = Vec::new();
        File::open(&current)
            .map_err(|e| PatchError::Io(format!("read patch input {shown}: {e}")))?
            .read_to_end(&mut bytes)
            .map_err(|e| PatchError::Io(e.to_string()))?;
        text_lines(&bytes)
    }

    fn permission(&mut self, path: &str) -> PResult<i32> {
        let Some((_, meta)) = self.resolved(path)? else {
            return Err(PatchError::Io(format!(
                "{} (No such file or directory)",
                self.display(path)
            )));
        };
        Ok(permission_value(mode_of(&meta)))
    }

    fn delete(&mut self, path: &str) -> PResult<()> {
        if path.is_empty() {
            return Err(PatchError::Io(format!(
                "{} (Directory not empty)",
                self.display
            )));
        }
        let current = self.checked(path)?;
        match fs::symlink_metadata(&current) {
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    || e.kind() == io::ErrorKind::NotADirectory =>
            {
                Ok(())
            }
            Err(e) => Err(PatchError::Io(format!(
                "inspect patch file {}: {e}",
                current.display()
            ))),
            Ok(meta) if meta.is_dir() => fs::remove_dir(&current).map_err(|_| {
                PatchError::Io(format!("{} (Directory not empty)", current.display()))
            }),
            Ok(_) => fs::remove_file(&current)
                .map_err(|e| PatchError::Io(format!("delete patch file: {e}"))),
        }
    }

    fn write(&mut self, path: &str, lines: &[String], permission: i32) -> PResult<()> {
        let bytes = encode_lines(lines);
        let destination = self.checked(path)?;
        if path.is_empty() {
            return Err(PatchError::Io(format!("{} (Is a directory)", self.display)));
        }
        create_parents(&self.root, Path::new(path))?;
        // FileOutputStream truncates an existing file in place (mode kept);
        // a leaf link is replaced instead of written through (native).
        let mut base = 0o644;
        match fs::symlink_metadata(&destination) {
            Ok(meta) if meta.is_dir() => {
                return Err(PatchError::Io(format!(
                    "{} (Is a directory)",
                    destination.display()
                )));
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                fs::remove_file(&destination)
                    .map_err(|e| PatchError::Io(format!("delete patch link: {e}")))?;
            }
            Ok(meta) => {
                base = mode_of(&meta);
                fs::remove_file(&destination)
                    .map_err(|e| PatchError::Io(format!("replace patch file: {e}")))?;
            }
            Err(_) => {}
        }
        let mode = if permission == -1 {
            base
        } else {
            set_permission_bits(base, permission)
        };
        // The store represents 0644/0755 only.
        let mode = if mode & 0o100 != 0 { 0o755 } else { 0o644 };
        // Executables under the exclusive spawn lock (crate::spawn_lock).
        crate::spawn_lock::exclusive(mode == 0o755, || {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .map_err(|e| PatchError::Io(format!("write patch file {destination:?}: {e}")))?;
            file.write_all(&bytes)
                .map_err(|e| PatchError::Io(format!("write patch content: {e}")))
        })?;
        set_mode(&destination, mode).map_err(PatchError::Io)
    }
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(_meta: &fs::Metadata) -> u32 {
    0o644
}

/// getFilePermissionValue (java:184-188) over UnixFileSystem.isReadable/
/// isWritable/isExecutable (0400/0200/0100, unix/UnixFileSystem.java:179-191).
fn permission_value(mode: u32) -> i32 {
    (if mode & 0o400 != 0 { 4 } else { 0 })
        + (if mode & 0o200 != 0 { 2 } else { 0 })
        + (if mode & 0o100 != 0 { 1 } else { 0 })
}

/// setFilePermission (java:190-198) over setReadable 0400, setWritable 0200,
/// setExecutable 0111 (unix/UnixFileSystem.java:200-225).
fn set_permission_bits(mode: u32, permission: i32) -> u32 {
    let mut mode = mode;
    for (bit, bits) in [(4, 0o400), (2, 0o200), (1, 0o111)] {
        if permission & bit != 0 {
            mode |= bits;
        } else {
            mode &= !bits;
        }
    }
    mode
}

impl Patch {
    /// ModuleFileFunction.maybePatchModuleFile (java:935-955):
    /// `PatchUtil.applyToSingleFile` on an in-memory `/module/MODULE.bazel`.
    /// Without an applicable section the original bytes are kept exactly.
    pub fn apply_module(&self, original: &[u8]) -> Result<Vec<u8>, String> {
        let mut fs = ModuleFs {
            files: BTreeMap::from([("MODULE.bazel".to_owned(), (text_lines(original)?, 6))]),
            original: Some(original.to_vec()),
        };
        apply_internal(&self.lines, self.strip, &mut fs, Some("MODULE.bazel"))?;
        if let Some(original) = fs.original {
            return Ok(original);
        }
        let (lines, _) = fs
            .files
            .get("MODULE.bazel")
            .ok_or("single_version_override patch deleted the module file")?;
        Ok(encode_lines(lines))
    }

    /// PatchUtil.apply (java:427-430) on a caller-owned private stage.
    pub fn apply_tree(&self, root: &Path) -> Result<(), String> {
        self.apply_tree_detailed(root).map_err(String::from)
    }

    /// Like [`Patch::apply_tree`], distinguishing PatchFailedException.
    pub fn apply_tree_detailed(&self, root: &Path) -> PResult<()> {
        let mut fs = TreeFs::new(root)?;
        apply_internal(&self.lines, self.strip, &mut fs, None)
    }
}

fn directory(path: &Path) -> PResult<()> {
    let meta = fs::symlink_metadata(path)
        .map_err(|e| PatchError::Io(format!("inspect patch directory {path:?}: {e}")))?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(PatchError::Io(format!(
            "unsafe patch directory/symlink: {path:?}"
        )));
    }
    Ok(())
}

/// Existing ancestors must be real directories. A missing output directory
/// (and missing ancestors below the first existing one) is created by the
/// first write, as `writeLinesAs` creates parents.
fn check_root(root: &Path) -> PResult<()> {
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| PatchError::Io(e.to_string()))?
            .join(root)
    };
    let mut existing = false;
    for ancestor in absolute.ancestors() {
        if !existing {
            match fs::symlink_metadata(ancestor) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                _ => existing = true,
            }
        }
        directory(ancestor)?;
    }
    Ok(())
}

fn create_parents(root: &Path, path: &Path) -> PResult<()> {
    let mut chain: Vec<PathBuf> = root
        .ancestors()
        .take_while(|ancestor| fs::symlink_metadata(ancestor).is_err())
        .map(Path::to_path_buf)
        .collect();
    chain.reverse();
    if let Some(parent) = path.parent() {
        let mut current = root.to_path_buf();
        for part in parent.components() {
            current.push(part);
            chain.push(current.clone());
        }
    }
    for current in chain {
        match fs::symlink_metadata(&current) {
            Ok(_) => directory(&current)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)
                    .map_err(|e| PatchError::Io(format!("create patch directory: {e}")))?;
                set_mode(&current, 0o755).map_err(PatchError::Io)?;
            }
            Err(e) => return Err(PatchError::Io(format!("inspect patch parent: {e}"))),
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn patch(body: &str, strip: usize) -> Patch {
        parse(body.as_bytes(), strip).unwrap()
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
            Self(fs::canonicalize(path).unwrap())
        }
        fn write(&self, path: &str, bytes: &[u8]) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        fn read(&self, path: &str) -> Vec<u8> {
            fs::read(self.0.join(path)).unwrap()
        }
        fn exists(&self, path: &str) -> bool {
            fs::symlink_metadata(self.0.join(path)).is_ok()
        }
        fn mode(&self, path: &str) -> u32 {
            mode_of(&fs::symlink_metadata(self.0.join(path)).unwrap())
        }
        fn apply(&self, body: &str, strip: usize) -> PResult<()> {
            patch(body, strip).apply_tree_detailed(&self.0)
        }
        fn shown(&self) -> String {
            self.0.display().to_string()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const FIX: &str = "--- a/p.txt\n+++ b/p.txt\n@@ -1 +1 @@\n-a\n+b\n";

    fn failure(error: PatchError) -> String {
        match error {
            PatchError::Failed(message) => message,
            other => panic!("expected PatchFailedException, got {other:?}"),
        }
    }

    #[test]
    fn module_projection_adds_a_dependency_and_strips_hunk_context() {
        let p = patch(
            "--- MODULE.bazel\n+++ MODULE.bazel\n@@ -1 +1,2 @@ function context\n module(name = \"a\")\n+bazel_dep(name = \"b\", version = \"1\")\n",
            0,
        );
        assert_eq!(
            p.apply_module(b"module(name = \"a\")\n").unwrap(),
            b"module(name = \"a\")\nbazel_dep(name = \"b\", version = \"1\")\n"
        );
    }

    #[test]
    fn module_projection_skips_other_files_creation_deletion_rename_and_modes() {
        let original = b"module(name = 'a')\r\n\xff no final newline";
        for body in [
            "",
            "From mail preamble\nSubject: x\n",
            "--- a/data\n+++ b/data\n@@ -1 +1 @@\n-x\n+y\n",
            "diff --git a/MODULE.bazel b/MODULE.bazel\nnew file mode 100644\n--- /dev/null\n+++ b/MODULE.bazel\n@@ -0,0 +1 @@\n+x\n",
            "--- a/MODULE.bazel\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n",
            "diff --git a/data b/other\nsimilarity index 100%\nrename from data\nrename to other\n",
            "diff --git a/data b/data\nGIT binary patch\nliteral 1\nA00000\n",
        ] {
            assert_eq!(
                patch(body, 1).apply_module(original).unwrap(),
                original,
                "{body}"
            );
        }
        // A mode-only section on MODULE.bazel itself rewrites it as lines.
        let rewritten = patch(
            "diff --git a/MODULE.bazel b/MODULE.bazel\nold mode 100644\nnew mode 100755\n",
            1,
        )
        .apply_module(b"a\r\nb")
        .unwrap();
        assert_eq!(rewritten, b"a\nb\n");
    }

    #[test]
    fn module_projection_reports_bazel_prefix_hint_and_format_errors_of_any_section() {
        let error = patch(
            "--- a/MODULE.bazel\n+++ b/MODULE.bazel\n@@ -1 +1 @@\n-a\n+b\n",
            0,
        )
        .apply_module(b"a\n")
        .unwrap_err();
        assert_eq!(
            error,
            "error at line 1: the patch file contains a/b prefixes, did you forget to set patch_strip = 1?"
        );
        let error = patch("--- a/data\n+++ b/data\n@@ -1,2 +1,2 @@\n-old\n+new\n", 1)
            .apply_module(b"a\n")
            .unwrap_err();
        assert_eq!(error, "Expecting more chunk line at line 6");
        let error = patch("--- a/../escape\n+++ b/../escape\n@@ -1 +1 @@\n-a\n+b\n", 1)
            .apply_module(b"a\n")
            .unwrap_err();
        assert_eq!(
            error,
            "Cannot patch file outside of external repository (/module), file path = \"../escape\" at line 1"
        );
        let error = patch(
            "diff --git a/MODULE.bazel b/other\nrename from MODULE.bazel\nrename to other\n",
            1,
        )
        .apply_module(b"a\n")
        .unwrap_err();
        assert_eq!(
            error,
            "Renaming /module/MODULE.bazel while applying patches to it as a single file is not supported."
        );
    }

    #[test]
    fn captured_messages_for_missing_targets_strip_and_malformed_patches() {
        let t = Temp::new();
        t.write("p.txt", b"a\n");
        for (body, strip, message) in [
            (
                FIX,
                0,
                "Cannot find file to patch (near line 1), old file name (a/p.txt) doesn't exist, new file name (b/p.txt) doesn't exist.",
            ),
            (
                FIX,
                3,
                "Cannot determine file name with strip = 3 at line 1:\n--- a/p.txt",
            ),
            (
                "--- a/p.txt\n+++ b/p.txt\n@@ -1,2 +1,2 @@\n-a\n+b\n",
                1,
                "Expecting more chunk line at line 6",
            ),
            (
                "--- a/p.txt\n+++ b/p.txt\n@@ garbage @@\n-a\n+b\n",
                1,
                "Looks like a unified diff at line 1, but no patch chunk was found.",
            ),
            (
                "--- a/p.txt\n@@ -1 +1 @@\n-a\n+b\n",
                1,
                "The patch content must start with ---/+++ prelude lines at line 1.",
            ),
            (
                "--- a/q.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-a\n",
                1,
                "Cannot find file to patch (near line 1), old file name (q.txt) doesn't exist, new file name is not specified.",
            ),
            (
                "--- a/p.txt\n+++ b/p.txt\n@@ -1 +1 @@\n-a\n-b\n",
                1,
                "Wrong chunk detected near line 5: -b, does not expect a deleted line here.",
            ),
            (
                "diff --git a/p.txt b/p.txt\nnew mode 100x55\n",
                1,
                "Wrong file mode format at line 2: new mode 100x55",
            ),
        ] {
            assert_eq!(failure(t.apply(body, strip).unwrap_err()), message);
        }
        assert_eq!(t.read("p.txt"), b"a\n");
        // A negative Starlark strip strips nothing but is shown.
        let error = parse_signed(b"--- \n+++ b\n", -1)
            .unwrap()
            .apply_tree_detailed(&t.0)
            .unwrap_err();
        assert_eq!(
            failure(error),
            "Cannot determine file name with strip = -1 at line 1:\n--- "
        );
    }

    #[test]
    fn fuzz_skips_applies_partially_and_reports_content_mismatch() {
        let t = Temp::new();
        // B4/mismatch_two_line: with fuzz 1 nothing is compared or changed.
        t.write("p.txt", b"a\nkeep\n");
        t.apply(
            "--- a/p.txt\n+++ b/p.txt\n@@ -1,2 +1,2 @@\n-zzz\n+b\n keep\n",
            1,
        )
        .unwrap();
        assert_eq!(t.read("p.txt"), b"a\nkeep\n");
        // B4/mismatch_context_lines: wrong context, change applied (fuzz 1).
        t.write("p.txt", b"x\na\ny\n");
        t.apply(
            "--- a/p.txt\n+++ b/p.txt\n@@ -1,3 +1,3 @@\n q\n-a\n+b\n r\n",
            1,
        )
        .unwrap();
        assert_eq!(t.read("p.txt"), b"x\nb\ny\n");
        // B4/mismatch_middle.
        t.write("p.txt", b"1\n2\nX\n4\n5\n");
        let error = t
            .apply(
                "--- a/p.txt\n+++ b/p.txt\n@@ -1,5 +1,5 @@\n 1\n 2\n-3\n+B\n 4\n 5\n",
                1,
            )
            .unwrap_err();
        assert_eq!(
            failure(error),
            format!(
                "in patch applied to {}/p.txt: could not apply patch due to CONTENT_DOES_NOT_MATCH_TARGET, error applying change near line 1",
                t.shown()
            )
        );
        // Offsets: the nearest earlier position wins, then later ones.
        t.write("p.txt", b"1\n2\n3\na\n5\n");
        t.apply(
            "--- a/p.txt\n+++ b/p.txt\n@@ -1,3 +1,3 @@\n 3\n-a\n+b\n 5\n",
            1,
        )
        .unwrap();
        assert_eq!(t.read("p.txt"), b"1\n2\n3\nb\n5\n");
        // Each hunk applies separately, last hunk first.
        t.write("p.txt", b"a\nb\nc\nd\ne\nf\n");
        t.apply(
            "--- a/p.txt\n+++ b/p.txt\n@@ -1,2 +1,2 @@\n-a\n+A\n b\n@@ -5,2 +5,2 @@\n e\n-f\n+F\n",
            1,
        )
        .unwrap();
        assert_eq!(t.read("p.txt"), b"A\nb\nc\nd\ne\nF\n");
    }

    #[test]
    fn one_line_fuzz_crash_and_index_errors_are_explicit() {
        let t = Temp::new();
        // B4/mismatch_context: genuine Bazel crashes (exit 37) in subList(1, 0).
        t.write("p.txt", b"a\n");
        let error = t
            .apply("--- a/p.txt\n+++ b/p.txt\n@@ -1 +1 @@\n-zzz\n+b\n", 1)
            .unwrap_err();
        assert_eq!(
            error,
            PatchError::Crash(
                "java.lang.IllegalArgumentException: fromIndex(1) > toIndex(0)".into()
            )
        );
        assert!(error.to_string().contains("genuine Bazel 9.3 crashes"));
        assert_eq!(t.read("p.txt"), b"a\n");
        // verifyChunk reads one line past a short file: IndexOutOfBounds,
        // caught and reported by applyPatchToFile.
        t.write("p.txt", b"a\nb\nc\n");
        let error = t
            .apply(
                "--- a/p.txt\n+++ b/p.txt\n@@ -1,4 +1,4 @@\n a\n b\n c\n-d\n+D\n",
                1,
            )
            .unwrap_err();
        assert_eq!(
            failure(error),
            format!(
                "in patch applied to {}/p.txt: Index 3 out of bounds for length 3, error applying change near line 1",
                t.shown()
            )
        );
    }

    #[test]
    fn creation_deletion_missing_directories_and_existing_targets() {
        let t = Temp::new();
        let create = "--- /dev/null\n+++ b/new/dir/n.txt\n@@ -0,0 +1,2 @@\n+n1\n+n2\n";
        t.apply(create, 1).unwrap();
        assert_eq!(t.read("new/dir/n.txt"), b"n1\nn2\n");
        assert_eq!(t.mode("new/dir/n.txt"), 0o644);
        // Creation over an existing file prepends (InsertDelta at 0).
        t.write("new/dir/n.txt", b"old\n");
        t.apply(create, 1).unwrap();
        assert_eq!(t.read("new/dir/n.txt"), b"n1\nn2\nold\n");
        // A missing output directory is created by the write.
        let nested = Temp::new();
        patch(create, 1)
            .apply_tree_detailed(&nested.0.join("nosuch"))
            .unwrap();
        assert_eq!(nested.read("nosuch/new/dir/n.txt"), b"n1\nn2\n");
        // Deletion removes the file; parents stay.
        t.write("p.txt", b"a\n");
        t.apply("--- a/p.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-a\n", 1)
            .unwrap();
        assert!(!t.exists("p.txt"));
        // A deletion whose remaining content is nonempty rewrites the file.
        t.write("p.txt", b"a\nb\n");
        t.apply("--- a/p.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-a\n", 1)
            .unwrap();
        assert_eq!(t.read("p.txt"), b"b\n");
    }

    #[test]
    fn concatenated_plain_sections_apply_to_the_last_file_only() {
        // B4/multi_file: without a separator the first hunk joins the second
        // section and, with fuzz, changes nothing.
        let t = Temp::new();
        t.write("p.txt", b"a\n");
        t.write("q.txt", b"q\n");
        t.apply(
            &format!("{FIX}--- a/q.txt\n+++ b/q.txt\n@@ -1 +1 @@\n-q\n+Q\n"),
            1,
        )
        .unwrap();
        assert_eq!(t.read("p.txt"), b"a\n");
        assert_eq!(t.read("q.txt"), b"Q\n");
        // Git headers separate sections.
        t.apply(
            "diff --git a/p.txt b/p.txt\n--- a/p.txt\n+++ b/p.txt\n@@ -1 +1 @@\n-a\n+P\ndiff --git a/q.txt b/q.txt\n--- a/q.txt\n+++ b/q.txt\n@@ -1 +1 @@\n-Q\n+R\n",
            1,
        )
        .unwrap();
        assert_eq!(t.read("p.txt"), b"P\n");
        assert_eq!(t.read("q.txt"), b"R\n");
    }

    #[test]
    fn git_modes_renames_and_new_file_modes() {
        let t = Temp::new();
        t.write("x.sh", b"#!/bin/sh\n");
        set_mode(&t.0.join("x.sh"), 0o755).unwrap();
        t.apply(
            "diff --git a/x.sh b/x.sh\nold mode 100755\nnew mode 100644\n",
            1,
        )
        .unwrap();
        assert_eq!(t.mode("x.sh"), 0o644);
        t.apply(
            "diff --git a/x.sh b/x.sh\nold mode 100644\nnew mode 100755\n",
            1,
        )
        .unwrap();
        assert_eq!(t.mode("x.sh"), 0o755);
        t.write("p.txt", b"a\n");
        t.apply(
            "diff --git a/p.txt b/r.txt\nsimilarity index 100%\nrename from p.txt\nrename to r.txt\n",
            1,
        )
        .unwrap();
        assert!(!t.exists("p.txt"));
        assert_eq!(t.read("r.txt"), b"a\n");
        let error = t
            .apply(
                "diff --git a/p.txt b/r.txt\nrename from p.txt\nrename to r.txt\n",
                1,
            )
            .unwrap_err();
        assert_eq!(
            failure(error),
            "Cannot rename file (near line 4), old file name (p.txt) doesn't exist, new file name (r.txt) already exists."
        );
        t.apply(
            "diff --git a/e.sh b/e.sh\nnew file mode 100755\nindex 0000000..1111111\n--- /dev/null\n+++ b/e.sh\n@@ -0,0 +1 @@\n+echo e\n",
            1,
        )
        .unwrap();
        assert_eq!(t.read("e.sh"), b"echo e\n");
        assert_eq!(t.mode("e.sh"), 0o755);
        // Executable modes are kept by content patches.
        t.write("x.sh", b"#!/bin/sh\necho a\n");
        set_mode(&t.0.join("x.sh"), 0o755).unwrap();
        t.apply(
            "--- a/x.sh\n+++ b/x.sh\n@@ -1,2 +1,2 @@\n #!/bin/sh\n-echo a\n+echo b\n",
            1,
        )
        .unwrap();
        assert_eq!(t.mode("x.sh"), 0o755);
    }

    #[test]
    fn text_lines_line_endings_markers_and_malformed_utf8() {
        let t = Temp::new();
        t.write("q.txt", b"a");
        t.apply(
            "--- a/q.txt\n+++ b/q.txt\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n",
            1,
        )
        .unwrap();
        assert_eq!(t.read("q.txt"), b"b\n");
        t.write("p.txt", b"a\r\nz\rend");
        t.apply(
            "--- a/p.txt\r\n+++ b/p.txt\r\n@@ -1 +1 @@\r\n-a\r\n+b\r\n",
            1,
        )
        .unwrap();
        assert_eq!(t.read("p.txt"), b"b\nz\nend\n");
        // UTF-8 decoding replaces malformed bytes (writeLinesAs re-encodes).
        t.write("u.txt", b"\xff\x00x\n");
        t.apply(
            "--- a/u.txt\n+++ b/u.txt\n@@ -1 +1,2 @@\n \u{fffd}\0x\n+caf\u{e9}\n",
            1,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(t.read("u.txt"), b"\xef\xbf\xbd\x00x\ncaf\xc3\xa9\n");
        // Text that is no patch is ignored.
        t.apply("this is not a patch\n", 1).unwrap();
        t.apply("", 1).unwrap();
    }

    #[test]
    fn native_link_safety_profile() {
        use std::os::unix::fs::symlink;
        let t = Temp::new();
        let outside = Temp::new();
        // An in-tree leaf link is read through and replaced (referent kept).
        t.write("t.txt", b"a\n");
        symlink("t.txt", t.0.join("p.txt")).unwrap();
        t.apply(FIX, 1).unwrap();
        assert!(fs::symlink_metadata(t.0.join("p.txt")).unwrap().is_file());
        assert_eq!(t.read("p.txt"), b"b\n");
        assert_eq!(t.read("t.txt"), b"a\n");
        // Escaping leaf links and symlinked ancestors are explicit errors.
        outside.write("p.txt", b"a\n");
        fs::remove_file(t.0.join("p.txt")).unwrap();
        symlink(outside.0.join("p.txt"), t.0.join("p.txt")).unwrap();
        assert!(
            matches!(t.apply(FIX, 1), Err(PatchError::Io(m)) if m.contains("unsafe patch symlink"))
        );
        symlink(&outside.0, t.0.join("dir")).unwrap();
        assert!(matches!(
            t.apply("--- a/dir/p.txt\n+++ b/dir/p.txt\n@@ -1 +1 @@\n-a\n+b\n", 1),
            Err(PatchError::Io(m)) if m.contains("unsafe patch symlink")
        ));
        assert_eq!(outside.read("p.txt"), b"a\n");
        let error = t
            .apply("--- a/../x\n+++ b/../x\n@@ -1 +1 @@\n-a\n+b\n", 1)
            .unwrap_err();
        assert_eq!(
            failure(error),
            format!(
                "Cannot patch file outside of external repository ({}), file path = \"../x\" at line 1",
                t.shown()
            )
        );
    }

    #[test]
    fn hard_links_are_broken_like_bazel() {
        let t = Temp::new();
        t.write("p.txt", b"a\n");
        fs::hard_link(t.0.join("p.txt"), t.0.join("h.txt")).unwrap();
        t.apply(FIX, 1).unwrap();
        assert_eq!(t.read("p.txt"), b"b\n");
        assert_eq!(t.read("h.txt"), b"a\n");
    }

    #[test]
    fn huge_hunk_numbers_crash_bazel_and_large_inputs_have_no_native_cap() {
        let error = patch("--- a/p\n+++ b/p\n@@ -1,99999999999 +1 @@\n-a\n+b\n", 1)
            .apply_module(b"a\n")
            .unwrap_err();
        assert!(error.contains("NumberFormatException"), "{error}");
        // Above the former 16 MiB / 100000-line native caps: PatchUtil has none.
        let body = "x\n".repeat(150_000);
        let lines: String = "+x\n".repeat(150_000);
        let text =
            format!("--- a/MODULE.bazel\n+++ b/MODULE.bazel\n@@ -1 +1,150000 @@\n-x\n{lines}");
        let patched = patch(&text, 1).apply_module(b"x\n").unwrap();
        assert!(
            patched == body.as_bytes(),
            "patched length {}",
            patched.len()
        );
        parse(&vec![b'x'; 17 * 1024 * 1024], 0).unwrap();
    }
}
