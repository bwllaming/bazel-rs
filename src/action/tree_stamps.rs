//! Directory stamps of a cache entry's published trees (docs/HARDLINK_STAGING.md,
//! "Cache checks of published trees").
//!
//! A published tree artifact (and a published runfiles tree) is a directory
//! hierarchy whose files are read-only store inodes and whose directories
//! are read-only. A cache check used to walk every such tree on every
//! command — list each directory, inspect every entry, look its digest up —
//! which on a TypeScript build (npm package trees) was millions of `statx`
//! calls per no-op build. Once a check found a tree valid, the entry keeps
//! the identity and timestamps of each of the tree's directories next to
//! its manifest (`actions/<key>/tree-stamps`): a directory's change time
//! moves with every entry added, removed or renamed in it (and with every
//! change of its own mode or times), so while every directory still shows
//! its stamp — same device, inode, mode, modification and change time —
//! the tree still has exactly the names, and each name exactly the inode,
//! that the full check found. The next check then inspects only the
//! directories and takes the files from the manifest (digests and modes)
//! and the stamp (sizes), like the per-process memos take a published
//! output's metadata once per command. The files' contents follow the
//! trust of every published output (docs/DIGEST_MEMO.md, "Trust"): they are
//! read-only store inodes, and an in-place edit of one is not re-detected
//! by a cache check.
//!
//! A stamp is recorded only for directories that were settled when the
//! check started (modification and change time at least
//! crate::digest_memo::RACY_MARGIN older) and unchanged by the end of the
//! full check, so that no later change can show the stamped state again.
//! A missing, foreign or corrupt stamp file, or a stamp that no longer
//! matches, means the full check (which records a new stamp); stamps are
//! written only when they change, so a no-op build writes nothing.
//! `BAZEL_RS_VERIFY_OUTPUTS=full` always takes the full check.

use super::{
    Input, RunfilesTreeManifest, Sha256Digest, Tree, TreeManifest, io_error, tree_valid, unhex,
};
use crate::dir_stamps::{DirectoryStamp, open_directory, walk};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// The stamp file in an action entry, next to `manifest.json`.
pub(super) const FILE_NAME: &str = "tree-stamps";
const MAGIC: &[u8; 8] = b"BZRSTSTP";
const VERSION: u32 = 1;
/// Magic, version, the first 8 bytes of the body's blake3 hash.
const HEADER_BYTES: usize = 8 + 4 + 8;

/// Kinds of stamped trees.
const TREE: u8 = 0;
const RUNFILES: u8 = 1;

/// The stamp of one published tree of the entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    /// The output's index in the manifest.
    output: u32,
    kind: u8,
    /// The root, then the tree's directories in manifest order.
    directories: Vec<DirectoryStamp>,
    /// A tree artifact's file sizes in manifest order (none for runfiles).
    sizes: Vec<u64>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StampFile {
    /// blake3 of the manifest's bytes: stamps of another manifest never
    /// count.
    manifest: [u8; 32],
    stamps: Vec<Stamp>,
}

/// Counters under `BAZEL_RS_TIMING` (crate::action::report_staging): trees
/// checked by their stamps, trees checked in full, stamp files written.
pub(super) static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// `BAZEL_RS_VERIFY_OUTPUTS=full`: never trust a stamp.
fn verify_full() -> bool {
    static FULL: OnceLock<bool> = OnceLock::new();
    *FULL.get_or_init(|| {
        std::env::var_os("BAZEL_RS_VERIFY_OUTPUTS").is_some_and(|value| value == "full")
    })
}

/// The stamps of one cache entry's check ([`super::cache_valid`]).
pub(super) struct EntryStamps<'a> {
    entry: &'a Path,
    manifest: [u8; 32],
    start: SystemTime,
    /// The stored stamps, read on first use (`None` inside: none usable).
    stored: Option<Option<StampFile>>,
    fresh: Vec<Stamp>,
}

impl<'a> EntryStamps<'a> {
    pub(super) fn new(entry: &'a Path, manifest_bytes: &[u8]) -> Self {
        Self {
            entry,
            manifest: *blake3::hash(manifest_bytes).as_bytes(),
            start: SystemTime::now(),
            stored: None,
            fresh: Vec::new(),
        }
    }

    fn stored(&mut self, output: u32, kind: u8) -> Option<&Stamp> {
        if verify_full() {
            return None;
        }
        let (entry, manifest) = (self.entry, self.manifest);
        self.stored
            .get_or_insert_with(|| {
                read(&entry.join(FILE_NAME)).filter(|file| file.manifest == manifest)
            })
            .as_ref()?
            .stamps
            .iter()
            .find(|stamp| stamp.output == output && stamp.kind == kind)
    }

    /// [`tree_valid`] of the published tree at `path` (output `output`),
    /// by its stamp when every directory still shows it.
    pub(super) fn tree(
        &mut self,
        output: usize,
        path: &Path,
        expected: &TreeManifest,
        root: &Path,
    ) -> Result<Option<Tree>, String> {
        let output = u32::try_from(output).unwrap_or(u32::MAX);
        let Some(names) = names(&expected.directories) else {
            return tree_valid(path, expected, root);
        };
        if let Some(stamp) = self.stored(output, TREE)
            && stamp.sizes.len() == expected.files.len()
            && matches(path, &names, &stamp.directories, root)
            && let Some(tree) = stamped_tree(path, expected, &names, &stamp.sizes)
        {
            let stamp = stamp.clone();
            self.fresh.push(stamp);
            COUNTS[0].fetch_add(1, Ordering::Relaxed);
            return Ok(Some(tree));
        }
        COUNTS[1].fetch_add(1, Ordering::Relaxed);
        let before = stamps(path, &names);
        let tree = tree_valid(path, expected, root)?;
        if let (Some(tree), Some(before)) = (&tree, before)
            && before.iter().all(|stamp| stamp.settled(self.start))
            && stamps(path, &names).as_ref() == Some(&before)
            && let Some(sizes) = sizes(tree, expected)
        {
            self.fresh.push(Stamp {
                output,
                kind: TREE,
                directories: before,
                sizes,
            });
        }
        Ok(tree)
    }

    /// Whether the published runfiles tree `directory` still has exactly
    /// the entries of `expected` (`listed`: the full check,
    /// crate::action::runfiles_listing), by its stamp when every directory
    /// still shows it.
    pub(super) fn runfiles(
        &mut self,
        output: usize,
        directory: &Path,
        expected: &RunfilesTreeManifest,
        root: &Path,
        listed: impl FnOnce() -> bool,
    ) -> bool {
        let output = u32::try_from(output).unwrap_or(u32::MAX);
        let directories: Option<Vec<&String>> = Some(
            expected
                .entries
                .iter()
                .filter(|(_, kind)| kind == "dir")
                .map(|(name, _)| name)
                .collect(),
        );
        let Some(names) = directories.and_then(names) else {
            return listed();
        };
        if let Some(stamp) = self.stored(output, RUNFILES)
            && matches(directory, &names, &stamp.directories, root)
        {
            let stamp = stamp.clone();
            self.fresh.push(stamp);
            COUNTS[0].fetch_add(1, Ordering::Relaxed);
            return true;
        }
        COUNTS[1].fetch_add(1, Ordering::Relaxed);
        let before = stamps(directory, &names);
        if !listed() {
            return false;
        }
        if let Some(before) = before
            && before.iter().all(|stamp| stamp.settled(self.start))
            && stamps(directory, &names).as_ref() == Some(&before)
        {
            self.fresh.push(Stamp {
                output,
                kind: RUNFILES,
                directories: before,
                sizes: Vec::new(),
            });
        }
        true
    }

    /// The entry is valid: keep the stamps of this check when they differ
    /// from the stored ones (`write`: the caller holds the entry's lock;
    /// otherwise only whether they differ is returned). Failing to write
    /// them only costs the next check its shortcut.
    pub(super) fn finish(mut self, write: bool) -> bool {
        if self.stored.is_none() && self.fresh.is_empty() {
            return false;
        }
        let stored = self
            .stored
            .take()
            .unwrap_or_else(|| read(&self.entry.join(FILE_NAME)));
        self.fresh.sort_by_key(|stamp| (stamp.output, stamp.kind));
        if stored
            .as_ref()
            .is_some_and(|stored| stored.manifest == self.manifest && stored.stamps == self.fresh)
            || (stored.is_none() && self.fresh.is_empty())
        {
            return false;
        }
        if !write {
            return true;
        }
        let file = StampFile {
            manifest: self.manifest,
            stamps: std::mem::take(&mut self.fresh),
        };
        if write_file(&self.entry.join(FILE_NAME), &file).is_ok() {
            COUNTS[2].fetch_add(1, Ordering::Relaxed);
        }
        false
    }
}

/// The hex-spelled relative directory names, decoded (`None` if one is not
/// exactly a lowercase hex spelling).
fn names<'s>(spelled: impl IntoIterator<Item = &'s String>) -> Option<Vec<PathBuf>> {
    spelled
        .into_iter()
        .map(|name| {
            let bytes = unhex(name)?;
            (super::hex(&bytes) == *name).then(|| PathBuf::from(OsStr::from_bytes(&bytes)))
        })
        .collect()
}

/// The stamps of `root` and of each of `names` below it (`None` when one is
/// not a real directory or cannot be inspected).
fn stamps(root: &Path, names: &[PathBuf]) -> Option<Vec<DirectoryStamp>> {
    std::iter::once(root.to_path_buf())
        .chain(names.iter().map(|name| root.join(name)))
        .map(|path| DirectoryStamp::at(&path))
        .collect()
}

/// Whether `root` (a real directory with no linked ancestor, like every
/// check of a published tree) and its directories `names` still show
/// `expected`, stopping at the first difference (crate::dir_stamps::walk:
/// one-component lookups from each parent's descriptor).
fn matches(root: &Path, names: &[PathBuf], expected: &[DirectoryStamp], cache: &Path) -> bool {
    if expected.len() != names.len() + 1 {
        return false;
    }
    let Ok(root_file) = open_directory(None, root) else {
        return false;
    };
    let Ok(metadata) = root_file
        .metadata()
        .map_err(|e| e.to_string())
        .and_then(|leaf| super::checked_path(root, Some(cache), Some(leaf)))
    else {
        return false;
    };
    if DirectoryStamp::of(&metadata).as_ref() != Some(&expected[0]) {
        return false;
    }
    let mut all = true;
    walk(root_file, names, |index, stamp| {
        all = stamp.as_ref() == Some(&expected[index + 1]);
        all
    });
    all
}

/// The tree [`tree_valid`] found at `path` when it recorded the stamp: its
/// files with the manifest's digests and modes and the stamp's sizes, at
/// their published paths; no links (a published tree has none).
fn stamped_tree(
    path: &Path,
    expected: &TreeManifest,
    directories: &[PathBuf],
    sizes: &[u64],
) -> Option<Tree> {
    let modes: BTreeMap<&str, u32> = expected
        .modes
        .iter()
        .map(|(name, mode)| (name.as_str(), *mode))
        .collect();
    let mut files = BTreeMap::new();
    // Built from the manifest, decoded exactly (every name and digest is its
    // own lowercase spelling, no name twice): what the full check accepted
    // when it recorded the stamp of this manifest.
    for ((name, digest_text), size) in expected.files.iter().zip(sizes) {
        let bytes = unhex(name)?;
        if super::hex(&bytes) != *name {
            return None;
        }
        let digest = Sha256Digest(unhex(digest_text)?.try_into().ok()?);
        if !digest.spelled(digest_text) {
            return None;
        }
        let mode = modes
            .get(name.as_str())
            .copied()
            .unwrap_or(super::OUTPUT_MODE);
        if mode != 0o444 | (mode & 0o111) {
            return None;
        }
        let name = PathBuf::from(OsStr::from_bytes(&bytes));
        let input = Input {
            path: path.join(&name),
            digest,
            size: *size,
            executable: mode & 0o111,
            mode,
        };
        if files.insert(name, input).is_some() {
            return None;
        }
    }
    let tree = Tree {
        files,
        directories: directories.iter().cloned().collect::<BTreeSet<_>>(),
        links: BTreeSet::new(),
    };
    Some(tree)
}

/// A valid tree's file sizes in manifest order.
fn sizes(tree: &Tree, expected: &TreeManifest) -> Option<Vec<u64>> {
    expected
        .files
        .iter()
        .map(|(name, _)| {
            let name = PathBuf::from(OsStr::from_bytes(&unhex(name)?));
            tree.files.get(&name).map(|input| input.size)
        })
        .collect()
}

/// A stamp file, or `None` when it is missing, foreign or corrupt.
fn read(path: &Path) -> Option<StampFile> {
    let bytes = fs::read(path).ok()?;
    let (header, body) = bytes.split_at_checked(HEADER_BYTES)?;
    if &header[..8] != MAGIC
        || header[8..12] != VERSION.to_le_bytes()
        || header[12..20] != blake3::hash(body).as_bytes()[..8]
    {
        return None;
    }
    postcard::from_bytes(body).ok()
}

/// Write a stamp file atomically (a temporary file renamed over it).
fn write_file(path: &Path, file: &StampFile) -> Result<(), String> {
    let body = postcard::to_stdvec(file).map_err(|e| e.to_string())?;
    let mut bytes = Vec::with_capacity(HEADER_BYTES + body.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&blake3::hash(&body).as_bytes()[..8]);
    bytes.extend_from_slice(&body);
    let temporary = path.with_file_name(format!(".{FILE_NAME}.tmp-{}", std::process::id()));
    let mut out =
        fs::File::create(&temporary).map_err(|e| io_error("cannot write", &temporary, e))?;
    out.write_all(&bytes)
        .and_then(|()| fs::rename(&temporary, path))
        .map_err(|e| {
            let _ = fs::remove_file(&temporary);
            io_error("cannot write", path, e)
        })
}
